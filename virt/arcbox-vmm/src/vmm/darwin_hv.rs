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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};

use arcbox_hv::{HvVm, MemoryPermission};

use crate::boot::arm64;
use crate::device::{DeviceTreeEntry, DeviceType};
use crate::error::{Result, VmmError};
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
#[allow(dead_code)] // Fields read by secondary vCPU threads / future diagnostics
pub struct CpuOnRequest {
    /// Target MPIDR (CPU affinity identifier).
    pub target_cpu: u64,
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
const PAGE_SIZE: usize = 4096;

/// Base address for VirtIO MMIO device region.
/// Starts at 0x0A00_0000 to avoid conflict with PL011 UART at 0x0900_0000.
const VIRTIO_MMIO_BASE: u64 = 0x0A00_0000;

/// Size of each VirtIO MMIO device region.
const VIRTIO_MMIO_SIZE: u64 = 0x200;

/// Maximum number of VirtIO MMIO devices.
const VIRTIO_MMIO_MAX_DEVICES: u64 = 32;

/// First SPI interrupt number for VirtIO devices (GIC SPI numbering).
/// Retained for unit tests; the live path now uses IrqChip::allocate_irq().
#[allow(dead_code)]
const VIRTIO_IRQ_BASE: u32 = 48;

/// Guest RAM is mapped starting at IPA 0.
/// Guest RAM is mapped at 1 GiB to leave the lower address space for
/// GIC (0x0800_0000), PL011 (0x0900_0000) and VirtIO MMIO (0x0A00_0000).
const RAM_BASE_IPA: u64 = 0x4000_0000;

/// Type alias for `GuestRam` used by the parent `Vmm` struct to hold the
/// guest RAM allocation (HV backend).
pub(super) type HvGuestRam = GuestRam;

/// Holds a page-aligned host allocation that backs guest RAM.
pub(super) struct GuestRam {
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

/// PL011 MMIO base address (matches the FDT uart@9000000 node).
const PL011_BASE: u64 = 0x0900_0000;
/// PL011 MMIO region size.
const PL011_SIZE: u64 = 0x1000;

// PL011 register offsets.
const PL011_DR: u64 = 0x000; // Data Register
const PL011_FR: u64 = 0x018; // Flag Register
#[allow(dead_code)]
const PL011_IBRD: u64 = 0x024; // Integer Baud Rate
#[allow(dead_code)]
const PL011_FBRD: u64 = 0x028; // Fractional Baud Rate
#[allow(dead_code)]
const PL011_LCR_H: u64 = 0x02C; // Line Control
#[allow(dead_code)]
const PL011_CR: u64 = 0x030; // Control Register
#[allow(dead_code)]
const PL011_IMSC: u64 = 0x038; // Interrupt Mask

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
///
/// Retained for unit tests; the live path uses DeviceManager registration.
#[allow(dead_code)]
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
///
/// Retained for unit tests; the live path uses `device_manager.device_tree_entries()`.
#[allow(dead_code)]
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
///
/// Retained for unit tests; the live path uses `memory_manager.allocate_mmio()`.
#[allow(dead_code)]
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
        // SAFETY: guest_ram will be leaked below (std::mem::forget) so the
        // pointer remains valid for the lifetime of the DeviceManager.
        unsafe {
            device_manager.set_guest_memory(guest_ram.as_ptr(), ram_size);
        }

        // Wire IRQ callback so device completions trigger GIC interrupts.
        {
            let irq_chip_clone = Arc::clone(&irq_chip);
            let callback: crate::device::DeviceIrqCallback = Arc::new(move |irq, level| {
                irq_chip_clone
                    .trigger_irq(irq)
                    .map_err(|e| VmmError::Irq(format!("trigger_irq({irq}, {level}): {e}")))
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

        // VirtioFS shared directories
        for dir in &self.config.shared_dirs {
            let fs_config = arcbox_virtio::fs::FsConfig {
                tag: dir.tag.clone(),
                num_queues: 1,
                queue_size: 1024,
                shared_dir: dir.host_path.to_string_lossy().into_owned(),
            };
            let fs_dev = arcbox_virtio::fs::VirtioFs::new(fs_config);
            let name = format!("virtiofs-{}", dir.tag);
            device_manager.register_virtio_device(
                DeviceType::VirtioFs,
                name,
                fs_dev,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        // Block devices
        for block_dev in &self.config.block_devices {
            let blk =
                arcbox_virtio::blk::VirtioBlock::from_path(&block_dev.path, block_dev.read_only)
                    .map_err(|e| VmmError::Device(format!("block device: {e}")))?;
            let name = format!("virtio-blk-{}", block_dev.path.display());
            device_manager.register_virtio_device(
                DeviceType::VirtioBlock,
                name,
                blk,
                &mut memory_manager,
                &irq_chip,
            )?;
        }

        // Network (TSO-enabled)
        if self.config.networking {
            let net_config = arcbox_virtio::net::NetConfig {
                mac: arcbox_virtio::net::NetConfig::random_mac(),
                ..Default::default()
            };
            let mut net_dev = arcbox_virtio::net::VirtioNet::new(net_config);
            // Enable TSO for the custom VMM path — this is the whole point of
            // using Hypervisor.framework instead of VZ framework.
            net_dev.enable_tso_features();
            device_manager.register_virtio_device(
                DeviceType::VirtioNet,
                "virtio-net",
                net_dev,
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

        // --- 8. Generate FDT ---
        let fdt_entries = device_manager.device_tree_entries();
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

        // --- 10. Store managers ---
        let event_loop = crate::event::EventLoop::new()?;

        // Store managers (VM and GuestRam ownership will be moved to vCPU
        // threads in the start path — for now, store enough state to start).
        self.memory_manager = Some(memory_manager);
        self.device_manager = Some(device_manager);
        self.irq_chip = Some(irq_chip);
        self.event_loop = Some(event_loop);

        // Store HV-specific state in the Vmm struct for lifecycle management.
        self.hv_vm = Some(vm);
        self.hv_guest_ram = Some(Box::new(guest_ram));
        #[cfg(feature = "gic")]
        {
            self.hv_gic = gic;
        }
        self.hv_kernel_entry = Some(RAM_BASE_IPA + arm64::KERNEL_LOAD_ADDR);
        // fdt_addr is a RAM-internal offset; convert to GPA for vCPU register.
        self.hv_fdt_addr = Some(RAM_BASE_IPA + fdt_addr);
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

        let device_manager = Arc::new(
            self.device_manager
                .take()
                .ok_or_else(|| VmmError::config("device manager not initialized".to_string()))?,
        );
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

        // Cleanup in correct order: GIC → VM → RAM.
        #[cfg(feature = "gic")]
        {
            self.hv_gic.take();
        }
        self.hv_vm.take();
        self.hv_guest_ram.take();

        tracing::info!("Custom VMM stopped");
        Ok(())
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
    //   CPSR = EL1h, DAIF masked
    if let Err(e) = vcpu.set_reg(reg::PC, entry_addr) {
        tracing::error!("vCPU {vcpu_id}: set PC failed: {e}");
        return;
    }
    if let Err(e) = vcpu.set_reg(reg::X0, x0_value) {
        tracing::error!("vCPU {vcpu_id}: set X0 failed: {e}");
        return;
    }
    if let Err(e) = vcpu.set_reg(reg::CPSR, CPSR_EL1H) {
        tracing::error!("vCPU {vcpu_id}: set CPSR failed: {e}");
        return;
    }

    tracing::info!(
        "vCPU {vcpu_id}: starting at PC={:#x}, X0={:#x}",
        entry_addr,
        x0_value
    );

    loop {
        if !running.load(Ordering::Relaxed) {
            tracing::info!("vCPU {vcpu_id}: shutdown requested");
            break;
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
                            let value = match vcpu.get_reg(u32::from(mmio.register)) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::error!(
                                        "vCPU {vcpu_id}: get_reg(X{}) failed: {e}",
                                        mmio.register
                                    );
                                    0
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
                        let value = match vcpu.get_reg(u32::from(mmio.register)) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::error!(
                                    "vCPU {vcpu_id}: get_reg(X{}) failed: {e}",
                                    mmio.register
                                );
                                // Advance PC even on register read failure.
                                let pc = vcpu.get_reg(reg::PC).unwrap_or(0);
                                let _ = vcpu.set_reg(reg::PC, pc + 4);
                                continue;
                            }
                        };
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
                // Park the thread with a timeout instead of busy-spinning.
                // The IRQ callback (GIC set_spi) calls unpark() on all vCPU
                // threads, so we wake promptly when an interrupt fires.
                // The 1ms timeout prevents missed-wakeup deadlocks while still
                // saving significant CPU compared to yield_now().
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
                        target_cpu: target_mpidr,
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
        fdt_config.initrd_addr = Some(RAM_BASE_IPA + arm64::INITRD_LOAD_ADDR);
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
    fn test_virtio_mmio_base_does_not_overlap_pl011() {
        // PL011 occupies 0x0900_0000..0x0900_1000.
        // VirtIO MMIO starts at 0x0A00_0000.
        assert!(VIRTIO_MMIO_BASE >= PL011_BASE + PL011_SIZE);
    }
}
