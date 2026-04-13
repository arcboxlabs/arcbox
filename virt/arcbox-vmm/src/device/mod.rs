//! Device registry and MMIO dispatch for the custom VMM.
//!
//! `DeviceManager` owns a registry of `RegisteredDevice` entries (one per
//! virtio-mmio device) and routes guest MMIO accesses to the right
//! `VirtioDevice` implementation. Per-device hot paths (TX descriptor
//! drain, RX injection, vsock connection state) live on the device
//! structs in `arcbox-virtio` itself; the manager only handles the
//! transport-level dispatch and a few typed shortcuts (`primary_net`,
//! `bridge_net`, `vsock`) for VMM-driven setup and bookkeeping.

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex, RwLock};

#[cfg(test)]
use arcbox_virtio::net::VirtioNetHeader;
use arcbox_virtio::{DeviceStatus, QueueConfig, VirtioDevice};

use crate::error::{Result, VmmError};
use crate::irq::{Irq, IrqChip};
use crate::memory::MemoryManager;

mod checksum;
mod mmio_state;

use checksum::finalize_virtio_net_checksum;
pub use mmio_state::{MmioDevice, VirtioMmioState, virtio_mmio};

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
    /// DeviceId of the primary VirtioNet (NIC1) for targeted IRQ delivery
    /// and worker-spawn dispatch. Required because
    /// `raise_interrupt_for(DeviceType::VirtioNet)` uses HashMap iteration
    /// which is non-deterministic — with two VirtioNet devices it could
    /// match the bridge NIC instead of the primary NIC.
    primary_net_device_id: Option<DeviceId>,
    /// Typed handle to the primary VirtioNet (NIC1 — NAT datapath).
    /// Shares the same `Arc<Mutex<_>>` as the generic entry in `devices`;
    /// exposes the concrete device so QUEUE_NOTIFY dispatch can call
    /// inherent hot-path methods without dyn dispatch. Host fd and TX
    /// avail-index cursor live on the device via `NetPort`.
    primary_net: Option<Arc<Mutex<arcbox_virtio::net::VirtioNet>>>,
    /// DeviceId of the bridge VirtioNet so QUEUE_NOTIFY can dispatch correctly
    /// to `bridge_net` without a HashMap lookup.
    bridge_net_device_id: Option<DeviceId>,
    /// Typed handle to the bridge VirtioNet (NIC2 — vmnet bridge). Shares
    /// the same `Arc<Mutex<_>>` as the generic entry in `devices`; exposes
    /// the concrete device so DeviceManager can call inherent hot-path
    /// methods (`handle_tx`, `poll_rx`) without dyn dispatch. Host fd and
    /// TX avail-index cursor live on the device itself via `NetPort`.
    bridge_net: Option<Arc<Mutex<arcbox_virtio::net::VirtioNet>>>,
    /// Host-side vsock connection manager (HV backend only). Same `Arc`
    /// is also bound onto the `vsock` typed shortcut's device via
    /// `bind_connections`, so the device's `process_queue` can read it
    /// directly without `QueueConfig` plumbing.
    vsock_connections:
        std::sync::Arc<std::sync::Mutex<crate::vsock_manager::VsockConnectionManager>>,
    /// Typed handle to the VirtioVsock device. Shares the same `Arc`
    /// stored in `devices`. Used by `set_vsock` to bind the device's
    /// `DeviceCtx` and connection manager at registration time.
    vsock: Option<Arc<Mutex<arcbox_virtio::vsock::VirtioVsock>>>,
    /// Per-block-device async I/O worker handles. When present, QUEUE_NOTIFY
    /// for block devices is dispatched to the worker instead of processing
    /// synchronously on the vCPU thread.
    blk_workers: HashMap<DeviceId, crate::blk_worker::BlkWorkerHandle>,
    /// Net-io worker thread handle. Spawned once at DRIVER_OK for the
    /// primary VirtioNet device. Joined on shutdown. Wrapped in Mutex
    /// because the spawn happens inside `handle_mmio_write(&self)`.
    net_rx_worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// IRQ callback for the net-io worker thread (cloned from irq_callback).
    net_rx_irq_callback: Option<DeviceIrqCallback>,
    /// Force-exit all vCPUs closure for the net-io worker thread.
    net_rx_exit_vcpus: Option<Arc<dyn Fn() + Send + Sync>>,
    /// VM-wide running flag shared with worker threads.
    running: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Primary NIC host fd, wrapped in Mutex so it can be taken from
    /// the `&self` DRIVER_OK handler. Mirrors `net_host_fd` ownership.
    net_host_fd_slot: Mutex<Option<i32>>,
    /// Crossbeam channel receiving frames from the datapath loop for
    /// direct guest memory injection. Set before DRIVER_OK; taken once
    /// to construct the `RxInjectThread`.
    rx_inject_channel: Mutex<Option<crossbeam_channel::Receiver<Vec<u8>>>>,
    /// Channel for promoted inline (vhost-style) TCP connections. The
    /// datapath sends `InlineConn` values; the inject thread receives
    /// them and reads directly from host sockets into guest buffers.
    inline_conn_channel:
        Mutex<Option<crossbeam_channel::Receiver<arcbox_net_inject::inline_conn::InlineConn>>>,
}

// SAFETY: `DeviceManager` contains several types that are not `Send`/`Sync`
// by default; we assert it manually because the actual access discipline is:
//
// * `guest_ram_base: *mut u8` — initialized once at VM start, then treated
//   as read-only from the struct's perspective. All mutation of the pointee
//   happens through slice views reconstructed per-call under either the
//   vCPU thread's exclusive lock or the per-device `mmio_state`/`virtio_dev`
//   lock. No pointer arithmetic escapes the struct.
//
// * `net_host_fd_slot: Mutex<Option<i32>>` — transfer-of-ownership slot
//   the DRIVER_OK handler uses to hand the primary NIC fd to the
//   net-io worker. Set-once from `set_net_host_fd`. The live TX-path fd
//   and TX cursor for each NIC live on its `VirtioNet::NetPort`, not on
//   DeviceManager.
//
// * `primary_net` / `bridge_net` / `vsock: Option<Arc<Mutex<...>>>` —
//   typed shortcuts; the same `Arc` is stored in the `devices` HashMap
//   via type erasure. Hot paths read OnceLock-guarded `NetPort` and
//   atomics directly — no mutex contention on the TX fast path.
//
// * `vsock_connections`, `blk_workers`, `net_rx_worker_handle`,
//   `rx_inject_channel`, `inline_conn_channel` — each uses its own
//   `Arc<Mutex<...>>` / `Mutex<...>` / crossbeam channel, providing
//   per-field thread safety.
//
// Cross-thread invariant: the raw `*mut u8` in `guest_ram_base` must never
// be used to produce two overlapping `&mut [u8]` slices live at the same
// time. This is upheld by construction — each caller builds a fresh slice
// under the appropriate lock, uses it briefly, then drops it.
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
            primary_net_device_id: None,
            primary_net: None,
            bridge_net_device_id: None,
            bridge_net: None,
            vsock_connections: std::sync::Arc::new(std::sync::Mutex::new(
                crate::vsock_manager::VsockConnectionManager::new(),
            )),
            vsock: None,
            blk_workers: HashMap::new(),
            net_rx_worker_handle: Mutex::new(None),
            net_rx_irq_callback: None,
            net_rx_exit_vcpus: None,
            running: None,
            net_host_fd_slot: Mutex::new(None),
            rx_inject_channel: Mutex::new(None),
            inline_conn_channel: Mutex::new(None),
        }
    }

    /// Provides guest physical memory access for queue processing.
    ///
    /// # Safety
    ///
    /// The caller must guarantee all of the following for the entire
    /// lifetime of this `DeviceManager`:
    ///
    /// * `base` is non-null and points to an allocation of at least `size`
    ///   bytes (the backing guest RAM mapping returned by the hypervisor).
    /// * The allocation is not unmapped, moved, or freed until after this
    ///   `DeviceManager` is dropped.
    /// * No other Rust reference produces a `&mut [u8]` over the same
    ///   region concurrently — internal code only constructs fresh slices
    ///   under device or vCPU locks.
    /// * `gpa_base` is the guest physical address where `base` is mapped;
    ///   descriptor GPAs are translated by subtracting `gpa_base`.
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
    ///
    /// The fd is (a) bound onto the primary `VirtioNet` itself via
    /// `NetPort` so the device's TX hot path can write to it directly,
    /// and (b) copied into `net_host_fd_slot` so the DRIVER_OK handler
    /// can still take ownership for the net-io worker thread.
    pub fn set_net_host_fd(&mut self, fd: std::os::unix::io::RawFd, device_id: DeviceId) {
        use arcbox_virtio::net::NetPort;
        self.primary_net_device_id = Some(device_id);
        *self
            .net_host_fd_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fd);

        if let Some(primary) = self.primary_net.as_ref() {
            let port = NetPort {
                host_fd: fd,
                last_avail_tx: std::sync::atomic::AtomicU16::new(0),
            };
            if let Ok(dev) = primary.lock() {
                if dev.bind_port(port).is_err() {
                    tracing::warn!("primary_net port already bound — ignoring rebind");
                }
            }
        } else {
            tracing::error!("set_net_host_fd called before set_primary_net");
        }
    }

    /// Registers a typed handle to the primary VirtioNet (NIC1) and binds
    /// its `DeviceCtx`. Must be called after `set_guest_memory` +
    /// `set_irq_callback` so both ingredients exist, and before
    /// `set_net_host_fd` so the fd binding can reach the concrete device.
    pub fn set_primary_net(
        &mut self,
        device_id: DeviceId,
        device: Arc<Mutex<arcbox_virtio::net::VirtioNet>>,
    ) {
        self.primary_net_device_id = Some(device_id);

        if let Some(ctx) = self.build_device_ctx(device_id) {
            if let Ok(mut dev) = device.lock() {
                dev.bind_ctx(ctx);
            }
        } else {
            tracing::warn!(
                "set_primary_net: DeviceCtx not built (guest_mem or irq_callback missing) — \
                 primary NIC hot paths will be no-ops"
            );
        }

        self.primary_net = Some(device);
    }

    /// Returns the primary NIC device ID (for targeted IRQ delivery).
    pub fn primary_net_device_id(&self) -> Option<DeviceId> {
        self.primary_net_device_id
    }

    /// Returns the typed handle to the primary VirtioNet if one was registered.
    pub fn primary_net(&self) -> Option<&Arc<Mutex<arcbox_virtio::net::VirtioNet>>> {
        self.primary_net.as_ref()
    }

    /// Registers a typed handle to the VirtioVsock device and binds its
    /// `DeviceCtx` plus the host-side connection manager. Must be called
    /// after `set_guest_memory` + `set_irq_callback`.
    pub fn set_vsock(
        &mut self,
        device_id: DeviceId,
        device: Arc<Mutex<arcbox_virtio::vsock::VirtioVsock>>,
    ) {
        if let Some(ctx) = self.build_device_ctx(device_id) {
            if let Ok(mut dev) = device.lock() {
                dev.bind_ctx(ctx);
                dev.bind_connections(self.vsock_connections.clone());
            }
        } else {
            tracing::warn!(
                "set_vsock: DeviceCtx not built (guest_mem or irq_callback missing) — \
                 vsock TX hot path will fall back to QueueConfig plumbing"
            );
        }
        self.vsock = Some(device);
    }

    /// Returns the typed handle to the VirtioVsock device if registered.
    pub fn vsock(&self) -> Option<&Arc<Mutex<arcbox_virtio::vsock::VirtioVsock>>> {
        self.vsock.as_ref()
    }

    /// Registers a typed handle to the bridge VirtioNet (NIC2) and binds
    /// its `DeviceCtx` (guest memory + IRQ trigger). Must be called after
    /// `set_guest_memory` + `set_irq_callback` so both ingredients exist,
    /// and before `set_bridge_host_fd` so the fd binding can reach the
    /// concrete device.
    pub fn set_bridge_net(
        &mut self,
        device_id: DeviceId,
        device: Arc<Mutex<arcbox_virtio::net::VirtioNet>>,
    ) {
        self.bridge_net_device_id = Some(device_id);

        if let Some(ctx) = self.build_device_ctx(device_id) {
            if let Ok(mut dev) = device.lock() {
                dev.bind_ctx(ctx);
            }
        } else {
            tracing::warn!(
                "set_bridge_net: DeviceCtx not built (guest_mem or irq_callback missing) — \
                 bridge hot paths will be no-ops"
            );
        }

        self.bridge_net = Some(device);
    }

    /// Constructs a `DeviceCtx` for a given device: a `GuestMemWriter`
    /// over guest RAM plus a `raise_irq` closure pre-bound to this
    /// device's GSI and MMIO state. Returns `None` if prerequisites are
    /// missing — caller decides whether to tolerate the absence.
    fn build_device_ctx(&self, device_id: DeviceId) -> Option<arcbox_virtio::DeviceCtx> {
        let ram_base = self.guest_ram_base?;
        if self.guest_ram_size == 0 {
            return None;
        }
        let device = self.devices.get(&device_id)?;
        let irq = device.info.irq?;
        let mmio_arc = device.mmio_state.as_ref()?.clone();
        let irq_callback = self.irq_callback.as_ref()?.clone();

        // SAFETY: `ram_base` is the host mapping returned by the platform
        // hypervisor and is valid for `guest_ram_size` bytes for the
        // lifetime of the DeviceManager (same contract as the other
        // GuestMemWriter constructions in this crate).
        let mem = unsafe {
            arcbox_virtio::GuestMemWriter::new(
                ram_base,
                self.guest_ram_size,
                self.guest_ram_gpa as usize,
            )
        };

        let raise_irq: Arc<dyn Fn(u32) + Send + Sync> = Arc::new(move |reason: u32| {
            if let Ok(mut s) = mmio_arc.write() {
                s.trigger_interrupt(reason);
            }
            let _ = irq_callback(irq, true);
        });

        Some(arcbox_virtio::DeviceCtx {
            mem: Arc::new(mem),
            raise_irq,
        })
    }

    /// Sets the bridge NIC host fd (NIC2 — vmnet bridge). The fd is stored
    /// on the bridge `VirtioNet` itself via `NetPort`; DeviceManager no
    /// longer owns it.
    pub fn set_bridge_host_fd(&mut self, fd: std::os::unix::io::RawFd, _device_id: DeviceId) {
        use arcbox_virtio::net::NetPort;
        let Some(bridge) = self.bridge_net.as_ref() else {
            tracing::error!("set_bridge_host_fd called before set_bridge_net");
            return;
        };
        let port = NetPort {
            host_fd: fd,
            last_avail_tx: std::sync::atomic::AtomicU16::new(0),
        };
        if let Ok(dev) = bridge.lock() {
            if dev.bind_port(port).is_err() {
                tracing::warn!("bridge_net port already bound — ignoring rebind");
            }
        }
    }

    /// Returns the typed handle to the bridge VirtioNet if one was registered.
    pub fn bridge_net(&self) -> Option<&Arc<Mutex<arcbox_virtio::net::VirtioNet>>> {
        self.bridge_net.as_ref()
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

    /// Stores the hooks that the net-io worker thread needs for interrupt
    /// injection and vCPU cancellation. Called once from `start_darwin_hv`
    /// before the `DeviceManager` Arc is shared.
    pub fn set_net_rx_hooks(
        &mut self,
        irq_callback: Arc<dyn Fn(crate::irq::Irq, bool) -> crate::error::Result<()> + Send + Sync>,
        exit_vcpus: Arc<dyn Fn() + Send + Sync>,
    ) {
        self.net_rx_irq_callback = Some(irq_callback);
        self.net_rx_exit_vcpus = Some(exit_vcpus);
    }

    /// Stores the VM-wide `running` flag so the DRIVER_OK handler can
    /// pass it to the net-io worker context.
    pub fn set_running(&mut self, running: Arc<std::sync::atomic::AtomicBool>) {
        self.running = Some(running);
    }

    /// Stores the RX inject channel so the DRIVER_OK handler can take it
    /// and spawn the `RxInjectThread`.
    pub fn set_rx_inject_channel(&mut self, rx: crossbeam_channel::Receiver<Vec<u8>>) {
        *self
            .rx_inject_channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
    }

    /// Stores the inline connection channel so the DRIVER_OK handler can
    /// pass it to the `RxInjectThread`.
    pub fn set_inline_conn_channel(
        &mut self,
        rx: crossbeam_channel::Receiver<arcbox_net_inject::inline_conn::InlineConn>,
    ) {
        *self
            .inline_conn_channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
    }

    /// Returns the primary NIC host fd (without removing it).
    /// The fd is shared: net-io thread reads, handle_net_tx writes.
    fn get_net_host_fd_slot(&self) -> Option<i32> {
        *self
            .net_host_fd_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the net-io worker thread handle for join on shutdown.
    pub fn take_net_rx_worker_handle(&self) -> Option<std::thread::JoinHandle<()>> {
        self.net_rx_worker_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Spawns the net-io worker thread if `device_id` is the primary VirtioNet
    /// and the worker has not already been spawned. Called from the DRIVER_OK
    /// handler (which only has `&self`).
    ///
    /// Prefers the `RxInjectThread` path (channel-based, no socketpair reads)
    /// when `rx_inject_channel` is set. Falls back to the legacy
    /// `net_rx_worker` (kqueue on socketpair fd) for VZ backend compatibility.
    fn maybe_spawn_net_rx_worker(
        &self,
        device_id: DeviceId,
        mmio_arc: &Arc<RwLock<VirtioMmioState>>,
    ) {
        // Only spawn for the primary VirtioNet device.
        if self.primary_net_device_id != Some(device_id) {
            return;
        }
        let device = match self.devices.get(&device_id) {
            Some(d) if d.info.device_type == DeviceType::VirtioNet => d,
            _ => return,
        };

        // Guard: only spawn once.
        {
            let guard = self
                .net_rx_worker_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_some() {
                return;
            }
        }

        let mmio = match mmio_arc.read() {
            Ok(m) => m,
            Err(_) => return,
        };

        let qi = 0; // RX queue index.
        let Some(guest_base) = self.guest_ram_base else {
            tracing::warn!("net-io: guest_ram_base not set");
            return;
        };
        let Some(irq_callback) = self.net_rx_irq_callback.clone() else {
            tracing::warn!("net-io: irq_callback not set");
            return;
        };
        let Some(exit_vcpus) = self.net_rx_exit_vcpus.clone() else {
            tracing::warn!("net-io: exit_vcpus not set");
            return;
        };
        let Some(irq) = device.info.irq else {
            tracing::warn!("net-io: device has no IRQ");
            return;
        };
        let Some(running) = self.running.clone() else {
            tracing::warn!("net-io: running flag not set");
            return;
        };

        // Try the new RxInjectThread path (channel-based, HV backend).
        let rx_channel = self
            .rx_inject_channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        if let Some(rx_channel) = rx_channel {
            // SAFETY: `guest_base` is the host mapping returned by
            // Virtualization.framework, valid for `guest_ram_size` bytes
            // for the lifetime of the VM.
            let guest_mem = unsafe {
                arcbox_net_inject::guest_mem::GuestMemWriter::new(
                    guest_base,
                    self.guest_ram_size,
                    self.guest_ram_gpa as usize,
                )
            };

            let queue = arcbox_net_inject::queue::RxQueueConfig {
                desc_gpa: mmio.queue_desc[qi],
                avail_gpa: mmio.queue_driver[qi],
                used_gpa: mmio.queue_device[qi],
                size: mmio.queue_num[qi],
            };
            drop(mmio);

            // Wrap the VMM IRQ callback to match the inject crate's type.
            let vmm_callback = irq_callback;
            let inject_callback: Arc<arcbox_net_inject::irq::IrqCallback> =
                Arc::new(move |gsi, level| {
                    vmm_callback(gsi, level)
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                });

            let mmio_arc_clone = mmio_arc.clone();
            let set_interrupt_status: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Ok(mut s) = mmio_arc_clone.write() {
                    s.trigger_interrupt(1); // INT_VRING
                }
            });

            // Take the inline connection channel if available; otherwise
            // create an unbounded channel with a dummy sender that is
            // immediately dropped (the receiver will never yield items).
            let conn_rx = self
                .inline_conn_channel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_else(|| {
                    let (_tx, rx) = crossbeam_channel::unbounded();
                    rx
                });

            let inject_thread = arcbox_net_inject::inject::RxInjectThread {
                rx: rx_channel,
                conn_rx,
                guest_mem,
                queue,
                irq: arcbox_net_inject::irq::IrqHandle {
                    callback: inject_callback,
                    exit_vcpus,
                    irq,
                },
                set_interrupt_status,
                running,
            };

            match std::thread::Builder::new()
                .name("rx-inject".to_string())
                .spawn(move || inject_thread.run())
            {
                Ok(handle) => {
                    *self
                        .net_rx_worker_handle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                    tracing::info!(
                        "Spawned rx-inject thread for primary VirtioNet (channel-based)"
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to spawn rx-inject thread: {e}");
                }
            }
            return;
        }

        // Fallback: legacy net_rx_worker (kqueue on socketpair fd).
        let Some(net_fd) = self.get_net_host_fd_slot() else {
            tracing::warn!("net-io: no host fd available for primary VirtioNet");
            return;
        };

        let rx_queue = crate::net_rx_worker::RxQueueConfig {
            desc_gpa: mmio.queue_desc[qi],
            avail_gpa: mmio.queue_driver[qi],
            used_gpa: mmio.queue_device[qi],
            size: mmio.queue_num[qi],
        };
        drop(mmio);

        // SAFETY: `guest_base` is the host mapping returned by
        // Virtualization.framework, valid for `guest_ram_size` bytes
        // for the lifetime of the VM.
        let guest_mem = unsafe {
            crate::blk_worker::GuestMemWriter::new(
                guest_base,
                self.guest_ram_size,
                self.guest_ram_gpa as usize,
            )
        };

        let ctx = crate::net_rx_worker::NetRxWorkerContext {
            net_host_fd: net_fd,
            guest_mem,
            rx_queue,
            mmio_state: mmio_arc.clone(),
            irq_callback,
            irq,
            exit_vcpus,
            running,
        };

        match std::thread::Builder::new()
            .name("net-io".to_string())
            .spawn(move || crate::net_rx_worker::net_rx_worker_loop(ctx))
        {
            Ok(handle) => {
                *self
                    .net_rx_worker_handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                tracing::info!("Spawned net-io worker thread for primary VirtioNet (legacy)");
            }
            Err(e) => {
                tracing::error!("Failed to spawn net-io worker thread: {e}");
            }
        }
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
    ) -> Result<(DeviceId, Arc<Mutex<D>>)> {
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
        // Keep the concrete `Arc<Mutex<D>>` so the caller can hold a typed
        // handle (needed for hot-path shortcuts like `bridge_net` /
        // `primary_net` on DeviceManager). The trait-object form goes into
        // the generic HashMap used for MMIO dispatch.
        let virtio_device: Arc<Mutex<D>> = Arc::new(Mutex::new(device));
        let virtio_device_erased: Arc<Mutex<dyn VirtioDevice>> = virtio_device.clone();

        self.mmio_map.insert(mmio_base, id);
        self.devices.insert(
            id,
            RegisteredDevice {
                info,
                mmio_state: Some(mmio_state),
                virtio_device: Some(virtio_device_erased),
            },
        );

        tracing::info!(
            "Registered VirtIO device '{}' (type {:?}) at MMIO {:#x}, IRQ {}",
            name_str,
            device_type,
            mmio_base,
            irq
        );

        Ok((id, virtio_device))
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

                        // Spawn the net-io worker for the primary VirtioNet device.
                        self.maybe_spawn_net_rx_worker(device_id, state);
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
                                }
                            } else {
                                QueueConfig::default()
                            }
                        };

                        if let (Some(ram_base), ram_size) =
                            (self.guest_ram_base, self.guest_ram_size)
                        {
                            // Build a guest memory slice covering the guest RAM region.
                            // The host pointer `ram_base` maps to GPA `guest_ram_gpa`.
                            // All GPA-based indices must subtract `gpa_base` to obtain
                            // the correct offset within this slice.
                            //
                            // SAFETY: `ram_base` is the host mapping returned by
                            // Virtualization.framework and is valid for `ram_size` bytes.
                            let gpa_base = self.guest_ram_gpa as usize;
                            let guest_mem =
                                unsafe { std::slice::from_raw_parts_mut(ram_base, ram_size) };

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
                            // This bypasses the generic process_queue — the
                            // concrete `VirtioNet` owns its fd + TX cursor via
                            // `NetPort` and implements the hot path itself.
                            else if device.info.device_type == DeviceType::VirtioNet
                                && queue_idx == 1
                                && (self.primary_net.is_some() || self.bridge_net.is_some())
                            {
                                let is_bridge = self
                                    .bridge_net_device_id
                                    .is_some_and(|bid| bid == device_id);
                                let typed = if is_bridge {
                                    self.bridge_net.as_ref()
                                } else {
                                    self.primary_net.as_ref()
                                };
                                let net_completions = match typed {
                                    Some(arc) => arc
                                        .lock()
                                        .map(|d| {
                                            d.drain_tx_queue(&qcfg, finalize_virtio_net_checksum)
                                        })
                                        .unwrap_or_default(),
                                    None => Vec::new(),
                                };
                                let _ = guest_mem; // unused on this branch now

                                if !net_completions.is_empty() {
                                    // Update used ring for completed TX descriptors.
                                    // Translate GPAs to slice offsets (checked).
                                    let Some(used_off) =
                                        (qcfg.used_addr as usize).checked_sub(gpa_base)
                                    else {
                                        tracing::warn!(
                                            "invalid used GPA {:#x} below ram base {:#x}",
                                            qcfg.used_addr,
                                            gpa_base
                                        );
                                        return Ok(());
                                    };
                                    let q_size = qcfg.size as usize;
                                    let used_idx_off = used_off + 2;
                                    let mut used_idx = u16::from_le_bytes([
                                        guest_mem[used_idx_off],
                                        guest_mem[used_idx_off + 1],
                                    ]);
                                    for &(head, len) in &net_completions {
                                        let entry =
                                            used_off + 4 + ((used_idx as usize) % q_size) * 8;
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
                                    if let Some(avail_off) =
                                        (qcfg.avail_addr as usize).checked_sub(gpa_base)
                                    {
                                        let avail_idx = u16::from_le_bytes([
                                            guest_mem[avail_off + 2],
                                            guest_mem[avail_off + 3],
                                        ]);
                                        let avail_event_off = used_off + 4 + q_size * 8;
                                        if avail_event_off + 2 <= guest_mem.len() {
                                            guest_mem[avail_event_off..avail_event_off + 2]
                                                .copy_from_slice(&avail_idx.to_le_bytes());
                                        }
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

    /// Injects a raw packet into the vsock RX queue (queue 0).
    /// Writes a packet into the next available RX descriptor in guest memory.
    ///
    /// Pops one descriptor from the avail ring, walks the chain writing data,
    /// and updates the used ring. Returns number of bytes written (0 on failure).
    ///
    /// `desc_addr`, `avail_addr`, `used_addr` are already translated to slice
    /// offsets (GPA minus `gpa_base`). `gpa_base` is needed to translate
    /// descriptor buffer addresses which are raw GPAs in guest memory.
    #[allow(clippy::too_many_arguments)]
    fn write_to_rx_descriptor(
        &self,
        guest_mem: &mut [u8],
        desc_addr: usize,
        avail_addr: usize,
        used_addr: usize,
        q_size: usize,
        gpa_base: usize,
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
            let addr_gpa =
                u64::from_le_bytes(guest_mem[d_off..d_off + 8].try_into().unwrap()) as usize;
            let len =
                u32::from_le_bytes(guest_mem[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(guest_mem[d_off + 12..d_off + 14].try_into().unwrap());
            let next = u16::from_le_bytes(guest_mem[d_off + 14..d_off + 16].try_into().unwrap());
            let Some(addr) = addr_gpa.checked_sub(gpa_base) else {
                continue;
            };

            desc_count += 1;
            tracing::trace!(
                "  desc[{}]: addr={:#x} len={} flags={:#06x} (W={} N={}) next={}",
                idx,
                addr_gpa,
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
                        addr_gpa,
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
                let first_gpa =
                    u64::from_le_bytes(guest_mem[first_d_off..first_d_off + 8].try_into().unwrap())
                        as usize;
                if let Some(first_off) = first_gpa.checked_sub(gpa_base) {
                    if first_off + 44 <= guest_mem.len() {
                        let readback = &guest_mem[first_off..first_off + 44];
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
                    // Translate TX queue GPAs to slice offsets (checked).
                    if let (Some(tx_avail_off), Some(tx_used_off)) = (
                        (mmio.queue_driver[txi] as usize).checked_sub(gpa_base),
                        (mmio.queue_device[txi] as usize).checked_sub(gpa_base),
                    ) {
                        if tx_avail_off + 4 <= guest_mem.len() && tx_used_off + 4 <= guest_mem.len()
                        {
                            let tx_avail = u16::from_le_bytes([
                                guest_mem[tx_avail_off + 2],
                                guest_mem[tx_avail_off + 3],
                            ]);
                            let tx_used = u16::from_le_bytes([
                                guest_mem[tx_used_off + 2],
                                guest_mem[tx_used_off + 3],
                            ]);
                            tracing::trace!(
                                "  TX queue state: avail_idx={} used_idx={} (delta={})",
                                tx_avail,
                                tx_used,
                                tx_avail.wrapping_sub(tx_used),
                            );
                        }
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

        // Translate GPAs to slice offsets (checked against ram base).
        let gpa_base = qcfg.gpa_base as usize;
        let Some(desc_addr) = (qcfg.desc_addr as usize).checked_sub(gpa_base) else {
            tracing::warn!(
                "dispatch_blk_async: desc GPA {:#x} below ram base {:#x}",
                qcfg.desc_addr,
                gpa_base
            );
            return Ok(false);
        };
        let Some(avail_addr) = (qcfg.avail_addr as usize).checked_sub(gpa_base) else {
            tracing::warn!(
                "dispatch_blk_async: avail GPA {:#x} below ram base {:#x}",
                qcfg.avail_addr,
                gpa_base
            );
            return Ok(false);
        };
        let Some(used_addr) = (qcfg.used_addr as usize).checked_sub(gpa_base) else {
            tracing::warn!(
                "dispatch_blk_async: used GPA {:#x} below ram base {:#x}",
                qcfg.used_addr,
                gpa_base
            );
            return Ok(false);
        };
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
                    // Translate GPA to slice offset for direct memory access.
                    first_desc = false;
                    if len >= 16 {
                        let Some(hdr_off) = (addr as usize).checked_sub(gpa_base) else {
                            break;
                        };
                        if hdr_off + 16 <= memory.len() {
                            let req_type = u32::from_le_bytes(
                                memory[hdr_off..hdr_off + 4].try_into().unwrap(),
                            );
                            sector = u64::from_le_bytes(
                                memory[hdr_off + 8..hdr_off + 16].try_into().unwrap(),
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

    /// Polls the bridge (vmnet) host fd for inbound frames and injects
    /// them into the bridge VirtioNet RX queue. Thin shim that reads the
    /// device's current MMIO-transport queue configuration, hands it to
    /// `VirtioNet::poll_rx`, and returns whether any frame was injected.
    /// Caller fires the used-ring interrupt on `true`.
    pub fn poll_bridge_rx(&self) -> bool {
        let Some(bridge_arc) = self.bridge_net.as_ref() else {
            return false;
        };
        let Some(bridge_id) = self.bridge_net_device_id else {
            return false;
        };
        let Some(device) = self.devices.get(&bridge_id) else {
            return false;
        };
        let Some(mmio_arc) = device.mmio_state.as_ref() else {
            return false;
        };

        // Build a snapshot of the RX queue (idx 0) from MMIO state.
        let rx_qcfg = {
            let Ok(mmio) = mmio_arc.read() else {
                return false;
            };
            let qi = 0usize;
            if !mmio.queue_ready[qi] || mmio.queue_num[qi] == 0 {
                return false;
            }
            QueueConfig {
                desc_addr: mmio.queue_desc[qi],
                avail_addr: mmio.queue_driver[qi],
                used_addr: mmio.queue_device[qi],
                size: mmio.queue_num[qi],
                ready: true,
                gpa_base: self.guest_ram_gpa,
            }
        };

        let Ok(dev) = bridge_arc.lock() else {
            return false;
        };
        dev.poll_rx(&rx_qcfg)
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
        // SAFETY: `ram_base` is the host mapping returned by
        // Virtualization.framework and is valid for `ram_size` bytes.
        let guest_mem = unsafe { std::slice::from_raw_parts_mut(ram_base, ram_size) };

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
                // SAFETY: `*fd` is owned by VsockConnectionManager and kept
                // live for the duration of this peek. `peek_buf` is a valid
                // mutable slice of 1 byte. MSG_DONTWAIT ensures non-blocking.
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
                    tracing::debug!(
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

        // Translate GPAs to slice offsets (checked against ram base).
        let Some(desc_addr) = (mmio.queue_desc[rxi] as usize).checked_sub(gpa_base) else {
            return false;
        };
        let Some(avail_addr) = (mmio.queue_driver[rxi] as usize).checked_sub(gpa_base) else {
            return false;
        };
        let Some(used_addr) = (mmio.queue_device[rxi] as usize).checked_sub(gpa_base) else {
            return false;
        };
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
                            tracing::debug!(
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
                            tracing::debug!(
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
                                    // SAFETY: `fd` is borrowed from `conn.internal_fd`
                                    // which remains live through `conn`. `buf` is a
                                    // valid mutable allocation of `max_read` bytes.
                                    // The fd is non-blocking so read returns promptly.
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
                guest_mem, desc_addr, avail_addr, used_addr, q_size, gpa_base, &packet,
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
    fn assert_sync<T: Sync>() {}
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
