//! Device management for VMM.
//!
//! This module provides device management including `VirtIO` MMIO transport.
//!
//! The device manager bridges between:
//! - MMIO transport layer (`VirtioMmioState`) that handles guest MMIO accesses
//! - `VirtIO` device implementations (`VirtioDevice`) from arcbox-virtio

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex, RwLock};

use arcbox_net::nat_engine::checksum::{tcp_checksum, udp_checksum};
use arcbox_virtio::net::VirtioNetHeader;
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
    /// `VirtIO` entropy (RNG).
    VirtioRng,
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
    /// SHM region selector (for DAX window discovery).
    pub shm_sel: u32,
    /// SHM regions: (base_ipa, length). Index = region ID.
    pub shm_regions: Vec<(u64, u64)>,
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
            shm_sel: 0,
            shm_regions: Vec::new(),
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
            // SHM registers for DAX window discovery (VirtIO 1.2+).
            0x0ac => self.shm_sel,
            0x0b0 => {
                // SHMLenLow
                self.shm_regions
                    .get(self.shm_sel as usize)
                    .map_or(0, |r| r.1 as u32)
            }
            0x0b4 => {
                // SHMLenHigh
                self.shm_regions
                    .get(self.shm_sel as usize)
                    .map_or(0, |r| (r.1 >> 32) as u32)
            }
            0x0b8 => {
                // SHMBaseLow
                self.shm_regions
                    .get(self.shm_sel as usize)
                    .map_or(0, |r| r.0 as u32)
            }
            0x0bc => {
                // SHMBaseHigh
                self.shm_regions
                    .get(self.shm_sel as usize)
                    .map_or(0, |r| (r.0 >> 32) as u32)
            }
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
            0x0ac => self.shm_sel = value, // SHMSel write
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
pub struct RegisteredDevice {
    pub info: DeviceInfo,
    pub mmio_state: Option<Arc<RwLock<VirtioMmioState>>>,
    /// The actual `VirtIO` device implementation.
    pub virtio_device: Option<Arc<Mutex<dyn VirtioDevice>>>,
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
    /// Network host fd for HV path (NIC1 — primary, outbound traffic).
    /// Data read from this fd is injected into the VirtioNet RX queue;
    /// data extracted from TX queue is written here.
    net_host_fd: Option<std::os::unix::io::RawFd>,
    /// Last processed avail index for VirtioNet TX queue (NIC1).
    net_last_avail_tx: std::sync::atomic::AtomicUsize,
    /// DeviceId of the primary VirtioNet (NIC1) for targeted IRQ delivery.
    /// Required because `raise_interrupt_for(DeviceType::VirtioNet)` uses
    /// HashMap iteration which is non-deterministic — with two VirtioNet
    /// devices it could match the bridge NIC instead of the primary NIC.
    primary_net_device_id: Option<DeviceId>,
    /// Bridge host fd for HV path (NIC2 — vmnet bridge, container IP routing).
    /// Same semantics as `net_host_fd` but for the bridge NIC connected to vmnet.
    bridge_host_fd: Option<std::os::unix::io::RawFd>,
    /// DeviceId of the bridge VirtioNet so QUEUE_NOTIFY can dispatch correctly.
    bridge_net_device_id: Option<DeviceId>,
    /// Last processed avail index for bridge VirtioNet TX queue (NIC2).
    bridge_last_avail_tx: std::sync::atomic::AtomicUsize,
    /// Host-side vsock connection manager (HV backend only).
    vsock_connections:
        std::sync::Arc<std::sync::Mutex<crate::vsock_manager::VsockConnectionManager>>,
    /// Per-block-device async I/O worker handles. When present, QUEUE_NOTIFY
    /// for block devices is dispatched to the worker instead of processing
    /// synchronously on the vCPU thread.
    blk_workers: HashMap<DeviceId, crate::blk_worker::BlkWorkerHandle>,
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
            net_host_fd: None,
            net_last_avail_tx: std::sync::atomic::AtomicUsize::new(0),
            primary_net_device_id: None,
            bridge_host_fd: None,
            bridge_net_device_id: None,
            bridge_last_avail_tx: std::sync::atomic::AtomicUsize::new(0),
            vsock_connections: std::sync::Arc::new(std::sync::Mutex::new(
                crate::vsock_manager::VsockConnectionManager::new(),
            )),
            blk_workers: HashMap::new(),
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

    /// Sets the host-side network fd for HV path frame exchange (NIC1).
    pub fn set_net_host_fd(&mut self, fd: std::os::unix::io::RawFd, device_id: DeviceId) {
        self.net_host_fd = Some(fd);
        self.primary_net_device_id = Some(device_id);
    }

    /// Returns the primary NIC device ID (for targeted IRQ delivery).
    pub fn primary_net_device_id(&self) -> Option<DeviceId> {
        self.primary_net_device_id
    }

    /// Sets the bridge NIC host fd and device ID (NIC2 — vmnet bridge).
    pub fn set_bridge_host_fd(&mut self, fd: std::os::unix::io::RawFd, device_id: DeviceId) {
        self.bridge_host_fd = Some(fd);
        self.bridge_net_device_id = Some(device_id);
    }

    /// Returns the guest RAM base pointer (for worker thread context).
    pub fn guest_ram_base_ptr(&self) -> Option<*mut u8> {
        self.guest_ram_base
    }

    /// Returns the guest RAM size.
    pub fn guest_ram_size(&self) -> usize {
        self.guest_ram_size
    }

    /// Returns the guest RAM GPA base.
    pub fn guest_ram_gpa(&self) -> u64 {
        self.guest_ram_gpa
    }

    /// Returns a reference to a registered device by ID.
    pub fn get_registered_device(&self, id: DeviceId) -> Option<&RegisteredDevice> {
        self.devices.get(&id)
    }

    /// Registers an async block I/O worker set for a device (one per queue).
    pub fn set_blk_worker(
        &mut self,
        device_id: DeviceId,
        handle: crate::blk_worker::BlkWorkerHandle,
    ) {
        self.blk_workers.insert(device_id, handle);
    }

    /// Returns a clone of the IRQ callback Arc (if set).
    pub fn irq_callback_clone(&self) -> Option<DeviceIrqCallback> {
        self.irq_callback.clone()
    }

    /// Returns a clone of the vsock connection manager Arc.
    pub fn vsock_connections(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::vsock_manager::VsockConnectionManager>> {
        self.vsock_connections.clone()
    }

    /// Sets the GIC SPI level to match a device's interrupt_status.
    ///
    /// For level-triggered SPIs, the line must reflect whether interrupt_status
    /// has any bits set. Call this after ANY mutation of interrupt_status
    /// (trigger_interrupt or INTERRUPT_ACK).
    ///
    /// Skips devices that haven't reached DRIVER_OK to avoid "nobody cared"
    /// in the guest kernel before the IRQ handler is installed.
    pub fn sync_irq_level(&self, device_id: DeviceId) {
        let Some(device) = self.devices.get(&device_id) else {
            return;
        };
        let Some(irq) = device.info.irq else {
            return;
        };
        let Some(ref mmio_arc) = device.mmio_state else {
            return;
        };
        let Ok(mmio) = mmio_arc.read() else {
            return;
        };

        // Don't inject IRQs before the guest driver is ready.
        if mmio.status & DeviceStatus::DRIVER_OK == 0 {
            tracing::trace!(
                "sync_irq_level: device {:?} not DRIVER_OK (status={:#x}), skipping",
                device.info.device_type,
                mmio.status,
            );
            return;
        }

        let level = mmio.interrupt_status != 0;
        tracing::trace!(
            "sync_irq_level: device {:?} irq={} interrupt_status={} -> SPI level={}",
            device.info.device_type,
            irq,
            mmio.interrupt_status,
            level,
        );
        if let Some(ref cb) = self.irq_callback {
            let _ = cb(irq, level);
        }
    }

    /// Triggers an IRQ through the configured callback (if set).
    ///
    /// Only fires if the device owning this IRQ has reached DRIVER_OK status.
    pub fn trigger_irq_callback(&self, irq: Irq, level: bool) {
        // Guard: check that the device owning this IRQ is activated.
        let device_ready = self.devices.values().any(|d| {
            d.info.irq == Some(irq)
                && d.mmio_state
                    .as_ref()
                    .and_then(|s| s.read().ok())
                    .is_some_and(|s| s.status & DeviceStatus::DRIVER_OK != 0)
        });
        if !device_ready {
            return;
        }
        if let Some(ref cb) = self.irq_callback {
            let _ = cb(irq, level);
        }
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
        let irq = irq_chip.allocate_level_irq()?;

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
        let irq = irq_chip.allocate_level_irq()?;

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

    /// Sets interrupt_status and syncs the GIC SPI level for a device type.
    /// Used by the vCPU polling paths (vsock RX, net RX) after injecting data.
    /// Note: matches the FIRST device of the given type. For bridge NIC, use
    /// `raise_interrupt_for_device` with the specific device ID.
    pub fn raise_interrupt_for(&self, device_type: DeviceType, reason: u32) {
        for (id, dev) in &self.devices {
            if dev.info.device_type == device_type {
                if let Some(ref mmio_arc) = dev.mmio_state {
                    if let Ok(mut s) = mmio_arc.write() {
                        s.trigger_interrupt(reason);
                    }
                }
                self.sync_irq_level(*id);
                break;
            }
        }
    }

    /// Raises interrupt for a specific device ID. Used for the bridge NIC
    /// which shares `DeviceType::VirtioNet` with the primary NIC.
    pub fn raise_interrupt_for_device(&self, device_id: DeviceId, reason: u32) {
        if let Some(dev) = self.devices.get(&device_id) {
            if let Some(ref mmio_arc) = dev.mmio_state {
                if let Ok(mut s) = mmio_arc.write() {
                    s.trigger_interrupt(reason);
                }
            }
            self.sync_irq_level(device_id);
        }
    }

    /// Returns the bridge NIC device ID (if configured).
    pub fn bridge_device_id(&self) -> Option<DeviceId> {
        self.bridge_net_device_id
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
                    // Log vsock TX notifications at trace level (per-kick hot path).
                    if device.info.device_type == DeviceType::VirtioVsock && queue_idx == 1 {
                        tracing::trace!("QUEUE_NOTIFY: vsock TX queue 1 kicked by guest!",);
                    }
                    tracing::trace!(
                        "QUEUE_NOTIFY: device {} ({:?}) queue {}",
                        device_id.0,
                        device.info.device_type,
                        queue_idx,
                    );

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
                                    vsock_connections: Some(self.vsock_connections.clone()),
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

                            // VirtioBlock async path: dispatch to worker thread
                            // instead of blocking the vCPU with synchronous I/O.
                            if device.info.device_type == DeviceType::VirtioBlock
                                && self.blk_workers.contains_key(&device_id)
                            {
                                tracing::trace!("blk async dispatch for device {}", device_id.0);
                                match self
                                    .dispatch_blk_async(guest_mem, &qcfg, device_id, queue_idx)
                                {
                                    Ok(true) => {
                                        // Worker will handle completions and IRQ.
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        tracing::warn!("blk async dispatch error: {e}");
                                    }
                                }
                            }
                            // VirtioNet TX (queue 1): extract ethernet frames
                            // from guest memory and write to the network host fd.
                            // This bypasses the generic process_queue.
                            // Dispatch to correct fd: NIC1 (net_host_fd) or NIC2 (bridge).
                            else if device.info.device_type == DeviceType::VirtioNet
                                && queue_idx == 1
                                && (self.net_host_fd.is_some() || self.bridge_host_fd.is_some())
                            {
                                let is_bridge = self
                                    .bridge_net_device_id
                                    .is_some_and(|bid| bid == device_id);
                                let net_completions = if is_bridge {
                                    self.handle_bridge_tx(guest_mem, &qcfg)
                                } else {
                                    self.handle_net_tx(guest_mem, &qcfg)
                                };
                                if !net_completions.is_empty() {
                                    // Update used ring for completed TX descriptors.
                                    let used_addr = qcfg.used_addr as usize;
                                    let q_size = qcfg.size as usize;
                                    let used_idx_off = used_addr + 2;
                                    let mut used_idx = u16::from_le_bytes([
                                        guest_mem[used_idx_off],
                                        guest_mem[used_idx_off + 1],
                                    ]);
                                    for &(head, len) in &net_completions {
                                        let entry =
                                            used_addr + 4 + ((used_idx as usize) % q_size) * 8;
                                        if entry + 8 <= guest_mem.len() {
                                            guest_mem[entry..entry + 4]
                                                .copy_from_slice(&(head as u32).to_le_bytes());
                                            guest_mem[entry + 4..entry + 8]
                                                .copy_from_slice(&len.to_le_bytes());
                                            used_idx = used_idx.wrapping_add(1);
                                        }
                                    }
                                    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                                    guest_mem[used_idx_off..used_idx_off + 2]
                                        .copy_from_slice(&used_idx.to_le_bytes());

                                    // Write avail_event in the used ring to request
                                    // kicks from the guest on future TX submissions.
                                    // With VIRTIO_F_EVENT_IDX, the guest checks
                                    // vring_need_event(avail_event, new, old) before
                                    // kicking. Setting avail_event = current avail_idx
                                    // ensures the guest kicks on the next submission.
                                    let avail_idx = u16::from_le_bytes([
                                        guest_mem[qcfg.avail_addr as usize + 2],
                                        guest_mem[qcfg.avail_addr as usize + 3],
                                    ]);
                                    let avail_event_off = used_addr + 4 + q_size * 8;
                                    if avail_event_off + 2 <= guest_mem.len() {
                                        guest_mem[avail_event_off..avail_event_off + 2]
                                            .copy_from_slice(&avail_idx.to_le_bytes());
                                    }

                                    if let Some(_irq) = device.info.irq {
                                        {
                                            let mut s = state.write().map_err(|e| {
                                                VmmError::Device(format!(
                                                    "Failed to lock state: {e}"
                                                ))
                                            })?;
                                            s.trigger_interrupt(virtio_mmio::INT_VRING);
                                        }
                                        self.sync_irq_level(device_id);
                                    }
                                }
                            } else {
                                // Generic process_queue for all other devices.
                                let mut dev = virtio_dev.lock().map_err(|e| {
                                    VmmError::Device(format!("Failed to lock device: {e}"))
                                })?;
                                // Log vsock TX processing at trace level (per-kick hot path).
                                let is_vsock_tx = device.info.device_type
                                    == DeviceType::VirtioVsock
                                    && queue_idx == 1;
                                match dev.process_queue(queue_idx, guest_mem, &qcfg) {
                                    Ok(completions) if !completions.is_empty() => {
                                        if is_vsock_tx {
                                            tracing::trace!(
                                                "Vsock QUEUE_NOTIFY TX: {} completions processed!",
                                                completions.len(),
                                            );
                                        }
                                        tracing::trace!(
                                            "Device {} queue {} processed {} completions",
                                            device_id.0,
                                            queue_idx,
                                            completions.len()
                                        );
                                        // Console TX completions don't need interrupts —
                                        // the guest doesn't wait for host ACK on console output.
                                        // Skipping avoids interrupt storms with level-triggered SPIs.
                                        let skip_irq = device.info.device_type
                                            == DeviceType::VirtioConsole
                                            && queue_idx == 1;
                                        if !skip_irq {
                                            {
                                                let mut s = state.write().map_err(|e| {
                                                    VmmError::Device(format!(
                                                        "Failed to lock state: {e}"
                                                    ))
                                                })?;
                                                s.trigger_interrupt(virtio_mmio::INT_VRING);
                                            }
                                            self.sync_irq_level(device_id);
                                        }
                                    }
                                    Ok(_) => {
                                        if is_vsock_tx {
                                            tracing::trace!(
                                                "Vsock QUEUE_NOTIFY TX: kicked but 0 completions \
                                                 (last_avail_idx_tx may already be current)",
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Device {} queue {} error: {e}",
                                            device_id.0,
                                            queue_idx
                                        );
                                    }
                                }
                            } // end else (non-VirtioNet)
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
                virtio_mmio::regs::INTERRUPT_ACK => {
                    // Sync the GIC SPI level with the updated interrupt_status.
                    // If all bits are cleared, the SPI goes low; if bits remain
                    // (from a concurrent completion), the SPI stays high.
                    self.sync_irq_level(device_id);
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
        // Sort by MMIO base address so the FDT node order is deterministic.
        // Linux discovers virtio-mmio devices in FDT order, so the first
        // virtio-blk node becomes vda, the second vdb, etc. Without sorting,
        // HashMap iteration order is arbitrary and block device naming becomes
        // non-deterministic (root=/dev/vda may point at the wrong disk).
        let mut entries: Vec<DeviceTreeEntry> = self
            .devices
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
            .collect();
        entries.sort_by_key(|e| e.reg_base);
        entries
    }

    /// Polls host vsock fds for incoming data and injects into guest RX queue.
    ///
    /// Injects a vsock OP_REQUEST into the guest RX queue to initiate a connection.
    /// If the device isn't ready yet, queues the request for later injection
    /// during poll_vsock_rx.
    /// Injects an OP_REQUEST for a newly allocated vsock connection.
    ///
    /// The connection must have already been allocated in the manager via
    /// `VsockConnectionManager::allocate()`. This method attempts immediate
    /// injection; if the device isn't ready, the manager's pending list
    /// ensures deferred injection via `poll_vsock_rx`.
    /// Attempts immediate RX fill for a newly allocated connection.
    ///
    /// The connection has already been allocated with `RxOps::REQUEST` enqueued
    /// and pushed to `backend_rxq`. This method tries to fill an RX descriptor
    /// now. If the device isn't ready, the pending op stays in `backend_rxq`
    /// for deferred processing by `poll_vsock_rx`.
    /// Injects the OP_REQUEST for a newly allocated vsock connection directly
    /// into the guest RX virtqueue.
    ///
    /// This uses `inject_vsock_rx_raw` (low-level direct guest memory write)
    /// instead of going through `poll_vsock_rx`, so it does NOT acquire the
    /// `vsock_connections` mutex and cannot deadlock with the vCPU thread.
    ///
    /// **Thread safety**: writes to guest memory are safe because the vCPU is
    /// either (a) inside `hv_vcpu_run` (guest code doesn't touch RX used ring
    /// while the device hasn't signaled) or (b) in the BSP poll loop which
    /// only touches RX descriptors through `poll_vsock_rx` (mutual exclusion
    /// by the GIL-like BSP poll structure). The `inject_vsock_rx_raw` method
    /// uses `fence(Release)` to ensure descriptor data is visible before the
    /// used_idx update.
    pub fn inject_vsock_connect(
        &self,
        id: crate::vsock_manager::VsockConnectionId,
        guest_cid: u64,
    ) -> bool {
        use arcbox_virtio::vsock::{VsockAddr, VsockHeader, VsockOp};

        let hdr = VsockHeader::new(
            VsockAddr::host(id.host_port),
            VsockAddr::new(guest_cid, id.guest_port),
            VsockOp::Request,
        );

        // Try direct injection with retry. The vsock device may not be
        // DRIVER_OK yet during early boot — poll until it is, up to 10s.
        // This ensures the daemon's fd is usable immediately after return
        // (guest will either RST or RESPONSE within one vCPU cycle).
        //
        // We drain the REQUEST from backend_rxq on success to prevent
        // poll_vsock_rx from injecting a duplicate.
        for attempt in 0..1000 {
            if self.inject_vsock_rx_raw(&hdr.to_bytes()) {
                if let Ok(mut mgr) = self.vsock_connections.lock() {
                    mgr.backend_rxq.retain(|qid| *qid != id);
                    if let Some(conn) = mgr.get_mut(&id) {
                        conn.rx_queue.dequeue();
                    }
                }
                return true;
            }
            if attempt == 0 {
                tracing::debug!(
                    "vsock inject: device not ready, polling (guest_port={})",
                    id.guest_port,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        tracing::warn!(
            "vsock inject: gave up after 10s (guest_port={}), leaving in backend_rxq",
            id.guest_port,
        );
        false
    }

    /// Injects a raw packet into the vsock RX queue (queue 0).
    /// Writes a packet into the next available RX descriptor in guest memory.
    ///
    /// Pops one descriptor from the avail ring, walks the chain writing data,
    /// and updates the used ring. Returns number of bytes written (0 on failure).
    fn write_to_rx_descriptor(
        &self,
        guest_mem: &mut [u8],
        desc_addr: usize,
        avail_addr: usize,
        used_addr: usize,
        q_size: usize,
        packet: &[u8],
    ) -> usize {
        let avail_idx =
            u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
        let used_idx_off = used_addr + 2;
        let used_idx =
            u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;

        if avail_idx == used_idx {
            return 0; // No available descriptors.
        }

        let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
        if ring_off + 2 > guest_mem.len() {
            return 0;
        }
        let head_idx = u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

        tracing::trace!(
            "vsock write_to_rx_desc: avail_idx={} used_idx={} head_idx={} pkt_len={} q_size={}",
            avail_idx,
            used_idx,
            head_idx,
            packet.len(),
            q_size,
        );

        // Walk descriptor chain, writing packet data to WRITE-flagged descriptors.
        let mut written = 0;
        let mut idx = head_idx;
        let mut desc_count = 0u32;
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

            desc_count += 1;
            tracing::trace!(
                "  desc[{}]: addr={:#x} len={} flags={:#06x} (W={} N={}) next={}",
                idx,
                addr,
                len,
                flags,
                flags & 2 != 0,
                flags & 1 != 0,
                next,
            );

            // WRITE flag = 0x02 (VRING_DESC_F_WRITE).
            if flags & 2 != 0 && addr + len <= guest_mem.len() {
                let remaining = packet.len().saturating_sub(written);
                let to_write = remaining.min(len);
                if to_write > 0 {
                    guest_mem[addr..addr + to_write]
                        .copy_from_slice(&packet[written..written + to_write]);
                    written += to_write;
                    tracing::trace!(
                        "  → wrote {} bytes at GPA {:#x} (total={})",
                        to_write,
                        addr,
                        written,
                    );
                }
            } else if flags & 2 == 0 {
                tracing::warn!("  desc[{}] has no WRITE flag!", idx);
            }

            // NEXT flag = 0x01 (VRING_DESC_F_NEXT).
            if flags & 1 == 0 || written >= packet.len() {
                break;
            }
            idx = next as usize;
        }

        if written == 0 {
            tracing::error!(
                "vsock write_to_rx_desc: FAILED — 0 bytes written! {} descs examined",
                desc_count,
            );
            return 0;
        }

        // Update used ring entry.
        let used_entry = used_addr + 4 + (used_idx % q_size) * 8;
        if used_entry + 8 <= guest_mem.len() {
            guest_mem[used_entry..used_entry + 4].copy_from_slice(&(head_idx as u32).to_le_bytes());
            guest_mem[used_entry + 4..used_entry + 8]
                .copy_from_slice(&(written as u32).to_le_bytes());
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            let new_used = (used_idx + 1) as u16;
            guest_mem[used_idx_off..used_idx_off + 2].copy_from_slice(&new_used.to_le_bytes());

            tracing::trace!(
                "vsock write_to_rx_desc: OK — {} bytes, used_idx {} → {}",
                written,
                used_idx,
                used_idx + 1,
            );

            // Dump first 44 bytes of packet for verification.
            if packet.len() >= 44 {
                let src_cid = u64::from_le_bytes(packet[0..8].try_into().unwrap());
                let dst_cid = u64::from_le_bytes(packet[8..16].try_into().unwrap());
                let src_port = u32::from_le_bytes(packet[16..20].try_into().unwrap());
                let dst_port = u32::from_le_bytes(packet[20..24].try_into().unwrap());
                let pkt_len = u32::from_le_bytes(packet[24..28].try_into().unwrap());
                let sock_type = u16::from_le_bytes(packet[28..30].try_into().unwrap());
                let op = u16::from_le_bytes(packet[30..32].try_into().unwrap());
                let flags = u32::from_le_bytes(packet[32..36].try_into().unwrap());
                let buf_alloc = u32::from_le_bytes(packet[36..40].try_into().unwrap());
                let fwd_cnt = u32::from_le_bytes(packet[40..44].try_into().unwrap());
                tracing::trace!(
                    "  header: src={}:{} dst={}:{} len={} type={} op={} flags={} buf_alloc={} fwd_cnt={}",
                    src_cid,
                    src_port,
                    dst_cid,
                    dst_port,
                    pkt_len,
                    sock_type,
                    op,
                    flags,
                    buf_alloc,
                    fwd_cnt,
                );
            }

            // Readback verification: read the header from guest memory.
            if desc_count >= 1 && written >= 44 {
                let first_d_off = desc_addr + head_idx * 16;
                let first_addr =
                    u64::from_le_bytes(guest_mem[first_d_off..first_d_off + 8].try_into().unwrap())
                        as usize;
                if first_addr + 44 <= guest_mem.len() {
                    let readback = &guest_mem[first_addr..first_addr + 44];
                    let rb_dst_cid = u64::from_le_bytes(readback[8..16].try_into().unwrap());
                    let rb_op = u16::from_le_bytes(readback[30..32].try_into().unwrap());
                    tracing::trace!(
                        "  readback: dst_cid={} op={} first_8_bytes={:02x?}",
                        rb_dst_cid,
                        rb_op,
                        &readback[..8],
                    );
                }
            }
        }

        // Also check TX queue state for diagnostics.
        if let Some(mmio_arc) = self
            .devices
            .values()
            .find(|d| d.info.device_type == DeviceType::VirtioVsock)
            .and_then(|d| d.mmio_state.as_ref())
        {
            if let Ok(mmio) = mmio_arc.read() {
                let txi = 1usize;
                if txi < 8 && mmio.queue_ready[txi] && mmio.queue_num[txi] > 0 {
                    let tx_avail_addr = mmio.queue_driver[txi] as usize;
                    let tx_used_addr = mmio.queue_device[txi] as usize;
                    if tx_avail_addr + 4 <= guest_mem.len() && tx_used_addr + 4 <= guest_mem.len() {
                        let tx_avail = u16::from_le_bytes([
                            guest_mem[tx_avail_addr + 2],
                            guest_mem[tx_avail_addr + 3],
                        ]);
                        let tx_used = u16::from_le_bytes([
                            guest_mem[tx_used_addr + 2],
                            guest_mem[tx_used_addr + 3],
                        ]);
                        tracing::trace!(
                            "  TX queue state: avail_idx={} used_idx={} (delta={})",
                            tx_avail,
                            tx_used,
                            tx_avail.wrapping_sub(tx_used),
                        );
                    }
                } else {
                    tracing::warn!(
                        "  TX queue NOT ready: ready={} num={}",
                        if txi < 8 {
                            mmio.queue_ready[txi]
                        } else {
                            false
                        },
                        if txi < 8 { mmio.queue_num[txi] } else { 0 },
                    );
                }
            }
        }

        written
    }

    /// Injects a raw packet into the vsock RX queue (queue 0).
    ///
    /// Directly writes to guest memory and updates the used ring. Safe to call
    /// from the daemon thread — does NOT acquire `vsock_connections` mutex.
    /// Used by `inject_vsock_connect` for immediate OP_REQUEST delivery.
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

        tracing::debug!(
            "inject_vsock_rx_raw: desc_addr={:#x} avail_addr={:#x} used_addr={:#x} q_size={}",
            desc_addr,
            avail_addr,
            used_addr,
            q_size,
        );

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
        tracing::debug!(
            "inject_vsock_rx_raw: avail_idx={} used_idx={} q_size={} packet_len={}",
            avail_idx,
            used_idx,
            q_size,
            packet.len(),
        );

        if avail_idx == used_idx {
            tracing::warn!("inject_vsock_rx_raw: no available RX descriptors (avail == used)");
            return false;
        }

        let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
        if ring_off + 2 > guest_mem.len() {
            return false;
        }
        let head_idx = u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;
        tracing::debug!("inject_vsock_rx_raw: head_idx={}", head_idx);

        let mut written = 0;
        let mut idx = head_idx;
        let mut desc_count = 0u32;
        for _ in 0..q_size {
            let d_off = desc_addr + idx * 16;
            if d_off + 16 > guest_mem.len() {
                tracing::warn!(
                    "inject_vsock_rx_raw: descriptor offset {:#x} out of bounds",
                    d_off
                );
                break;
            }
            let addr = u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
            let len =
                u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
            let next = u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());

            desc_count += 1;
            tracing::debug!(
                "inject_vsock_rx_raw: desc[{}] addr={:#x} len={} flags={:#06x} (WRITE={} NEXT={}) next={}",
                idx,
                addr,
                len,
                flags,
                flags & 2 != 0,
                flags & 1 != 0,
                next,
            );

            if flags & 2 != 0 && addr + len <= guest_mem.len() {
                let remaining = packet.len().saturating_sub(written);
                let to_write = remaining.min(len);
                guest_mem[addr..addr + to_write]
                    .copy_from_slice(&packet[written..written + to_write]);
                written += to_write;
                tracing::debug!(
                    "inject_vsock_rx_raw: wrote {} bytes at GPA {:#x} (total written={})",
                    to_write,
                    addr,
                    written,
                );
            } else if flags & 2 == 0 {
                tracing::warn!(
                    "inject_vsock_rx_raw: descriptor {} has no WRITE flag (flags={:#06x}), skipping!",
                    idx,
                    flags,
                );
            } else {
                tracing::warn!(
                    "inject_vsock_rx_raw: descriptor {} addr+len ({:#x}+{}) exceeds guest memory ({:#x})",
                    idx,
                    addr,
                    len,
                    guest_mem.len(),
                );
            }
            if flags & 1 == 0 || written >= packet.len() {
                break;
            }
            idx = next as usize;
        }

        if written == 0 {
            tracing::error!(
                "inject_vsock_rx_raw: FAILED — 0 bytes written! {} descriptors examined. \
                 Guest will silently drop the empty packet.",
                desc_count,
            );
            return false;
        }

        if written < packet.len() {
            tracing::warn!(
                "inject_vsock_rx_raw: partial write: {}/{} bytes (guest may drop short packet)",
                written,
                packet.len(),
            );
        }

        let used_entry = used_addr + 4 + (used_idx % q_size) * 8;
        if used_entry + 8 <= guest_mem.len() {
            guest_mem[used_entry..used_entry + 4].copy_from_slice(&(head_idx as u32).to_le_bytes());
            guest_mem[used_entry + 4..used_entry + 8]
                .copy_from_slice(&(written as u32).to_le_bytes());
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            let new_used = (used_idx + 1) as u16;
            guest_mem[used_idx_off..used_idx_off + 2].copy_from_slice(&new_used.to_le_bytes());
            tracing::trace!(
                "inject_vsock_rx_raw: OK — {} bytes injected, used_idx {} -> {}",
                written,
                used_idx,
                used_idx + 1,
            );
        }

        // Verify: read back first 8 bytes from the buffer to confirm data integrity.
        if written >= 8 {
            let first_desc_off = desc_addr + head_idx * 16;
            if first_desc_off + 8 <= guest_mem.len() {
                let verify_addr = u64::from_le_bytes(
                    guest_mem[first_desc_off..first_desc_off + 8]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if verify_addr + 8 <= guest_mem.len() {
                    let readback = &guest_mem[verify_addr..verify_addr + 8];
                    tracing::debug!(
                        "inject_vsock_rx_raw: verify first 8 bytes at {:#x}: {:02x?}",
                        verify_addr,
                        readback,
                    );
                }
            }
        }

        true
    }

    /// Dispatches block I/O descriptors to the async worker thread.
    ///
    /// Parses the avail ring, builds `BlkWorkItem`s, and sends them via the
    /// channel. The worker thread performs pread/pwrite and writes completions.
    /// Returns Ok(true) if any items were dispatched.
    pub fn dispatch_blk_async(
        &self,
        memory: &mut [u8],
        qcfg: &QueueConfig,
        device_id: DeviceId,
        queue_idx: u16,
    ) -> Result<bool> {
        use crate::blk_worker::{BlkRequestType, BlkWorkItem};

        let Some(handle) = self.blk_workers.get(&device_id) else {
            return Ok(false);
        };
        let Some(worker) = handle.get_queue(queue_idx) else {
            return Ok(false);
        };

        if !qcfg.ready || qcfg.size == 0 {
            return Ok(false);
        }

        let desc_addr = qcfg.desc_addr as usize;
        let avail_addr = qcfg.avail_addr as usize;
        let used_addr = qcfg.used_addr as usize;
        let q_size = qcfg.size as usize;

        if avail_addr + 4 > memory.len() {
            return Ok(false);
        }
        let avail_idx = u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]);

        let last_avail = worker
            .last_avail_idx
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut current = last_avail;
        let mut dispatched = false;

        while current != avail_idx {
            let ring_off = avail_addr + 4 + 2 * ((current as usize) % q_size);
            if ring_off + 2 > memory.len() {
                break;
            }
            let head_idx = u16::from_le_bytes([memory[ring_off], memory[ring_off + 1]]);

            // Walk descriptor chain.
            let mut buffers = Vec::new();
            let mut status_gpa: u64 = 0;
            let mut total_data_len: u32 = 0;
            let mut request_type = BlkRequestType::Read;
            let mut sector: u64 = 0;
            let mut first_desc = true;
            let mut idx = head_idx as usize;

            loop {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > memory.len() {
                    break;
                }
                let addr = u64::from_le_bytes(memory[d_off..d_off + 8].try_into().unwrap());
                let len = u32::from_le_bytes(memory[d_off + 8..d_off + 12].try_into().unwrap());
                let flags = u16::from_le_bytes(memory[d_off + 12..d_off + 14].try_into().unwrap());
                let next = u16::from_le_bytes(memory[d_off + 14..d_off + 16].try_into().unwrap());
                let is_write = flags & 2 != 0;

                if first_desc {
                    // First descriptor = block request header (16 bytes).
                    first_desc = false;
                    if len >= 16 {
                        let hdr_start = addr as usize;
                        if hdr_start + 16 <= memory.len() {
                            let req_type = u32::from_le_bytes(
                                memory[hdr_start..hdr_start + 4].try_into().unwrap(),
                            );
                            sector = u64::from_le_bytes(
                                memory[hdr_start + 8..hdr_start + 16].try_into().unwrap(),
                            );
                            request_type = match req_type {
                                0 => BlkRequestType::Read,
                                1 => BlkRequestType::Write,
                                4 => BlkRequestType::Flush,
                                8 => BlkRequestType::GetId,
                                _ => BlkRequestType::Read,
                            };
                        }
                    }
                } else {
                    buffers.push((addr, len, is_write));
                    // Last writable descriptor's last byte = status byte.
                    if is_write && len > 0 {
                        status_gpa = addr + u64::from(len) - 1;
                    }
                    // Count data bytes (exclude 1-byte status descriptor).
                    if len > 1 {
                        total_data_len += len;
                    }
                }

                if flags & 1 == 0 {
                    break; // No NEXT flag.
                }
                idx = next as usize;
                if idx >= q_size {
                    break;
                }
            }

            let item = BlkWorkItem {
                head_idx,
                request_type,
                sector,
                buffers,
                status_gpa,
                total_data_len,
                used_addr: qcfg.used_addr,
                avail_addr: qcfg.avail_addr,
                queue_size: qcfg.size,
            };

            if worker.tx.send(item).is_err() {
                tracing::warn!("blk worker channel closed, falling back to sync");
                // Remove from map on next opportunity. For now, break.
                break;
            }
            dispatched = true;
            current = current.wrapping_add(1);
        }

        worker
            .last_avail_idx
            .store(current, std::sync::atomic::Ordering::Relaxed);

        // Update avail_event for EVENT_IDX.
        if dispatched {
            let avail_event_off = used_addr + 4 + q_size * 8;
            if avail_event_off + 2 <= memory.len() {
                memory[avail_event_off..avail_event_off + 2]
                    .copy_from_slice(&current.to_le_bytes());
            }
        }

        Ok(dispatched)
    }

    /// Called from the vCPU run loop during WFI (guest idle). Returns true
    /// if any data was injected (caller should trigger interrupt).
    #[allow(unused_assignments, unused_variables)]
    pub fn poll_vsock_rx(&self) -> bool {
        use crate::vsock_manager::RxOps;
        use arcbox_virtio::vsock::{VsockAddr, VsockHeader, VsockOp};

        let mut injected = false;

        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return false;
        };
        let gpa_base = self.guest_ram_gpa as usize;
        let guest_mem =
            unsafe { std::slice::from_raw_parts_mut(ram_base.sub(gpa_base), gpa_base + ram_size) };

        // ------------------------------------------------------------------
        // Phase 1: Check connected streams for readable data → enqueue RW
        // ------------------------------------------------------------------
        {
            let connected_fds = self
                .vsock_connections
                .lock()
                .map(|mgr| mgr.connected_fds())
                .unwrap_or_default();

            // Log at INFO once per unique count change to avoid spam.
            static LAST_COUNT: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let count = connected_fds.len();
            if count != LAST_COUNT.swap(count, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("vsock Phase 1: {} connected fds", count);
            }

            for (conn_id, fd) in &connected_fds {
                // Peek if there's data without consuming it.
                let mut peek_buf = [0u8; 1];
                let n = unsafe {
                    libc::recv(
                        *fd,
                        peek_buf.as_mut_ptr().cast::<libc::c_void>(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if n > 0 {
                    tracing::trace!(
                        "vsock Phase 1: data available on fd {} for {:?} — enqueue RW",
                        fd,
                        conn_id,
                    );
                    if let Ok(mut mgr) = self.vsock_connections.lock() {
                        mgr.enqueue_rw(*conn_id);
                    }
                } else if n == 0 {
                    tracing::info!(
                        "vsock Phase 1: EOF on fd {} for {:?} — enqueue RST",
                        fd,
                        conn_id,
                    );
                    // Host stream closed — enqueue RST.
                    if let Ok(mut mgr) = self.vsock_connections.lock() {
                        mgr.enqueue_reset(*conn_id);
                    }
                }
                // n < 0 with EAGAIN/EWOULDBLOCK = no data, skip.
            }
        }

        // ------------------------------------------------------------------
        // Phase 2: Drain backend_rxq → fill available RX descriptors
        // ------------------------------------------------------------------
        // Get RX queue MMIO state.
        let mmio_state = self
            .devices
            .values()
            .find(|d| d.info.device_type == DeviceType::VirtioVsock)
            .and_then(|d| d.mmio_state.as_ref());
        let Some(mmio_arc) = mmio_state else {
            return false;
        };
        let Ok(mmio) = mmio_arc.read() else {
            return false;
        };

        let rxi = 0usize;
        if rxi >= 8 || !mmio.queue_ready[rxi] || mmio.queue_num[rxi] == 0 {
            return false;
        }

        let desc_addr = mmio.queue_desc[rxi] as usize;
        let avail_addr = mmio.queue_driver[rxi] as usize;
        let used_addr = mmio.queue_device[rxi] as usize;
        let q_size = mmio.queue_num[rxi] as usize;

        // Also grab TX queue config for Phase 3.
        let txi = 1usize;
        let tx_ready = txi < 8 && mmio.queue_ready[txi] && mmio.queue_num[txi] > 0;
        let tx_qcfg = if tx_ready {
            Some(arcbox_virtio::QueueConfig {
                desc_addr: mmio.queue_desc[txi],
                avail_addr: mmio.queue_driver[txi],
                used_addr: mmio.queue_device[txi],
                size: mmio.queue_num[txi],
                ready: true,
                gpa_base: self.guest_ram_gpa,
                vsock_connections: Some(self.vsock_connections.clone()),
            })
        } else {
            None
        };
        drop(mmio);

        if avail_addr + 4 > guest_mem.len() {
            return false;
        }

        // Process backend_rxq: pop connections, fill RX descriptors.
        //
        // IMPORTANT: If we break out of this loop because RX descriptors are
        // exhausted (avail_idx == used_idx) while backend_rxq still has entries,
        // those entries remain in the queue for the next poll cycle. However,
        // the guest won't refill RX descriptors until its rx_work runs, which
        // requires an interrupt. We set `injected = true` in that case so the
        // caller triggers INT_VRING, waking the guest's virtio_vsock_rx_fill.
        let mut rxq_starved = false;
        loop {
            // Check available RX descriptors.
            let avail_idx =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
            let used_idx_off = used_addr + 2;
            let used_idx =
                u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;

            if avail_idx == used_idx {
                // No RX descriptors available. If backend_rxq still has pending
                // entries, mark as starved so we trigger an interrupt to make
                // the guest refill its RX queue.
                if let Ok(mgr) = self.vsock_connections.lock() {
                    if !mgr.backend_rxq.is_empty() {
                        rxq_starved = true;
                    }
                }
                break;
            }

            // Pop next connection from backend_rxq.
            let conn_id = {
                let Ok(mut mgr) = self.vsock_connections.lock() else {
                    break;
                };
                mgr.backend_rxq.pop_front()
            };
            let Some(conn_id) = conn_id else {
                break; // No pending connections.
            };

            // Build the packet for this connection's highest-priority RX op.
            let packet = {
                let Ok(mut mgr) = self.vsock_connections.lock() else {
                    break;
                };
                let Some(conn) = mgr.get_mut(&conn_id) else {
                    continue; // Connection removed while queued.
                };

                // Peek: if Reset is highest priority, handle teardown.
                if conn.rx_queue.peek() == RxOps::RESET {
                    conn.rx_queue.dequeue();
                    let hdr = VsockHeader::new(
                        VsockAddr::host(conn_id.host_port),
                        VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                        VsockOp::Rst,
                    );
                    let pkt = hdr.to_bytes().to_vec();
                    // Remove connection after sending RST.
                    mgr.remove(&conn_id);
                    pkt
                } else {
                    let op = conn.rx_queue.dequeue();
                    if op == 0 {
                        continue; // Spurious entry — no pending ops.
                    }

                    match op {
                        RxOps::REQUEST => {
                            let hdr = VsockHeader::new(
                                VsockAddr::host(conn_id.host_port),
                                VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                VsockOp::Request,
                            );
                            tracing::info!(
                                "Vsock RX: sending OP_REQUEST guest_port={} host_port={}",
                                conn_id.guest_port,
                                conn_id.host_port,
                            );
                            hdr.to_bytes().to_vec()
                        }
                        RxOps::RESPONSE => {
                            conn.connect = true;
                            let hdr = VsockHeader::new(
                                VsockAddr::host(conn_id.host_port),
                                VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                VsockOp::Response,
                            );
                            tracing::info!(
                                "Vsock RX: sending OP_RESPONSE guest_port={} host_port={}",
                                conn_id.guest_port,
                                conn_id.host_port,
                            );
                            hdr.to_bytes().to_vec()
                        }
                        RxOps::RW => {
                            if !conn.connect {
                                // Not connected yet — send RST instead.
                                let hdr = VsockHeader::new(
                                    VsockAddr::host(conn_id.host_port),
                                    VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                    VsockOp::Rst,
                                );
                                mgr.remove(&conn_id);
                                hdr.to_bytes().to_vec()
                            } else {
                                // Check credit before reading.
                                let credit = conn.peer_avail_credit();
                                if credit == 0 {
                                    // No guest buffer space — request credit update.
                                    let mut hdr = VsockHeader::new(
                                        VsockAddr::host(conn_id.host_port),
                                        VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                        VsockOp::CreditRequest,
                                    );
                                    hdr.buf_alloc = crate::vsock_manager::TX_BUFFER_SIZE;
                                    hdr.fwd_cnt = conn.fwd_cnt.0;
                                    // Re-enqueue RW so we retry after credit update.
                                    conn.rx_queue.enqueue(RxOps::RW);
                                    hdr.to_bytes().to_vec()
                                } else {
                                    // Read from host fd.
                                    let fd = conn.internal_fd.as_raw_fd();
                                    let max_read = credit.min(4096);
                                    let mut buf = vec![0u8; max_read];
                                    let n = unsafe {
                                        libc::read(
                                            fd,
                                            buf.as_mut_ptr().cast::<libc::c_void>(),
                                            max_read,
                                        )
                                    };
                                    if n <= 0 {
                                        if n == 0 {
                                            // Stream closed — send SHUTDOWN.
                                            let mut hdr = VsockHeader::new(
                                                VsockAddr::host(conn_id.host_port),
                                                VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                                VsockOp::Shutdown,
                                            );
                                            hdr.flags = 3; // VIRTIO_VSOCK_SHUTDOWN_RCV | SEND
                                            hdr.buf_alloc = crate::vsock_manager::TX_BUFFER_SIZE;
                                            hdr.fwd_cnt = conn.fwd_cnt.0;
                                            hdr.to_bytes().to_vec()
                                        } else {
                                            continue; // EAGAIN — no data right now.
                                        }
                                    } else {
                                        let data = &buf[..n as usize];
                                        let mut hdr = VsockHeader::new(
                                            VsockAddr::host(conn_id.host_port),
                                            VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                            VsockOp::Rw,
                                        );
                                        hdr.len = data.len() as u32;
                                        hdr.buf_alloc = crate::vsock_manager::TX_BUFFER_SIZE;
                                        hdr.fwd_cnt = conn.fwd_cnt.0;

                                        conn.record_rx(data.len() as u32);

                                        let hdr_bytes = hdr.to_bytes();
                                        let mut pkt =
                                            Vec::with_capacity(VsockHeader::SIZE + data.len());
                                        pkt.extend_from_slice(&hdr_bytes[..VsockHeader::SIZE]);
                                        pkt.extend_from_slice(data);

                                        tracing::debug!(
                                            "Vsock RX: OP_RW {} bytes guest_port={} host_port={} fwd_cnt={}",
                                            data.len(),
                                            conn_id.guest_port,
                                            conn_id.host_port,
                                            conn.fwd_cnt.0,
                                        );
                                        pkt
                                    }
                                }
                            }
                        }
                        RxOps::CREDIT_UPDATE => {
                            let mut hdr = VsockHeader::new(
                                VsockAddr::host(conn_id.host_port),
                                VsockAddr::new(conn.guest_cid, conn_id.guest_port),
                                VsockOp::CreditUpdate,
                            );
                            hdr.buf_alloc = crate::vsock_manager::TX_BUFFER_SIZE;
                            hdr.fwd_cnt = conn.fwd_cnt.0;
                            conn.mark_credit_sent();
                            hdr.to_bytes().to_vec()
                        }
                        _ => continue,
                    }
                }
            };

            // Write the packet to an available RX descriptor.
            let written = self.write_to_rx_descriptor(
                guest_mem, desc_addr, avail_addr, used_addr, q_size, &packet,
            );

            if written > 0 {
                injected = true;

                // Fire injected_notify for REQUEST ops — unblocks the daemon
                // thread waiting in connect_vsock_hv.
                if let Ok(mut mgr) = self.vsock_connections.lock() {
                    if let Some(conn) = mgr.get_mut(&conn_id) {
                        if let Some(tx) = conn.injected_notify.take() {
                            let _ = tx.send(());
                        }
                    }
                }
            }

            // If connection still has pending ops, re-push to backend_rxq.
            if let Ok(mut mgr) = self.vsock_connections.lock() {
                if let Some(conn) = mgr.get(&conn_id) {
                    if conn.rx_queue.pending() {
                        mgr.backend_rxq.push_back(conn_id);
                    }
                }
            }
        }

        // If backend_rxq still has entries but we couldn't inject because
        // the guest's RX vring is full, signal an interrupt. This wakes the
        // guest's virtio_vsock rx_work → rx_fill, replenishing descriptors.
        // On the next poll cycle we'll retry the stalled backend_rxq entries.
        if rxq_starved {
            injected = true;
        }

        // ------------------------------------------------------------------
        // Phase 3: TX poll — drain vsock TX queue for guest→host responses
        // ------------------------------------------------------------------
        if let Some(qcfg) = tx_qcfg {
            if let Some(dev) = self
                .devices
                .values()
                .find(|d| d.info.device_type == DeviceType::VirtioVsock)
                .and_then(|d| d.virtio_device.as_ref())
            {
                if let Ok(mut vdev) = dev.lock() {
                    match vdev.process_queue(1, guest_mem, &qcfg) {
                        Ok(completions) if !completions.is_empty() => {
                            tracing::trace!("Vsock TX poll: {} completions", completions.len());
                            injected = true;

                            // After TX processing, check if any connections now
                            // have pending RX (e.g., CreditUpdate after OP_RW).
                            if let Ok(mut mgr) = self.vsock_connections.lock() {
                                let ids: Vec<_> = mgr.connections_with_pending_rx();
                                for id in ids {
                                    mgr.backend_rxq.push_back(id);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Vsock TX poll error: {e}");
                        }
                        _ => {}
                    }
                }
            }
        }

        injected
    }

    /// Polls the network host fd for incoming frames and injects them into
    /// the VirtioNet RX queue (queue 0). Returns true if any frame was injected.
    ///
    /// Each injected frame is prefixed with a 12-byte virtio-net header (all zeros
    /// for simple passthrough — no checksum offload or GSO on the RX path yet).
    pub fn poll_net_rx(&self) -> bool {
        let Some(net_fd) = self.net_host_fd else {
            return false;
        };
        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return false;
        };
        let gpa_base = self.guest_ram_gpa as usize;

        // Find the primary VirtioNet device MMIO state (by stored ID, not
        // HashMap scan, to avoid hitting the bridge NIC non-deterministically).
        let primary_id = match self.primary_net_device_id {
            Some(id) => id,
            None => return false,
        };
        let mmio_arc = self
            .devices
            .get(&primary_id)
            .and_then(|d| d.mmio_state.as_ref());
        let Some(mmio_arc) = mmio_arc else {
            return false;
        };
        let Ok(mmio) = mmio_arc.read() else {
            return false;
        };

        // RX = queue 0 for VirtioNet.
        let qi = 0usize;
        if qi >= 8 || !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
            return false;
        }
        let desc_addr = mmio.queue_desc[qi] as usize;
        let avail_addr = mmio.queue_driver[qi] as usize;
        let used_addr = mmio.queue_device[qi] as usize;
        let q_size = mmio.queue_num[qi] as usize;

        drop(mmio); // Release lock before accessing guest memory.

        let guest_mem =
            unsafe { std::slice::from_raw_parts_mut(ram_base.sub(gpa_base), gpa_base + ram_size) };

        if avail_addr + 4 > guest_mem.len() {
            return false;
        }
        let avail_idx =
            u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
        let used_idx_off = used_addr + 2;
        let mut used_idx =
            u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;

        let mut injected = false;

        // Read up to 64 frames per poll to avoid starving other devices.
        for _ in 0..64 {
            if used_idx >= avail_idx {
                break; // No more RX descriptors available.
            }

            // Non-blocking read from the network host fd.
            #[allow(clippy::large_stack_arrays)]
            let mut frame_buf = [0u8; 65536]; // Max jumbo + virtio-net header
            let n = unsafe {
                libc::read(
                    net_fd,
                    frame_buf.as_mut_ptr().cast::<libc::c_void>(),
                    frame_buf.len(),
                )
            };
            if n <= 0 {
                break; // No data or error (EAGAIN for non-blocking).
            }
            let frame = &frame_buf[..n as usize];

            // Build packet: 12-byte virtio-net header (zeroed) + ethernet frame.
            let virtio_net_hdr = [0u8; 12]; // GSO_NONE, no csum offload
            let total_len = virtio_net_hdr.len() + frame.len();

            // Pop an available RX descriptor.
            let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
            if ring_off + 2 > guest_mem.len() {
                break;
            }
            let head_idx =
                u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

            // Walk descriptor chain and write the packet.
            let mut written = 0;
            let mut idx = head_idx;
            let packet_data: Vec<u8> = [&virtio_net_hdr[..], frame].concat();
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > guest_mem.len() {
                    break;
                }
                let addr =
                    u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
                let len = u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap())
                    as usize;
                let flags =
                    u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
                let next =
                    u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());

                // RX descriptors are device-writable.
                if flags & 2 != 0 && addr + len <= guest_mem.len() {
                    let remaining = total_len.saturating_sub(written);
                    let to_write = remaining.min(len);
                    guest_mem[addr..addr + to_write]
                        .copy_from_slice(&packet_data[written..written + to_write]);
                    written += to_write;
                }

                if flags & 1 == 0 || written >= total_len {
                    break;
                }
                idx = next as usize;
            }

            if written == 0 {
                break;
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
        }

        // Write avail_event in the used ring (VIRTIO_F_EVENT_IDX) to
        // request kicks from the guest for new RX buffer submissions.
        if injected {
            let avail_idx_now =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]);
            let avail_event_off = used_addr + 4 + q_size * 8;
            if avail_event_off + 2 <= guest_mem.len() {
                guest_mem[avail_event_off..avail_event_off + 2]
                    .copy_from_slice(&avail_idx_now.to_le_bytes());
            }
        }

        injected
    }

    /// Extracts ethernet frames from VirtioNet TX queue descriptors and writes
    /// them to the network host fd. Returns (head_idx, total_len) completions.
    fn handle_net_tx(&self, memory: &[u8], qcfg: &QueueConfig) -> Vec<(u16, u32)> {
        let Some(net_fd) = self.net_host_fd else {
            return Vec::new();
        };
        if !qcfg.ready || qcfg.size == 0 {
            return Vec::new();
        }

        let desc_addr = qcfg.desc_addr as usize;
        let avail_addr = qcfg.avail_addr as usize;
        let q_size = qcfg.size as usize;

        if avail_addr + 4 > memory.len() {
            return Vec::new();
        }
        let avail_idx =
            u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]) as usize;

        let mut current_avail = self
            .net_last_avail_tx
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut completions = Vec::new();

        while current_avail != avail_idx {
            let ring_off = avail_addr + 4 + 2 * (current_avail % q_size);
            if ring_off + 2 > memory.len() {
                break;
            }
            let head_idx = u16::from_le_bytes([memory[ring_off], memory[ring_off + 1]]) as usize;

            // Walk descriptor chain to extract the full packet (virtio-net header + frame).
            let mut packet_data = Vec::new();
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > memory.len() {
                    break;
                }
                let addr =
                    u64::from_le_bytes(memory[d_off..d_off + 8].try_into().unwrap()) as usize;
                let len =
                    u32::from_le_bytes(memory[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(memory[d_off + 12..d_off + 14].try_into().unwrap());
                let next = u16::from_le_bytes(memory[d_off + 14..d_off + 16].try_into().unwrap());

                // TX descriptors are read-only (guest→host).
                if flags & 2 == 0 && addr + len <= memory.len() {
                    packet_data.extend_from_slice(&memory[addr..addr + len]);
                }

                if flags & 1 == 0 {
                    break;
                }
                idx = next as usize;
            }

            let total_len = packet_data.len() as u32;
            finalize_virtio_net_checksum(&mut packet_data);

            // Strip the virtio-net header after applying checksum offload.
            if packet_data.len() > VirtioNetHeader::SIZE {
                let frame = &packet_data[VirtioNetHeader::SIZE..];
                // Write raw ethernet frame to the network datapath fd.
                let n = unsafe {
                    libc::write(net_fd, frame.as_ptr().cast::<libc::c_void>(), frame.len())
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EAGAIN) {
                        tracing::warn!("net TX write failed: {err}");
                    }
                }
            }

            completions.push((head_idx as u16, total_len));
            current_avail += 1;
        }

        self.net_last_avail_tx
            .store(current_avail, std::sync::atomic::Ordering::Relaxed);
        completions
    }

    /// Writes a raw ethernet frame to the network host fd (for VirtioNet TX).
    /// Called from the QUEUE_NOTIFY handler when the guest sends a packet.
    pub fn write_net_tx_frame(&self, frame: &[u8]) {
        if let Some(fd) = self.net_host_fd {
            let n = unsafe { libc::write(fd, frame.as_ptr().cast::<libc::c_void>(), frame.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EAGAIN) {
                    tracing::warn!("net TX write failed: {err}");
                }
            }
        }
    }

    // ========================================================================
    // Bridge NIC (NIC2) — vmnet bridge for container IP routing
    // ========================================================================

    /// Extracts ethernet frames from bridge VirtioNet TX queue and writes
    /// them to the bridge (vmnet) host fd. Mirrors `handle_net_tx`.
    fn handle_bridge_tx(&self, memory: &[u8], qcfg: &QueueConfig) -> Vec<(u16, u32)> {
        let Some(bridge_fd) = self.bridge_host_fd else {
            return Vec::new();
        };
        if !qcfg.ready || qcfg.size == 0 {
            return Vec::new();
        }

        let desc_addr = qcfg.desc_addr as usize;
        let avail_addr = qcfg.avail_addr as usize;
        let q_size = qcfg.size as usize;

        if avail_addr + 4 > memory.len() {
            return Vec::new();
        }
        let avail_idx =
            u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]) as usize;

        let mut current_avail = self
            .bridge_last_avail_tx
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut completions = Vec::new();

        while current_avail != avail_idx {
            let ring_off = avail_addr + 4 + 2 * (current_avail % q_size);
            if ring_off + 2 > memory.len() {
                break;
            }
            let head_idx = u16::from_le_bytes([memory[ring_off], memory[ring_off + 1]]) as usize;

            let mut packet_data = Vec::new();
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > memory.len() {
                    break;
                }
                let addr =
                    u64::from_le_bytes(memory[d_off..d_off + 8].try_into().unwrap()) as usize;
                let len =
                    u32::from_le_bytes(memory[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(memory[d_off + 12..d_off + 14].try_into().unwrap());
                let next = u16::from_le_bytes(memory[d_off + 14..d_off + 16].try_into().unwrap());

                if flags & 2 == 0 && addr + len <= memory.len() {
                    packet_data.extend_from_slice(&memory[addr..addr + len]);
                }
                if flags & 1 == 0 {
                    break;
                }
                idx = next as usize;
            }

            let total_len = packet_data.len() as u32;
            finalize_virtio_net_checksum(&mut packet_data);

            // Strip the virtio-net header after applying checksum offload.
            if packet_data.len() > VirtioNetHeader::SIZE {
                let frame = &packet_data[VirtioNetHeader::SIZE..];
                let n = unsafe {
                    libc::write(
                        bridge_fd,
                        frame.as_ptr().cast::<libc::c_void>(),
                        frame.len(),
                    )
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EAGAIN) {
                        tracing::warn!("bridge TX write failed: {err}");
                    }
                }
            }

            completions.push((head_idx as u16, total_len));
            current_avail += 1;
        }

        self.bridge_last_avail_tx
            .store(current_avail, std::sync::atomic::Ordering::Relaxed);
        completions
    }

    /// Polls the bridge (vmnet) host fd for incoming frames and injects them
    /// into the bridge VirtioNet RX queue. Mirrors `poll_net_rx`.
    pub fn poll_bridge_rx(&self) -> bool {
        let Some(bridge_fd) = self.bridge_host_fd else {
            return false;
        };
        let (Some(ram_base), ram_size) = (self.guest_ram_base, self.guest_ram_size) else {
            return false;
        };
        let gpa_base = self.guest_ram_gpa as usize;
        let guest_mem =
            unsafe { std::slice::from_raw_parts_mut(ram_base.sub(gpa_base), gpa_base + ram_size) };

        // Find the bridge VirtioNet device's MMIO state.
        let bridge_device_id = match self.bridge_net_device_id {
            Some(id) => id,
            None => return false,
        };
        let device = match self.devices.get(&bridge_device_id) {
            Some(d) => d,
            None => return false,
        };
        let mmio_arc = match device.mmio_state.as_ref() {
            Some(m) => m,
            None => return false,
        };
        let Ok(mmio) = mmio_arc.read() else {
            return false;
        };

        let qi = 0usize; // RX queue
        if qi >= 8 || !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
            return false;
        }

        let desc_addr = mmio.queue_desc[qi] as usize;
        let avail_addr = mmio.queue_driver[qi] as usize;
        let used_addr = mmio.queue_device[qi] as usize;
        let q_size = mmio.queue_num[qi] as usize;
        drop(mmio);

        if avail_addr + 4 > guest_mem.len() {
            return false;
        }

        let mut injected = false;

        // Read up to 64 frames per poll (same as NIC1).
        for _ in 0..64 {
            let avail_idx =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]) as usize;
            let used_idx_off = used_addr + 2;
            let used_idx =
                u16::from_le_bytes([guest_mem[used_idx_off], guest_mem[used_idx_off + 1]]) as usize;

            if avail_idx == used_idx {
                break; // No RX descriptors available.
            }

            // Non-blocking read from bridge fd.
            let mut buf = [0u8; 9216]; // MAX_FRAME_SIZE
            let n = unsafe {
                libc::recv(
                    bridge_fd,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            let frame = &buf[..n as usize];

            // Prepend 12-byte virtio-net header (all zeros = no offload).
            let virtio_hdr = [0u8; 12];
            let total = virtio_hdr.len() + frame.len();

            // Pop an available RX descriptor and write header + frame.
            let ring_off = avail_addr + 4 + 2 * (used_idx % q_size);
            if ring_off + 2 > guest_mem.len() {
                break;
            }
            let head_idx =
                u16::from_le_bytes([guest_mem[ring_off], guest_mem[ring_off + 1]]) as usize;

            let mut written = 0;
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > guest_mem.len() {
                    break;
                }
                let addr =
                    u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
                let len = u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap())
                    as usize;
                let flags =
                    u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
                let next =
                    u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());

                if flags & 2 != 0 && addr + len <= guest_mem.len() {
                    // Write from [virtio_hdr | frame] combined.
                    let remaining = total.saturating_sub(written);
                    let to_write = remaining.min(len);
                    if to_write > 0 {
                        // Scatter from virtio_hdr then frame.
                        let hdr_remaining = virtio_hdr.len().saturating_sub(written);
                        if hdr_remaining > 0 {
                            let hdr_write = hdr_remaining.min(to_write);
                            guest_mem[addr..addr + hdr_write]
                                .copy_from_slice(&virtio_hdr[written..written + hdr_write]);
                            if to_write > hdr_write {
                                let frame_write = to_write - hdr_write;
                                guest_mem[addr + hdr_write..addr + hdr_write + frame_write]
                                    .copy_from_slice(&frame[..frame_write]);
                            }
                        } else {
                            let frame_off = written - virtio_hdr.len();
                            guest_mem[addr..addr + to_write]
                                .copy_from_slice(&frame[frame_off..frame_off + to_write]);
                        }
                        written += to_write;
                    }
                }

                if flags & 1 == 0 || written >= total {
                    break;
                }
                idx = next as usize;
            }

            if written == 0 {
                continue;
            }

            // Update used ring.
            let used_entry = used_addr + 4 + (used_idx % q_size) * 8;
            if used_entry + 8 <= guest_mem.len() {
                guest_mem[used_entry..used_entry + 4]
                    .copy_from_slice(&(head_idx as u32).to_le_bytes());
                guest_mem[used_entry + 4..used_entry + 8]
                    .copy_from_slice(&(written as u32).to_le_bytes());
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                let new_used = (used_idx + 1) as u16;
                guest_mem[used_idx_off..used_idx_off + 2].copy_from_slice(&new_used.to_le_bytes());
            }

            injected = true;
        }

        // Write avail_event for EVENT_IDX.
        if injected {
            let avail_idx_now =
                u16::from_le_bytes([guest_mem[avail_addr + 2], guest_mem[avail_addr + 3]]);
            let avail_event_off = used_addr + 4 + q_size * 8;
            if avail_event_off + 2 <= guest_mem.len() {
                guest_mem[avail_event_off..avail_event_off + 2]
                    .copy_from_slice(&avail_idx_now.to_le_bytes());
            }
        }

        injected
    }
}

/// Completes guest-requested checksum offload before forwarding the frame.
fn finalize_virtio_net_checksum(packet_data: &mut [u8]) {
    const ETH_HEADER_LEN: usize = 14;
    const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
    const PROTOCOL_TCP: u8 = 6;
    const PROTOCOL_UDP: u8 = 17;

    if packet_data.len() <= VirtioNetHeader::SIZE {
        return;
    }

    let Some(header) = VirtioNetHeader::from_bytes(&packet_data[..VirtioNetHeader::SIZE]) else {
        return;
    };
    if header.flags & VirtioNetHeader::FLAG_NEEDS_CSUM == 0 {
        return;
    }

    let frame = &mut packet_data[VirtioNetHeader::SIZE..];
    if frame.len() < ETH_HEADER_LEN + 20 || frame[12..14] != ETHERTYPE_IPV4 {
        return;
    }

    let ip_start = ETH_HEADER_LEN;
    let version = frame[ip_start] >> 4;
    let ihl = usize::from(frame[ip_start] & 0x0F) * 4;
    if version != 4 || ihl < 20 || frame.len() < ip_start + ihl {
        return;
    }

    let total_len = usize::from(u16::from_be_bytes([
        frame[ip_start + 2],
        frame[ip_start + 3],
    ]));
    if total_len < ihl || frame.len() < ip_start + total_len {
        return;
    }

    let checksum_start = usize::from(header.csum_start);
    let checksum_offset = usize::from(header.csum_offset);
    let payload_end = ip_start + total_len;
    let checksum_end = checksum_start
        .checked_add(checksum_offset)
        .and_then(|offset| offset.checked_add(2));
    if checksum_start < ip_start + ihl || checksum_end.is_none_or(|end| end > payload_end) {
        return;
    }

    let protocol = frame[ip_start + 9];
    let src_ip = [
        frame[ip_start + 12],
        frame[ip_start + 13],
        frame[ip_start + 14],
        frame[ip_start + 15],
    ];
    let dst_ip = [
        frame[ip_start + 16],
        frame[ip_start + 17],
        frame[ip_start + 18],
        frame[ip_start + 19],
    ];

    let Some(checksum_start_offset) = checksum_start.checked_add(checksum_offset) else {
        return;
    };
    frame[checksum_start_offset..checksum_start_offset + 2].fill(0);

    let checksum = match protocol {
        PROTOCOL_TCP => tcp_checksum(src_ip, dst_ip, &frame[checksum_start..payload_end]),
        PROTOCOL_UDP => udp_checksum(src_ip, dst_ip, &frame[checksum_start..payload_end]),
        _ => return,
    };
    frame[checksum_start_offset..checksum_start_offset + 2]
        .copy_from_slice(&checksum.to_be_bytes());
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
    use std::net::Ipv4Addr;

    use arcbox_net::ethernet::{TcpFrameParams, build_tcp_ack_frame, build_udp_ip_ethernet};
    use arcbox_net::nat_engine::checksum::{tcp_checksum, udp_checksum};

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

    #[test]
    fn test_finalize_virtio_net_checksum_repairs_ipv4_tcp_frame() {
        let params = TcpFrameParams {
            src_ip: Ipv4Addr::new(10, 0, 2, 2),
            dst_ip: Ipv4Addr::new(198, 18, 30, 95),
            src_port: 36402,
            dst_port: 443,
            seq: 1234,
            ack: 0,
            window: 64240,
            src_mac: [0x52, 0x54, 0xAB, 0xFA, 0x2A, 0x70],
            dst_mac: [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01],
        };
        let mut frame = build_tcp_ack_frame(&params);
        let tcp_start = 14 + 20;
        frame[tcp_start + 13] = 0x02;
        frame[tcp_start + 16..tcp_start + 18].fill(0);

        let header = VirtioNetHeader {
            flags: VirtioNetHeader::FLAG_NEEDS_CSUM,
            gso_type: VirtioNetHeader::GSO_NONE,
            hdr_len: 0,
            gso_size: 0,
            csum_start: tcp_start as u16,
            csum_offset: 16,
            num_buffers: 1,
        };
        let mut packet_data = header.to_bytes().to_vec();
        packet_data.extend_from_slice(&frame);

        finalize_virtio_net_checksum(&mut packet_data);

        let frame = &packet_data[VirtioNetHeader::SIZE..];
        let stored = u16::from_be_bytes([frame[tcp_start + 16], frame[tcp_start + 17]]);
        let mut tcp_segment = frame[tcp_start..].to_vec();
        tcp_segment[16..18].fill(0);

        assert_ne!(stored, 0);
        assert_eq!(
            stored,
            tcp_checksum(params.src_ip.octets(), params.dst_ip.octets(), &tcp_segment)
        );
    }

    #[test]
    fn test_finalize_virtio_net_checksum_repairs_ipv4_udp_frame() {
        let src_ip = Ipv4Addr::new(10, 0, 2, 2);
        let dst_ip = Ipv4Addr::new(10, 0, 2, 1);
        let payload = b"hello dns";
        let src_mac = [0x52, 0x54, 0xAB, 0xFA, 0x2A, 0x70];
        let dst_mac = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];
        let mut frame = build_udp_ip_ethernet(src_ip, dst_ip, 49152, 53, payload, src_mac, dst_mac);
        let udp_start = 14 + 20;
        frame[udp_start + 6..udp_start + 8].fill(0);

        let header = VirtioNetHeader {
            flags: VirtioNetHeader::FLAG_NEEDS_CSUM,
            gso_type: VirtioNetHeader::GSO_NONE,
            hdr_len: 0,
            gso_size: 0,
            csum_start: udp_start as u16,
            csum_offset: 6,
            num_buffers: 1,
        };
        let mut packet_data = header.to_bytes().to_vec();
        packet_data.extend_from_slice(&frame);

        finalize_virtio_net_checksum(&mut packet_data);

        let frame = &packet_data[VirtioNetHeader::SIZE..];
        let stored = u16::from_be_bytes([frame[udp_start + 6], frame[udp_start + 7]]);
        let mut udp_datagram = frame[udp_start..].to_vec();
        udp_datagram[6..8].fill(0);

        assert_ne!(stored, 0);
        assert_eq!(
            stored,
            udp_checksum(src_ip.octets(), dst_ip.octets(), &udp_datagram)
        );
    }
}
