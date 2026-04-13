//! `VirtioBlock` device — request dispatch, queue handling, `VirtioDevice` impl.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use arcbox_virtio_core::error::{Result, VirtioError};
use arcbox_virtio_core::queue::{Descriptor, VirtQueue};
use arcbox_virtio_core::{QueueConfig, VirtioDevice, VirtioDeviceId, virtio_bindings};

use crate::request::{BlockConfig, BlockRequestHeader, BlockRequestType, BlockStatus};

/// `VirtIO` block device.
pub struct VirtioBlock {
    config: BlockConfig,
    features: u64,
    acked_features: u64,
    /// Backing file handle (for flush/close).
    file: Option<Arc<RwLock<File>>>,
    /// Raw fd for pread/pwrite — avoids seek+lock on every I/O.
    raw_fd: Option<std::os::unix::io::RawFd>,
    /// Request queue.
    queue: Option<VirtQueue>,
    /// Device ID string.
    device_id: String,
    /// Last-seen available ring index for guest-memory queue processing.
    last_avail_idx: u16,
}

impl VirtioBlock {
    // Feature bits sourced from `virtio_bindings::virtio_blk`.
    // The crate exports bit *positions*, so we shift 1 left by that position.

    /// Feature: Maximum segment size.
    pub const FEATURE_SIZE_MAX: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_SIZE_MAX;
    /// Feature: Maximum number of segments.
    pub const FEATURE_SEG_MAX: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_SEG_MAX;
    /// Feature: Disk geometry.
    pub const FEATURE_GEOMETRY: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_GEOMETRY;
    /// Feature: Read-only.
    pub const FEATURE_RO: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_RO;
    /// Feature: Block size.
    pub const FEATURE_BLK_SIZE: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_BLK_SIZE;
    /// Feature: Flush command.
    pub const FEATURE_FLUSH: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_FLUSH;
    /// Feature: Topology.
    pub const FEATURE_TOPOLOGY: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_TOPOLOGY;
    /// Feature: Configuration writeback.
    pub const FEATURE_CONFIG_WCE: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_CONFIG_WCE;
    /// Feature: Discard command.
    pub const FEATURE_DISCARD: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_DISCARD;
    /// Feature: Write zeroes command.
    pub const FEATURE_WRITE_ZEROES: u64 =
        1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_WRITE_ZEROES;
    /// Feature: Multiple request queues.
    pub const FEATURE_MQ: u64 = 1 << virtio_bindings::virtio_blk::VIRTIO_BLK_F_MQ;
    /// `VirtIO` 1.0 feature.
    pub const FEATURE_VERSION_1: u64 = 1 << virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;

    /// Creates a new block device.
    #[must_use]
    pub fn new(config: BlockConfig) -> Self {
        let mut features = Self::FEATURE_SIZE_MAX
            | Self::FEATURE_SEG_MAX
            | Self::FEATURE_BLK_SIZE
            | Self::FEATURE_FLUSH
            | Self::FEATURE_VERSION_1
            | arcbox_virtio_core::queue::VIRTIO_F_EVENT_IDX;

        if config.read_only {
            features |= Self::FEATURE_RO;
        }
        if config.num_queues > 1 {
            features |= Self::FEATURE_MQ;
        }

        let device_id = format!(
            "arcbox-blk-{}",
            config
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        );

        Self {
            config,
            features,
            acked_features: 0,
            file: None,
            raw_fd: None,
            queue: None,
            device_id,
            last_avail_idx: 0,
        }
    }

    /// Creates a new block device from a file path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or its size cannot be determined.
    pub fn from_path(path: impl Into<PathBuf>, read_only: bool) -> Result<Self> {
        let path = path.into();

        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(&path)
            .map_err(|e| VirtioError::Io(format!("Failed to open {}: {}", path.display(), e)))?;

        let metadata = file
            .metadata()
            .map_err(|e| VirtioError::Io(format!("Failed to get metadata: {e}")))?;

        let capacity = metadata.len() / 512;

        let config = BlockConfig {
            capacity,
            blk_size: 512,
            path: path.clone(),
            read_only,
            num_queues: 1,
        };

        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();

        // Disable page cache on macOS for large disk images.
        #[cfg(target_os = "macos")]
        // SAFETY: F_NOCACHE on a fd we own; ignored on failure.
        unsafe {
            libc::fcntl(fd, libc::F_NOCACHE, 1);
        }

        let mut device = Self::new(config);
        device.raw_fd = Some(fd);
        device.file = Some(Arc::new(RwLock::new(file)));

        tracing::info!(
            "Created block device from {}: capacity={} sectors ({} bytes)",
            path.display(),
            capacity,
            capacity * 512
        );

        Ok(device)
    }

    /// Returns the disk capacity in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.config.capacity * u64::from(self.config.blk_size)
    }

    /// Returns the raw fd for pread/pwrite (if activated).
    #[must_use]
    pub fn raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.raw_fd
    }

    /// Returns the block size in bytes.
    #[must_use]
    pub fn blk_size(&self) -> u32 {
        self.config.blk_size
    }

    /// Returns true if the device is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.config.read_only
    }

    /// Returns the device ID string.
    #[must_use]
    pub fn device_id_string(&self) -> &str {
        &self.device_id
    }

    /// Returns the number of request queues.
    #[must_use]
    pub fn num_queues(&self) -> u16 {
        self.config.num_queues
    }

    /// Sets the number of request queues (must call before activate).
    pub fn set_num_queues(&mut self, n: u16) {
        self.config.num_queues = n.max(1);
        if self.config.num_queues > 1 {
            self.features |= Self::FEATURE_MQ;
        }
    }

    /// Handles a block request.
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be processed.
    pub fn handle_request(&self, header: &BlockRequestHeader, data: &mut [u8]) -> Result<usize> {
        let request_type = BlockRequestType::try_from(header.request_type)?;

        match request_type {
            BlockRequestType::In => self.handle_read(header.sector, data),
            BlockRequestType::Out => self.handle_write(header.sector, data),
            BlockRequestType::Flush => self.handle_flush(),
            BlockRequestType::GetId => self.handle_get_id(data),
            BlockRequestType::Discard | BlockRequestType::WriteZeroes => {
                Err(VirtioError::NotSupported("Operation not supported".into()))
            }
        }
    }

    /// Reads from disk using pread — no seek, no lock, position-independent.
    fn handle_read(&self, sector: u64, data: &mut [u8]) -> Result<usize> {
        let fd = self
            .raw_fd
            .ok_or_else(|| VirtioError::NotReady("Block device not activated".into()))?;

        #[allow(clippy::cast_possible_wrap)]
        let offset = (sector * u64::from(self.config.blk_size)) as libc::off_t;
        // SAFETY: fd is valid, data is a valid mutable buffer.
        let n = unsafe {
            libc::pread(
                fd,
                data.as_mut_ptr().cast::<libc::c_void>(),
                data.len(),
                offset,
            )
        };
        if n < 0 {
            return Err(VirtioError::Io(format!(
                "pread failed at sector {}: {}",
                sector,
                std::io::Error::last_os_error()
            )));
        }
        Ok(n as usize)
    }

    /// Writes to disk using pwrite — no seek, no lock, position-independent.
    fn handle_write(&self, sector: u64, data: &[u8]) -> Result<usize> {
        if self.config.read_only {
            return Err(VirtioError::InvalidOperation("Device is read-only".into()));
        }

        let fd = self
            .raw_fd
            .ok_or_else(|| VirtioError::NotReady("Block device not activated".into()))?;

        #[allow(clippy::cast_possible_wrap)]
        let offset = (sector * u64::from(self.config.blk_size)) as libc::off_t;
        // SAFETY: fd is valid, data is a valid buffer.
        let n =
            unsafe { libc::pwrite(fd, data.as_ptr().cast::<libc::c_void>(), data.len(), offset) };
        if n < 0 {
            return Err(VirtioError::Io(format!(
                "pwrite failed at sector {}: {}",
                sector,
                std::io::Error::last_os_error()
            )));
        }
        Ok(n as usize)
    }

    fn handle_flush(&self) -> Result<usize> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| VirtioError::NotReady("Block device not activated".into()))?;

        let file = file
            .write()
            .map_err(|e| VirtioError::Io(format!("Failed to lock file: {e}")))?;

        file.sync_all()
            .map_err(|e| VirtioError::Io(format!("Flush failed: {e}")))?;

        tracing::trace!("Flushed block device");
        Ok(0)
    }

    fn handle_get_id(&self, data: &mut [u8]) -> Result<usize> {
        let id_bytes = self.device_id.as_bytes();
        let len = id_bytes.len().min(data.len());
        data[..len].copy_from_slice(&id_bytes[..len]);
        Ok(len)
    }

    /// Processes a descriptor chain from the virtqueue.
    ///
    /// # Errors
    ///
    /// Returns an error if processing fails.
    pub fn process_descriptor_chain(
        &self,
        descriptors: &[Descriptor],
        memory: &mut [u8],
    ) -> Result<(usize, BlockStatus)> {
        if descriptors.is_empty() {
            return Err(VirtioError::InvalidQueue("Empty descriptor chain".into()));
        }

        let header_desc = &descriptors[0];
        let header_start = header_desc.addr as usize;
        let header_end = header_start + BlockRequestHeader::SIZE;

        if header_end > memory.len() {
            return Err(VirtioError::InvalidQueue("Header out of bounds".into()));
        }

        let header = BlockRequestHeader::from_bytes(&memory[header_start..header_end])
            .ok_or_else(|| VirtioError::InvalidQueue("Failed to parse header".into()))?;

        let request_type = BlockRequestType::try_from(header.request_type)?;

        match request_type {
            BlockRequestType::In => {
                let mut total_bytes = 0;
                let mut current_sector = header.sector;
                for desc in descriptors.iter().skip(1) {
                    if !desc.is_write_only() {
                        continue;
                    }
                    if desc.len == 1 {
                        continue; // Status byte
                    }

                    let start = desc.addr as usize;
                    let end = start + desc.len as usize;
                    if end > memory.len() {
                        return Err(VirtioError::InvalidQueue("Data out of bounds".into()));
                    }

                    let data = &mut memory[start..end];
                    match self.handle_read(current_sector, data) {
                        Ok(n) => {
                            total_bytes += n;
                            current_sector += (n as u64) / 512;
                        }
                        Err(_) => return Ok((0, BlockStatus::IoErr)),
                    }
                }
                Ok((total_bytes, BlockStatus::Ok))
            }
            BlockRequestType::Out => {
                let mut total_bytes = 0;
                for desc in descriptors.iter().skip(1) {
                    if desc.is_write_only() {
                        continue;
                    }

                    let start = desc.addr as usize;
                    let end = start + desc.len as usize;
                    if end > memory.len() {
                        return Err(VirtioError::InvalidQueue("Data out of bounds".into()));
                    }

                    let data = &memory[start..end];
                    match self.handle_write(header.sector + (total_bytes as u64 / 512), data) {
                        Ok(n) => total_bytes += n,
                        Err(_) => return Ok((0, BlockStatus::IoErr)),
                    }
                }
                Ok((total_bytes, BlockStatus::Ok))
            }
            BlockRequestType::Flush => match self.handle_flush() {
                Ok(_) => Ok((0, BlockStatus::Ok)),
                Err(_) => Ok((0, BlockStatus::IoErr)),
            },
            _ => Ok((0, BlockStatus::Unsupp)),
        }
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_id(&self) -> VirtioDeviceId {
        VirtioDeviceId::Block
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, features: u64) {
        self.acked_features = self.features & features;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // VirtIO 1.1 config space layout:
        // 0:  capacity (u64)
        // 8:  size_max (u32)
        // 12: seg_max (u32)
        // 16: geometry (4 bytes)
        // 20: blk_size (u32)
        // 24: topology (10 bytes)
        // 34: num_queues (u16, only with VIRTIO_BLK_F_MQ)
        let config_data = [
            self.config.capacity.to_le_bytes().as_slice(),
            &(1u32 << 12).to_le_bytes(), // size_max: 4KB
            &128u32.to_le_bytes(),       // seg_max: 128 segments
            &[0u8; 4],                   // geometry: not used
            &self.config.blk_size.to_le_bytes(),
            &[0u8; 10], // topology: not used
            &self.config.num_queues.to_le_bytes(),
        ]
        .concat();

        let offset = offset as usize;
        let len = data.len().min(config_data.len().saturating_sub(offset));
        if len > 0 {
            data[..len].copy_from_slice(&config_data[offset..offset + len]);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // Block device config is read-only
    }

    fn activate(&mut self) -> Result<()> {
        if self.file.is_none() && !self.config.path.as_os_str().is_empty() {
            let file = OpenOptions::new()
                .read(true)
                .write(!self.config.read_only)
                .open(&self.config.path)
                .map_err(|e| {
                    VirtioError::Io(format!(
                        "Failed to open {}: {}",
                        self.config.path.display(),
                        e
                    ))
                })?;

            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();

            // F_NOCACHE is intentionally NOT set for data disks here:
            // container metadata scanning benefits from page-cache warmup, and
            // the pread/pwrite path (no seek, no RwLock) is the primary
            // performance win over the older seek+lock+read pattern.

            self.raw_fd = Some(fd);
            self.file = Some(Arc::new(RwLock::new(file)));
        }

        let event_idx = (self.acked_features & arcbox_virtio_core::queue::VIRTIO_F_EVENT_IDX) != 0;
        let mut queue = VirtQueue::new(256)?;
        queue.set_event_idx(event_idx);
        self.queue = Some(queue);

        tracing::info!("VirtIO block device activated: {:?}", self.config.path);
        Ok(())
    }

    fn reset(&mut self) {
        self.acked_features = 0;
        self.queue = None;
        self.last_avail_idx = 0;
        // Keep file handle open for quick reactivation
    }

    fn process_queue(
        &mut self,
        _queue_idx: u16,
        memory: &mut [u8],
        queue_config: &QueueConfig,
    ) -> Result<Vec<(u16, u32)>> {
        if !queue_config.ready || queue_config.size == 0 {
            return Ok(Vec::new());
        }

        // Translate GPAs to slice offsets by subtracting gpa_base (checked to
        // guard against a malicious guest providing a GPA below the RAM base).
        let gpa_base = queue_config.gpa_base as usize;
        let desc_table_addr = (queue_config.desc_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid desc GPA {:#x} below ram base {:#x}",
                    queue_config.desc_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("desc GPA below ram base".into())
            })?;
        let avail_addr = (queue_config.avail_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid avail GPA {:#x} below ram base {:#x}",
                    queue_config.avail_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("avail GPA below ram base".into())
            })?;
        let used_addr = (queue_config.used_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid used GPA {:#x} below ram base {:#x}",
                    queue_config.used_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("used GPA below ram base".into())
            })?;
        let q_size = queue_config.size as usize;

        if avail_addr + 4 + 2 * q_size > memory.len() {
            return Err(VirtioError::InvalidQueue("avail ring out of bounds".into()));
        }
        let avail_idx = u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]);

        let last_avail = self.last_avail_idx;

        let mut completions = Vec::new();

        let mut current_avail = last_avail;
        while current_avail != avail_idx {
            let ring_offset = avail_addr + 4 + 2 * ((current_avail as usize) % q_size);
            let head_idx =
                u16::from_le_bytes([memory[ring_offset], memory[ring_offset + 1]]) as usize;

            // Walk the descriptor chain starting at head_idx.
            let mut descriptors = Vec::new();
            let mut idx = head_idx;
            loop {
                let desc_offset = desc_table_addr + idx * 16;
                if desc_offset + 16 > memory.len() {
                    return Err(VirtioError::InvalidQueue("descriptor out of bounds".into()));
                }
                let raw_gpa =
                    u64::from_le_bytes(memory[desc_offset..desc_offset + 8].try_into().unwrap());
                let addr = match raw_gpa.checked_sub(gpa_base as u64) {
                    Some(a) => a,
                    None => {
                        tracing::warn!(
                            "invalid descriptor GPA {:#x} below ram base {:#x}",
                            raw_gpa,
                            gpa_base
                        );
                        break;
                    }
                };
                let len = u32::from_le_bytes(
                    memory[desc_offset + 8..desc_offset + 12]
                        .try_into()
                        .unwrap(),
                );
                let flags = u16::from_le_bytes(
                    memory[desc_offset + 12..desc_offset + 14]
                        .try_into()
                        .unwrap(),
                );
                let next = u16::from_le_bytes(
                    memory[desc_offset + 14..desc_offset + 16]
                        .try_into()
                        .unwrap(),
                );

                descriptors.push(Descriptor {
                    addr,
                    len,
                    flags,
                    next,
                });

                if flags & arcbox_virtio_core::queue::flags::NEXT == 0 {
                    break;
                }
                idx = next as usize;
                if idx >= q_size {
                    return Err(VirtioError::InvalidQueue(
                        "descriptor next index out of bounds".into(),
                    ));
                }
            }

            let (bytes, status) = self.process_descriptor_chain(&descriptors, memory)?;

            // Write the status byte into the last writable descriptor.
            if let Some(last_wr) = descriptors.iter().rev().find(|d| d.is_write_only()) {
                let status_offset = last_wr.addr as usize + last_wr.len as usize - 1;
                if status_offset < memory.len() {
                    memory[status_offset] = status as u8;
                }
            }

            // Push completion into the used ring.
            let used_idx_offset = used_addr + 2;
            let used_idx =
                u16::from_le_bytes([memory[used_idx_offset], memory[used_idx_offset + 1]]);
            let used_ring_entry = used_addr + 4 + ((used_idx as usize) % q_size) * 8;
            if used_ring_entry + 8 <= memory.len() {
                memory[used_ring_entry..used_ring_entry + 4]
                    .copy_from_slice(&(head_idx as u32).to_le_bytes());
                memory[used_ring_entry + 4..used_ring_entry + 8]
                    .copy_from_slice(&(bytes as u32).to_le_bytes());
                // Write barrier: ensure ring entry is visible before idx update.
                // ARM64 weak memory ordering requires this for correct guest observation.
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                let new_used_idx = used_idx.wrapping_add(1);
                memory[used_idx_offset..used_idx_offset + 2]
                    .copy_from_slice(&new_used_idx.to_le_bytes());
            }

            completions.push((head_idx as u16, bytes as u32));
            current_avail = current_avail.wrapping_add(1);
        }

        self.last_avail_idx = current_avail;

        // When VIRTIO_F_EVENT_IDX is negotiated, set avail_event = current
        // avail_idx so the driver notifies on the very next request.
        // avail_event lives at used_ring + 4 + 8 * queue_size.
        if !completions.is_empty() {
            let avail_event_offset = used_addr + 4 + 8 * q_size;
            if avail_event_offset + 2 <= memory.len() {
                memory[avail_event_offset..avail_event_offset + 2]
                    .copy_from_slice(&current_avail.to_le_bytes());
            }
        }

        Ok(completions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_block_device_creation() {
        let config = BlockConfig {
            capacity: 2048,
            blk_size: 512,
            path: PathBuf::from("/tmp/test.img"),
            read_only: false,
            num_queues: 1,
        };

        let device = VirtioBlock::new(config);
        assert_eq!(device.capacity_bytes(), 2048 * 512);
    }

    #[test]
    fn test_block_device_from_file() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();
        assert_eq!(device.config.capacity, 8); // 4096 / 512
    }

    #[test]
    fn test_block_config_features() {
        let ro_config = BlockConfig {
            capacity: 1024,
            blk_size: 512,
            path: PathBuf::new(),
            read_only: true,
            num_queues: 1,
        };

        let device = VirtioBlock::new(ro_config);
        assert!(device.features() & VirtioBlock::FEATURE_RO != 0);
    }

    #[test]
    fn test_read_write() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        let write_data = b"Hello, VirtIO!";
        device.handle_write(0, write_data).unwrap();

        let mut read_data = vec![0u8; write_data.len()];
        device.handle_read(0, &mut read_data).unwrap();

        assert_eq!(&read_data, write_data);
    }

    #[test]
    fn test_block_device_not_activated() {
        let config = BlockConfig {
            capacity: 1024,
            blk_size: 512,
            path: PathBuf::new(),
            read_only: false,
            num_queues: 1,
        };

        let device = VirtioBlock::new(config);

        let mut buf = vec![0u8; 512];
        let result = device.handle_read(0, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_block_device_read_only_write() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), true).unwrap();

        let result = device.handle_write(0, b"test");
        assert!(result.is_err());
    }

    #[test]
    fn test_block_device_flush() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        assert!(device.handle_flush().is_ok());
    }

    #[test]
    fn test_block_device_get_id() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        let mut buf = vec![0u8; 64];
        let len = device.handle_get_id(&mut buf).unwrap();
        assert!(len > 0);

        let id = String::from_utf8_lossy(&buf[..len]);
        assert!(id.starts_with("arcbox-blk-"));
    }

    #[test]
    fn test_block_device_activate_and_reset() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let mut device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        device.activate().unwrap();
        assert!(device.queue.is_some());

        device.reset();
        assert!(device.queue.is_none());
        assert_eq!(device.acked_features, 0);
    }

    #[test]
    fn test_block_config_space() {
        let config = BlockConfig {
            capacity: 2048,
            blk_size: 512,
            path: PathBuf::new(),
            read_only: false,
            num_queues: 1,
        };

        let device = VirtioBlock::new(config);

        let mut buf = [0u8; 8];
        device.read_config(0, &mut buf);
        let capacity = u64::from_le_bytes(buf);
        assert_eq!(capacity, 2048);

        let mut buf = [0u8; 4];
        device.read_config(20, &mut buf);
        let blk_size = u32::from_le_bytes(buf);
        assert_eq!(blk_size, 512);
    }

    #[test]
    fn test_block_device_large_io() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let size = 1024 * 1024; // 1MB
        temp_file.write_all(&vec![0u8; size]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        let write_data = vec![0xAB; 65536];
        device.handle_write(0, &write_data).unwrap();

        let mut read_data = vec![0u8; 65536];
        device.handle_read(0, &mut read_data).unwrap();
        assert_eq!(read_data, write_data);
    }

    #[test]
    fn test_block_device_sector_alignment() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 8192]).unwrap();

        let device = VirtioBlock::from_path(temp_file.path(), false).unwrap();

        let write_data = b"Sector2Data";
        device.handle_write(2, write_data).unwrap();

        let mut read_data = vec![0u8; write_data.len()];
        device.handle_read(2, &mut read_data).unwrap();
        assert_eq!(&read_data, write_data);

        let mut sector0 = vec![0u8; 512];
        device.handle_read(0, &mut sector0).unwrap();
        assert!(sector0.iter().all(|&b| b == 0));
    }
}
