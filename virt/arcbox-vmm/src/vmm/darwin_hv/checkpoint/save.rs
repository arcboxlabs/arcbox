//! Taking a checkpoint of the running machine.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
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

/// `mincore` dispositions from `<sys/mman.h>` that the `libc` crate does not
/// export.
const MINCORE_PAGED_OUT: u8 = 0x20;
const MINCORE_ANONYMOUS: u8 = 0x80;
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
        // A restored machine's RAM is mapped from the checkpoint it resumed.
        let base = match mem.file_offset() {
            Some(f) if f.start() == 0 => Some(f.file()),
            Some(_) => return Err(VmmError::invalid_state("RAM image mapped at an offset")),
            None => None,
        };
        // SAFETY: the region is mapped for `len` bytes while the machine
        // lives, and no vCPU or I/O worker writes it now.
        let ram = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem.len() as usize) };
        let written = write_ram(dir, ram, base)?;
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
             disk fsync+clone {:.1} ms, RAM {} pages written ({} MiB), {} zeroed, {} kept from \
             the base image, {:.0} ms, total {:.0} ms",
            state.tcp_resets.len(),
            ms(t_pause),
            ms(t_drain.saturating_sub(t_pause)),
            ms(t_disk.saturating_sub(t_drain)),
            written.pages,
            written.bytes >> 20,
            written.zeroed,
            written.kept,
            ms(t_all.saturating_sub(t_disk)),
            ms(t_all),
        ))
    }
}

/// What [`write_ram`] did, in host pages.
struct RamWrite {
    /// Pages copied from RAM, and their bytes.
    pages: usize,
    bytes: u64,
    /// Pages of the base image punched back to zero.
    zeroed: usize,
    /// Pages of the base image left as they were.
    kept: usize,
}

/// Writes guest RAM to `dir/mem.bin`, reading only the pages the guest can
/// have changed.
///
/// `mincore` tells them apart without reading RAM. A page that is not in
/// an anonymous object is still the clean page of the image the machine was
/// restored from (`base`, mapped `MAP_PRIVATE`), so the new image starts
/// as an APFS clone of that one and keeps it. An anonymous page that is
/// resident or compressed was written: by the guest, by a device, by the
/// boot loader, or as a private copy of a base page. An anonymous page
/// that is neither was never touched since the mapping (or the balloon's
/// refresh of it, `page_release`) was made, and reads zero. Without a base
/// the image starts empty, zero pages stay holes, and so do written pages
/// that are zero again.
///
/// The image is written beside the target and renamed over it, so an image
/// that other machines map stays as it was.
fn write_ram(dir: &Path, ram: &[u8], base: Option<&File>) -> Result<RamWrite> {
    let path = dir.join("mem.bin");
    let tmp = dir.join("mem.bin.tmp");
    let _ = fs::remove_file(&tmp);
    if let Some(base) = base {
        fclonefile(base, &tmp)?;
    } else {
        File::create(&tmp)?.set_len(ram.len() as u64)?;
    }
    let file = File::options().read(true).write(true).open(&tmp)?;
    if file.metadata()?.len() != ram.len() as u64 {
        return Err(VmmError::invalid_state(
            "the base RAM image does not match the RAM size",
        ));
    }

    // SAFETY: sysconf has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .map_err(|_| VmmError::Memory("page size".into()))?;
    let mut flags = vec![0u8; ram.len().div_ceil(page)];
    // SAFETY: `ram` is a live page-aligned mapping and `flags` holds one
    // byte per page of it.
    if unsafe { libc::mincore(ram.as_ptr().cast(), ram.len(), flags.as_mut_ptr().cast()) } != 0 {
        return Err(VmmError::Memory(format!(
            "mincore: {}",
            std::io::Error::last_os_error()
        )));
    }
    let present = libc::MINCORE_INCORE as u8 | MINCORE_PAGED_OUT;
    let mut done = RamWrite {
        pages: 0,
        bytes: 0,
        zeroed: 0,
        kept: 0,
    };
    // Consecutive pages that get the same treatment go to the file as one
    // write or one punch.
    let mut run: Option<(Page, usize, usize)> = None;
    for (i, &f) in flags.iter().enumerate() {
        let start = i * page;
        let end = ram.len().min(start + page);
        let op = if f & MINCORE_ANONYMOUS == 0 && base.is_some() {
            done.kept += 1;
            Page::Keep
        } else if f & MINCORE_ANONYMOUS == 0 || f & present != 0 {
            if base.is_none() && ram[start..end].iter().all(|&b| b == 0) {
                Page::Keep
            } else {
                done.pages += 1;
                done.bytes += (end - start) as u64;
                Page::Write
            }
        } else if base.is_some() {
            done.zeroed += 1;
            Page::Zero
        } else {
            Page::Keep
        };
        match &mut run {
            Some((o, _, e)) if *o == op => *e = end,
            _ => {
                if let Some(r) = run.replace((op, start, end)) {
                    apply(&file, ram, r)?;
                }
            }
        }
    }
    if let Some(r) = run {
        apply(&file, ram, r)?;
    }
    file.sync_all()?;
    if cfg!(debug_assertions) {
        verify_ram(&file, ram)?;
    }
    fs::rename(&tmp, &path)?;
    Ok(done)
}

/// What a page of the RAM image needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    /// Already right in the file.
    Keep,
    /// Copy from RAM.
    Write,
    /// Deallocate in the file, so it reads zero.
    Zero,
}

/// Applies one run of pages, `start..end` in RAM and in the file.
fn apply(file: &File, ram: &[u8], (op, start, end): (Page, usize, usize)) -> Result<()> {
    match op {
        Page::Keep => Ok(()),
        Page::Write => Ok(file.write_all_at(&ram[start..end], start as u64)?),
        Page::Zero => punch_hole(file, start as u64, (end - start) as u64),
    }
}

/// Debug builds check the image against RAM: a page `mincore` misjudged
/// would otherwise surface only as a corrupt guest after some later
/// restore.
fn verify_ram(file: &File, ram: &[u8]) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    for (i, chunk) in ram.chunks(buf.len()).enumerate() {
        let got = &mut buf[..chunk.len()];
        file.read_exact_at(got, (i * (1 << 20)) as u64)?;
        if got != chunk {
            return Err(VmmError::Memory(format!(
                "RAM image differs from guest RAM in MiB {i}"
            )));
        }
    }
    Ok(())
}

/// Deallocates `len` bytes at `offset`, which then read zero.
fn punch_hole(file: &File, offset: u64, len: u64) -> Result<()> {
    let off =
        |v: u64| libc::off_t::try_from(v).map_err(|_| VmmError::Memory("RAM image offset".into()));
    let arg = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: off(offset)?,
        fp_length: off(len)?,
    };
    // SAFETY: `arg` is a valid fpunchhole_t for the duration of the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &raw const arg) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// `fclonefileat(2)`: an APFS clone of the open file `src` at `dst`. By
/// descriptor, because the image a machine was restored from may have been
/// unlinked or replaced since.
fn fclonefile(src: &File, dst: &Path) -> Result<()> {
    let d = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| VmmError::config(format!("path: {e}")))?;
    // SAFETY: `src` is an open descriptor and `d` a NUL-terminated path.
    if unsafe { libc::fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, d.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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
