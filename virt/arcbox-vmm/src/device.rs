//! Device management for VMM.
//!
//! This module provides device management including `VirtIO` MMIO transport.
//!
//! The device manager bridges between:
//! - MMIO transport layer (`VirtioMmioState`) that handles guest MMIO accesses
//! - `VirtIO` device implementations (`VirtioDevice`) from arcbox-virtio

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use arcbox_virtio::{DeviceStatus, QueueConfig, VirtioDevice};

use crate::error::{Result, VmmError};
use crate::irq::{Irq, IrqChip};
use crate::memory::MemoryManager;

/// Device identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId(u32);

impl DeviceId {
    /// Creates a new device ID.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Returns the raw ID value.
    #[must_use]
    pub const fn raw(&self) -> u32 {
        self.0
    }
}

/// Device type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    /// Serial port.
    Serial,
    /// `VirtIO` block device.
    VirtioBlock,
    /// `VirtIO` network device.
    VirtioNet,
    /// `VirtIO` console.
    VirtioConsole,
    /// `VirtIO` filesystem.
    VirtioFs,
    /// `VirtIO` vsock.
    VirtioVsock,
    /// Other device.
    Other,
}

/// Device information.
#[derive(Debug)]
pub struct DeviceInfo {
    /// Device ID.
    pub id: DeviceId,
    /// Device type.
    pub device_type: DeviceType,
    /// Device name.
    pub name: String,
    /// MMIO base address.
    pub mmio_base: Option<u64>,
    /// MMIO size.
    pub mmio_size: u64,
    /// Assigned IRQ.
    pub irq: Option<Irq>,
}

/// `VirtIO` MMIO transport registers.
///
/// Based on `VirtIO` 1.1 specification. Register offsets are sourced from
/// `virtio_bindings::virtio_mmio`.
pub mod virtio_mmio {
    use arcbox_virtio::virtio_bindings::virtio_mmio as vmmio;

    /// Magic value ("virt"). Not a register offset -- this is the *value*
    /// returned when the guest reads the MagicValue register.
    pub const MAGIC_VALUE: u32 = 0x74726976;
    /// `VirtIO` version (legacy: 1, modern: 2). Runtime value, not an offset.
    pub const VERSION: u32 = 2;
    /// Vendor ID. Runtime value, not an offset.
    pub const VENDOR_ID: u32 = 0x554D4551; // "QEMU"

    /// Interrupt reason: used buffer notification.
    pub const INT_VRING: u32 = vmmio::VIRTIO_MMIO_INT_VRING;
    /// Interrupt reason: configuration change.
    pub const INT_CONFIG: u32 = vmmio::VIRTIO_MMIO_INT_CONFIG;

    /// Register offsets sourced from `virtio_bindings::virtio_mmio`.
    pub mod regs {
        use super::vmmio;

        /// Magic value.
        pub const MAGIC: u64 = vmmio::VIRTIO_MMIO_MAGIC_VALUE as u64;
        /// Version.
        pub const VERSION: u64 = vmmio::VIRTIO_MMIO_VERSION as u64;
        /// Device type.
        pub const DEVICE_ID: u64 = vmmio::VIRTIO_MMIO_DEVICE_ID as u64;
        /// Vendor ID.
        pub const VENDOR_ID: u64 = vmmio::VIRTIO_MMIO_VENDOR_ID as u64;
        /// Device features.
        pub const DEVICE_FEATURES: u64 = vmmio::VIRTIO_MMIO_DEVICE_FEATURES as u64;
        /// Device features selector.
        pub const DEVICE_FEATURES_SEL: u64 = vmmio::VIRTIO_MMIO_DEVICE_FEATURES_SEL as u64;
        /// Driver features.
        pub const DRIVER_FEATURES: u64 = vmmio::VIRTIO_MMIO_DRIVER_FEATURES as u64;
        /// Driver features selector.
        pub const DRIVER_FEATURES_SEL: u64 = vmmio::VIRTIO_MMIO_DRIVER_FEATURES_SEL as u64;
        /// Queue selector.
        pub const QUEUE_SEL: u64 = vmmio::VIRTIO_MMIO_QUEUE_SEL as u64;
        /// Queue size (max).
        pub const QUEUE_NUM_MAX: u64 = vmmio::VIRTIO_MMIO_QUEUE_NUM_MAX as u64;
        /// Queue size.
        pub const QUEUE_NUM: u64 = vmmio::VIRTIO_MMIO_QUEUE_NUM as u64;
        /// Queue ready.
        pub const QUEUE_READY: u64 = vmmio::VIRTIO_MMIO_QUEUE_READY as u64;
        /// Queue notify.
        pub const QUEUE_NOTIFY: u64 = vmmio::VIRTIO_MMIO_QUEUE_NOTIFY as u64;
        /// Interrupt status.
        pub const INTERRUPT_STATUS: u64 = vmmio::VIRTIO_MMIO_INTERRUPT_STATUS as u64;
        /// Interrupt acknowledge.
        pub const INTERRUPT_ACK: u64 = vmmio::VIRTIO_MMIO_INTERRUPT_ACK as u64;
        /// Device status.
        pub const STATUS: u64 = vmmio::VIRTIO_MMIO_STATUS as u64;
        /// Queue descriptor low.
        pub const QUEUE_DESC_LOW: u64 = vmmio::VIRTIO_MMIO_QUEUE_DESC_LOW as u64;
        /// Queue descriptor high.
        pub const QUEUE_DESC_HIGH: u64 = vmmio::VIRTIO_MMIO_QUEUE_DESC_HIGH as u64;
        /// Queue driver low.
        pub const QUEUE_DRIVER_LOW: u64 = vmmio::VIRTIO_MMIO_QUEUE_AVAIL_LOW as u64;
        /// Queue driver high.
        pub const QUEUE_DRIVER_HIGH: u64 = vmmio::VIRTIO_MMIO_QUEUE_AVAIL_HIGH as u64;
        /// Queue device low.
        pub const QUEUE_DEVICE_LOW: u64 = vmmio::VIRTIO_MMIO_QUEUE_USED_LOW as u64;
        /// Queue device high.
        pub const QUEUE_DEVICE_HIGH: u64 = vmmio::VIRTIO_MMIO_QUEUE_USED_HIGH as u64;
        /// Config generation.
        pub const CONFIG_GENERATION: u64 = vmmio::VIRTIO_MMIO_CONFIG_GENERATION as u64;
        /// Config space starts here.
        pub const CONFIG: u64 = vmmio::VIRTIO_MMIO_CONFIG as u64;
    }

    /// MMIO region size.
    pub const MMIO_SIZE: u64 = 0x200;
}

/// `VirtIO` MMIO device state.
pub struct VirtioMmioState {
    /// Device type ID.
    pub device_id: u32,
    /// Device features.
    pub device_features: u64,
    /// Driver features.
    pub driver_features: u64,
    /// Device features selector.
    pub device_features_sel: u32,
    /// Driver features selector.
    pub driver_features_sel: u32,
    /// Queue selector.
    pub queue_sel: u32,
    /// Queue sizes.
    pub queue_num: [u16; 8],
    /// Queue ready flags.
    pub queue_ready: [bool; 8],
    /// Queue descriptor addresses.
    pub queue_desc: [u64; 8],
    /// Queue driver addresses.
    pub queue_driver: [u64; 8],
    /// Queue device addresses.
    pub queue_device: [u64; 8],
    /// Device status.
    pub status: u8,
    /// Interrupt status.
    pub interrupt_status: u32,
    /// Configuration generation.
    pub config_generation: u32,
}

impl VirtioMmioState {
    /// Creates a new `VirtIO` MMIO state.
    #[must_use]
    pub const fn new(device_id: u32, features: u64) -> Self {
        Self {
            device_id,
            device_features: features,
            driver_features: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queue_num: [0; 8],
            queue_ready: [false; 8],
            queue_desc: [0; 8],
            queue_driver: [0; 8],
            queue_device: [0; 8],
            status: 0,
            interrupt_status: 0,
            config_generation: 0,
        }
    }

    /// Reads from MMIO register.
    #[must_use]
    pub fn read(&self, offset: u64) -> u32 {
        use virtio_mmio::regs;

        match offset {
            regs::MAGIC => virtio_mmio::MAGIC_VALUE,
            regs::VERSION => virtio_mmio::VERSION,
            regs::DEVICE_ID => self.device_id,
            regs::VENDOR_ID => virtio_mmio::VENDOR_ID,
            regs::DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    self.device_features as u32
                } else {
                    (self.device_features >> 32) as u32
                }
            }
            regs::QUEUE_NUM_MAX => 256, // Max queue size
            regs::QUEUE_READY => {
                if (self.queue_sel as usize) < 8 {
                    u32::from(self.queue_ready[self.queue_sel as usize])
                } else {
                    0
                }
            }
            regs::INTERRUPT_STATUS => self.interrupt_status,
            regs::STATUS => u32::from(self.status),
            regs::CONFIG_GENERATION => self.config_generation,
            _ => {
                tracing::trace!("VirtIO MMIO read unknown offset: {:#x}", offset);
                0
            }
        }
    }

    /// Writes to MMIO register.
    pub fn write(&mut self, offset: u64, value: u32) {
        use virtio_mmio::regs;

        match offset {
            regs::DEVICE_FEATURES_SEL => self.device_features_sel = value,
            regs::DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | u64::from(value);
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | (u64::from(value) << 32);
                }
            }
            regs::DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            regs::QUEUE_SEL => self.queue_sel = value,
            regs::QUEUE_NUM => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_num[self.queue_sel as usize] = value as u16;
                }
            }
            regs::QUEUE_READY => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_ready[self.queue_sel as usize] = value != 0;
                }
            }
            regs::QUEUE_NOTIFY => {
                // Guest is notifying us about available buffers
                tracing::trace!("VirtIO queue {} notified", value);
            }
            regs::INTERRUPT_ACK => {
                self.interrupt_status &= !value;
            }
            regs::STATUS => {
                self.status = value as u8;
                if value == 0 {
                    // Device reset
                    self.driver_features = 0;
                    self.queue_sel = 0;
                    self.queue_num = [0; 8];
                    self.queue_ready = [false; 8];
                    self.interrupt_status = 0;
                }
            }
            regs::QUEUE_DESC_LOW => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_desc[self.queue_sel as usize] =
                        (self.queue_desc[self.queue_sel as usize] & 0xFFFF_FFFF_0000_0000)
                            | u64::from(value);
                }
            }
            regs::QUEUE_DESC_HIGH => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_desc[self.queue_sel as usize] =
                        (self.queue_desc[self.queue_sel as usize] & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(value) << 32);
                }
            }
            regs::QUEUE_DRIVER_LOW => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_driver[self.queue_sel as usize] =
                        (self.queue_driver[self.queue_sel as usize] & 0xFFFF_FFFF_0000_0000)
                            | u64::from(value);
                }
            }
            regs::QUEUE_DRIVER_HIGH => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_driver[self.queue_sel as usize] =
                        (self.queue_driver[self.queue_sel as usize] & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(value) << 32);
                }
            }
            regs::QUEUE_DEVICE_LOW => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_device[self.queue_sel as usize] =
                        (self.queue_device[self.queue_sel as usize] & 0xFFFF_FFFF_0000_0000)
                            | u64::from(value);
                }
            }
            regs::QUEUE_DEVICE_HIGH => {
                if (self.queue_sel as usize) < 8 {
                    self.queue_device[self.queue_sel as usize] =
                        (self.queue_device[self.queue_sel as usize] & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(value) << 32);
                }
            }
            _ => {
                tracing::trace!("VirtIO MMIO write unknown offset: {:#x}", offset);
            }
        }
    }

    /// Triggers an interrupt.
    pub const fn trigger_interrupt(&mut self, reason: u32) {
        self.interrupt_status |= reason;
    }
}

/// Trait for devices that can handle MMIO operations.
pub trait MmioDevice: Send + Sync {
    /// Reads from the device MMIO region.
    fn mmio_read(&self, offset: u64, size: usize) -> u64;

    /// Writes to the device MMIO region.
    fn mmio_write(&mut self, offset: u64, size: usize, value: u64);
}

/// A registered device with MMIO state and `VirtIO` device implementation.
struct RegisteredDevice {
    info: DeviceInfo,
    mmio_state: Option<Arc<RwLock<VirtioMmioState>>>,
    /// The actual `VirtIO` device implementation.
    virtio_device: Option<Arc<Mutex<dyn VirtioDevice>>>,
}

/// IRQ trigger callback type for device-initiated interrupts.
pub type DeviceIrqCallback = Arc<dyn Fn(Irq, bool) -> Result<()> + Send + Sync>;

/// Manages devices attached to the VM.
pub struct DeviceManager {
    devices: HashMap<DeviceId, RegisteredDevice>,
    next_id: u32,
    /// MMIO address to device mapping.
    mmio_map: HashMap<u64, DeviceId>,
    /// Base pointer to guest physical memory (set by custom HV path).
    guest_ram_base: Option<*mut u8>,
    /// Size of guest physical memory in bytes.
    guest_ram_size: usize,
    /// GPA where the guest RAM region starts (e.g. 0x40000000).
    /// Used to translate descriptor GPAs to memory slice offsets.
    guest_ram_gpa: u64,
    /// IRQ trigger callback for injecting interrupts into the guest.
    irq_callback: Option<DeviceIrqCallback>,
    /// Vsock host connection fds, keyed by guest port.
    /// Set by connect_vsock_hv, read by vsock process_queue.
    vsock_host_fds: std::sync::Arc<std::sync::Mutex<HashMap<u32, std::os::unix::io::RawFd>>>,
    /// Pending vsock connect requests (port, guest_cid) to inject when device is ready.
    pending_vsock_connects: std::sync::Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
    /// Vsock ports where OP_RESPONSE has been received (connection established).
    vsock_connected_ports: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u32>>>,
}

// SAFETY: guest_ram_base points to memory that is valid for the lifetime of the
// DeviceManager and is accessed exclusively through synchronized vCPU/device paths.
unsafe impl Send for DeviceManager {}
unsafe impl Sync for DeviceManager {}

impl DeviceManager {
    /// Creates a new device manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
            next_id: 0,
            mmio_map: HashMap::new(),
            guest_ram_base: None,
            guest_ram_size: 0,
            guest_ram_gpa: 0,
            irq_callback: None,
            vsock_host_fds: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_vsock_connects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            vsock_connected_ports: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
        }
    }

    /// Provides guest physical memory access for queue processing.
    ///
    /// # Safety
    ///
    /// `base` must point to a valid allocation of at least `size` bytes that
    /// remains valid for the lifetime of the `DeviceManager`.
    pub unsafe fn set_guest_memory(&mut self, base: *mut u8, size: usize, gpa_base: u64) {
        self.guest_ram_base = Some(base);
        self.guest_ram_size = size;
        self.guest_ram_gpa = gpa_base;
    }

    /// Sets the callback used to inject interrupts into the guest.
    pub fn set_irq_callback(&mut self, callback: DeviceIrqCallback) {
        self.irq_callback = Some(callback);
    }

    /// Returns the set of vsock ports where connections are established.
    pub fn vsock_connected_ports(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u32>>> {
        self.vsock_connected_ports.clone()
    }

    /// Marks a vsock port as connected (OP_RESPONSE received).
    pub fn mark_vsock_connected(&self, port: u32) {
        if let Ok(mut ports) = self.vsock_connected_ports.lock() {
            ports.insert(port);
        }
    }

    /// Triggers an IRQ through the configured callback (if set).
    pub fn trigger_irq_callback(&self, irq: Irq, level: bool) {
        if let Some(ref cb) = self.irq_callback {
            let _ = cb(irq, level);
        }
    }

    /// Returns a clone of the vsock host fds map for external access.
    pub fn vsock_host_fds(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<u32, std::os::unix::io::RawFd>>> {
        self.vsock_host_fds.clone()
    }

    /// Registers a new device.
    ///
    /// # Errors
    ///
    /// Returns an error if device registration fails.
    pub fn register(
        &mut self,
        device_type: DeviceType,
        name: impl Into<String>,
    ) -> Result<DeviceId> {
        let id = DeviceId::new(self.next_id);
        self.next_id += 1;

        let info = DeviceInfo {
            id,
            device_type,
            name: name.into(),
            mmio_base: None,
            mmio_size: 0,
            irq: None,
        };

        self.devices.insert(
            id,
            RegisteredDevice {
                info,
                mmio_state: None,
                virtio_device: None,
            },
        );

        Ok(id)
    }

    /// Registers a `VirtIO` device with MMIO transport (without actual device).
    ///
    /// Use `register_virtio_device` to register with an actual `VirtIO` device implementation.
    ///
    /// # Errors
    ///
    /// Returns an error if registration fails.
    pub fn register_virtio(
        &mut self,
        device_type: DeviceType,
        name: impl Into<String>,
        virtio_device_id: u32,
        features: u64,
        memory_manager: &mut MemoryManager,
        irq_chip: &IrqChip,
    ) -> Result<DeviceId> {
        let id = DeviceId::new(self.next_id);
        self.next_id += 1;

        // Allocate MMIO region
        let mmio_base = memory_manager.allocate_mmio(virtio_mmio::MMIO_SIZE, &name.into())?;
        let irq = irq_chip.allocate_irq()?;

        let name_str = format!("{}", id.0);
        let info = DeviceInfo {
            id,
            device_type,
            name: name_str,
            mmio_base: Some(mmio_base),
            mmio_size: virtio_mmio::MMIO_SIZE,
            irq: Some(irq),
        };

        let mmio_state = Arc::new(RwLock::new(VirtioMmioState::new(
            virtio_device_id,
            features,
        )));

        self.mmio_map.insert(mmio_base, id);
        self.devices.insert(
            id,
            RegisteredDevice {
                info,
                mmio_state: Some(mmio_state),
                virtio_device: None,
            },
        );

        tracing::info!(
            "Registered VirtIO device {} at MMIO {:#x}, IRQ {}",
            id.0,
            mmio_base,
            irq
        );

        Ok(id)
    }

    /// Registers a `VirtIO` device with MMIO transport and device implementation.
    ///
    /// This is the preferred method for registering `VirtIO` devices as it connects
    /// the MMIO transport layer with the actual device logic.
    ///
    /// # Errors
    ///
    /// Returns an error if registration fails.
    pub fn register_virtio_device<D: VirtioDevice + 'static>(
        &mut self,
        device_type: DeviceType,
        name: impl Into<String>,
        device: D,
        memory_manager: &mut MemoryManager,
        irq_chip: &IrqChip,
    ) -> Result<DeviceId> {
        let id = DeviceId::new(self.next_id);
        self.next_id += 1;

        let virtio_device_id = device.device_id() as u32;
        let features = device.features();
        let name_str = name.into();

        // Allocate MMIO region
        let mmio_base = memory_manager.allocate_mmio(virtio_mmio::MMIO_SIZE, &name_str)?;
        let irq = irq_chip.allocate_irq()?;

        let info = DeviceInfo {
            id,
            device_type,
            name: name_str.clone(),
            mmio_base: Some(mmio_base),
            mmio_size: virtio_mmio::MMIO_SIZE,
            irq: Some(irq),
        };

        let mmio_state = Arc::new(RwLock::new(VirtioMmioState::new(
            virtio_device_id,
            features,
        )));
        let virtio_device = Arc::new(Mutex::new(device));

        self.mmio_map.insert(mmio_base, id);
        self.devices.insert(
            id,
            RegisteredDevice {
                info,
                mmio_state: Some(mmio_state),
                virtio_device: Some(virtio_device),
            },
        );

        tracing::info!(
            "Registered VirtIO device '{}' (type {:?}) at MMIO {:#x}, IRQ {}",
            name_str,
            device_type,
            mmio_base,
            irq
        );

        Ok(id)
    }

    /// Gets device info by ID.
    #[must_use]
    pub fn get(&self, id: DeviceId) -> Option<&DeviceInfo> {
        self.devices.get(&id).map(|d| &d.info)
    }

    /// Gets the MMIO state for a device.
    #[must_use]
    pub fn get_mmio_state(&self, id: DeviceId) -> Option<Arc<RwLock<VirtioMmioState>>> {
        self.devices.get(&id).and_then(|d| d.mmio_state.clone())
    }

    /// Gets the `VirtIO` device for a device ID.
    #[must_use]
    pub fn get_virtio_device(&self, id: DeviceId) -> Option<Arc<Mutex<dyn VirtioDevice>>> {
        self.devices.get(&id).and_then(|d| d.virtio_device.clone())
    }

    /// Triggers an interrupt for a device.
    ///
    /// # Errors
    ///
    /// Returns an error if the device doesn't exist or interrupt fails.
    pub fn trigger_interrupt(&self, id: DeviceId, reason: u32) -> Result<()> {
        let device = self
            .devices
            .get(&id)
            .ok_or_else(|| VmmError::Device(format!("Device {} not found", id.0)))?;

        if let Some(state) = &device.mmio_state {
            let mut state = state
                .write()
                .map_err(|e| VmmError::Device(format!("Failed to lock device state: {e}")))?;
            state.trigger_interrupt(reason);
        }

        Ok(())
    }

    /// Finds device by MMIO address.
    #[must_use]
    pub fn find_by_mmio(&self, addr: u64) -> Option<DeviceId> {
        for (base, id) in &self.mmio_map {
            if let Some(device) = self.devices.get(id) {
                if addr >= *base && addr < *base + device.info.mmio_size {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Handles MMIO read.
    ///
    /// # Errors
    ///
    /// Returns an error if the read fails.
    pub fn handle_mmio_read(&self, addr: u64, size: usize) -> Result<u64> {
        let device_id = self
            .find_by_mmio(addr)
            .ok_or_else(|| VmmError::Device(format!("No device at MMIO address {addr:#x}")))?;

        let device = self
            .devices
            .get(&device_id)
            .ok_or_else(|| VmmError::Device(format!("Device {} not found", device_id.0)))?;

        let base = device.info.mmio_base.unwrap_or(0);
        let offset = addr - base;

        if let Some(state) = &device.mmio_state {
            let state = state
                .read()
                .map_err(|e| VmmError::Device(format!("Failed to lock device state: {e}")))?;

            // Handle config space reads - forward to actual device
            if offset >= virtio_mmio::regs::CONFIG {
                let config_offset = offset - virtio_mmio::regs::CONFIG;
                if let Some(virtio_dev) = &device.virtio_device {
                    let dev = virtio_dev.lock().map_err(|e| {
                        VmmError::Device(format!("Failed to lock virtio device: {e}"))
                    })?;
                    let mut data = vec![0u8; size];
                    dev.read_config(config_offset, &mut data);
                    tracing::trace!(
                        "Config read: device={} offset={:#x} size={} data={:?}",
                        device_id.0,
                        config_offset,
                        size,
                        &data[..size.min(8)]
                    );
                    return Ok(match size {
                        1 => u64::from(data[0]),
                        2 => u64::from(u16::from_le_bytes([data[0], data[1]])),
                        4 => u64::from(u32::from_le_bytes([data[0], data[1], data[2], data[3]])),
                        8 => u64::from_le_bytes([
                            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                        ]),
                        _ => 0,
                    });
                }
                return Ok(0);
            }

            let value = state.read(offset);
            let result = match size {
                1 => u64::from(value as u8),
                2 => u64::from(value as u16),
                4 => u64::from(value),
                _ => u64::from(value),
            };

            Ok(result)
        } else {
            Ok(0)
        }
    }

    /// Handles MMIO write.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn handle_mmio_write(&self, addr: u64, size: usize, value: u64) -> Result<()> {
        let device_id = self
            .find_by_mmio(addr)
            .ok_or_else(|| VmmError::Device(format!("No device at MMIO address {addr:#x}")))?;

        let device = self
            .devices
            .get(&device_id)
            .ok_or_else(|| VmmError::Device(format!("Device {} not found", device_id.0)))?;

        let base = device.info.mmio_base.unwrap_or(0);
        let offset = addr - base;

        if let Some(state) = &device.mmio_state {
            let old_status = {
                let s = state
                    .read()
                    .map_err(|e| VmmError::Device(format!("Failed to lock device state: {e}")))?;
                s.status
            };

            // Handle config space writes - forward to actual device
            if offset >= virtio_mmio::regs::CONFIG {
                let config_offset = offset - virtio_mmio::regs::CONFIG;
                if let Some(virtio_dev) = &device.virtio_device {
                    let mut dev = virtio_dev.lock().map_err(|e| {
                        VmmError::Device(format!("Failed to lock virtio device: {e}"))
                    })?;
                    let data: Vec<u8> = match size {
                        1 => vec![value as u8],
                        2 => (value as u16).to_le_bytes().to_vec(),
                        4 => (value as u32).to_le_bytes().to_vec(),
                        8 => value.to_le_bytes().to_vec(),
                        _ => return Ok(()),
                    };
                    dev.write_config(config_offset, &data);
                }
                return Ok(());
            }

            let value32 = match size {
                1 => value as u32 & 0xFF,
                2 => value as u32 & 0xFFFF,
                4 | 8 => value as u32,
                _ => value as u32,
            };

            // Write to MMIO state
            {
                let mut state = state
                    .write()
                    .map_err(|e| VmmError::Device(format!("Failed to lock device state: {e}")))?;
                state.write(offset, value32);
            }

            // Handle special cases after write
            match offset {
                virtio_mmio::regs::STATUS => {
                    let new_status = value32 as u8;

                    // Handle feature acknowledgment
                    if new_status & DeviceStatus::FEATURES_OK != 0
                        && old_status & DeviceStatus::FEATURES_OK == 0
                    {
                        if let Some(virtio_dev) = &device.virtio_device {
                            let mmio_state = state.read().map_err(|e| {
                                VmmError::Device(format!("Failed to lock device state: {e}"))
                            })?;
                            let mut dev = virtio_dev.lock().map_err(|e| {
                                VmmError::Device(format!("Failed to lock virtio device: {e}"))
                            })?;
                            dev.ack_features(mmio_state.driver_features);
                            tracing::debug!(
                                "Device {} acknowledged features: {:#x}",
                                device_id.0,
                                mmio_state.driver_features
                            );
                        }
                    }

                    // Handle device activation
                    if new_status & DeviceStatus::DRIVER_OK != 0
                        && old_status & DeviceStatus::DRIVER_OK == 0
                    {
                        if let Some(virtio_dev) = &device.virtio_device {
                            let mut dev = virtio_dev.lock().map_err(|e| {
                                VmmError::Device(format!("Failed to lock virtio device: {e}"))
                            })?;
                            dev.activate().map_err(|e| {
                                VmmError::Device(format!("Failed to activate device: {e}"))
                            })?;
                            tracing::info!("Device {} activated", device_id.0);
                        }
                    }

                    // Handle device reset
                    if new_status == 0 {
                        if let Some(virtio_dev) = &device.virtio_device {
                            let mut dev = virtio_dev.lock().map_err(|e| {
                                VmmError::Device(format!("Failed to lock virtio device: {e}"))
                            })?;
                            dev.reset();
                            tracing::info!("Device {} reset", device_id.0);
                        }
                    }
                }
                virtio_mmio::regs::QUEUE_NOTIFY => {
                    let queue_idx = value32 as u16;

                    if let Some(virtio_dev) = &device.virtio_device {
                        // Build QueueConfig from current MMIO state for the
                        // notified queue index.
                        let qcfg = {
                            let mmio_state = state.read().map_err(|e| {
                                VmmError::Device(format!("Failed to lock state: {e}"))
                            })?;
                            let qi = queue_idx as usize;
                            if qi < 8 {
                                QueueConfig {
                                    desc_addr: mmio_state.queue_desc[qi],
                                    avail_addr: mmio_state.queue_driver[qi],
                                    used_addr: mmio_state.queue_device[qi],
                                    size: mmio_state.queue_num[qi],
                                    ready: mmio_state.queue_ready[qi],
                                    gpa_base: self.guest_ram_gpa,
                                    vsock_host_fds: Some(self.vsock_host_fds.clone()),
                                    vsock_connected_ports: Some(self.vsock_connected_ports.clone()),
                                }
                            } else {
                                QueueConfig::default()
                            }
                        };

                        if let (Some(ram_base), ram_size) =
                            (self.guest_ram_base, self.guest_ram_size)
                        {
                            // Build a guest memory slice that can be indexed by GPA directly.
                            // The host pointer corresponds to GPA `guest_ram_gpa`, so we
                            // offset it back so that index 0 = GPA 0. This lets device code
                            // use `desc.addr as usize` (which is a GPA) without translation.
                            //
                            // SAFETY: ram_base is valid for ram_size bytes. We extend the
                            // slice to cover [GPA 0 .. GPA guest_ram_gpa + ram_size] by
                            // adjusting the base pointer. Accesses below guest_ram_gpa are
                            // invalid but device descriptors always point within RAM.
                            let gpa_base = self.guest_ram_gpa as usize;
                            let guest_mem = unsafe {
                                std::slice::from_raw_parts_mut(
                                    ram_base.sub(gpa_base),
                                    gpa_base + ram_size,
                                )
                            };

                            let mut dev = virtio_dev.lock().map_err(|e| {
                                VmmError::Device(format!("Failed to lock device: {e}"))
                            })?;

                            match dev.process_queue(queue_idx, guest_mem, &qcfg) {
                                Ok(completions) if !completions.is_empty() => {
                                    tracing::trace!(
                                        "Device {} queue {} processed {} completions",
                                        device_id.0,
                                        queue_idx,
                                        completions.len()
                                    );
                                    // Inject interrupt to signal used ring updates.
                                    if let Some(irq) = device.info.irq {
                                        {
                                            let mut s = state.write().map_err(|e| {
                                                VmmError::Device(format!(
                                                    "Failed to lock state: {e}"
                                                ))
                                            })?;
                                            s.trigger_interrupt(virtio_mmio::INT_VRING);
                                        }
                                        if let Some(ref callback) = self.irq_callback {
                                            let _ = callback(irq, true);
                                        }
                                    }
                                }
                                Ok(_) => {
                                    // No completions, no interrupt needed.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Device {} queue {} error: {e}",
                                        device_id.0,
                                        queue_idx
                                    );
                                }
                            }
                        } else {
                            tracing::trace!(
                                "Device {} queue {} notified but no guest memory set",
                                device_id.0,
                                queue_idx
                            );
                        }
                    } else {
                        tracing::trace!(
                            "Device {} queue {} notified (no device impl)",
                            device_id.0,
                            queue_idx
                        );
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Returns an iterator over all devices.
    pub fn iter(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.devices.values().map(|d| &d.info)
    }

    /// Returns device tree entries for all `VirtIO` devices.
    #[must_use]
    pub fn device_tree_entries(&self) -> Vec<DeviceTreeEntry> {
        self.devices
            .values()
            .filter_map(|d| {
                if let (Some(base), Some(irq)) = (d.info.mmio_base, d.info.irq) {
                    Some(DeviceTreeEntry {
                        compatible: "virtio,mmio".to_string(),
                        reg_base: base,
                        reg_size: d.info.mmio_size,
                        irq,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Polls host vsock fds for incoming data and injects into guest RX queue.
    ///
    /// Injects a vsock OP_REQUEST into the guest RX queue to initiate a connection.
    /// If the device isn't ready yet, queues the request for later injection
    /// during poll_vsock_rx.
    pub fn inject_vsock_connect(&self, port: u32, guest_cid: u64) -> bool {
        let mut hdr = [0u8; 44];
        hdr[0..8].copy_from_slice(&2u64.to_le_bytes()); // src_cid = HOST (2)
        hdr[8..16].copy_from_slice(&guest_cid.to_le_bytes());
        hdr[16..20].copy_from_slice(&port.to_le_bytes()); // src_port
        hdr[20..24].copy_from_slice(&port.to_le_bytes()); // dst_port
        hdr[28..30].copy_from_slice(&1u16.to_le_bytes()); // type = STREAM
        hdr[30..32].copy_from_slice(&1u16.to_le_bytes()); // op = REQUEST
        hdr[36..40].copy_from_slice(&(64 * 1024u32).to_le_bytes()); // buf_alloc
        tracing::info!("Vsock: injecting OP_REQUEST for port {port}");
        if self.inject_vsock_rx_raw(&hdr) {
            true
        } else {
            // Device not ready yet — queue for later.
            tracing::info!("Vsock: device not ready, queueing OP_REQUEST for port {port}");
            if let Ok(mut pending) = self.pending_vsock_connects.lock() {
                if !pending.iter().any(|&(p, _)| p == port) {
                    pending.push((port, guest_cid));
                }
            }
            false
        }
    }

    /// Injects a raw packet into the vsock RX queue (queue 0).
    fn inject_vsock_rx_raw(&self, packet: &[u8]) -> bool {
        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return false;
        };
        let gpa_base = self.guest_ram_gpa as usize;

        let mmio_arc = self
            .devices
            .values()
            .find(|d| d.info.device_type == DeviceType::VirtioVsock)
            .and_then(|d| d.mmio_state.as_ref());
        let Some(mmio_arc) = mmio_arc else {
            return false;
        };
        let Ok(mmio) = mmio_arc.read() else {
            return false;
        };

        let qi = 0usize;
        if qi >= 8 || !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
            return false;
        }
        let desc_addr = mmio.queue_desc[qi] as usize;
        let avail_addr = mmio.queue_driver[qi] as usize;
        let used_addr = mmio.queue_device[qi] as usize;
        let q_size = mmio.queue_num[qi] as usize;

        let guest_mem =
            unsafe { std::slice::from_raw_parts_mut(ram_base.sub(gpa_base), gpa_base + ram_size) };

        if avail_addr + 4 > guest_mem.len() {
            return false;
        }
        let avail_idx =
            u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
        let used_idx_off = used_addr + 2;
        let used_idx =
            u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;
        tracing::info!(
            "inject_vsock_rx_raw: avail_idx={} used_idx={} q_size={} desc_addr={:#x}",
            avail_idx,
            used_idx,
            q_size,
            desc_addr,
        );

        if avail_idx == used_idx {
            return false;
        }

        let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
        if ring_off + 2 > guest_mem.len() {
            return false;
        }
        let head_idx = u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

        let mut written = 0;
        let mut idx = head_idx;
        for _ in 0..q_size {
            let d_off = desc_addr + idx * 16;
            if d_off + 16 > guest_mem.len() {
                break;
            }
            let addr = u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
            let len =
                u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
            let next = u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());

            if flags & 2 != 0 && addr + len <= guest_mem.len() {
                let remaining = packet.len() - written;
                let to_write = remaining.min(len);
                guest_mem[addr..addr + to_write]
                    .copy_from_slice(&packet[written..written + to_write]);
                written += to_write;
            }
            if flags & 1 == 0 || written >= packet.len() {
                break;
            }
            idx = next as usize;
        }

        let used_entry = used_addr + 4 + (used_idx % q_size) * 8;
        if used_entry + 8 <= guest_mem.len() {
            guest_mem[used_entry..used_entry + 4].copy_from_slice(&(head_idx as u32).to_le_bytes());
            guest_mem[used_entry + 4..used_entry + 8]
                .copy_from_slice(&(written as u32).to_le_bytes());
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            let new_used = (used_idx + 1) as u16;
            guest_mem[used_idx_off..used_idx_off + 2].copy_from_slice(&new_used.to_le_bytes());
        }
        true
    }

    /// Called from the vCPU run loop during WFI (guest idle). Returns true
    /// if any data was injected (caller should trigger interrupt).
    #[allow(unused_assignments, unused_variables)]
    pub fn poll_vsock_rx(&self) -> bool {
        // Process any pending connect requests first.
        let pending: Vec<(u32, u64)> = {
            if let Ok(mut p) = self.pending_vsock_connects.lock() {
                std::mem::take(&mut *p)
            } else {
                Vec::new()
            }
        };
        let mut injected = false;
        for (port, guest_cid) in pending {
            let mut hdr = [0u8; 44];
            hdr[0..8].copy_from_slice(&2u64.to_le_bytes());
            hdr[8..16].copy_from_slice(&guest_cid.to_le_bytes());
            hdr[16..20].copy_from_slice(&port.to_le_bytes());
            hdr[20..24].copy_from_slice(&port.to_le_bytes());
            hdr[28..30].copy_from_slice(&1u16.to_le_bytes());
            hdr[30..32].copy_from_slice(&1u16.to_le_bytes()); // OP_REQUEST
            hdr[36..40].copy_from_slice(&(64 * 1024u32).to_le_bytes());
            if self.inject_vsock_rx_raw(&hdr) {
                tracing::info!("Vsock: deferred OP_REQUEST injected for port {port}");
                injected = true;
            } else {
                // Still not ready — re-queue.
                if let Ok(mut p) = self.pending_vsock_connects.lock() {
                    p.push((port, guest_cid));
                }
            }
        }

        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return injected;
        };
        let gpa_base = self.guest_ram_gpa as usize;
        let guest_mem =
            unsafe { std::slice::from_raw_parts_mut(ram_base.sub(gpa_base), gpa_base + ram_size) };

        let fds = match self.vsock_host_fds.lock() {
            Ok(f) => f,
            Err(_) => return false,
        };
        if !fds.is_empty() {
            // Find the vsock device's RX queue (queue 0) MMIO state.
            let mut vsock_device_id = None;
            for (id, dev) in &self.devices {
                if dev.info.device_type == DeviceType::VirtioVsock {
                    vsock_device_id = Some(*id);
                    break;
                }
            }
            let Some(dev_id) = vsock_device_id else {
                return false;
            };
            let Some(device) = self.devices.get(&dev_id) else {
                return false;
            };
            let Some(ref mmio_state) = device.mmio_state else {
                return false;
            };
            let Ok(mmio) = mmio_state.read() else {
                return false;
            };

            // RX queue is queue index 0 for vsock.
            let qi = 0usize;
            if qi >= 8 || !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
                return false;
            }

            let desc_addr = mmio.queue_desc[qi] as usize;
            let avail_addr = mmio.queue_driver[qi] as usize;
            let used_addr = mmio.queue_device[qi] as usize;
            let q_size = mmio.queue_num[qi] as usize;

            if avail_addr + 4 > guest_mem.len() {
                return false;
            }
            let avail_idx =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
            let used_idx_off = used_addr + 2;
            let mut used_idx =
                u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;

            // Check: are there available RX descriptors? If avail == used, no space.
            if avail_idx == used_idx {
                return false;
            }

            let mut injected = false;

            let connected = self
                .vsock_connected_ports
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default();

            for (&port, &fd) in fds.iter() {
                // Only forward data for established connections.
                if !connected.contains(&port) {
                    continue;
                }
                // Try non-blocking read from host fd.
                let mut buf = [0u8; 4096];
                let n =
                    unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
                if n <= 0 {
                    continue; // No data or error.
                }
                let data = &buf[..n as usize];

                // Build vsock header (44 bytes): host→guest, OP_RW.
                let mut hdr_bytes = [0u8; 44];
                // src_cid = HOST_CID (2)
                hdr_bytes[0..8].copy_from_slice(&2u64.to_le_bytes());
                // dst_cid = guest_cid (read from config — use 3 as default)
                hdr_bytes[8..16].copy_from_slice(&3u64.to_le_bytes());
                // src_port = port
                hdr_bytes[16..20].copy_from_slice(&port.to_le_bytes());
                // dst_port = 1024 (agent port, but guest sees it as src_port of its connection)
                hdr_bytes[20..24].copy_from_slice(&port.to_le_bytes());
                // len = payload length
                hdr_bytes[24..28].copy_from_slice(&(data.len() as u32).to_le_bytes());
                // type = VIRTIO_VSOCK_TYPE_STREAM (1)
                hdr_bytes[28..30].copy_from_slice(&1u16.to_le_bytes());
                // op = OP_RW (5)
                hdr_bytes[30..32].copy_from_slice(&5u16.to_le_bytes());
                // buf_alloc, fwd_cnt = 0 for now
                let packet = [&hdr_bytes[..], data].concat();

                // Find an available RX descriptor.
                if used_idx >= avail_idx {
                    break; // No more space.
                }
                let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
                if ring_off + 2 > guest_mem.len() {
                    break;
                }
                let head_idx =
                    u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

                // Write packet into write-only descriptors.
                let mut written = 0;
                let mut idx = head_idx;
                for _ in 0..q_size {
                    let d_off = desc_addr + idx * 16;
                    if d_off + 16 > guest_mem.len() {
                        break;
                    }
                    let addr = u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap())
                        as usize;
                    let len =
                        u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap())
                            as usize;
                    let flags =
                        u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
                    let next =
                        u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());

                    // RX descriptors should be write-only (device writes to guest).
                    if flags & 2 != 0 && addr + len <= guest_mem.len() {
                        let remaining = packet.len() - written;
                        let to_write = remaining.min(len);
                        guest_mem[addr..addr + to_write]
                            .copy_from_slice(&packet[written..written + to_write]);
                        written += to_write;
                    }

                    if flags & 1 == 0 || written >= packet.len() {
                        break;
                    }
                    idx = next as usize;
                }

                // Update used ring.
                let used_entry = used_addr + 4 + (used_idx % q_size) * 8;
                if used_entry + 8 <= guest_mem.len() {
                    guest_mem[used_entry..used_entry + 4]
                        .copy_from_slice(&(head_idx as u32).to_le_bytes());
                    guest_mem[used_entry + 4..used_entry + 8]
                        .copy_from_slice(&(written as u32).to_le_bytes());
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                    used_idx += 1;
                    guest_mem[used_idx_off..used_idx_off + 2]
                        .copy_from_slice(&(used_idx as u16).to_le_bytes());
                }

                injected = true;
                tracing::trace!("Vsock RX: injected {} bytes for port {}", written, port);
            }
        } // end if !fds.is_empty()

        // Release the fds lock before proceeding.
        drop(fds);

        tracing::info!("poll_vsock_rx: reaching TX poll section, injected={injected}");
        // Also drain vsock TX queue (queue 1) for pending guest→host responses.
        // Guest may have put OP_RESPONSE/OP_RST in the TX queue without kicking
        // (avail_event suppression or batched processing).
        if let Some(mmio_arc) = self
            .devices
            .values()
            .find(|d| d.info.device_type == DeviceType::VirtioVsock)
            .and_then(|d| d.mmio_state.as_ref())
        {
            if let Ok(mmio) = mmio_arc.read() {
                let tqi = 1usize; // TX queue index
                if tqi < 8 && mmio.queue_ready[tqi] && mmio.queue_num[tqi] > 0 {
                    let qcfg = arcbox_virtio::QueueConfig {
                        desc_addr: mmio.queue_desc[tqi],
                        avail_addr: mmio.queue_driver[tqi],
                        used_addr: mmio.queue_device[tqi],
                        size: mmio.queue_num[tqi],
                        ready: true,
                        gpa_base: self.guest_ram_gpa,
                        vsock_host_fds: Some(self.vsock_host_fds.clone()),
                        vsock_connected_ports: Some(self.vsock_connected_ports.clone()),
                    };
                    drop(mmio); // Release lock before calling process_queue.

                    // Find the vsock device and call process_queue for TX.
                    if let Some(dev) = self
                        .devices
                        .values()
                        .find(|d| d.info.device_type == DeviceType::VirtioVsock)
                        .and_then(|d| d.virtio_device.as_ref())
                    {
                        if let Ok(mut vdev) = dev.lock() {
                            match vdev.process_queue(1, guest_mem, &qcfg) {
                                Ok(completions) if !completions.is_empty() => {
                                    tracing::info!(
                                        "Vsock TX poll: {} completions",
                                        completions.len()
                                    );
                                    injected = true;
                                }
                                Err(e) => {
                                    tracing::warn!("Vsock TX poll error: {e}");
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        injected
    }
}

/// Device tree entry for FDT generation.
#[derive(Debug, Clone)]
pub struct DeviceTreeEntry {
    /// Compatible string.
    pub compatible: String,
    /// Register base address.
    pub reg_base: u64,
    /// Register region size.
    pub reg_size: u64,
    /// IRQ number.
    pub irq: Irq,
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

// Verify that DeviceManager can still be shared across threads despite
// containing a raw pointer (Send + Sync are implemented above).
#[cfg(test)]
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Send>() {}
    fn _check() {
        assert_send::<DeviceManager>();
        assert_sync::<DeviceManager>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_registration() {
        let mut manager = DeviceManager::new();
        let id = manager.register(DeviceType::Serial, "serial0").unwrap();

        let info = manager.get(id);
        assert!(info.is_some());
        assert_eq!(info.unwrap().name, "serial0");
    }

    #[test]
    fn test_virtio_mmio_state() {
        let state = VirtioMmioState::new(2, 0x1234_5678);

        assert_eq!(
            state.read(virtio_mmio::regs::MAGIC),
            virtio_mmio::MAGIC_VALUE
        );
        assert_eq!(state.read(virtio_mmio::regs::VERSION), virtio_mmio::VERSION);
        assert_eq!(state.read(virtio_mmio::regs::DEVICE_ID), 2);
    }

    #[test]
    fn test_virtio_mmio_features() {
        let mut state = VirtioMmioState::new(2, 0xDEAD_BEEF_CAFE_BABE);

        // Read low 32 bits
        assert_eq!(state.read(virtio_mmio::regs::DEVICE_FEATURES), 0xCAFE_BABE);

        // Select high 32 bits
        state.write(virtio_mmio::regs::DEVICE_FEATURES_SEL, 1);
        assert_eq!(state.read(virtio_mmio::regs::DEVICE_FEATURES), 0xDEAD_BEEF);
    }
}
