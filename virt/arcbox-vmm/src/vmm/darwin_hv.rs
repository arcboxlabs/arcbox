//! macOS custom VMM using Hypervisor.framework (manual execution).
//!
//! This is the **alternative** to the VZ framework managed-execution path in
//! `darwin.rs`. It uses `arcbox-hv` directly, giving us full control over
//! VirtIO device emulation — critically, the ability to negotiate TSO with
//! the guest and handle VirtIO-net headers in userspace.
//!
//! The design mirrors `linux.rs` (KVM manual execution):
//! - Guest RAM is allocated on the host and mapped into guest IPA.
//! - VirtIO devices are registered with `DeviceManager` and exposed via
//!   MMIO transport. The guest discovers them through the FDT.
//! - vCPU threads call `HvVcpu::run()` in a loop, dispatching MMIO traps
//!   to the device manager.
//! - GICv3 is provided by Hypervisor.framework's hardware emulation
//!   (macOS 15+); device interrupts are injected via `Gic::set_spi()`.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::Arc;

use arcbox_hv::{HvVm, MemoryPermission};

use crate::boot::arm64;
use crate::device::DeviceTreeEntry;
use crate::error::{Result, VmmError};
use crate::fdt::{FdtConfig, generate_fdt};
use crate::irq::IrqChip;
#[cfg(feature = "gic")]
use crate::irq::{Gsi, IrqTriggerCallback};

use super::*;

/// Page size on ARM64.
const PAGE_SIZE: usize = 4096;

/// Base address for VirtIO MMIO device region.
const VIRTIO_MMIO_BASE: u64 = 0x0900_0000;

/// Size of each VirtIO MMIO device region.
const VIRTIO_MMIO_SIZE: u64 = 0x200;

/// Maximum number of VirtIO MMIO devices.
const VIRTIO_MMIO_MAX_DEVICES: u64 = 32;

/// First SPI interrupt number for VirtIO devices (GIC SPI numbering).
const VIRTIO_IRQ_BASE: u32 = 48;

/// Guest RAM is mapped starting at IPA 0.
const RAM_BASE_IPA: u64 = 0;

/// Holds a page-aligned host allocation that backs guest RAM.
struct GuestRam {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: The allocation is owned exclusively and accessed via synchronized
// vCPU threads + device manager behind Arc<Mutex>. No aliasing.
unsafe impl Send for GuestRam {}
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Allocates page-aligned zeroed memory for guest RAM.
    fn new(size: usize) -> Result<Self> {
        let layout = Layout::from_size_align(size, PAGE_SIZE)
            .map_err(|e| VmmError::Memory(format!("invalid RAM layout: {e}")))?;

        // SAFETY: Layout is valid and non-zero.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(VmmError::Memory(format!(
                "failed to allocate {} bytes for guest RAM",
                size
            )));
        }

        Ok(Self { ptr, layout })
    }

    /// Returns the host pointer to guest RAM.
    fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Returns guest RAM as a mutable slice.
    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr was allocated with this layout by alloc_zeroed and
        // &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    /// Returns the size in bytes.
    #[allow(dead_code)] // Used once vCPU run loop is wired (Phase 2b).
    fn size(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated with this layout by alloc_zeroed.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// Device slot tracking for MMIO address and IRQ assignment.
struct DeviceSlot {
    /// MMIO base address in guest IPA.
    mmio_base: u64,
    /// MMIO region size.
    mmio_size: u64,
    /// Assigned SPI interrupt number.
    irq: u32,
    /// Device name for diagnostics.
    name: String,
}

/// Collect information needed to generate FDT entries for VirtIO devices.
fn build_device_tree_entries(slots: &[DeviceSlot]) -> Vec<DeviceTreeEntry> {
    slots
        .iter()
        .map(|s| DeviceTreeEntry {
            compatible: "virtio,mmio".to_string(),
            reg_base: s.mmio_base,
            reg_size: s.mmio_size,
            irq: s.irq,
        })
        .collect()
}

/// Allocate the next MMIO device slot.
fn allocate_device_slot(index: u64, name: impl Into<String>) -> Result<DeviceSlot> {
    if index >= VIRTIO_MMIO_MAX_DEVICES {
        return Err(VmmError::Device("too many VirtIO MMIO devices".to_string()));
    }
    Ok(DeviceSlot {
        mmio_base: VIRTIO_MMIO_BASE + index * VIRTIO_MMIO_SIZE,
        mmio_size: VIRTIO_MMIO_SIZE,
        irq: VIRTIO_IRQ_BASE + index as u32,
        name: name.into(),
    })
}

impl Vmm {
    /// Custom VMM initialization using Hypervisor.framework (manual execution).
    ///
    /// This path is an alternative to `initialize_darwin()` (VZ framework).
    /// It creates a VM via `arcbox-hv`, allocates guest RAM, sets up GIC,
    /// registers VirtIO devices, generates an FDT, and prepares vCPU state
    /// for boot.
    #[allow(dead_code)] // Will be wired in Phase 4
    pub(super) fn initialize_darwin_hv(&mut self) -> Result<()> {
        tracing::info!("Initializing custom VMM via Hypervisor.framework");

        let ram_size = self.config.memory_size as usize;

        // --- 1. Allocate guest RAM on the host ---
        let mut guest_ram = GuestRam::new(ram_size)?;
        tracing::debug!("Allocated {} MB guest RAM", ram_size / (1024 * 1024));

        // --- 2. Create Hypervisor.framework VM ---
        // Use 40-bit IPA for up to ~1 TB guest physical address space,
        // which accommodates RAM + MMIO + GIC regions.
        let vm = HvVm::with_ipa_size(40)
            .map_err(|e| VmmError::Device(format!("hv_vm_create failed: {e}")))?;

        // --- 3. Map guest RAM into IPA ---
        // SAFETY: guest_ram.as_ptr() points to a valid page-aligned allocation
        // of exactly ram_size bytes. The mapping lives as long as the VM.
        unsafe {
            vm.map_memory(
                guest_ram.as_ptr(),
                RAM_BASE_IPA,
                ram_size,
                MemoryPermission::READ_WRITE | MemoryPermission::EXEC,
            )
            .map_err(|e| VmmError::Memory(format!("failed to map guest RAM: {e}")))?;
        }
        tracing::debug!(
            "Mapped guest RAM: IPA {:#x}..{:#x}",
            RAM_BASE_IPA,
            ram_size as u64
        );

        // --- 4. Initialize GIC (macOS 15+) ---
        #[cfg(feature = "gic")]
        let gic = {
            let g = arcbox_hv::Gic::new()
                .map_err(|e| VmmError::Device(format!("GIC initialization failed: {e}")))?;
            let dist_base = g
                .get_distributor_base()
                .map_err(|e| VmmError::Device(format!("GIC distributor base: {e}")))?;
            tracing::info!("GICv3 initialized: distributor @ {:#x}", dist_base);
            Some(Arc::new(g))
        };
        #[cfg(not(feature = "gic"))]
        tracing::warn!("GIC feature not enabled — interrupts will not work with custom VMM");

        // --- 5. Set up IRQ chip with GIC callback ---
        let irq_chip = Arc::new(IrqChip::new()?);

        #[cfg(feature = "gic")]
        if let Some(ref gic_ref) = gic {
            let gic_weak = Arc::downgrade(gic_ref);
            let callback: IrqTriggerCallback = Box::new(move |gsi: Gsi, level: bool| {
                if let Some(g) = gic_weak.upgrade() {
                    g.set_spi(gsi, level).map_err(|e| {
                        VmmError::Irq(format!("GIC set_spi({gsi}, {level}) failed: {e}"))
                    })?;
                    tracing::trace!("GIC: SPI {gsi} level={level}");
                } else {
                    tracing::warn!("GIC: dropped, cannot inject SPI {gsi}");
                }
                Ok(())
            });
            irq_chip.set_trigger_callback(Arc::new(callback));
            tracing::debug!("IRQ callback wired to hardware GIC");
        }

        // --- 6. Allocate VirtIO device slots ---
        let mut device_slots: Vec<DeviceSlot> = Vec::new();
        let mut slot_index: u64 = 0;

        // Console (serial)
        if self.config.serial_console || self.config.virtio_console {
            device_slots.push(allocate_device_slot(slot_index, "virtio-console")?);
            slot_index += 1;
        }

        // VirtioFS shared directories
        for dir in &self.config.shared_dirs {
            device_slots.push(allocate_device_slot(
                slot_index,
                format!("virtiofs-{}", dir.tag),
            )?);
            slot_index += 1;
        }

        // Block devices
        for (i, _) in self.config.block_devices.iter().enumerate() {
            device_slots.push(allocate_device_slot(slot_index, format!("virtio-blk-{i}"))?);
            slot_index += 1;
        }

        // Network (TSO-enabled)
        if self.config.networking {
            device_slots.push(allocate_device_slot(slot_index, "virtio-net")?);
            slot_index += 1;
        }

        // Vsock
        if self.config.vsock {
            device_slots.push(allocate_device_slot(slot_index, "virtio-vsock")?);
            // slot_index += 1; // Last device, no increment needed.
        }

        for slot in &device_slots {
            tracing::info!(
                "VirtIO slot: {} @ {:#x} IRQ {}",
                slot.name,
                slot.mmio_base,
                slot.irq
            );
        }

        // --- 7. Generate FDT ---
        let fdt_entries = build_device_tree_entries(&device_slots);
        let fdt_config = build_hv_fdt_config(&self.config, &fdt_entries)?;
        let fdt_blob = generate_fdt(&fdt_config)?;

        if fdt_blob.len() > arm64::FDT_MAX_SIZE {
            return Err(VmmError::Memory("generated FDT exceeds 2 MB limit".into()));
        }

        let fdt_addr = choose_fdt_addr_hv(self.config.memory_size, fdt_blob.len())?;

        // Write FDT into guest RAM (VM not running yet, exclusive access).
        let ram = guest_ram.as_mut_slice();
        let fdt_start = fdt_addr as usize;
        let fdt_end = fdt_start + fdt_blob.len();
        if fdt_end > ram.len() {
            return Err(VmmError::Memory("FDT does not fit in guest RAM".into()));
        }
        ram[fdt_start..fdt_end].copy_from_slice(&fdt_blob);

        tracing::info!(
            "FDT written: addr={:#x}, size={} bytes, devices={}",
            fdt_addr,
            fdt_blob.len(),
            fdt_entries.len()
        );

        // --- 8. Load kernel + initrd into guest RAM ---
        load_kernel_into_ram(ram, &self.config.kernel_path)?;

        if let Some(ref initrd_path) = self.config.initrd_path {
            load_initrd_into_ram(ram, initrd_path)?;
        }

        // --- 9. Initialize managers ---
        let mut memory_manager = MemoryManager::new();
        memory_manager.initialize(self.config.memory_size)?;

        let device_manager = DeviceManager::new();
        let event_loop = crate::event::EventLoop::new()?;

        // Store managers (VM and GuestRam ownership will be moved to vCPU
        // threads in the start path — for now, store enough state to start).
        self.memory_manager = Some(memory_manager);
        self.device_manager = Some(device_manager);
        self.irq_chip = Some(irq_chip);
        self.event_loop = Some(event_loop);

        // TODO(Phase 2b): Store HvVm, GuestRam, GIC, and device slots for
        // the vCPU run loop. This requires adding fields to Vmm (Phase 4).
        // For now, leak the VM to keep it alive — will be fixed when
        // Vmm struct gets the darwin_hv fields.
        std::mem::forget(vm);
        std::mem::forget(guest_ram);

        tracing::info!("Custom Hypervisor.framework VMM initialized");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FDT helpers for the custom VMM path
// ---------------------------------------------------------------------------

fn build_hv_fdt_config(
    config: &VmmConfig,
    virtio_devices: &[DeviceTreeEntry],
) -> Result<FdtConfig> {
    let mut fdt_config = FdtConfig {
        num_cpus: config.vcpu_count,
        memory_size: config.memory_size,
        memory_base: RAM_BASE_IPA,
        cmdline: config.kernel_cmdline.clone(),
        virtio_devices: virtio_devices.to_vec(),
        ..Default::default()
    };

    if let Some(initrd) = &config.initrd_path {
        let size = std::fs::metadata(initrd)
            .map_err(|e| VmmError::config(format!("cannot stat initrd: {e}")))?
            .len();
        fdt_config.initrd_addr = Some(arm64::INITRD_LOAD_ADDR);
        fdt_config.initrd_size = Some(size);
    }

    Ok(fdt_config)
}

fn choose_fdt_addr_hv(memory_size: u64, fdt_size: usize) -> Result<u64> {
    let fdt_size = fdt_size as u64;
    let gib: u64 = 1024 * 1024 * 1024;
    let preferred = if memory_size >= gib {
        arm64::FDT_LOAD_ADDR
    } else {
        0x0800_0000
    };

    if fdt_size > memory_size {
        return Err(VmmError::Memory("FDT exceeds guest memory".into()));
    }
    if preferred + fdt_size > memory_size {
        return Err(VmmError::Memory("FDT does not fit at load address".into()));
    }

    Ok(preferred)
}

// ---------------------------------------------------------------------------
// Kernel / initrd loading
// ---------------------------------------------------------------------------

/// Loads the kernel Image into guest RAM at the ARM64 load address.
fn load_kernel_into_ram(ram: &mut [u8], kernel_path: &std::path::Path) -> Result<()> {
    use std::io::Read;

    let mut file = std::fs::File::open(kernel_path)
        .map_err(|e| VmmError::config(format!("cannot open kernel: {e}")))?;

    let metadata = file
        .metadata()
        .map_err(|e| VmmError::config(format!("cannot stat kernel: {e}")))?;
    let kernel_size = metadata.len() as usize;

    let load_addr = arm64::KERNEL_LOAD_ADDR as usize;
    let end = load_addr + kernel_size;
    if end > ram.len() {
        return Err(VmmError::Memory(format!(
            "kernel ({} bytes) does not fit at {:#x} in {} bytes RAM",
            kernel_size,
            load_addr,
            ram.len()
        )));
    }

    file.read_exact(&mut ram[load_addr..end])
        .map_err(|e| VmmError::config(format!("failed to read kernel: {e}")))?;

    tracing::info!(
        "Kernel loaded: addr={:#x}, size={} bytes",
        load_addr,
        kernel_size
    );

    Ok(())
}

/// Loads the initrd into guest RAM at the ARM64 load address.
fn load_initrd_into_ram(ram: &mut [u8], initrd_path: &std::path::Path) -> Result<()> {
    use std::io::Read;

    let mut file = std::fs::File::open(initrd_path)
        .map_err(|e| VmmError::config(format!("cannot open initrd: {e}")))?;

    let metadata = file
        .metadata()
        .map_err(|e| VmmError::config(format!("cannot stat initrd: {e}")))?;
    let initrd_size = metadata.len() as usize;

    let load_addr = arm64::INITRD_LOAD_ADDR as usize;
    let end = load_addr + initrd_size;
    if end > ram.len() {
        return Err(VmmError::Memory(format!(
            "initrd ({} bytes) does not fit at {:#x} in {} bytes RAM",
            initrd_size,
            load_addr,
            ram.len()
        )));
    }

    file.read_exact(&mut ram[load_addr..end])
        .map_err(|e| VmmError::config(format!("failed to read initrd: {e}")))?;

    tracing::info!(
        "Initrd loaded: addr={:#x}, size={} bytes",
        load_addr,
        initrd_size
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_device_slot() {
        let slot = allocate_device_slot(0, "test").unwrap();
        assert_eq!(slot.mmio_base, VIRTIO_MMIO_BASE);
        assert_eq!(slot.mmio_size, VIRTIO_MMIO_SIZE);
        assert_eq!(slot.irq, VIRTIO_IRQ_BASE);
        assert_eq!(slot.name, "test");
    }

    #[test]
    fn test_allocate_device_slot_second() {
        let slot = allocate_device_slot(1, "net").unwrap();
        assert_eq!(slot.mmio_base, VIRTIO_MMIO_BASE + VIRTIO_MMIO_SIZE);
        assert_eq!(slot.irq, VIRTIO_IRQ_BASE + 1);
    }

    #[test]
    fn test_allocate_device_slot_overflow() {
        let result = allocate_device_slot(VIRTIO_MMIO_MAX_DEVICES, "overflow");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_device_tree_entries() {
        let slots = vec![
            DeviceSlot {
                mmio_base: 0x0900_0000,
                mmio_size: 0x200,
                irq: 48,
                name: "net".into(),
            },
            DeviceSlot {
                mmio_base: 0x0900_0200,
                mmio_size: 0x200,
                irq: 49,
                name: "blk".into(),
            },
        ];
        let entries = build_device_tree_entries(&slots);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].reg_base, 0x0900_0000);
        assert_eq!(entries[0].irq, 48);
        assert_eq!(entries[1].reg_base, 0x0900_0200);
        assert_eq!(entries[1].irq, 49);
    }

    #[test]
    fn test_choose_fdt_addr_large_ram() {
        let addr = choose_fdt_addr_hv(2 * 1024 * 1024 * 1024, 0x1000).unwrap();
        assert_eq!(addr, arm64::FDT_LOAD_ADDR);
    }

    #[test]
    fn test_choose_fdt_addr_small_ram() {
        let addr = choose_fdt_addr_hv(512 * 1024 * 1024, 0x1000).unwrap();
        assert_eq!(addr, 0x0800_0000);
    }

    #[test]
    fn test_choose_fdt_addr_too_big() {
        let result = choose_fdt_addr_hv(1024, 2048);
        assert!(result.is_err());
    }

    #[test]
    fn test_guest_ram_allocation() {
        let ram = GuestRam::new(4096).unwrap();
        assert!(!ram.as_ptr().is_null());
        assert_eq!(ram.size(), 4096);
    }

    #[test]
    fn test_guest_ram_write_read() {
        let mut ram = GuestRam::new(4096).unwrap();
        let slice = ram.as_mut_slice();
        slice[0] = 0xAB;
        slice[4095] = 0xCD;
        assert_eq!(slice[0], 0xAB);
        assert_eq!(slice[4095], 0xCD);
    }
}
