//! Resuming a checkpoint in a fresh process.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, PoisonError};
use std::time::Duration;

use arcbox_hv::HvVcpu;
use arcbox_hv::snapshot::{gic_restore, mach_time, resume_vtimer};
use vm_memory::{FileOffset, GuestAddress, GuestRegionMmap};

use crate::device::DeviceManager;
use crate::error::{Result, VmmError};

use super::super::{HvGuestMem, HvMachine, Pl031, RAM_BASE_IPA};
use super::{DRIVER_OK, MachineState, PAUSE_TIMEOUT};

/// A checkpoint being resumed: built by [`HvMachine::prepare_restore`],
/// consumed by `initialize` (RAM) and `start` (devices, vCPUs).
pub(in crate::vmm) struct Restore {
    dir: PathBuf,
    state: MachineState,
    gic: Vec<u8>,
    /// Every vCPU thread passes both restore phases.
    barrier: Barrier,
    gic_result: Mutex<Option<String>>,
    resume_at: AtomicU64,
    /// vCPUs past their restore phases, and the first error among them.
    finished: Mutex<(usize, Option<String>)>,
    finished_cv: Condvar,
}

impl Restore {
    /// Guest RAM mapped copy-on-write from `mem.bin`.
    pub(in crate::vmm) fn guest_memory(&self, ram_size: usize) -> Result<HvGuestMem> {
        let file = File::open(self.dir.join("mem.bin"))?;
        if file.metadata()?.len() != ram_size as u64 {
            return Err(VmmError::invalid_state(
                "mem.bin does not match the RAM size",
            ));
        }
        let region = vm_memory::MmapRegion::build(
            Some(FileOffset::new(file, 0)),
            ram_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
        )
        .map_err(|e| VmmError::Memory(format!("mapping mem.bin: {e}")))?;
        let region = GuestRegionMmap::new(region, GuestAddress(RAM_BASE_IPA))
            .ok_or_else(|| VmmError::Memory("mem.bin region overflows".into()))?;
        HvGuestMem::from_regions(vec![region])
            .map_err(|e| VmmError::Memory(format!("guest memory from mem.bin: {e}")))
    }

    pub(in crate::vmm) fn cpu_on(&self) -> &[bool] {
        &self.state.cpu_on
    }

    pub(in crate::vmm) fn pl031(&self) -> Pl031 {
        self.state.pl031.clone()
    }

    /// Writes the saved transport state into the fresh devices and replays
    /// what the guest's status writes did to the device objects: feature
    /// acknowledgement and activation. Before any vCPU runs.
    pub(in crate::vmm) fn apply_devices(&self, dm: &DeviceManager) -> Result<()> {
        for saved in &self.state.devices {
            let info = dm
                .iter()
                .find(|d| d.mmio_base == Some(saved.mmio_base))
                .ok_or_else(|| {
                    VmmError::invalid_state(format!("no device at {:#x}", saved.mmio_base))
                })?;
            let mmio = dm
                .get_mmio_state(info.id)
                .ok_or_else(|| VmmError::invalid_state("device without MMIO state"))?;
            saved.apply(&mut mmio.write().unwrap_or_else(PoisonError::into_inner))?;
            if let Some(dev) = dm.get_virtio_device(info.id) {
                let mut dev = dev.lock().unwrap_or_else(PoisonError::into_inner);
                dev.ack_features(saved.driver_features);
                if saved.status & DRIVER_OK != 0 {
                    dev.activate()
                        .map_err(|e| VmmError::Device(format!("activate: {e}")))?;
                }
            }
        }
        Ok(())
    }

    /// The restore phases on vCPU `id`'s thread, after every vCPU was
    /// created in id order and has its MPIDR. Returns whether the vCPU was
    /// powered on, i.e. resumes rather than waits for CPU_ON. Every thread
    /// passes both barriers, even on error, so none strands the others.
    pub(in crate::vmm) fn restore_vcpu(
        &self,
        id: u32,
        vcpu: &HvVcpu,
        dm: &DeviceManager,
    ) -> Result<bool> {
        let result = self.restore_phases(id, vcpu, dm);
        let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        finished.0 += 1;
        if let Err(ref e) = result {
            finished.1.get_or_insert_with(|| e.to_string());
        }
        self.finished_cv.notify_all();
        result
    }

    /// Waits until every vCPU is past its restore phases; the first
    /// vCPU's error if one failed.
    fn wait_restored(&self, vcpus: usize, timeout: Duration) -> Result<()> {
        let (finished, _) = self
            .finished_cv
            .wait_timeout_while(
                self.finished.lock().unwrap_or_else(PoisonError::into_inner),
                timeout,
                |f| f.0 < vcpus,
            )
            .unwrap_or_else(PoisonError::into_inner);
        match &finished.1 {
            Some(e) => Err(VmmError::invalid_state(format!("restore failed: {e}"))),
            None if finished.0 < vcpus => {
                Err(VmmError::Vcpu("vCPUs did not finish restoring".into()))
            }
            None => Ok(()),
        }
    }

    fn restore_phases(&self, id: u32, vcpu: &HvVcpu, dm: &DeviceManager) -> Result<bool> {
        let state = self.state.vcpus.get(id as usize).and_then(Option::as_ref);
        let sys = state.map_or(Ok(()), |s| s.set_sys(vcpu));
        // The GIC once, now that every vCPU has its system registers; then
        // the device lines, which the GIC state overwrites; then one time
        // base for every vCPU so their counters stay in step.
        if self.barrier.wait().is_leader() {
            if let Err(e) = gic_restore(&self.gic) {
                *self
                    .gic_result
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
            }
            for dev in dm.iter() {
                dm.sync_irq_level(dev.id);
            }
            self.resume_at.store(mach_time(), Ordering::SeqCst);
        }
        self.barrier.wait();
        let hv = |e: arcbox_hv::HvError| VmmError::Vcpu(format!("vCPU {id} restore: {e}"));
        sys.map_err(hv)?;
        if let Some(e) = self
            .gic_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Err(VmmError::Vcpu(format!("GIC restore: {e}")));
        }
        let resume_at = self.resume_at.load(Ordering::SeqCst);
        match state {
            Some(s) => {
                s.set_icc(vcpu).map_err(hv)?;
                s.set_gprs(vcpu).map_err(hv)?;
                s.set_simd(vcpu).map_err(hv)?;
                s.set_vtimer(vcpu, Some(resume_at)).map_err(hv)?;
                Ok(true)
            }
            // Off: comes up on vCPU 0's counter at its next CPU_ON.
            None => {
                let base = self.state.vcpus[0].as_ref().map_or(0, |s| s.vtimer_offset);
                resume_vtimer(vcpu, base, resume_at).map_err(hv)?;
                Ok(false)
            }
        }
    }
}

impl HvMachine {
    /// Loads the checkpoint in `dir` for the next `initialize` + `start`.
    pub(in crate::vmm) fn prepare_restore(&mut self, dir: &Path) -> Result<()> {
        let state: MachineState = serde_json::from_slice(&fs::read(dir.join("state.json"))?)
            .map_err(|e| VmmError::invalid_state(format!("state.json: {e}")))?;
        let n = self.config.vcpu_count as usize;
        if state.vcpus.len() != n || state.cpu_on.len() != n {
            return Err(VmmError::invalid_state(format!(
                "checkpoint has {} vCPUs, the machine {n}",
                state.vcpus.len()
            )));
        }
        if state.vcpus[0].is_none() {
            return Err(VmmError::invalid_state(
                "checkpoint has no state for vCPU 0",
            ));
        }
        self.hv_restore = Some(Arc::new(Restore {
            dir: dir.to_path_buf(),
            gic: fs::read(dir.join("gic.bin"))?,
            state,
            barrier: Barrier::new(n),
            gic_result: Mutex::new(None),
            resume_at: AtomicU64::new(0),
            finished: Mutex::new((0, None)),
            finished_cv: Condvar::new(),
        }));
        Ok(())
    }

    /// After `start`: waits until every vCPU has restored its state, and
    /// fails if one could not. Then hands the resumed guest a reset for
    /// every TCP flow it had.
    pub(in crate::vmm) fn finish_restore(&mut self) -> Result<()> {
        let restore = self
            .hv_restore
            .take()
            .ok_or_else(|| VmmError::invalid_state("no restore in progress"))?;
        restore.wait_restored(self.config.vcpu_count as usize, PAUSE_TIMEOUT)?;
        let resets = restore.state.tcp_resets.clone();
        if let (false, Some(net)) = (resets.is_empty(), &self.net_control) {
            net.send_to_guest(resets)
                .map_err(|e| VmmError::Device(format!("sending TCP resets: {e}")))?;
        }
        Ok(())
    }
}
