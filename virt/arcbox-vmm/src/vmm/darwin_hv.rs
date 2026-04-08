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

#[cfg(test)]
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};

use arcbox_hv::{HvVm, MemoryPermission};
use linux_loader::loader::{KernelLoader as LinuxKernelLoader, pe::PE};
use vm_fdt::FdtWriter;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemory as VmGuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

use crate::boot::arm64;
#[cfg(test)]
use crate::device::DeviceTreeEntry;
use crate::device::DeviceType;
use crate::error::{Result, VmmError};
#[allow(unused_imports)] // Used by retained VZ-path / test helpers below.
use crate::fdt::{FdtConfig, generate_fdt};
use crate::irq::IrqChip;
#[cfg(feature = "gic")]
use crate::irq::{Gsi, IrqTriggerCallback};

use super::*;

// ---------------------------------------------------------------------------
// PSCI function IDs (SMC Calling Convention)
// ---------------------------------------------------------------------------

/// PSCI CPU_ON (64-bit): power up a secondary CPU.
const PSCI_CPU_ON_64: u64 = 0xC400_0003;

/// PSCI SYSTEM_OFF: shut the system down.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// PSCI SYSTEM_RESET: reset the system.
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// PSCI PSCI_VERSION: return PSCI version.
const PSCI_VERSION: u64 = 0x8400_0000;

/// PSCI return code: success.
const PSCI_SUCCESS: u64 = 0;

/// PSCI return code: the target CPU is already on (-4 in two's complement).
const PSCI_ALREADY_ON: u64 = (-4_i64) as u64;

/// Request to power on a secondary vCPU via PSCI CPU_ON.
/// Fields are written by the BSP and read by the secondary vCPU thread.
pub struct CpuOnRequest {
    /// Target MPIDR (CPU affinity identifier). Logged for diagnostics;
    /// the actual target is determined by channel routing in start_darwin_hv.
    pub _target_cpu: u64,
    /// Guest IPA where the secondary CPU begins executing.
    pub entry_point: u64,
    /// Value passed as X0 to the secondary CPU.
    pub context_id: u64,
}

/// Shared state for secondary vCPU wake-up channels.
///
/// Index `i` corresponds to vCPU `i` (0-based). The BSP (vCPU 0) does not
/// have an entry. Each `Option<Sender>` is `take()`-n exactly once when the
/// guest calls PSCI CPU_ON for that vCPU, preventing double-start.
type CpuOnSenders = Arc<Mutex<Vec<Option<mpsc::Sender<CpuOnRequest>>>>>;

/// Shared registry of vCPU thread handles for WFI unparking.
///
/// When a GIC interrupt is injected, the IRQ callback iterates this list
/// and calls `unpark()` on every thread so that WFI-parked vCPUs wake up.
type VcpuThreadHandles = Arc<Mutex<Vec<std::thread::Thread>>>;

/// Page size on ARM64.
#[cfg(test)]
const PAGE_SIZE: usize = 4096;

/// Base address for VirtIO MMIO device region.
/// Starts at 0x0C00_0000 to avoid the GIC redistributor region
/// (GICR ends at 0x080A_0000 + 32 MB = 0x0A0A_0000) and PL011 UART (0x0B00_0000).
const VIRTIO_MMIO_BASE: u64 = 0x0C00_0000;

/// Size of each VirtIO MMIO device region.
#[cfg(test)]
const VIRTIO_MMIO_SIZE: u64 = 0x200;

/// Maximum number of VirtIO MMIO devices.
const VIRTIO_MMIO_MAX_DEVICES: u64 = 32;

/// First SPI interrupt number for VirtIO devices (GIC SPI numbering).
#[cfg(test)]
const VIRTIO_IRQ_BASE: u32 = 48;

/// Guest RAM is mapped starting at IPA 0.
/// Guest RAM is mapped at 1 GiB to leave the lower address space for
/// GIC (0x0800_0000), PL011 (0x0B00_0000) and VirtIO MMIO (0x0C00_0000).
const RAM_BASE_IPA: u64 = 0x4000_0000;

/// GIC distributor base address.
const GIC_DIST_ADDR: u64 = 0x0800_0000;
/// GIC distributor region size (64 KB from hv_gic_get_distributor_size).
const GIC_DIST_SIZE: u64 = 0x1_0000;
/// GIC redistributor base address.
const GIC_REDIST_ADDR: u64 = 0x080A_0000;
/// GIC redistributor region size (32 MB, enough for max vCPUs).
const GIC_REDIST_SIZE: u64 = 0x200_0000;

/// Type alias for the guest memory backing used by the parent `Vmm` struct
/// (HV backend). Now backed by `vm-memory`'s mmap abstraction.
pub(super) type HvGuestMem = GuestMemoryMmap;

/// Type alias for `GuestRam` used by the parent `Vmm` struct to hold the
/// Holds a page-aligned host allocation that backs guest RAM.
/// Superseded by GuestMemoryMmap in the live boot path; retained for tests.
#[cfg(test)]
struct GuestRam {
    ptr: *mut u8,
    layout: Layout,
}

#[cfg(test)]
unsafe impl Send for GuestRam {}
#[cfg(test)]
unsafe impl Sync for GuestRam {}

#[cfg(test)]
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

    fn size(&self) -> usize {
        self.layout.size()
    }
}

#[cfg(test)]
impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated with this layout by alloc_zeroed.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

// ---------------------------------------------------------------------------
// PL011 UART emulation
// ---------------------------------------------------------------------------

/// Minimal PL011 UART emulator for early boot console output.
///
/// Only implements enough to capture kernel boot messages. The FDT already
/// has a PL011 node at 0x0900_0000 — this emulator handles the data path
/// so early `earlycon` output reaches the host log.
pub(super) struct Pl011 {
    /// Accumulated output buffer (line buffered).
    output: Vec<u8>,
}

/// PL011 MMIO base address. Placed at 0x0B00_0000 to avoid the GIC
/// redistributor region (0x080A_0000 + 32 MB = 0x0A0A_0000).
const PL011_BASE: u64 = 0x0B00_0000;
/// PL011 MMIO region size.
const PL011_SIZE: u64 = 0x1000;

// PL011 register offsets (only those we emulate).
const PL011_DR: u64 = 0x000; // Data Register
const PL011_FR: u64 = 0x018; // Flag Register

impl Pl011 {
    /// Creates a new PL011 UART emulator.
    pub fn new() -> Self {
        Self { output: Vec::new() }
    }

    /// Returns `true` if `addr` falls within the PL011 MMIO range.
    pub fn contains(&self, addr: u64) -> bool {
        (PL011_BASE..PL011_BASE + PL011_SIZE).contains(&addr)
    }

    /// Handles an MMIO read from the PL011 region.
    pub fn read(&self, addr: u64, _size: usize) -> u64 {
        let offset = addr - PL011_BASE;
        match offset {
            // Flag Register: TX FIFO never full, RX FIFO always empty.
            PL011_FR => 0,
            _ => 0,
        }
    }

    /// Handles an MMIO write to the PL011 region.
    pub fn write(&mut self, addr: u64, _size: usize, value: u64) {
        let offset = addr - PL011_BASE;
        if offset == PL011_DR {
            let byte = (value & 0xFF) as u8;
            self.output.push(byte);
            // Emit to host log when we get a newline.
            if byte == b'\n' {
                if let Ok(line) = std::str::from_utf8(&self.output) {
                    tracing::info!(target: "guest_serial", "{}", line.trim_end());
                }
                self.output.clear();
            }
        }
        // Ignore writes to control registers — we only care about data.
    }

    /// Flush any remaining partial-line output.
    pub fn flush(&mut self) {
        if !self.output.is_empty() {
            if let Ok(line) = std::str::from_utf8(&self.output) {
                tracing::info!(target: "guest_serial", "{}", line.trim_end());
            }
            self.output.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Device slot tracking
// ---------------------------------------------------------------------------

/// Device slot tracking for MMIO address and IRQ assignment.
/// Superseded by DeviceManager::register_virtio_device(); retained for tests.
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

/// Convert a `vm_fdt::Error` into our `VmmError`.
fn fdt_err(e: vm_fdt::Error) -> VmmError {
    VmmError::Memory(format!("FDT error: {e}"))
}

impl Vmm {
    /// Custom VMM initialization using Hypervisor.framework (manual execution).
    ///
    /// This path is an alternative to `initialize_darwin()` (VZ framework).
    /// It creates a VM via `arcbox-hv`, allocates guest RAM, sets up GIC,
    /// registers VirtIO devices, generates an FDT, and prepares vCPU state
    /// for boot.
    pub(super) fn initialize_darwin_hv(&mut self) -> Result<()> {
        tracing::info!("Initializing custom VMM via Hypervisor.framework");

        let ram_size = self.config.memory_size as usize;

        // --- 1. Allocate guest RAM via vm-memory's mmap abstraction ---
        // This allocates anonymous memory and provides type-safe GPA access.
        let guest_mem =
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(RAM_BASE_IPA), ram_size)])
                .map_err(|e| VmmError::Memory(format!("guest memory allocation failed: {e}")))?;
        tracing::debug!(
            "Allocated {} MB guest RAM via vm-memory",
            ram_size / (1024 * 1024)
        );

        // --- 2. Create Hypervisor.framework VM ---
        // Use 40-bit IPA for up to ~1 TB guest physical address space,
        // which accommodates RAM + MMIO + GIC regions.
        let vm = HvVm::with_ipa_size(40)
            .map_err(|e| VmmError::Device(format!("hv_vm_create failed: {e}")))?;

        // --- 3. Map guest RAM into HV IPA space ---
        // Get the host virtual address for the mmap'd region and map it
        // into the guest's physical address space via Hypervisor.framework.
        for region in guest_mem.iter() {
            let host_ptr = region.as_ptr();
            let guest_addr = region.start_addr().raw_value();
            let size = region.len() as usize;
            unsafe {
                vm.map_memory(
                    host_ptr,
                    guest_addr,
                    size,
                    MemoryPermission::READ_WRITE | MemoryPermission::EXEC,
                )
                .map_err(|e| VmmError::Memory(format!("hv_vm_map failed: {e}")))?;
            }
            tracing::debug!(
                "Mapped guest RAM: IPA {:#x}..{:#x} (host={:p})",
                guest_addr,
                guest_addr + size as u64,
                host_ptr,
            );
        }

        // --- 4. Initialize GIC (macOS 15+) ---
        #[cfg(feature = "gic")]
        let gic = {
            let gic_config = arcbox_hv::GicConfig {
                distributor_base: 0x0800_0000,
                redistributor_base: 0x080A_0000,
            };
            let g = arcbox_hv::Gic::new(gic_config)
                .map_err(|e| VmmError::Device(format!("GIC initialization failed: {e}")))?;
            tracing::info!(
                "GICv3 initialized: GICD @ {:#x}, GICR @ {:#x}",
                g.distributor_base(),
                g.redistributor_base(),
            );
            Some(Arc::new(g))
        };
        #[cfg(not(feature = "gic"))]
        tracing::warn!("GIC feature not enabled — interrupts will not work with custom VMM");

        // --- 5. Set up IRQ chip with GIC callback ---
        let irq_chip = Arc::new(IrqChip::new()?);

        // Shared registry for vCPU thread handles — the IRQ callback uses
        // this to unpark WFI-blocked vCPU threads when an interrupt fires.
        let vcpu_thread_handles: VcpuThreadHandles = Arc::new(Mutex::new(Vec::new()));

        #[cfg(feature = "gic")]
        if let Some(ref gic_ref) = gic {
            let gic_weak = Arc::downgrade(gic_ref);
            let threads_weak = Arc::downgrade(&vcpu_thread_handles);
            let callback: IrqTriggerCallback = Box::new(move |gsi: Gsi, level: bool| {
                if let Some(g) = gic_weak.upgrade() {
                    g.set_spi(gsi, level).map_err(|e| {
                        VmmError::Irq(format!("GIC set_spi({gsi}, {level}) failed: {e}"))
                    })?;
                    tracing::trace!("GIC: SPI {gsi} level={level}");
                } else {
                    tracing::warn!("GIC: dropped, cannot inject SPI {gsi}");
                }
                // Wake any WFI-parked vCPU threads so they can service the
                // interrupt. Only unpark on assertion (level=true) to avoid
                // spurious wakeups on de-assertion.
                if level {
                    if let Some(handles) = threads_weak.upgrade() {
                        if let Ok(handles) = handles.lock() {
                            for t in handles.iter() {
                                t.unpark();
                            }
                        }
                    }
                }
                Ok(())
            });
            irq_chip.set_trigger_callback(Arc::new(callback));
            tracing::debug!("IRQ callback wired to hardware GIC (with WFI unpark)");
        }

        // --- 6. Initialize managers ---
        // Use a custom MMIO base matching the ARM64 VirtIO MMIO layout so
        // that device addresses in the FDT match what the allocator assigns.
        // MMIO allocator aligns each slot to 4 KB, so reserve enough space
        // for the maximum number of devices at page granularity.
        let mmio_region_size = VIRTIO_MMIO_MAX_DEVICES * 0x1000;
        let mut memory_manager = MemoryManager::with_mmio_base(VIRTIO_MMIO_BASE, mmio_region_size);
        memory_manager.initialize(self.config.memory_size)?;

        let mut device_manager = DeviceManager::new();

        // Provide guest memory access so the QUEUE_NOTIFY handler can read
        // descriptors and write completions directly in guest RAM.
        // Get host pointer from the GuestMemoryMmap region for DeviceManager's
        // raw access path.
        {
            let region = guest_mem
                .iter()
                .next()
                .ok_or_else(|| VmmError::Memory("no guest memory regions".into()))?;
            let host_ptr = region.as_ptr();
            // SAFETY: guest_mem is stored in the Vmm struct and outlives the
            // DeviceManager, so the pointer remains valid.
            unsafe {
                device_manager.set_guest_memory(host_ptr, ram_size, RAM_BASE_IPA);
            }
        }

        // Wire IRQ callback so device completions trigger GIC interrupts.
        // For level-triggered SPIs, the callback must support both assert
        // (level=true) and deassert (level=false) to keep the SPI in sync
        // with the device's interrupt_status register.
        {
            let irq_chip_clone = Arc::clone(&irq_chip);
            let callback: crate::device::DeviceIrqCallback = Arc::new(move |irq, level| {
                if level {
                    irq_chip_clone
                        .trigger_irq(irq)
                        .map_err(|e| VmmError::Irq(format!("trigger_irq({irq}): {e}")))
                } else {
                    irq_chip_clone
                        .deassert_irq(irq)
                        .map_err(|e| VmmError::Irq(format!("deassert_irq({irq}): {e}")))
                }
            });
            device_manager.set_irq_callback(callback);
        }

        // --- 7. Register actual VirtIO device instances ---

        // Console
        if self.config.serial_console || self.config.virtio_console {
            let console = arcbox_virtio::console::VirtioConsole::new(
                arcbox_virtio::console::ConsoleConfig::default(),
            );
            device_manager.register_virtio_device(
                DeviceType::VirtioConsole,
                "virtio-console",
                console,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        // VirtioFS shared directories — create FsServer handler for each share
        // so the device can process FUSE requests from the guest.
        for dir in &self.config.shared_dirs {
            let fs_config = arcbox_virtio::fs::FsConfig {
                tag: dir.tag.clone(),
                num_queues: 1,
                queue_size: 1024,
                shared_dir: dir.host_path.to_string_lossy().into_owned(),
            };

            // Create and start the filesystem server (passthrough to host directory).
            let server_config = arcbox_fs::FsConfig {
                tag: dir.tag.clone(),
                source: dir.host_path.to_string_lossy().into_owned(),
                ..arcbox_fs::FsConfig::default()
            };
            let mut server = arcbox_fs::FsServer::new(server_config);
            server
                .start()
                .map_err(|e| VmmError::Device(format!("FsServer start failed: {e}")))?;
            let handler: std::sync::Arc<dyn arcbox_virtio::fs::FuseRequestHandler> =
                std::sync::Arc::new(server);

            let fs_dev = arcbox_virtio::fs::VirtioFs::with_handler(fs_config, handler);
            let name = format!("virtiofs-{}", dir.tag);
            device_manager.register_virtio_device(
                DeviceType::VirtioFs,
                name,
                fs_dev,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        // Block devices — capture raw_fd for async I/O worker.
        for block_dev in &self.config.block_devices {
            let blk =
                arcbox_virtio::blk::VirtioBlock::from_path(&block_dev.path, block_dev.read_only)
                    .map_err(|e| VmmError::Device(format!("block device: {e}")))?;
            let raw_fd = blk.raw_fd().unwrap_or(-1);
            let blk_size = blk.blk_size();
            let read_only = blk.is_read_only();
            let dev_id_str = blk.device_id_string().to_string();
            let name = format!("virtio-blk-{}", block_dev.path.display());
            let device_id = device_manager.register_virtio_device(
                DeviceType::VirtioBlock,
                name,
                blk,
                &mut memory_manager,
                &irq_chip,
            )?;
            if raw_fd >= 0 {
                self.hv_blk_devices
                    .push((device_id, raw_fd, blk_size, read_only, dev_id_str));
            }
        }

        // Network (TSO-enabled) with custom socket-proxy datapath.
        // Creates a SOCK_DGRAM socketpair: one end feeds the VirtioNet device
        // (via DeviceManager TX/RX bridging), the other end goes to the same
        // NetworkDatapath used by the VZ path (DHCP, DNS, NAT, TCP proxy).
        if self.config.networking {
            let net_config = arcbox_virtio::net::NetConfig {
                mac: arcbox_virtio::net::NetConfig::random_mac(),
                ..Default::default()
            };
            let mut net_dev = arcbox_virtio::net::VirtioNet::new(net_config);
            net_dev.enable_tso_features();
            device_manager.register_virtio_device(
                DeviceType::VirtioNet,
                "virtio-net",
                net_dev,
                &mut memory_manager,
                &irq_chip,
            )?;

            // Set up the network datapath (reuses VZ path's entire stack).
            self.create_hv_network_datapath(&mut device_manager)?;
        }

        // Entropy (RNG) — provides /dev/hwrng to the guest. Without this,
        // the kernel's crng never initializes and dockerd blocks on
        // /dev/urandom indefinitely.
        {
            let rng_dev = arcbox_virtio::rng::VirtioRng::new();
            device_manager.register_virtio_device(
                DeviceType::VirtioRng,
                "virtio-rng",
                rng_dev,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        // Vsock
        if self.config.vsock {
            let vsock_config = arcbox_virtio::vsock::VsockConfig {
                guest_cid: self.config.guest_cid.unwrap_or(3) as u64,
            };
            let vsock_dev = arcbox_virtio::vsock::VirtioVsock::new(vsock_config);
            device_manager.register_virtio_device(
                DeviceType::VirtioVsock,
                "virtio-vsock",
                vsock_dev,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        for dev_info in device_manager.iter() {
            tracing::info!(
                "VirtIO device: {} ({:?}) @ MMIO {:#x} IRQ {:?}",
                dev_info.name,
                dev_info.device_type,
                dev_info.mmio_base.unwrap_or(0),
                dev_info.irq
            );
        }

        // --- 8. Load kernel via linux-loader PE loader ---
        let mut kernel_file = std::fs::File::open(&self.config.kernel_path)
            .map_err(|e| VmmError::config(format!("cannot open kernel: {e}")))?;

        // PE::load writes the kernel image directly into GuestMemoryMmap.
        // The kernel_offset must be 2 MB aligned (ARM64 boot protocol).
        let kernel_result = PE::load(
            &guest_mem,
            Some(GuestAddress(RAM_BASE_IPA)),
            &mut kernel_file,
            None,
        )
        .map_err(|e| VmmError::config(format!("kernel loading failed: {e}")))?;

        let kernel_entry = kernel_result.kernel_load.raw_value();
        tracing::info!(
            "Kernel loaded via linux-loader: entry={:#x}, end={:#x}",
            kernel_entry,
            kernel_result.kernel_end,
        );

        // --- 9. Load initrd via vm-memory ---
        // Pass the initrd as-is to guest memory. The Linux kernel has built-in
        // decompression for gzip/xz/lz4/zstd compressed initramfs archives.
        let initrd_info: Option<(u64, u64)> = if let Some(ref initrd_path) = self.config.initrd_path
        {
            let initrd_data = std::fs::read(initrd_path)
                .map_err(|e| VmmError::config(format!("cannot read initrd: {e}")))?;

            // Place initrd well after the kernel to avoid corruption during
            // early boot memory setup. Use a fixed high address within RAM.
            // RAM: 0x40000000..0xC0000000, place initrd at 0x48000000 (128MB from RAM base).
            let initrd_addr = GuestAddress(RAM_BASE_IPA + 0x0800_0000);

            guest_mem
                .write_slice(&initrd_data, initrd_addr)
                .map_err(|e| VmmError::Memory(format!("failed to write initrd: {e}")))?;

            // Verify initrd was written correctly by reading back the first bytes.
            let mut verify = [0u8; 4];
            guest_mem
                .read_slice(&mut verify, initrd_addr)
                .map_err(|e| VmmError::Memory(format!("initrd verify read failed: {e}")))?;
            tracing::info!(
                "Initrd loaded: addr={:#x}, size={} bytes, magic={:02x}{:02x}{:02x}{:02x}",
                initrd_addr.raw_value(),
                initrd_data.len(),
                verify[0],
                verify[1],
                verify[2],
                verify[3],
            );

            Some((initrd_addr.raw_value(), initrd_data.len() as u64))
        } else {
            None
        };

        // --- 10. Generate FDT via vm-fdt ---
        let fdt_entries = device_manager.device_tree_entries();

        let fdt_blob = {
            let mut fdt = FdtWriter::new().map_err(fdt_err)?;

            // Root node
            let root = fdt.begin_node("").map_err(fdt_err)?;
            fdt.property_string("compatible", "linux,dummy-virt")
                .map_err(fdt_err)?;
            fdt.property_u32("#address-cells", 2).map_err(fdt_err)?;
            fdt.property_u32("#size-cells", 2).map_err(fdt_err)?;
            fdt.property_u32("interrupt-parent", 1).map_err(fdt_err)?; // GIC phandle

            // Chosen node
            let chosen = fdt.begin_node("chosen").map_err(fdt_err)?;
            fdt.property_string("bootargs", &self.config.kernel_cmdline)
                .map_err(fdt_err)?;
            fdt.property_string("stdout-path", "/pl011@b000000")
                .map_err(fdt_err)?;
            if let Some((initrd_start, initrd_size)) = initrd_info {
                fdt.property_u64("linux,initrd-start", initrd_start)
                    .map_err(fdt_err)?;
                fdt.property_u64("linux,initrd-end", initrd_start + initrd_size)
                    .map_err(fdt_err)?;
            }
            fdt.end_node(chosen).map_err(fdt_err)?;

            // Memory node
            let mem_node = fdt
                .begin_node(&format!("memory@{RAM_BASE_IPA:x}"))
                .map_err(fdt_err)?;
            fdt.property_string("device_type", "memory")
                .map_err(fdt_err)?;
            let mut reg = Vec::new();
            reg.extend_from_slice(&RAM_BASE_IPA.to_be_bytes());
            reg.extend_from_slice(&(ram_size as u64).to_be_bytes());
            fdt.property("reg", &reg).map_err(fdt_err)?;
            fdt.end_node(mem_node).map_err(fdt_err)?;

            // CPUs
            let cpus = fdt.begin_node("cpus").map_err(fdt_err)?;
            fdt.property_u32("#address-cells", 1).map_err(fdt_err)?;
            fdt.property_u32("#size-cells", 0).map_err(fdt_err)?;
            for i in 0..self.config.vcpu_count {
                let cpu = fdt.begin_node(&format!("cpu@{i}")).map_err(fdt_err)?;
                fdt.property_string("device_type", "cpu").map_err(fdt_err)?;
                fdt.property_string("compatible", "arm,arm-v8")
                    .map_err(fdt_err)?;
                fdt.property_string("enable-method", "psci")
                    .map_err(fdt_err)?;
                fdt.property_u32("reg", i).map_err(fdt_err)?;
                fdt.end_node(cpu).map_err(fdt_err)?;
            }
            fdt.end_node(cpus).map_err(fdt_err)?;

            // Timer
            let timer = fdt.begin_node("timer").map_err(fdt_err)?;
            fdt.property_string("compatible", "arm,armv8-timer")
                .map_err(fdt_err)?;
            fdt.property_null("always-on").map_err(fdt_err)?;
            // PPI interrupts: secure phys, non-secure phys, virt, hyp
            fdt.property_array_u32(
                "interrupts",
                &[
                    1, 13, 0x304, // Secure phys timer
                    1, 14, 0x304, // Non-secure phys timer
                    1, 11, 0x304, // Virtual timer
                    1, 10, 0x304, // Hyperphysical timer
                ],
            )
            .map_err(fdt_err)?;
            fdt.end_node(timer).map_err(fdt_err)?;

            // PSCI
            let psci = fdt.begin_node("psci").map_err(fdt_err)?;
            fdt.property_string("compatible", "arm,psci-1.0")
                .map_err(fdt_err)?;
            fdt.property_string("method", "hvc").map_err(fdt_err)?;
            fdt.end_node(psci).map_err(fdt_err)?;

            // GIC v3
            let intc = fdt
                .begin_node(&format!("intc@{GIC_DIST_ADDR:x}"))
                .map_err(fdt_err)?;
            fdt.property_string("compatible", "arm,gic-v3")
                .map_err(fdt_err)?;
            fdt.property_u32("#interrupt-cells", 3).map_err(fdt_err)?;
            fdt.property_null("interrupt-controller").map_err(fdt_err)?;
            fdt.property_phandle(1).map_err(fdt_err)?;
            // reg: distributor base+size, redistributor base+size
            let mut gic_reg = Vec::new();
            gic_reg.extend_from_slice(&GIC_DIST_ADDR.to_be_bytes());
            gic_reg.extend_from_slice(&GIC_DIST_SIZE.to_be_bytes());
            gic_reg.extend_from_slice(&GIC_REDIST_ADDR.to_be_bytes());
            gic_reg.extend_from_slice(&GIC_REDIST_SIZE.to_be_bytes());
            fdt.property("reg", &gic_reg).map_err(fdt_err)?;
            fdt.end_node(intc).map_err(fdt_err)?;

            // PL011 UART
            let uart = fdt.begin_node("pl011@b000000").map_err(fdt_err)?;
            fdt.property_string("compatible", "arm,pl011")
                .map_err(fdt_err)?;
            let mut uart_reg = Vec::new();
            uart_reg.extend_from_slice(&PL011_BASE.to_be_bytes());
            uart_reg.extend_from_slice(&PL011_SIZE.to_be_bytes());
            fdt.property("reg", &uart_reg).map_err(fdt_err)?;
            fdt.property_array_u32("interrupts", &[0, 1, 4])
                .map_err(fdt_err)?; // SPI 1, level
            fdt.property_u32("clock-frequency", 24_000_000)
                .map_err(fdt_err)?;
            fdt.end_node(uart).map_err(fdt_err)?;

            // VirtIO MMIO devices from DeviceManager
            for entry in &fdt_entries {
                let node = fdt
                    .begin_node(&format!("virtio_mmio@{:x}", entry.reg_base))
                    .map_err(fdt_err)?;
                fdt.property_string("compatible", &entry.compatible)
                    .map_err(fdt_err)?;
                let mut dev_reg = Vec::new();
                dev_reg.extend_from_slice(&entry.reg_base.to_be_bytes());
                dev_reg.extend_from_slice(&entry.reg_size.to_be_bytes());
                fdt.property("reg", &dev_reg).map_err(fdt_err)?;
                // GIC SPI numbering: FDT SPI number = INTID - 32.
                // hv_gic_set_spi uses INTID directly (starting at 32).
                fdt.property_array_u32("interrupts", &[0, entry.irq.saturating_sub(32), 4])
                    .map_err(fdt_err)?; // SPI, edge
                fdt.property_null("dma-coherent").map_err(fdt_err)?;
                fdt.end_node(node).map_err(fdt_err)?;
            }

            fdt.end_node(root).map_err(fdt_err)?;
            fdt.finish().map_err(fdt_err)?
        };

        if fdt_blob.len() > arm64::FDT_MAX_SIZE {
            return Err(VmmError::Memory("generated FDT exceeds 2 MB limit".into()));
        }

        // Place FDT at end of RAM, page-aligned backward.
        let fdt_addr =
            GuestAddress((RAM_BASE_IPA + ram_size as u64 - fdt_blob.len() as u64) & !0xFFF);
        guest_mem
            .write_slice(&fdt_blob, fdt_addr)
            .map_err(|e| VmmError::Memory(format!("failed to write FDT: {e}")))?;

        tracing::info!(
            "FDT written: addr={:#x}, size={} bytes, devices={}",
            fdt_addr.raw_value(),
            fdt_blob.len(),
            fdt_entries.len()
        );

        // --- 11. Store managers ---
        let event_loop = crate::event::EventLoop::new()?;

        self.memory_manager = Some(memory_manager);
        self.device_manager = Some(device_manager);
        self.irq_chip = Some(irq_chip);
        self.event_loop = Some(event_loop);

        // Store HV-specific state in the Vmm struct for lifecycle management.
        // GuestMemoryMmap must outlive the HvVm since the mapped memory must
        // remain valid for the entire VM lifetime.
        self.hv_vm = Some(vm);
        self.hv_guest_mem = Some(guest_mem);
        #[cfg(feature = "gic")]
        {
            self.hv_gic = gic;
        }
        self.hv_kernel_entry = Some(kernel_entry);
        self.hv_fdt_addr = Some(fdt_addr.raw_value());
        self.hv_vcpu_thread_handles = Some(vcpu_thread_handles);

        tracing::info!("Custom Hypervisor.framework VMM initialized");
        Ok(())
    }

    /// Starts the custom HV VMM by spawning vCPU threads.
    ///
    /// The BSP (vCPU 0) runs immediately. Secondary vCPUs (1..N) are spawned
    /// in a "parked" state and wait on a channel for a PSCI CPU_ON request
    /// from the BSP before entering their run loop.
    pub(super) fn start_darwin_hv(&mut self) -> Result<()> {
        let kernel_entry = self
            .hv_kernel_entry
            .ok_or_else(|| VmmError::config("HV kernel entry not set".to_string()))?;
        let fdt_addr = self
            .hv_fdt_addr
            .ok_or_else(|| VmmError::config("HV FDT address not set".to_string()))?;

        let mut device_manager = Arc::new(
            self.device_manager
                .take()
                .ok_or_else(|| VmmError::config("device manager not initialized".to_string()))?,
        );

        // Spawn async block I/O worker threads (one per block device).
        // Uses device info captured during initialize_darwin_hv.
        // Must happen before Arc is cloned to other threads.
        {
            let dm = Arc::get_mut(&mut device_manager).expect("single Arc ref");
            let (guest_ptr, guest_len) = if let (Some(base), size, gpa) = (
                dm.guest_ram_base_ptr(),
                dm.guest_ram_size(),
                dm.guest_ram_gpa(),
            ) {
                let gpa = gpa as usize;
                (unsafe { base.sub(gpa) }, gpa + size)
            } else {
                (std::ptr::null_mut(), 0)
            };

            // Collect IRQ info for each block device before spawning workers.
            let blk_infos = std::mem::take(&mut self.hv_blk_devices)
                .into_iter()
                .filter_map(|(dev_id, raw_fd, blk_size, read_only, dev_id_str)| {
                    let dev = dm.get_registered_device(dev_id)?;
                    let irq = dev.info.irq?;
                    let mmio_state = dev.mmio_state.as_ref()?.clone();
                    Some((
                        dev_id, raw_fd, blk_size, read_only, dev_id_str, irq, mmio_state,
                    ))
                })
                .collect::<Vec<_>>();

            for (dev_id, raw_fd, blk_size, read_only, dev_id_str, irq, mmio_state) in blk_infos {
                let (tx, rx) = std::sync::mpsc::channel::<crate::blk_worker::BlkWorkItem>();

                // Queue config will be filled when the guest activates the device.
                // For now use zeros — the worker only needs used_addr/avail_addr/queue_size
                let worker_ctx = crate::blk_worker::BlkWorkerContext {
                    guest_mem: unsafe {
                        crate::blk_worker::GuestMemWriter::new(guest_ptr, guest_len)
                    },
                    raw_fd,
                    blk_size,
                    read_only,
                    device_id: dev_id_str.clone(),
                    mmio_state,
                    irq_callback: dm.irq_callback_clone().unwrap_or_else(|| {
                        Arc::new(|_: crate::irq::Irq, _: bool| -> crate::error::Result<()> {
                            Ok(())
                        })
                    }),
                    irq,
                    running: self.running.clone(),
                };

                let thread_name = format!("blk-io-{}", dev_id_str);
                match std::thread::Builder::new()
                    .name(thread_name.clone())
                    .spawn(move || {
                        crate::blk_worker::blk_io_worker_loop(worker_ctx, rx);
                    }) {
                    Ok(t) => {
                        self.hv_blk_worker_threads.push(t);
                        dm.set_blk_worker(dev_id, tx);
                        tracing::info!("Spawned async block I/O worker: {}", dev_id_str);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to spawn blk worker {}: {}", thread_name, e);
                    }
                }
            }
        }

        // Store a shared reference for connect_vsock_hv to use after start.
        self.hv_device_manager = Some(Arc::clone(&device_manager));
        let running = self.running.clone();
        let vcpu_count = self.config.vcpu_count;
        let pl011 = Arc::new(std::sync::Mutex::new(Pl011::new()));

        let vcpu_thread_handles = self
            .hv_vcpu_thread_handles
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));

        // --- Set up PSCI CPU_ON channels for secondary vCPUs ---
        let cpu_on_senders: Option<CpuOnSenders> = if vcpu_count > 1 {
            let mut senders_vec: Vec<Option<mpsc::Sender<CpuOnRequest>>> = Vec::new();
            senders_vec.push(None); // Slot 0 = BSP

            for i in 1..vcpu_count {
                let (tx, rx) = mpsc::channel::<CpuOnRequest>();
                senders_vec.push(Some(tx));

                let r = running.clone();
                let dm = device_manager.clone();
                let th = vcpu_thread_handles.clone();
                let uart = pl011.clone();
                let senders_placeholder: Option<CpuOnSenders> = None;

                let t = std::thread::Builder::new()
                    .name(format!("hv-vcpu-{i}"))
                    .spawn(move || match rx.recv() {
                        Ok(req) => {
                            tracing::info!(
                                "vCPU {i}: received CPU_ON, starting at {:#x}",
                                req.entry_point
                            );
                            vcpu_run_loop(
                                i,
                                req.entry_point,
                                req.context_id,
                                dm,
                                r,
                                uart,
                                senders_placeholder,
                                th,
                            );
                        }
                        Err(_) => {
                            tracing::debug!("vCPU {i}: channel closed, never started");
                        }
                    })
                    .map_err(|e| VmmError::Vcpu(format!("spawn vcpu-{i}: {e}")))?;
                self.hv_vcpu_threads.push(t);
            }

            let senders = Arc::new(Mutex::new(senders_vec));
            self.hv_cpu_on_senders = Some(senders.clone());
            Some(senders)
        } else {
            None
        };

        // --- Spawn BSP (vCPU 0) ---
        {
            let t = std::thread::Builder::new()
                .name("hv-vcpu-0".to_string())
                .spawn(move || {
                    vcpu_run_loop(
                        0,
                        kernel_entry,
                        fdt_addr,
                        device_manager,
                        running,
                        pl011,
                        cpu_on_senders,
                        vcpu_thread_handles,
                    );
                })
                .map_err(|e| VmmError::Vcpu(format!("spawn vcpu-0: {e}")))?;
            self.hv_vcpu_threads.push(t);
        }

        tracing::info!(
            "Custom HV VMM started: {} vCPU(s) (BSP running, {} secondary parked)",
            vcpu_count,
            vcpu_count.saturating_sub(1)
        );
        Ok(())
    }

    /// Stops the HV backend by signaling vCPU threads and cleaning up resources.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn stop_darwin_hv(&mut self) -> Result<()> {
        // Signal all vCPU threads to exit.
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Force-exit all vCPUs from their run loops.
        if let Some(ref vm) = self.hv_vm {
            if let Err(e) = vm.exit_all_vcpus() {
                tracing::warn!("hv_vcpus_exit failed: {e}");
            }
        }

        // Join all vCPU threads.
        for t in self.hv_vcpu_threads.drain(..) {
            if let Err(e) = t.join() {
                tracing::warn!("vCPU thread join failed: {e:?}");
            }
        }

        // Cleanup in correct order: GIC → VM → guest memory.
        // Guest memory must be dropped after the VM so the mapped pages
        // remain valid until hv_vm_destroy completes.
        #[cfg(feature = "gic")]
        {
            self.hv_gic.take();
        }
        self.hv_vm.take();
        self.hv_guest_mem.take();

        tracing::info!("Custom VMM stopped");
        Ok(())
    }

    /// Creates the network datapath for the HV backend.
    ///
    /// Sets up a SOCK_DGRAM socketpair. One end is registered with DeviceManager
    /// for VirtioNet TX/RX bridging. The other end feeds NetworkDatapath (the
    /// same stack used by VZ: DHCP, DNS, socket proxy, TCP bridge).
    fn create_hv_network_datapath(
        &mut self,
        device_manager: &mut crate::device::DeviceManager,
    ) -> Result<()> {
        use arcbox_net::darwin::datapath_loop::NetworkDatapath;
        use arcbox_net::darwin::inbound_relay::InboundListenerManager;
        use arcbox_net::darwin::socket_proxy::SocketProxy;
        use arcbox_net::dhcp::{DhcpConfig, DhcpServer};
        use arcbox_net::dns::{DnsConfig, DnsForwarder};
        use std::net::Ipv4Addr;

        let gateway_ip = Ipv4Addr::new(10, 0, 2, 1);
        let guest_ip = Ipv4Addr::new(10, 0, 2, 2);
        let netmask = Ipv4Addr::new(255, 255, 255, 0);
        let gateway_mac: [u8; 6] = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];

        // 1. Create a SOCK_DGRAM socketpair for L2 Ethernet frame exchange.
        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: socketpair with valid parameters.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "net socketpair failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // fds[0] = HV side (DeviceManager reads/writes raw ethernet frames)
        // fds[1] = datapath side (NetworkDatapath reads/writes)
        let hv_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let host_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Set large socket buffers and non-blocking on HV side.
        unsafe {
            let buf_size: libc::c_int = 8 * 1024 * 1024;
            libc::setsockopt(
                hv_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            libc::setsockopt(
                hv_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let flags = libc::fcntl(hv_fd.as_raw_fd(), libc::F_GETFL, 0);
            libc::fcntl(hv_fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Register the HV-side fd with DeviceManager for TX/RX bridging.
        device_manager.set_net_host_fd(hv_fd.as_raw_fd());

        // 2. Cancellation token.
        let cancel = tokio_util::sync::CancellationToken::new();
        self.net_cancel = Some(cancel.clone());

        // 3. Create socket proxy and channels.
        let (reply_tx, reply_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(256);
        let socket_proxy =
            SocketProxy::new(gateway_ip, gateway_mac, guest_ip, reply_tx, cancel.clone());

        self.inbound_listener_manager = Some(InboundListenerManager::new(cmd_tx));

        // 4. Create DHCP + DNS.
        let dhcp_config = DhcpConfig::new(gateway_ip, netmask)
            .with_pool_range(guest_ip, Ipv4Addr::new(10, 0, 2, 254))
            .with_dns_servers(vec![gateway_ip]);
        let dhcp_server = DhcpServer::new(dhcp_config);

        let dns_config = DnsConfig::new(gateway_ip);
        let dns_forwarder = if let Some(ref shared_table) = self.shared_dns_hosts {
            DnsForwarder::with_shared_hosts(dns_config, std::sync::Arc::clone(shared_table))
        } else {
            DnsForwarder::new(dns_config)
        };

        // 5. Build and spawn the datapath.
        let net_mtu = arcbox_net::darwin::smoltcp_device::ENHANCED_ETHERNET_MTU;
        let datapath = NetworkDatapath::new(
            host_fd,
            socket_proxy,
            reply_rx,
            cmd_rx,
            dhcp_server,
            dns_forwarder,
            gateway_ip,
            guest_ip,
            gateway_mac,
            cancel,
            net_mtu,
        );

        let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
            VmmError::Device(format!(
                "tokio runtime not available for network datapath: {e}"
            ))
        })?;

        runtime.spawn(async move {
            if let Err(e) = datapath.run().await {
                tracing::error!("HV network datapath exited with error: {}", e);
            }
        });

        // Keep the HV-side fd alive for VM lifetime.
        self.hv_net_fd = Some(hv_fd);

        tracing::info!(
            "HV network datapath: gateway={}, guest={}, MTU={}",
            gateway_ip,
            guest_ip,
            net_mtu,
        );
        Ok(())
    }

    /// Connects to a vsock port on the guest VM (HV backend).
    ///
    /// Creates a Unix socketpair. One end is returned to the caller for
    /// host-side I/O. The other end is stored for the vsock forwarding
    /// path to relay data between host and guest VirtIO vsock queues.
    ///
    /// Note: full bidirectional forwarding requires vsock TX queue
    /// processing in the vCPU run loop (QUEUE_NOTIFY handler) and
    /// RX packet injection. Currently this provides the socketpair
    /// plumbing; actual data forwarding is handled by the VirtioVsock
    /// device's process_queue implementation.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn connect_vsock_hv(&self, port: u32) -> Result<std::os::unix::io::RawFd> {
        use std::os::unix::io::FromRawFd;

        // Create a Unix SOCK_STREAM socketpair for bidirectional data.
        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: socketpair with valid parameters.
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "vsock socketpair failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Set non-blocking on both ends.
        for &fd in &fds {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }

        // fds[0] = returned to caller (daemon agent client)
        // fds[1] = internal, owned by VsockConnectionManager
        // SAFETY: fds are valid from socketpair.
        let internal_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        if let Some(ref dm) = self.hv_device_manager {
            let guest_cid = self.config.guest_cid.unwrap_or(3) as u64;

            // Allocate a connection with a unique ephemeral host port.
            // The manager takes ownership of internal_fd (no mem::forget needed).
            let conn_id = {
                let conns = dm.vsock_connections();
                let mut mgr = conns
                    .lock()
                    .map_err(|e| VmmError::Device(format!("vsock manager lock failed: {e}")))?;
                mgr.allocate(port, guest_cid, internal_fd)
            };

            tracing::info!(
                "HV vsock connect: guest_port={}, host_port={}, host_fd={}",
                port,
                conn_id.host_port,
                fds[0],
            );

            // Inject OP_REQUEST. If device isn't ready, the manager's
            // pending list ensures deferred injection via poll_vsock_rx.
            if dm.inject_vsock_connect(conn_id, guest_cid) {
                dm.raise_interrupt_for(crate::device::DeviceType::VirtioVsock, 1);
            }
        }

        Ok(fds[0])
    }
}

// ---------------------------------------------------------------------------
// vCPU run loop
// ---------------------------------------------------------------------------

/// ARM64 register IDs re-exported from arcbox-hv.
mod reg {
    pub use arcbox_hv::reg::{
        HV_REG_CPSR as CPSR, HV_REG_PC as PC, HV_REG_X0 as X0, HV_REG_X1 as X1, HV_REG_X2 as X2,
        HV_REG_X3 as X3,
    };
}

/// CPSR value: EL1h with DAIF masked (all interrupts masked at boot).
const CPSR_EL1H: u64 = 0x3C5;

/// Runs a single vCPU in a loop, dispatching MMIO traps to the device manager.
///
/// This function is intended to be called from a dedicated thread per vCPU.
/// `HvVcpu` is `!Send`, so it must be created inside this function on the
/// thread that will run it.
///
/// # Arguments
///
/// * `vcpu_id` — Logical vCPU index (0-based, for logging).
/// * `entry_addr` — Guest IPA where execution begins. For the BSP this is
///   the kernel entry point; for a secondary vCPU it is the address passed
///   in PSCI CPU_ON.
/// * `x0_value` — Initial value of X0. For the BSP this is the FDT address;
///   for a secondary vCPU it is the context_id from PSCI CPU_ON.
/// * `device_manager` — Shared device manager for MMIO dispatch.
/// * `running` — Shared flag; the loop exits when this is set to `false`.
/// * `pl011` — Shared PL011 UART emulator for early console output.
/// * `cpu_on_senders` — Channel senders for waking secondary vCPUs via
///   PSCI CPU_ON. `None` when the VM has only one vCPU.
/// * `vcpu_thread_handles` — Registry of vCPU thread handles used by the
///   IRQ callback to unpark WFI-blocked threads.
#[allow(clippy::too_many_arguments)]
fn vcpu_run_loop(
    vcpu_id: u32,
    entry_addr: u64,
    x0_value: u64,
    device_manager: Arc<crate::device::DeviceManager>,
    running: Arc<AtomicBool>,
    pl011: Arc<std::sync::Mutex<Pl011>>,
    cpu_on_senders: Option<CpuOnSenders>,
    vcpu_thread_handles: VcpuThreadHandles,
) {
    use arcbox_hv::{ExceptionClass, HvVcpu, VcpuExit};
    use std::sync::atomic::Ordering;

    // Register this thread's handle so the IRQ callback can unpark us.
    {
        let mut handles = vcpu_thread_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        handles.push(std::thread::current());
    }

    let vcpu = match HvVcpu::new() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("vCPU {vcpu_id}: creation failed: {e}");
            return;
        }
    };

    // Set initial register state for ARM64 Linux boot protocol:
    //   PC   = entry address (kernel entry for BSP, PSCI entry for secondary)
    //   X0   = parameter (FDT address for BSP, context_id for secondary)
    //   X1-X3 = 0 (reserved per ARM64 boot protocol)
    //   CPSR = EL1h, DAIF masked
    if let Err(e) = vcpu.set_reg(reg::PC, entry_addr) {
        tracing::error!("vCPU {vcpu_id}: set PC failed: {e}");
        return;
    }
    if let Err(e) = vcpu.set_reg(reg::X0, x0_value) {
        tracing::error!("vCPU {vcpu_id}: set X0 failed: {e}");
        return;
    }
    let _ = vcpu.set_reg(reg::X1, 0);
    let _ = vcpu.set_reg(reg::X2, 0);
    let _ = vcpu.set_reg(reg::X3, 0);
    if let Err(e) = vcpu.set_reg(reg::CPSR, CPSR_EL1H) {
        tracing::error!("vCPU {vcpu_id}: set CPSR failed: {e}");
        return;
    }

    // ARM64 boot protocol: MMU must be off, caches can be on or off.
    // Set SCTLR_EL1 to safe reset value: MMU off, alignment check off,
    // data/instruction caches off, endianness LE.
    //
    // Bits: RES1 bits (11, 20, 22, 23, 28, 29) must be set per ARMv8.
    // SCTLR_EL1 reset value with MMU=0, C=0, I=0, A=0.
    const SCTLR_EL1_RESET: u64 = (1 << 11) // RES1
        | (1 << 20) // RES1
        | (1 << 22) // RES1
        | (1 << 23) // RES1
        | (1 << 28) // RES1
        | (1 << 29); // RES1
    if let Err(e) = vcpu.set_sys_reg(arcbox_hv::sys_reg::HV_SYS_REG_SCTLR_EL1, SCTLR_EL1_RESET) {
        tracing::warn!("vCPU {vcpu_id}: set SCTLR_EL1 failed: {e}");
    }

    // Set MPIDR_EL1 for this vCPU (used by GIC affinity routing).
    // Simple layout: Aff0 = vcpu_id, all other affinity fields 0.
    let mpidr = u64::from(vcpu_id) & 0xFF;
    if let Err(e) = vcpu.set_sys_reg(arcbox_hv::sys_reg::HV_SYS_REG_MPIDR_EL1, mpidr) {
        tracing::warn!("vCPU {vcpu_id}: set MPIDR failed (may not be writable): {e}");
    }

    tracing::info!(
        "vCPU {vcpu_id}: starting at PC={:#x}, X0={:#x}, SCTLR={:#x}",
        entry_addr,
        x0_value,
        SCTLR_EL1_RESET,
    );

    loop {
        if !running.load(Ordering::Relaxed) {
            tracing::info!("vCPU {vcpu_id}: shutdown requested");
            break;
        }

        // BSP (vCPU 0) handles all device polling to avoid lock contention.
        if vcpu_id == 0 {
            if device_manager.poll_vsock_rx() {
                device_manager.raise_interrupt_for(
                    crate::device::DeviceType::VirtioVsock,
                    1, // INT_VRING
                );
            }
            if device_manager.poll_net_rx() {
                device_manager.raise_interrupt_for(crate::device::DeviceType::VirtioNet, 1);
            }
        }

        let exit = match vcpu.run() {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("vCPU {vcpu_id}: run failed: {e}");
                running.store(false, Ordering::SeqCst);
                break;
            }
        };

        match exit {
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(ref mmio),
                ..
            } => {
                // Check PL011 UART region first, then fall through to DeviceManager.
                let handled_by_pl011 = {
                    let uart_match = {
                        let guard = pl011.lock().unwrap();
                        guard.contains(mmio.address)
                    };
                    if uart_match {
                        if mmio.is_write {
                            // ARM64: register 31 = XZR (zero), not SP.
                            let value = if mmio.register == 31 {
                                0u64
                            } else {
                                match vcpu.get_reg(u32::from(mmio.register)) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        tracing::error!(
                                            "vCPU {vcpu_id}: get_reg(X{}) failed: {e}",
                                            mmio.register
                                        );
                                        0
                                    }
                                }
                            };
                            pl011.lock().unwrap().write(
                                mmio.address,
                                mmio.access_size as usize,
                                value,
                            );
                        } else {
                            let value = pl011
                                .lock()
                                .unwrap()
                                .read(mmio.address, mmio.access_size as usize);
                            if let Err(e) = vcpu.set_reg(u32::from(mmio.register), value) {
                                tracing::error!(
                                    "vCPU {vcpu_id}: set_reg(X{}) failed: {e}",
                                    mmio.register
                                );
                            }
                        }
                        true
                    } else {
                        false
                    }
                };

                if !handled_by_pl011 {
                    // Dispatch to DeviceManager for VirtIO MMIO devices.
                    if mmio.is_write {
                        // ARM64: register 31 in a load/store is XZR (zero register),
                        // not SP. HV.framework's get_reg(31) returns SP, so we must
                        // handle XZR explicitly.
                        let value = if mmio.register == 31 {
                            0u64
                        } else {
                            match vcpu.get_reg(u32::from(mmio.register)) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::error!(
                                        "vCPU {vcpu_id}: get_reg(X{}) failed: {e}",
                                        mmio.register
                                    );
                                    let pc = vcpu.get_reg(reg::PC).unwrap_or(0);
                                    let _ = vcpu.set_reg(reg::PC, pc + 4);
                                    continue;
                                }
                            }
                        };
                        tracing::trace!(
                            "MMIO write: addr={:#x} offset={:#x} X{}={:#x} size={}",
                            mmio.address,
                            mmio.address.saturating_sub(
                                mmio.address & !0xFFF // base = addr & ~0xFFF
                            ),
                            mmio.register,
                            value,
                            mmio.access_size,
                        );
                        if let Err(e) = device_manager.handle_mmio_write(
                            mmio.address,
                            mmio.access_size as usize,
                            value,
                        ) {
                            tracing::warn!(
                                "vCPU {vcpu_id}: MMIO write {:#x} failed: {e}",
                                mmio.address
                            );
                        }
                    } else {
                        let value = match device_manager
                            .handle_mmio_read(mmio.address, mmio.access_size as usize)
                        {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "vCPU {vcpu_id}: MMIO read {:#x} failed: {e}",
                                    mmio.address
                                );
                                0 // Return 0 for unknown reads.
                            }
                        };
                        if let Err(e) = vcpu.set_reg(u32::from(mmio.register), value) {
                            tracing::error!(
                                "vCPU {vcpu_id}: set_reg(X{}) failed: {e}",
                                mmio.register
                            );
                        }
                    }
                }

                // Advance PC past the trapped instruction (ARM64 = fixed 4 bytes).
                // Hypervisor.framework does NOT auto-advance PC on data aborts.
                let pc = vcpu.get_reg(reg::PC).unwrap_or(0);
                let _ = vcpu.set_reg(reg::PC, pc + 4);
            }

            VcpuExit::Exception {
                class: ExceptionClass::WaitForInterrupt,
                ..
            } => {
                // Guest executed WFI — it is idle and waiting for an interrupt.
                // Before parking, poll vsock host fds for incoming data.
                // If data arrives, inject into RX queue and trigger interrupt
                // so the guest wakes up to process it.
                if device_manager.poll_vsock_rx() {
                    // Find vsock device IRQ and trigger it.
                    for dev in device_manager.iter() {
                        if dev.device_type == crate::device::DeviceType::VirtioVsock {
                            if let Some(irq) = dev.irq {
                                // Set interrupt status on MMIO state.
                                if let Some(state) = device_manager.get_mmio_state(dev.id) {
                                    if let Ok(mut s) = state.write() {
                                        s.trigger_interrupt(1); // INT_VRING
                                    }
                                }
                                // Fire GIC SPI via IRQ callback.
                                device_manager.trigger_irq_callback(irq, true);
                            }
                            break;
                        }
                    }
                    // Don't park — re-enter run loop immediately.
                    continue;
                }
                // No vsock data — park with timeout.
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }

            VcpuExit::Exception {
                class: ExceptionClass::HypercallHvc(imm),
                ..
            } => {
                tracing::debug!("vCPU {vcpu_id}: HVC #{imm}");
                // PSCI calls (power state coordination) come through HVC.
                let func_id = match vcpu.get_reg(reg::X0) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                handle_psci(vcpu_id, func_id, &vcpu, &running, cpu_on_senders.as_ref());
                if !running.load(Ordering::Relaxed) {
                    break;
                }
            }

            VcpuExit::Exception {
                class: ExceptionClass::SmcCall(_),
                ..
            } => {
                // Some guests route PSCI through SMC instead of HVC.
                let func_id = match vcpu.get_reg(reg::X0) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                handle_psci(vcpu_id, func_id, &vcpu, &running, cpu_on_senders.as_ref());
                if !running.load(Ordering::Relaxed) {
                    break;
                }
            }

            VcpuExit::VtimerActivated => {
                // Virtual timer fired. Unmask it so the guest sees the interrupt.
                let _ = vcpu.set_vtimer_mask(false);
            }

            VcpuExit::Canceled => {
                tracing::info!("vCPU {vcpu_id}: canceled");
                break;
            }

            VcpuExit::Exception {
                class: ref other, ..
            } => {
                tracing::warn!("vCPU {vcpu_id}: unhandled exception: {other:?}");
            }

            VcpuExit::Unknown(reason) => {
                tracing::warn!("vCPU {vcpu_id}: unknown exit reason {reason}");
            }
        }
    }

    // Flush any remaining UART output.
    pl011.lock().unwrap().flush();

    tracing::info!("vCPU {vcpu_id}: exited");
}

/// Handles PSCI function calls from a vCPU (HVC/SMC conduit).
///
/// Reads registers X1–X3 as needed and writes the return value into X0.
/// For SYSTEM_OFF / SYSTEM_RESET, sets `running` to `false` so the caller
/// can break out of its run loop.
fn handle_psci(
    vcpu_id: u32,
    func_id: u64,
    vcpu: &arcbox_hv::HvVcpu,
    running: &Arc<AtomicBool>,
    cpu_on_senders: Option<&CpuOnSenders>,
) {
    use std::sync::atomic::Ordering;

    match func_id {
        PSCI_VERSION => {
            // Return PSCI v1.0 (major=1, minor=0).
            let _ = vcpu.set_reg(reg::X0, 1 << 16);
        }

        PSCI_SYSTEM_OFF => {
            tracing::info!("vCPU {vcpu_id}: PSCI SYSTEM_OFF");
            running.store(false, Ordering::SeqCst);
        }

        PSCI_SYSTEM_RESET => {
            tracing::info!("vCPU {vcpu_id}: PSCI SYSTEM_RESET");
            running.store(false, Ordering::SeqCst);
        }

        PSCI_CPU_ON_64 => {
            let target_mpidr = vcpu.get_reg(reg::X1).unwrap_or(0);
            let entry_point = vcpu.get_reg(reg::X2).unwrap_or(0);
            let context_id = vcpu.get_reg(reg::X3).unwrap_or(0);

            // Extract CPU index from MPIDR Aff0 field (simple linear topology).
            let target_cpu = (target_mpidr & 0xFF) as usize;

            if let Some(senders) = cpu_on_senders {
                let mut senders_guard = senders
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);

                // Take the sender so it can only be used once (CPU_ON is
                // idempotent in the PSCI spec — a second call for the same
                // target returns ALREADY_ON).
                if let Some(sender) = senders_guard.get_mut(target_cpu).and_then(|s| s.take()) {
                    match sender.send(CpuOnRequest {
                        _target_cpu: target_mpidr,
                        entry_point,
                        context_id,
                    }) {
                        Ok(()) => {
                            tracing::info!(
                                "vCPU {vcpu_id}: PSCI CPU_ON target={target_cpu} \
                                 entry={entry_point:#x} ctx={context_id:#x}"
                            );
                            let _ = vcpu.set_reg(reg::X0, PSCI_SUCCESS);
                        }
                        Err(_) => {
                            // Receiver gone — secondary thread exited before
                            // we could send. Treat as ALREADY_ON.
                            tracing::warn!(
                                "vCPU {vcpu_id}: PSCI CPU_ON target={target_cpu} \
                                 channel closed"
                            );
                            let _ = vcpu.set_reg(reg::X0, PSCI_ALREADY_ON);
                        }
                    }
                } else {
                    // No sender for this CPU — either already started or
                    // invalid target.
                    tracing::debug!("vCPU {vcpu_id}: PSCI CPU_ON target={target_cpu} already on");
                    let _ = vcpu.set_reg(reg::X0, PSCI_ALREADY_ON);
                }
            } else {
                // Single-vCPU VM — CPU_ON is not supported.
                tracing::debug!("vCPU {vcpu_id}: PSCI CPU_ON ignored (single-vCPU VM)");
                let _ = vcpu.set_reg(reg::X0, u64::MAX); // NOT_SUPPORTED
            }
        }

        _ => {
            tracing::debug!("vCPU {vcpu_id}: unhandled PSCI func {func_id:#x}");
            // Return NOT_SUPPORTED (-1) in X0.
            let _ = vcpu.set_reg(reg::X0, u64::MAX);
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy helpers — superseded by linux-loader + vm-fdt in initialize_darwin_hv().
// Retained only for unit tests below.
// ---------------------------------------------------------------------------

#[cfg(test)]
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
        fdt_config.initrd_addr = Some(RAM_BASE_IPA + arm64::INITRD_LOAD_ADDR);
        fdt_config.initrd_size = Some(size);
    }

    Ok(fdt_config)
}

#[cfg(test)]
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

/// Loads the kernel Image into guest RAM at the ARM64 load address.
/// Superseded by linux-loader PE::load(); retained for tests.
#[cfg(test)]
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
/// Superseded by vm-memory write_slice(); retained for tests.
#[cfg(test)]
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

    #[test]
    fn test_pl011_contains() {
        let uart = Pl011::new();
        assert!(uart.contains(PL011_BASE));
        assert!(uart.contains(PL011_BASE + PL011_DR));
        assert!(uart.contains(PL011_BASE + PL011_SIZE - 1));
        assert!(!uart.contains(PL011_BASE + PL011_SIZE));
        assert!(!uart.contains(VIRTIO_MMIO_BASE));
    }

    #[test]
    fn test_pl011_write_and_flush() {
        let mut uart = Pl011::new();
        // Write "Hi\n" byte by byte.
        uart.write(PL011_BASE + PL011_DR, 1, b'H' as u64);
        uart.write(PL011_BASE + PL011_DR, 1, b'i' as u64);
        assert_eq!(uart.output.len(), 2);
        // Newline flushes the buffer.
        uart.write(PL011_BASE + PL011_DR, 1, b'\n' as u64);
        assert!(uart.output.is_empty());
    }

    #[test]
    fn test_pl011_read_flags() {
        let uart = Pl011::new();
        // Flag register should always return 0 (TX FIFO not full).
        assert_eq!(uart.read(PL011_BASE + PL011_FR, 4), 0);
    }

    #[test]
    fn test_pl011_flush_partial() {
        let mut uart = Pl011::new();
        uart.write(PL011_BASE + PL011_DR, 1, b'X' as u64);
        assert_eq!(uart.output.len(), 1);
        uart.flush();
        assert!(uart.output.is_empty());
    }

    #[test]
    fn test_mmio_regions_do_not_overlap() {
        // GIC redistributor ends at 0x080A_0000 + 0x200_0000 = 0x0A0A_0000.
        // VirtIO MMIO starts at VIRTIO_MMIO_BASE (0x0A00_0000) — may overlap
        // with GICR tail, but HV.framework handles GIC internally.
        // PL011 is at 0x0B00_0000, after both GIC and VirtIO regions.
        let gicr_end = GIC_REDIST_ADDR + GIC_REDIST_SIZE;
        assert!(PL011_BASE >= gicr_end, "PL011 must be outside GIC region");
        assert!(
            PL011_BASE + PL011_SIZE <= RAM_BASE_IPA,
            "PL011 must be below guest RAM"
        );
        // PL011 and VirtIO MMIO must not overlap.
        let pl011_range = PL011_BASE..PL011_BASE + PL011_SIZE;
        let virtio_start = VIRTIO_MMIO_BASE;
        let virtio_end = VIRTIO_MMIO_BASE + VIRTIO_MMIO_MAX_DEVICES * 0x1000;
        assert!(
            !pl011_range.contains(&virtio_start) && PL011_BASE >= virtio_end
                || PL011_BASE + PL011_SIZE <= virtio_start,
            "PL011 and VirtIO MMIO regions overlap"
        );
    }
}
