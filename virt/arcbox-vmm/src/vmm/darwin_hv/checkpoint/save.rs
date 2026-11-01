//! Taking a checkpoint of the running machine.

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use arcbox_hv::snapshot::{gic_capture, mach_time};
use vm_memory::{GuestMemory, GuestMemoryRegion};

use crate::error::{Result, VmmError};

use super::super::HvMachine;
use super::device_state::DeviceState;
use super::{MachineState, PAUSE_TIMEOUT, os_build};

/// Granularity of the sparse RAM image.
const PAGE: usize = 16 << 10;
/// How long the network datapath gets to abort its TCP flows.
const ABORT_TIMEOUT: Duration = Duration::from_secs(1);

impl HvMachine {
    /// Checkpoints the running machine into `dir` and returns a timing
    /// summary. The machine stays stopped in place: stop it next.
    pub(in crate::vmm) fn checkpoint(&mut self, dir: &Path) -> Result<String> {
        let t0 = Instant::now();
        let n = self.config.vcpu_count as usize;
        let vm = self
            .hv_vm
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("HV VM is not running"))?;
        let ids = self
            .hv_vcpu_ids
            .as_ref()
            .map(|ids| ids.lock().unwrap_or_else(PoisonError::into_inner).clone())
            .unwrap_or_default();
        let ctl = &self.hv_checkpoint;

        // Stop the vCPUs. One `hv_vcpus_exit` occasionally misses a vCPU,
        // so repeat it until every powered-on vCPU has reported.
        ctl.requested.store(true, Ordering::Release);
        let deadline = t0 + PAUSE_TIMEOUT;
        let cpu_on = loop {
            let on = self
                .hv_cpu_power
                .as_ref()
                .map_or_else(|| vec![true], |p| p.snapshot());
            let states = ctl.states.lock().unwrap_or_else(PoisonError::into_inner);
            if (0..n).all(|i| states[i].is_some() || !on[i]) {
                break on;
            }
            drop(states);
            if Instant::now() > deadline {
                return Err(VmmError::Vcpu(
                    "vCPUs did not stop for the checkpoint".into(),
                ));
            }
            vm.exit_vcpus(&ids)
                .map_err(|e| VmmError::Vcpu(format!("hv_vcpus_exit: {e}")))?;
            std::thread::sleep(Duration::from_millis(1));
        };
        let paused_at = mach_time();
        let t_pause = t0.elapsed();

        // Stop every thread that writes guest RAM on its own. The block
        // workers finish what they took from their queue once their
        // doorbells drop; the net RX, vsock and console workers exit at
        // their next poll.
        let dm = self
            .hv_device_manager
            .clone()
            .ok_or_else(|| VmmError::invalid_state("device manager not started"))?;
        self.io_running.store(false, Ordering::SeqCst);
        dm.clear_blk_workers();
        let workers = self
            .hv_blk_worker_threads
            .drain(..)
            .chain(dm.take_net_rx_worker_handle())
            .chain(self.hv_vsock_worker.take())
            .chain(self.hv_console_worker.take());
        for t in workers {
            let _ = t.join();
        }
        let t_drain = t0.elapsed();

        // With nothing writing the RX queue any more, each flow's SND.NXT
        // is what the guest has received.
        let tcp_resets = match &self.net_control {
            Some(net) => net
                .abort_tcp(ABORT_TIMEOUT)
                .map_err(|e| VmmError::Device(format!("aborting TCP flows: {e}")))?,
            None => Vec::new(),
        };

        let gic = gic_capture().map_err(|e| VmmError::Device(format!("GIC capture: {e}")))?;
        let vcpus = ctl
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
            .map(|s| match s.take() {
                Some(Ok(s)) => Ok(Some(s.paused_at(paused_at))),
                Some(Err(e)) => Err(VmmError::Vcpu(format!("vCPU capture: {e}"))),
                None => Ok(None),
            })
            .collect::<Result<Vec<_>>>()?;
        let devices = dm
            .iter()
            .filter_map(|d| Some((d, d.mmio_base?, dm.get_mmio_state(d.id)?)))
            .map(|(info, base, s)| {
                let config = dm
                    .get_virtio_device(info.id)
                    .map(|d| {
                        d.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .save_config()
                    })
                    .unwrap_or_default();
                DeviceState::capture(
                    base,
                    &s.read().unwrap_or_else(PoisonError::into_inner),
                    config,
                )
            })
            .collect();
        let pl031 = self
            .hv_pl031
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("PL031 not started"))?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();

        fs::create_dir_all(dir)?;
        for (i, disk) in self.config.block_devices.iter().enumerate() {
            File::open(&disk.path)?.sync_all()?;
            clonefile(&disk.path, &dir.join(format!("disk{i}.img")))?;
        }
        let t_disk = t0.elapsed();

        let mem = self
            .hv_guest_mem
            .as_ref()
            .and_then(|m| m.iter().next())
            .ok_or_else(|| VmmError::invalid_state("no guest memory"))?;
        // SAFETY: the region is mapped for `len` bytes while the machine
        // lives, and no vCPU or I/O worker writes it now.
        let ram = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem.len() as usize) };
        let (pages, bytes) = write_sparse(&dir.join("mem.bin"), ram)?;
        fs::write(dir.join("gic.bin"), &gic)?;
        #[cfg(feature = "vmnet")]
        let bridge_mac = self
            .vmnet_bridge
            .as_ref()
            .and_then(|v| v.interface_info().map(|i| i.mac));
        #[cfg(not(feature = "vmnet"))]
        let bridge_mac = None;
        let state = MachineState {
            vcpus,
            cpu_on,
            pl031,
            devices,
            tcp_resets,
            os_build: os_build()?,
            bridge_mac,
        };
        fs::write(
            dir.join("state.json"),
            serde_json::to_vec_pretty(&state).map_err(|e| VmmError::Device(e.to_string()))?,
        )?;
        let t_all = t0.elapsed();

        let on = state.cpu_on.iter().filter(|&&on| on).count();
        Ok(format!(
            "checkpoint: {on}/{n} vCPUs on, {} TCP flows; pause {:.1} ms, I/O drain {:.1} ms, \
             disk fsync+clone {:.1} ms, RAM {} pages ({} MiB on disk) {:.0} ms, total {:.0} ms",
            state.tcp_resets.len(),
            ms(t_pause),
            ms(t_drain.saturating_sub(t_pause)),
            ms(t_disk.saturating_sub(t_drain)),
            pages,
            bytes >> 20,
            ms(t_all.saturating_sub(t_disk)),
            ms(t_all),
        ))
    }
}

/// Writes only the non-zero pages of `bytes`, leaving holes for the rest.
/// Returns the page count and the bytes written.
fn write_sparse(path: &Path, bytes: &[u8]) -> Result<(usize, u64)> {
    let file = File::create(path)?;
    file.set_len(bytes.len() as u64)?;
    let mut pages = 0;
    for (i, page) in bytes.chunks(PAGE).enumerate() {
        if page.iter().any(|&b| b != 0) {
            file.write_all_at(page, (i * PAGE) as u64)?;
            pages += 1;
        }
    }
    file.sync_all()?;
    Ok((pages, (pages * PAGE) as u64))
}

/// `clonefile(2)`: an APFS copy-on-write clone, O(1) in the file size.
fn clonefile(src: &Path, dst: &Path) -> Result<()> {
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| VmmError::config(format!("path: {e}")))
    };
    let (s, d) = (c(src)?, c(dst)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
