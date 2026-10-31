use std::ffi::CString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arcbox_hv::{ExceptionClass, Gic, HvVcpu, HvVm, MemoryPermission, VcpuExit, reg, sys_reg};
use arcbox_virtio::VirtioDevice;
use arcbox_vmm::blk_worker::{
    BlkQueueWorker, BlkWorkerContext, BlkWorkerHandle, FlushBarrier, GuestMemWriter,
    blk_io_worker_loop,
};
use arcbox_vmm::device::{DeviceIrqCallback, IoCompletion, MAX_VIRTQUEUES, VirtioMmioState};
use arcbox_vmm::irq::{Gsi, Irq, IrqChip, IrqTriggerCallback};
use arcbox_vmm::memory::MemoryManager;
use arcbox_vmm::{DeviceId, DeviceManager, DeviceType, VmmError};

use crate::common::ram::{PAGE, Ram, write_sparse};
use crate::common::vcpu::{
    Result, VcpuState, check, gic_capture, gic_restore, hex, mach_time, max_rss, ms, wait_until,
};

const GUEST: &[u8] = include_bytes!("guest.bin");

const RAM_BASE: u64 = 0x4000_0000;
const RAM_SIZE: usize = 64 << 20;
const DISK_SIZE: u64 = 16 << 20;
const FAIL_MMIO: u64 = 0x0900_0000;
const TRAP_MMIO: u64 = 0x0900_0008;
/// Distributor plus the redistributor region (`arcbox_hv::Gic` defaults).
const GIC_MMIO: std::ops::Range<u64> = 0x0800_0000..0x0A0A_0000;
/// The HV VMM's virtio-mmio window (`setup.rs`).
const VIRTIO_MMIO_BASE: u64 = 0x0C00_0000;
const VIRTIO_MMIO_SLOTS: u64 = 32;
/// EL1h with DAIF masked: the reset state the guest starts from.
const CPSR_EL1H: u64 = 0x3c5;

/// Guest data page layout (`guest/guest.c`).
const SPI_COUNT: usize = 0x08;
fn stat(cpu: usize, field: usize) -> usize {
    0x100 + cpu * 64 + field * 8
}
const TICKS: usize = 0;
const IOS: usize = 1;
fn avail_idx_off(q: usize) -> usize {
    0x30_0000 + q * 0x4000 + 0x1000 + 2
}
fn used_idx_off(q: usize) -> usize {
    0x30_0000 + q * 0x4000 + 0x2000 + 2
}

/// A VM with its GIC, guest RAM and one virtio-blk device served by
/// production `arcbox-vmm` code. Fields drop in order: VM before RAM.
struct Machine {
    vm: HvVm,
    _gic: Arc<Gic>,
    ram: Ram,
    dm: Arc<DeviceManager>,
    blk_dev: Arc<Mutex<arcbox_virtio::blk::VirtioBlock>>,
    blk: DeviceId,
    mmio_base: u64,
    irq: Irq,
    nq: u16,
    workers: Vec<JoinHandle<()>>,
}

type Rings = Vec<(u16, u16)>;

/// State the vCPU threads share with the main thread.
struct Control {
    stop: AtomicBool,
    handles: Vec<AtomicU64>,
    ready: AtomicUsize,
    /// Restore: every vCPU has its MPIDR / the GIC state is in.
    barrier: Barrier,
    /// Restore: host time all vCPUs rebase their virtual counter on.
    resume_at: AtomicU64,
    /// A stopped vCPU captures its state, reports, and stays parked (alive)
    /// until the main thread releases it.
    report: Sender<()>,
}

/// The running vCPU threads.
struct Vcpus {
    ctl: Arc<Control>,
    threads: Vec<VcpuThread>,
    release: Vec<Sender<()>>,
    reports: Receiver<()>,
}

enum Start {
    Fresh,
    Restore {
        vcpu: Box<VcpuState>,
        /// Only vCPU 0 carries the GIC blob; it writes it for the whole VM.
        gic: Option<Vec<u8>>,
        skip: String,
    },
}

pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: checkpoint_blk_spike save|restore <dir> [run_ms] [--eager]";
    let mode = args.first().ok_or(usage)?;
    let dir = PathBuf::from(args.get(1).ok_or(usage)?);
    let run_ms: u64 = args.get(2).map_or(Ok(500), |s| s.parse())?;
    let eager = args.iter().any(|a| a == "--eager");
    match mode.as_str() {
        "save" => save(&dir, run_ms),
        "restore" => restore(&dir, run_ms, eager),
        _ => Err(usage.into()),
    }
}

fn save(dir: &Path, run_ms: u64) -> Result<()> {
    fs::create_dir_all(dir)?;
    let ncpu: usize = std::env::var("SPIKE_VCPUS").map_or(Ok(4), |v| v.parse())?;
    if !(1..=16).contains(&ncpu) {
        return Err("SPIKE_VCPUS must be 1..=16".into());
    }
    // SPIKE_LATE_GIC=1 reads the GIC after the vCPUs are destroyed
    // (negative control).
    let late_gic = std::env::var("SPIKE_LATE_GIC").is_ok_and(|v| v == "1");
    let live = dir.join("live.img");
    File::create(&live)?.set_len(DISK_SIZE)?;

    let ram = Ram::anon(RAM_SIZE)?;
    ram.bytes_mut()[..GUEST.len()].copy_from_slice(GUEST);
    let mut m = Machine::new(ram, &live, u16::try_from(ncpu)?)?;
    m.spawn_workers();
    let vcpus = Vcpus::spawn(&m, (0..ncpu).map(|_| Start::Fresh).collect());
    let started = vcpus.wait_ready().and_then(|()| {
        wait_until(
            || (0..ncpu).all(|c| m.ram.data(stat(c, IOS)) > 0),
            Duration::from_secs(2),
            "first I/O on every vCPU",
        )
    });
    if let Err(e) = started {
        return Err(vcpus.halt(&m).err().unwrap_or(e));
    }
    std::thread::sleep(Duration::from_millis(run_ms));

    // Pause: vCPUs first, so the guest submits nothing more; then the
    // device, whose workers finish every chain they popped and exit.
    let t = Instant::now();
    let paused = vcpus.pause(&m);
    let t_vcpu = t.elapsed();
    let t = Instant::now();
    m.quiesce_device();
    let t_dev = t.elapsed();
    let rings = m.rings();
    // The GIC last, while the vCPUs still exist: it now holds every
    // interrupt the drained completions raised.
    let gic = (paused.is_ok() && !late_gic).then(gic_capture);
    let vcpus = vcpus.finish()?;
    // The guest is paused from the first vCPU's stop; every vCPU's counter
    // is stored relative to that one instant.
    let paused_at = vcpus
        .iter()
        .map(|v| v.captured_at)
        .min()
        .unwrap_or_default();
    let rings_at_stop = paused?;
    let gic = match gic {
        Some(g) => g?,
        None => gic_capture()?,
    };

    let t = Instant::now();
    File::open(&live)?.sync_all()?;
    clone_file(&live, &dir.join("disk.img"))?;
    fs::remove_file(&live)?;
    let t_disk = t.elapsed();
    let t = Instant::now();
    let (pages, allocated) = write_sparse(&dir.join("mem.bin"), m.ram.bytes())?;
    let t_mem = t.elapsed();
    fs::write(dir.join("gic.bin"), &gic)?;
    for (i, v) in vcpus.iter().enumerate() {
        let v = v.clone().paused_at(paused_at);
        fs::write(
            dir.join(format!("vcpu{i}.json")),
            serde_json::to_string(&v)?,
        )?;
    }
    let mmio = m.mmio_state();
    fs::write(dir.join("mmio.txt"), m.encode_mmio(&mmio.read().unwrap()))?;

    println!(
        "saved {ncpu} vCPUs: ticks {:?} ios {:?} spis {}",
        m.per_cpu(TICKS),
        m.per_cpu(IOS),
        m.ram.data(SPI_COUNT)
    );
    let in_flight: Vec<u16> = rings_at_stop
        .iter()
        .map(|(a, u)| a.wrapping_sub(*u))
        .collect();
    let drained: Vec<u16> = rings
        .iter()
        .zip(&rings_at_stop)
        .map(|((_, u1), (_, u0))| u1.wrapping_sub(*u0))
        .collect();
    let left: Vec<u16> = rings.iter().map(|(a, u)| a.wrapping_sub(*u)).collect();
    println!(
        "  requests per queue: in flight at vCPU stop {in_flight:?}, completed by the drain {drained:?}, left in avail {left:?}"
    );
    println!(
        "  vCPU pause+capture {:.3} ms, device drain {:.3} ms, disk fsync+clone {:.3} ms, memory write {:.3} ms",
        ms(t_vcpu),
        ms(t_dev),
        ms(t_disk),
        ms(t_mem)
    );
    println!(
        "  gic blob {} B, interrupt_status {:#x}, mem.bin {pages} non-zero pages of {}, {} KiB allocated",
        gic.len(),
        mmio.read().unwrap().interrupt_status,
        RAM_SIZE / PAGE,
        allocated / 1024
    );
    Ok(())
}

fn restore(dir: &Path, run_ms: u64, eager: bool) -> Result<()> {
    let t0 = Instant::now();
    // SPIKE_SKIP leaves parts of the checkpoint out, as negative controls.
    let skip = std::env::var("SPIKE_SKIP").unwrap_or_default();
    let skipping = |part: &str| skip.split(',').any(|p| p == part);
    let mut vcpus = Vec::new();
    while let Ok(bytes) = fs::read(dir.join(format!("vcpu{}.json", vcpus.len()))) {
        vcpus.push(serde_json::from_slice::<VcpuState>(&bytes)?);
    }
    let ncpu = vcpus.len();
    if ncpu == 0 {
        return Err("no vcpu0.json in the checkpoint".into());
    }
    let mmio_text = fs::read_to_string(dir.join("mmio.txt"))?;
    let gic = fs::read(dir.join("gic.bin"))?;

    // Each run writes its own copy of the disk: an APFS clone shares the
    // checkpoint's blocks until written.
    let disk = dir.join(format!("run-{}.img", std::process::id()));
    if skipping("disk") {
        File::create(&disk)?.set_len(DISK_SIZE)?;
    } else {
        clone_file(&dir.join("disk.img"), &disk)?;
    }
    let result = (|| {
        let mem = dir.join("mem.bin");
        let ram = if eager {
            Ram::read_from(&mem, RAM_SIZE)?
        } else {
            Ram::map_private(&mem, RAM_SIZE)?
        };
        let mut m = Machine::new(ram, &disk, u16::try_from(ncpu)?)?;
        let saved = m.decode_mmio(&mmio_text)?;
        if !skipping("mmio") {
            m.restore_mmio(&saved)?;
        }
        m.spawn_workers();
        let t_built = t0.elapsed();
        let ticks0 = m.per_cpu(TICKS);
        let ios0 = m.per_cpu(IOS);
        let spis0 = m.ram.data(SPI_COUNT);

        let starts = vcpus
            .into_iter()
            .enumerate()
            .map(|(i, v)| Start::Restore {
                vcpu: Box::new(v),
                gic: (i == 0).then(|| gic.clone()),
                skip: skip.clone(),
            })
            .collect();
        let vcpus = Vcpus::spawn(&m, starts);
        let resumed = vcpus.wait_ready().map(|()| t0.elapsed()).and_then(|t_run| {
            let a = wait_for_all(&m, &ticks0, TICKS, "a tick on every vCPU")?;
            let b = wait_for_all(&m, &ios0, IOS, "an I/O on every vCPU")?;
            Ok((t_run, a, b))
        });
        let (t_running, t_tick, t_io) = match resumed {
            Ok(t) => t,
            Err(e) => {
                let ios = m.per_cpu(IOS);
                let spis = m.ram.data(SPI_COUNT);
                return Err(match vcpus.halt(&m) {
                    Ok(st) => {
                        let pcs: Vec<String> = st
                            .iter()
                            .map(|v| {
                                format!(
                                    "pc {:#x} ctl {:#x} cval-cnt {} masked {}",
                                    v.gpr[reg::HV_REG_PC as usize],
                                    v.sys_reg("CNTV_CTL_EL0"),
                                    v.sys_reg("CNTV_CVAL_EL0")
                                        .wrapping_sub(v.cntvct(mach_time()))
                                        .cast_signed(),
                                    v.vtimer_masked
                                )
                            })
                            .collect();
                        format!(
                            "{e}; ios {ios0:?} -> {ios:?}; spis {spis0} -> {spis}; vCPUs {pcs:?}"
                        )
                        .into()
                    }
                    Err(halt_err) => halt_err,
                });
            }
        };
        let (ticks_a, ios_a) = (m.per_cpu(TICKS), m.per_cpu(IOS));
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(run_ms));
        let (ticks_b, ios_b) = (m.per_cpu(TICKS), m.per_cpu(IOS));
        let secs = t.elapsed().as_secs_f64();
        vcpus.halt(&m)?;
        m.quiesce_device();

        let rates: Vec<String> = ticks_a
            .iter()
            .zip(&ticks_b)
            .map(|(a, b)| format!("{:.2}", (b - a) as f64 / secs / 1000.0))
            .collect();
        let iops: Vec<u64> = ios_a
            .iter()
            .zip(&ios_b)
            .map(|(a, b)| ((b - a) as f64 / secs) as u64)
            .collect();
        println!(
            "restored {ncpu} vCPUs ({}): ios {ios0:?} -> {:?}",
            if eager { "eager copy" } else { "MAP_PRIVATE" },
            m.per_cpu(IOS)
        );
        println!(
            "  built {:.3} ms, vCPUs running {:.3} ms, a tick on every vCPU {:.3} ms, an I/O on every vCPU {:.3} ms",
            ms(t_built),
            ms(t_running),
            ms(t_tick + t_running),
            ms(t_io + t_running)
        );
        println!("  per vCPU after resume: tick kHz {rates:?}, I/O per s {iops:?}");
        println!("  peak process RSS: {} MiB", max_rss() >> 20);
        // Per vCPU only "ticking": with 8+ busy vCPUs the host deschedules
        // vCPU threads for tens of ms, before a checkpoint as much as after.
        let khz: Vec<f64> = rates.iter().map(|r| r.parse().unwrap_or(0.0)).collect();
        let mean = khz.iter().sum::<f64>() / khz.len() as f64;
        if khz.iter().any(|&r| r < 0.5) || !(0.9..1.1).contains(&mean) {
            return Err(format!("tick rates out of range: {rates:?}").into());
        }
        if iops.contains(&0) {
            return Err(format!("a vCPU stopped completing I/O: {iops:?}").into());
        }
        Ok(())
    })();
    fs::remove_file(&disk)?;
    result
}

/// Waits until `field` of every vCPU's stats passes `base`, returning how long that took.
fn wait_for_all(m: &Machine, base: &[u64], field: usize, what: &str) -> Result<Duration> {
    let t = Instant::now();
    wait_until(
        || {
            base.iter()
                .enumerate()
                .all(|(c, &b)| m.ram.data(stat(c, field)) > b)
        },
        Duration::from_secs(2),
        what,
    )
    .map_err(|e| format!("{e}: {base:?} -> {:?}", m.per_cpu(field)))?;
    Ok(t.elapsed())
}

impl Machine {
    /// Builds the VM the way `darwin_hv/setup.rs` does, minus the boot
    /// path: GIC, `IrqChip` wired to it, `DeviceManager` over guest RAM,
    /// and a `VirtioBlock` with one queue per vCPU.
    fn new(ram: Ram, disk: &Path, nq: u16) -> Result<Self> {
        let vm = HvVm::new()?;
        // SAFETY: `ram` is stored in the returned `Machine`, whose field
        // order drops `vm` (and its mapping) first.
        unsafe { vm.map_memory(ram.ptr(), RAM_BASE, ram.len(), MemoryPermission::ALL)? };
        let gic = Arc::new(Gic::with_defaults()?);

        let irq_chip = Arc::new(IrqChip::new()?);
        let g = Arc::downgrade(&gic);
        let to_gic: IrqTriggerCallback = Box::new(move |gsi: Gsi, level: bool| {
            if let Some(g) = g.upgrade() {
                g.set_spi(gsi, level)
                    .map_err(|e| VmmError::Irq(format!("set_spi({gsi}): {e}")))?;
            }
            Ok(())
        });
        irq_chip.set_trigger_callback(Arc::new(to_gic));

        let mut mm = MemoryManager::with_mmio_base(VIRTIO_MMIO_BASE, VIRTIO_MMIO_SLOTS * 0x1000);
        mm.initialize(ram.len() as u64)?;
        let mut dm = DeviceManager::new();
        // SAFETY: the mapping lives in `ram`, which outlives `dm`'s use: the
        // workers and vCPU threads are joined before `Machine` drops.
        unsafe { dm.set_guest_memory(ram.ptr(), ram.len(), RAM_BASE) };
        let chip = Arc::clone(&irq_chip);
        let cb: DeviceIrqCallback = Arc::new(move |irq, level| {
            if level {
                chip.trigger_irq(irq)
            } else {
                chip.deassert_irq(irq)
            }
        });
        dm.set_irq_callback(cb);

        let mut blk = arcbox_virtio::blk::VirtioBlock::from_path(disk, false)?;
        blk.set_num_queues(nq);
        let (blk_id, blk_dev) =
            dm.register_virtio_device(DeviceType::VirtioBlock, "blk", blk, &mut mm, &irq_chip)?;
        let info = dm.get(blk_id).ok_or("blk device vanished")?;
        let mmio_base = info.mmio_base.ok_or("blk device has no MMIO base")?;
        let irq = info.irq.ok_or("blk device has no IRQ")?;
        Ok(Self {
            vm,
            _gic: gic,
            ram,
            dm: Arc::new(dm),
            blk_dev,
            blk: blk_id,
            mmio_base,
            irq,
            nq,
            workers: Vec::new(),
        })
    }

    fn mmio_state(&self) -> Arc<RwLock<VirtioMmioState>> {
        self.dm
            .get_mmio_state(self.blk)
            .expect("blk device has MMIO state")
    }

    /// One `blk_io_worker_loop` thread per queue, as `darwin_hv/lifecycle.rs` spawns them.
    fn spawn_workers(&mut self) {
        let dev = self.blk_dev.lock().unwrap();
        let (raw_fd, blk_size) = (dev.raw_fd().unwrap_or(-1), dev.blk_size());
        let capacity_sectors = dev.capacity_bytes() / u64::from(blk_size);
        drop(dev);
        let mmio_state = self.mmio_state();
        let irq_cb = self.dm.irq_callback_clone().expect("irq callback set");
        let flush_barrier = Arc::new(FlushBarrier::new());
        let running = Arc::new(AtomicBool::new(true));
        let mut queues = Vec::new();
        for qi in 0..self.nq {
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = BlkWorkerContext {
                queue_idx: qi,
                // SAFETY: guest RAM outlives the worker, which is joined in
                // `quiesce_device` before `Machine` drops.
                guest_mem: unsafe {
                    GuestMemWriter::new(self.ram.ptr(), self.ram.len(), RAM_BASE as usize)
                },
                raw_fd,
                blk_size,
                capacity_sectors,
                read_only: false,
                device_id: "blk".into(),
                mmio_state: Arc::clone(&mmio_state),
                completion: IoCompletion::new(Arc::clone(&mmio_state), self.irq, irq_cb.clone()),
                running: Arc::clone(&running),
                flush_barrier: Arc::clone(&flush_barrier),
            };
            self.workers
                .push(std::thread::spawn(move || blk_io_worker_loop(ctx, rx)));
            queues.push(BlkQueueWorker { doorbell: tx });
        }
        self.dm.set_blk_worker(self.blk, BlkWorkerHandle { queues });
    }

    /// Stops the device: dropping the doorbells ends each worker after it
    /// completes everything it popped. Joining makes that a guarantee.
    fn quiesce_device(&mut self) {
        self.dm.clear_blk_workers();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }

    /// `(avail.idx, used.idx)` of every queue, read from guest RAM.
    fn rings(&self) -> Rings {
        (0..usize::from(self.nq))
            .map(|q| {
                (
                    self.ram.read(avail_idx_off(q)),
                    self.ram.read(used_idx_off(q)),
                )
            })
            .collect()
    }

    fn per_cpu(&self, field: usize) -> Vec<u64> {
        (0..usize::from(self.nq))
            .map(|c| self.ram.data(stat(c, field)))
            .collect()
    }

    /// The device's transport state. Everything else the device has
    /// (queue cursors) lives in guest RAM since the blk worker derives its
    /// avail cursor from `used.idx`; the disk is cloned separately.
    fn encode_mmio(&self, s: &VirtioMmioState) -> String {
        let mut out = format!(
            "mmio_base {:#x}\nirq {:#x}\ndevice_features {:#x}\ndriver_features {:#x}\n\
             device_features_sel {:#x}\ndriver_features_sel {:#x}\nqueue_sel {:#x}\n\
             status {:#x}\ninterrupt_status {:#x}\nconfig_generation {:#x}\n",
            self.mmio_base,
            self.irq,
            s.device_features,
            s.driver_features,
            s.device_features_sel,
            s.driver_features_sel,
            s.queue_sel,
            s.status,
            s.interrupt_status,
            s.config_generation
        );
        for q in 0..usize::from(self.nq) {
            let _ = writeln!(
                out,
                "q{q}_num {:#x}\nq{q}_ready {:#x}\nq{q}_desc {:#x}\nq{q}_driver {:#x}\nq{q}_device {:#x}",
                s.queue_num[q],
                u8::from(s.queue_ready[q]),
                s.queue_desc[q],
                s.queue_driver[q],
                s.queue_device[q]
            );
        }
        out
    }

    fn decode_mmio(&self, text: &str) -> Result<Vec<(String, u64)>> {
        let fields = text
            .lines()
            .map(|l| {
                let (k, v) = l.split_once(' ').ok_or_else(|| format!("bad line {l}"))?;
                Ok((k.to_string(), hex(v)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|&(_, v)| v);
        // The device must land where the guest left it.
        if get("mmio_base") != Some(self.mmio_base) || get("irq") != Some(u64::from(self.irq)) {
            return Err(format!(
                "device layout changed: saved base/irq {:?}/{:?}, now {:#x}/{}",
                get("mmio_base"),
                get("irq"),
                self.mmio_base,
                self.irq
            )
            .into());
        }
        Ok(fields)
    }

    /// Writes the saved transport state into the fresh device and replays
    /// the two side effects the guest's status writes had on the device
    /// object: feature acknowledgement and activation.
    fn restore_mmio(&self, fields: &[(String, u64)]) -> Result<()> {
        let mmio = self.mmio_state();
        let mut s = mmio.write().unwrap();
        for (k, v) in fields {
            let v = *v;
            let small = || u32::try_from(v);
            match k.as_str() {
                "mmio_base" | "irq" => {}
                "device_features" => s.device_features = v,
                "driver_features" => s.driver_features = v,
                "device_features_sel" => s.device_features_sel = small()?,
                "driver_features_sel" => s.driver_features_sel = small()?,
                "queue_sel" => s.queue_sel = small()?,
                "status" => s.status = u8::try_from(v)?,
                "interrupt_status" => s.interrupt_status = small()?,
                "config_generation" => s.config_generation = small()?,
                _ => {
                    let (q, field) = k
                        .strip_prefix('q')
                        .and_then(|r| r.split_once('_'))
                        .ok_or_else(|| format!("unknown mmio field {k}"))?;
                    let q: usize = q.parse()?;
                    if q >= MAX_VIRTQUEUES {
                        return Err(format!("queue {q} out of range").into());
                    }
                    match field {
                        "num" => s.queue_num[q] = u16::try_from(v)?,
                        "ready" => s.queue_ready[q] = v != 0,
                        "desc" => s.queue_desc[q] = v,
                        "driver" => s.queue_driver[q] = v,
                        "device" => s.queue_device[q] = v,
                        _ => return Err(format!("unknown mmio field {k}").into()),
                    }
                }
            }
        }
        let (features, status) = (s.driver_features, s.status);
        drop(s);
        let mut dev = self.blk_dev.lock().unwrap();
        dev.ack_features(features);
        if status & 4 != 0 {
            dev.activate()?;
        }
        Ok(())
    }
}

impl Control {
    fn new(ncpu: usize, report: Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            handles: (0..ncpu).map(|_| AtomicU64::new(u64::MAX)).collect(),
            ready: AtomicUsize::new(0),
            barrier: Barrier::new(ncpu),
            resume_at: AtomicU64::new(0),
            report,
        })
    }
}

type VcpuThread = JoinHandle<Result<VcpuState>>;

struct GuestEnv {
    idx: usize,
    ncpu: u64,
    virtio: u64,
    intid: u64,
    blk: DeviceId,
}

/// Boot registers for a fresh vCPU (`guest/start.S` documents x0..x5).
fn fresh(vcpu: &HvVcpu, env: &GuestEnv) -> Result<()> {
    vcpu.set_sys_reg(sys_reg::HV_SYS_REG_MPIDR_EL1, env.idx as u64)?;
    let mut gicr = 0;
    // SAFETY: `vcpu` is live on this thread; `gicr` is a valid out-pointer.
    check(unsafe { arcbox_hv::ffi::hv_gic_get_redistributor_base(vcpu.id(), &raw mut gicr) })?;
    vcpu.set_reg(reg::HV_REG_PC, RAM_BASE)?;
    vcpu.set_reg(reg::HV_REG_X0, env.idx as u64)?;
    vcpu.set_reg(reg::HV_REG_X1, gicr)?;
    vcpu.set_reg(reg::HV_REG_X2, GIC_MMIO.start)?;
    vcpu.set_reg(reg::HV_REG_X3, env.virtio)?;
    vcpu.set_reg(reg::HV_REG_X4, env.intid)?;
    vcpu.set_reg(reg::HV_REG_X5, env.ncpu)?;
    vcpu.set_reg(reg::HV_REG_CPSR, CPSR_EL1H)?;
    vcpu.set_vtimer_mask(false)?;
    Ok(())
}

fn vcpu_main(
    start: Start,
    env: &GuestEnv,
    ctl: &Control,
    dm: &DeviceManager,
    release: &Receiver<()>,
) -> Result<VcpuState> {
    // Every phase ends at a barrier that all vCPU threads reach even on
    // error, so a failing thread never strands the others. No vCPU runs
    // before all exist: `hv_vcpu_create` blocks while another vCPU of the
    // VM is running (seen from the seventh vCPU on).
    let vcpu = HvVcpu::new();
    let phase1 = vcpu
        .as_ref()
        .map_err(|e| e.to_string().into())
        .and_then(|v| match &start {
            // Aff0 = index, as the HV VMM writes MPIDR. (Redistributor frames
            // follow vCPU creation order, not MPIDR.)
            Start::Fresh => fresh(v, env),
            // System registers, MPIDR first, on every vCPU before the GIC.
            Start::Restore { vcpu: s, .. } => s.set_sys(v).map_err(Into::into),
        });
    ctl.barrier.wait();
    // Once: the GIC, now that every vCPU has its MPIDR; then the device's
    // line level, which `set_state` would overwrite; then one time base for
    // all vCPUs, so their counters keep their relative offsets.
    let mut phase2 = Ok(());
    if let Start::Restore {
        gic: Some(blob),
        skip,
        ..
    } = &start
    {
        if !skip.split(',').any(|p| p == "gic") {
            phase2 = gic_restore(blob);
        }
        dm.sync_irq_level(env.blk);
        ctl.resume_at.store(mach_time(), Ordering::SeqCst);
    }
    ctl.barrier.wait();
    let vcpu = vcpu?;
    phase1?;
    phase2?;
    if let Start::Restore { vcpu: s, skip, .. } = &start {
        if !skip.split(',').any(|p| p == "icc") {
            s.set_icc(&vcpu)?;
        }
        s.set_gprs(&vcpu)?;
        s.set_simd(&vcpu)?;
        s.set_vtimer(&vcpu, Some(ctl.resume_at.load(Ordering::SeqCst)))?;
    }
    ctl.handles[env.idx].store(vcpu.raw_handle(), Ordering::SeqCst);
    ctl.ready.fetch_add(1, Ordering::SeqCst);

    loop {
        match vcpu.run()? {
            VcpuExit::VtimerActivated => vcpu.set_vtimer_mask(false)?,
            VcpuExit::Canceled if ctl.stop.load(Ordering::SeqCst) => {
                let state = VcpuState::capture(&vcpu)?;
                ctl.report.send(())?;
                // Either a release or a dropped sender ends the thread.
                let _ = release.recv();
                return Ok(state);
            }
            VcpuExit::Canceled => {}
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(info),
                ..
            } if info.isv => {
                let pc = vcpu.get_reg(reg::HV_REG_PC)?;
                let rt = u32::from(info.register);
                let size = usize::from(info.access_size);
                match info.address {
                    FAIL_MMIO => {
                        let v = vcpu.get_reg(rt)?;
                        return Err(format!(
                            "guest vCPU {} failed check {} (detail {:#x}) at pc {pc:#x}",
                            v >> 56,
                            (v >> 48) & 0xff,
                            v & 0xffff_ffff_ffff
                        )
                        .into());
                    }
                    TRAP_MMIO => {
                        let esr = vcpu.get_reg(rt)?;
                        return Err(format!("guest exception, ESR {esr:#x}, pc {pc:#x}").into());
                    }
                    a if GIC_MMIO.contains(&a) => {
                        // Registers Apple's GIC leaves to the VMM (GICR_WAKER
                        // among them): RAZ/WI, as the HV VMM's MMIO bus does.
                        if !info.is_write && rt < 31 {
                            vcpu.set_reg(rt, 0)?;
                        }
                    }
                    a if (VIRTIO_MMIO_BASE..VIRTIO_MMIO_BASE + VIRTIO_MMIO_SLOTS * 0x1000)
                        .contains(&a) =>
                    {
                        if info.is_write {
                            let v = if rt < 31 { vcpu.get_reg(rt)? } else { 0 };
                            dm.handle_mmio_write(a, size, v)?;
                        } else {
                            let v = dm.handle_mmio_read(a, size)?;
                            if rt < 31 {
                                vcpu.set_reg(rt, v)?;
                            }
                        }
                    }
                    a => return Err(format!("unhandled MMIO {a:#x} at pc {pc:#x}").into()),
                }
                vcpu.set_reg(reg::HV_REG_PC, pc + 4)?;
            }
            other => return Err(format!("vCPU {}: unexpected exit {other:?}", env.idx).into()),
        }
    }
}

impl Vcpus {
    fn spawn(m: &Machine, starts: Vec<Start>) -> Self {
        let ncpu = starts.len();
        let (report, reports) = mpsc::channel();
        let ctl = Control::new(ncpu, report);
        let mut release = Vec::new();
        let threads = starts
            .into_iter()
            .enumerate()
            .map(|(i, start)| {
                let (tx, rx) = mpsc::channel();
                release.push(tx);
                let ctl = Arc::clone(&ctl);
                let dm = Arc::clone(&m.dm);
                let env = GuestEnv {
                    idx: i,
                    ncpu: ncpu as u64,
                    virtio: m.mmio_base,
                    intid: u64::from(m.irq),
                    blk: m.blk,
                };
                std::thread::spawn(move || vcpu_main(start, &env, &ctl, &dm, &rx))
            })
            .collect();
        Self {
            ctl,
            threads,
            release,
            reports,
        }
    }

    /// Waits until every vCPU thread entered its run loop.
    fn wait_ready(&self) -> Result<()> {
        let n = self.threads.len();
        wait_until(
            || {
                self.ctl.ready.load(Ordering::SeqCst) == n
                    || self.threads.iter().any(JoinHandle::is_finished)
            },
            Duration::from_secs(2),
            "vCPU start",
        )?;
        if self.ctl.ready.load(Ordering::SeqCst) == n {
            Ok(())
        } else {
            Err("a vCPU thread exited before running".into())
        }
    }

    /// Parks every vCPU outside `hv_vcpu_run`, still alive: the "VM
    /// stopped" state `hv_gic_state_create` needs. A joined vCPU thread
    /// destroys its vCPU, and a GIC captured after that has lost the
    /// vCPU's redistributor. Returns the rings as they were the moment the
    /// pause took hold.
    fn pause(&self, m: &Machine) -> Result<Rings> {
        self.ctl.stop.store(true, Ordering::SeqCst);
        let handles: Vec<u64> = self
            .ctl
            .handles
            .iter()
            .map(|h| h.load(Ordering::SeqCst))
            .filter(|&h| h != u64::MAX)
            .collect();
        // An exit request can miss a vCPU that is between two runs (handling
        // an MMIO exit), so repeat it until every vCPU has reported.
        let mut stopped = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        while stopped < self.threads.len() && Instant::now() < deadline {
            m.vm.exit_vcpus(&handles)?;
            while stopped < self.threads.len()
                && self.reports.recv_timeout(Duration::from_millis(1)).is_ok()
            {
                stopped += 1;
            }
        }
        if stopped < self.threads.len() {
            return Err("a vCPU did not stop".into());
        }
        Ok(m.rings())
    }

    /// Ends the vCPU threads (destroying the vCPUs) and collects the state
    /// each captured at the pause, or the first error a vCPU hit.
    fn finish(self) -> Result<Vec<VcpuState>> {
        for r in &self.release {
            let _ = r.send(());
        }
        self.threads
            .into_iter()
            .map(|t| t.join().map_err(|_| "vCPU thread panicked")?)
            .collect()
    }

    /// Pauses and finishes. A vCPU's own error explains a failed pause
    /// better than the pause does, so it wins.
    fn halt(self, m: &Machine) -> Result<Vec<VcpuState>> {
        let paused = self.pause(m);
        let states = self.finish()?;
        paused.map(|_| states)
    }
}

/// `clonefile(2)`: an APFS copy-on-write clone, O(1) in the file size.
fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    let _ = fs::remove_file(dst);
    let s = CString::new(src.as_os_str().as_bytes())?;
    let d = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are NUL-terminated paths that outlive the call.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
