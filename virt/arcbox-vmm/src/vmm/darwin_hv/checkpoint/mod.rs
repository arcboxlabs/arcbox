//! Checkpoint and restore of a running HV machine (spike).
//!
//! A checkpoint is taken in place: every powered-on vCPU stops at the top
//! of its run loop, captures its own registers and stays parked with its
//! `HvVcpu` alive (the GIC state can only be read while every vCPU exists
//! and none is inside `hv_vcpu_run`). Then the I/O workers stop, and the
//! GIC, the virtio transport state, PSCI power states, the PL031, guest RAM
//! and an APFS clone of every disk are written to the checkpoint directory.
//! The machine is left stopped in place; the caller stops it.
//!
//! A restore builds the same machine in a fresh process (Hypervisor.framework
//! allows one VM per process), maps the RAM image copy-on-write, writes the
//! device state back, creates every vCPU in id order and resumes them where
//! they stopped. The caller provides the disks, cloned from the checkpoint.
//! Host-side connections do not survive: the guest gets a vsock
//! TRANSPORT_RESET and an RST for every TCP flow the checkpoint aborted, and
//! later segments of flows the new datapath never saw are answered with RST.
//!
//! Method and results: `docs/experiments/2026-10-31-hv-checkpoint-linux-spike.md`
//! and `docs/experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arcbox_hv::HvVcpu;
use arcbox_hv::snapshot::VcpuState;
use serde::{Deserialize, Serialize};

use super::Pl031;

mod device_state;
mod restore;
mod save;

pub(in crate::vmm) use restore::Restore;

use device_state::DeviceState;

/// `DRIVER_OK` in the virtio device status.
const DRIVER_OK: u8 = 4;
/// How long the vCPUs get to stop, and to restore.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The checkpoint request the vCPU threads poll, and the states they leave.
pub(in crate::vmm) struct CheckpointCtl {
    requested: AtomicBool,
    /// Index = vCPU id; set by a powered-on vCPU once it has stopped.
    states: Mutex<Vec<Option<std::result::Result<VcpuState, String>>>>,
}

impl CheckpointCtl {
    pub(in crate::vmm) fn new(vcpus: usize) -> Arc<Self> {
        Arc::new(Self {
            requested: AtomicBool::new(false),
            states: Mutex::new(vec![None; vcpus]),
        })
    }

    pub(in crate::vmm) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Runs on vCPU `id`'s thread, outside `hv_vcpu_run`: records the
    /// vCPU's state and parks until the request is withdrawn or the VM
    /// stops. The `HvVcpu` must stay alive meanwhile.
    pub(in crate::vmm) fn park(&self, id: u32, vcpu: &HvVcpu, running: &AtomicBool) {
        let state = VcpuState::capture(vcpu).map_err(|e| e.to_string());
        self.states.lock().unwrap_or_else(PoisonError::into_inner)[id as usize] = Some(state);
        while self.requested() && running.load(Ordering::Acquire) {
            std::thread::park();
        }
    }
}

/// `state.json`. The GIC blob and RAM image sit next to it as `gic.bin`
/// and `mem.bin`, the disks as `disk<N>.img`.
#[derive(Serialize, Deserialize)]
struct MachineState {
    /// Index = vCPU id; `None` for a vCPU that was powered off. Timer
    /// offsets are relative to the pause.
    vcpus: Vec<Option<VcpuState>>,
    cpu_on: Vec<bool>,
    pl031: Pl031,
    devices: Vec<DeviceState>,
    /// Guest-bound Ethernet RSTs for every TCP flow the datapath carried,
    /// sent to the guest once it resumes: the flows' host sockets end with
    /// the checkpointed process.
    #[serde(default)]
    tcp_resets: Vec<Vec<u8>>,
}
