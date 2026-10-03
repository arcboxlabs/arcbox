//! HVC fast-path block I/O.
//!
//! The guest's ArcBox block driver issues an HVC with a vendor-specific
//! SMCCC function ID (0xC200_XXXX range) instead of walking a virtqueue.
//! The hypervisor translates the buffer GPA to a host pointer and performs
//! a synchronous pread/pwrite/fsync directly against the backing file, and
//! punches DISCARDed ranges out of it so the sparse image shrinks with the
//! guest filesystem. Returns bytes transferred in X0, or a negative errno on
//! failure.

use std::sync::Arc;

use arcbox_hv::reg::{HV_REG_X1 as X1, HV_REG_X2 as X2, HV_REG_X3 as X3, HV_REG_X4 as X4};

/// One block device as the HVC fast path sees it: the backing file the
/// hypercalls act on, indexed by the device's position among the VM's block
/// devices (0 = vda, 1 = vdb, …).
#[derive(Debug, Clone, Copy)]
pub(in crate::vmm) struct HvcBlkDevice {
    /// Backing file, open for the VM's lifetime (owned by the virtio-blk
    /// device registered for the same image).
    pub raw_fd: i32,
    /// Sector size the guest addresses the device in.
    pub blk_size: u32,
    /// Device capacity in `blk_size` sectors.
    pub capacity_sectors: u64,
    /// A read-only image: writes fail, and DISCARD is refused at probe so the
    /// guest never advertises it for the device.
    pub read_only: bool,
}

/// The VM's block devices in device-index order, shared with every vCPU thread.
pub(in crate::vmm) type HvcBlkTable = Arc<Vec<HvcBlkDevice>>;

/// HVC probe: returns number of block devices available for fast path.
/// No arguments. Returns X0 = num_devices.
pub const ARCBOX_HVC_PROBE: u64 = 0xC200_0000;

/// HVC block read. X1=dev_idx, X2=sector, X3=buffer_gpa, X4=byte_len.
/// Returns X0 = bytes read (>0) or negative errno.
pub const ARCBOX_HVC_BLK_READ: u64 = 0xC200_0001;

/// HVC block write. X1=dev_idx, X2=sector, X3=buffer_gpa, X4=byte_len.
/// Returns X0 = bytes written (>0) or negative errno.
pub const ARCBOX_HVC_BLK_WRITE: u64 = 0xC200_0002;

/// HVC block flush (fsync). X1=dev_idx.
/// Returns X0 = 0 on success or negative errno.
pub const ARCBOX_HVC_BLK_FLUSH: u64 = 0xC200_0003;

/// HVC block capacity query. X1=dev_idx.
/// Returns X0 = device capacity in 512-byte sectors, or negative errno.
pub const ARCBOX_HVC_BLK_CAPACITY: u64 = 0xC200_0004;

/// HVC block discard. X1=dev_idx, X2=sector, X3=num_sectors.
/// Returns X0 = 0 on success or negative errno. A zero-length range is the
/// guest driver's capability probe: it answers 0 here, while a host that
/// predates this call answers the SMCCC "not supported" code (-1) through the
/// PSCI fallback, so an old host keeps the guest's DISCARD off.
pub const ARCBOX_HVC_BLK_DISCARD: u64 = 0xC200_0005;

/// HVC block read or write.
/// X1=device_idx, X2=sector, X3=buffer_gpa, X4=byte_length.
/// `is_write`: false=pread, true=pwrite.
pub fn handle_hvc_blk_io(
    vcpu: &arcbox_hv::HvVcpu,
    devices: &[HvcBlkDevice],
    device_manager: &crate::device::DeviceManager,
    is_write: bool,
) -> u64 {
    let Ok(device_idx) = vcpu.get_reg(X1) else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Ok(sector) = vcpu.get_reg(X2) else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Ok(buffer_gpa) = vcpu.get_reg(X3) else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Ok(byte_len) = vcpu.get_reg(X4) else {
        return (-libc::EINVAL as i64) as u64;
    };

    let byte_len = byte_len as usize;
    if byte_len == 0 {
        return (-libc::EINVAL as i64) as u64;
    }

    let Some(&HvcBlkDevice {
        raw_fd,
        blk_size,
        capacity_sectors,
        ..
    }) = devices.get(device_idx as usize)
    else {
        return (-libc::ENODEV as i64) as u64;
    };

    let Some(ram_base) = device_manager.guest_ram_base_ptr() else {
        return (-libc::EFAULT as i64) as u64;
    };
    let gpa_base = device_manager.guest_ram_gpa() as usize;
    let ram_size = device_manager.guest_ram_size();
    let gpa = buffer_gpa as usize;

    if gpa < gpa_base
        || gpa
            .checked_add(byte_len)
            .is_none_or(|end| end > gpa_base + ram_size)
    {
        return (-libc::EFAULT as i64) as u64;
    }

    // SAFETY: The bounds check above guarantees `gpa - gpa_base + byte_len`
    // is within the allocation pointed to by `ram_base`. `ram_base` is the
    // live host mapping tracked by `DeviceManager` for the VM's lifetime.
    let host_ptr = unsafe { ram_base.add(gpa - gpa_base) };

    let Ok((byte_offset, _)) =
        arcbox_virtio::blk::checked_io_byte_range(sector, byte_len, blk_size, capacity_sectors)
    else {
        return (-libc::EINVAL as i64) as u64;
    };
    #[allow(clippy::cast_possible_wrap)]
    let offset = byte_offset as libc::off_t;

    // SAFETY: `host_ptr` is valid for `byte_len` bytes (bounds-checked
    // above) and `raw_fd` is an open fd owned by the VMM for the VM's
    // lifetime. pread/pwrite take an exclusive borrow for the call duration;
    // the guest vCPU that triggered this HVC is parked, so no aliasing.
    let n = if is_write {
        unsafe { libc::pwrite(raw_fd, host_ptr.cast(), byte_len, offset) }
    } else {
        unsafe { libc::pread(raw_fd, host_ptr.cast(), byte_len, offset) }
    };
    if n < 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        return (-errno as i64) as u64;
    }
    if n as usize != byte_len {
        return (-libc::EIO as i64) as u64;
    }
    byte_len as u64
}

/// HVC block capacity query. X1=device_idx.
///
/// Returns the device's real capacity in 512-byte sectors so the guest driver
/// can size the disk correctly instead of assuming a fixed placeholder. A wrong
/// (too-small) size silently corrupts a Btrfs data disk whose metadata records
/// the true size — see the VZ↔HV capacity mismatch this closes.
pub fn handle_hvc_blk_capacity(vcpu: &arcbox_hv::HvVcpu, devices: &[HvcBlkDevice]) -> u64 {
    let Ok(device_idx) = vcpu.get_reg(X1) else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Some(device) = devices.get(device_idx as usize) else {
        return (-libc::ENODEV as i64) as u64;
    };
    device.capacity_sectors
}

/// HVC block discard. X1=device_idx, X2=sector, X3=num_sectors.
///
/// Punches the range out of the sparse backing file so the host reclaims the
/// space the guest filesystem freed (`discard=async`, `fstrim`). Mirrors the
/// virtio-blk worker's `process_discard`: the range is validated with the
/// shared checked helper, only the 4 KiB-aligned interior is punched, and a
/// punch failure is logged rather than returned — DISCARD is advisory, the
/// data is already gone from the guest's point of view, and an I/O error
/// would only make the filesystem stop issuing discards.
pub fn handle_hvc_blk_discard(vcpu: &arcbox_hv::HvVcpu, devices: &[HvcBlkDevice]) -> u64 {
    let (Ok(device_idx), Ok(sector), Ok(num_sectors)) =
        (vcpu.get_reg(X1), vcpu.get_reg(X2), vcpu.get_reg(X3))
    else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Some(device) = devices.get(device_idx as usize) else {
        return (-libc::ENODEV as i64) as u64;
    };
    if device.read_only {
        return (-libc::EROFS as i64) as u64;
    }
    let Ok(num_sectors) = u32::try_from(num_sectors) else {
        return (-libc::EINVAL as i64) as u64;
    };
    match punch_discard(device, sector, num_sectors) {
        Ok(()) => 0,
        Err(errno) => (-errno as i64) as u64,
    }
}

/// Hole-punches the block-aligned interior of `[sector, sector + num_sectors)`
/// in the backing file behind `raw_fd`. `Err` carries the errno for a range
/// the guest may not name (past capacity or overflowing); a punch that the
/// host filesystem refuses is not an error for the guest.
fn punch_discard(
    device: &HvcBlkDevice,
    sector: u64,
    num_sectors: u32,
) -> std::result::Result<(), i32> {
    use arcbox_virtio::blk::{DiscardWriteZeroesRange, aligned_punch_range, punch_hole};

    let range = DiscardWriteZeroesRange {
        sector,
        num_sectors,
        flags: 0,
    };
    let (start, end) = range
        .checked_byte_range(device.blk_size, device.capacity_sectors)
        .map_err(|_| libc::EINVAL)?;
    if let Some((offset, len)) = aligned_punch_range(start, end) {
        if let Err(e) = punch_hole(device.raw_fd, offset, len) {
            tracing::warn!(
                start,
                end,
                error = %e,
                "discard hole-punch failed (HVC fast path); range left allocated"
            );
        }
    }
    Ok(())
}

/// HVC block flush (fsync). X1=device_idx.
pub fn handle_hvc_blk_flush(vcpu: &arcbox_hv::HvVcpu, devices: &[HvcBlkDevice]) -> u64 {
    let Ok(device_idx) = vcpu.get_reg(X1) else {
        return (-libc::EINVAL as i64) as u64;
    };
    let Some(device) = devices.get(device_idx as usize) else {
        return (-libc::ENODEV as i64) as u64;
    };
    // SAFETY: `raw_fd` is an open fd owned by the VMM for the VM's lifetime;
    // fsync is side-effect-only on the file descriptor.
    let ret = unsafe { libc::fsync(device.raw_fd) };
    if ret < 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        return (-errno as i64) as u64;
    }
    0
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::io::AsRawFd as _;

    use super::{HvcBlkDevice, punch_discard};

    fn device(raw_fd: i32, capacity_sectors: u64) -> HvcBlkDevice {
        HvcBlkDevice {
            raw_fd,
            blk_size: 512,
            capacity_sectors,
            read_only: false,
        }
    }

    /// The HVC data disk is the System VM's only block path on HV, so its
    /// DISCARD must reach the backing file: a punched range frees host blocks.
    #[test]
    fn punch_discard_reclaims_backing_blocks() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        temp.write_all(&vec![0xABu8; 4 * 1024 * 1024]).unwrap();
        temp.as_file().sync_all().unwrap();
        let before = std::fs::metadata(temp.path()).unwrap().blocks() * 512;

        // Discard the middle 2 MiB (sectors [2048, 6144)) of the 4 MiB file.
        punch_discard(&device(temp.as_file().as_raw_fd(), 8192), 2048, 4096).unwrap();
        temp.as_file().sync_all().unwrap();

        let after = std::fs::metadata(temp.path()).unwrap().blocks() * 512;
        assert!(
            after + 2 * 1024 * 1024 <= before + 64 * 1024,
            "expected ~2 MiB reclaimed: before={before} after={after}"
        );
    }

    /// A zero-length range is the guest's capability probe and must succeed
    /// without touching the file.
    #[test]
    fn zero_length_discard_is_the_capability_probe() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            punch_discard(&device(temp.as_file().as_raw_fd(), 8192), 0, 0),
            Ok(())
        );
    }

    /// Guest-controlled sector fields are arbitrary bits: a range past the
    /// device or one that overflows is rejected with EINVAL, never punched.
    #[test]
    fn out_of_range_discard_is_rejected() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let fd = temp.as_file().as_raw_fd();
        assert_eq!(
            punch_discard(&device(fd, 8192), 8000, 1000),
            Err(libc::EINVAL)
        );
        assert_eq!(
            punch_discard(&device(fd, 8192), u64::MAX - 1, 8),
            Err(libc::EINVAL)
        );
        assert_eq!(
            punch_discard(&device(fd, u64::MAX), u64::MAX / 512 + 1, 1),
            Err(libc::EINVAL)
        );
    }
}
