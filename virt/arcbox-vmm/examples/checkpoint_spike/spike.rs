use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcbox_hv::{ExceptionClass, Gic, HvVcpu, HvVm, MemoryPermission, VcpuExit, ffi, reg, sys_reg};

use crate::common::ram::{PAGE, Ram, write_sparse};
use crate::common::vcpu::{
    Result, VcpuState, check, cntfrq, gic_capture, gic_restore, mach_time, max_rss, ms, wait_until,
};

const GUEST: &[u8] = include_bytes!("guest.bin");

const RAM_BASE: u64 = 0x4000_0000;
const FAIL_MMIO: u64 = 0x0900_0000;
const TRAP_MMIO: u64 = 0x0900_0008;
/// Distributor plus the redistributor region (`arcbox_hv::Gic` defaults).
const GIC_MMIO: std::ops::Range<u64> = 0x0800_0000..0x0A0A_0000;
/// EL1h with DAIF masked: the reset state the guest starts from.
const CPSR_EL1H: u64 = 0x3c5;
/// MPIDR_EL1 affinity of the only vCPU, as the HV VMM writes it.
const MPIDR_VCPU0: u64 = 0;

/// `state.json`: the vCPU plus the guest's counters at the checkpoint.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    ticks: u64,
    wakeups: u64,
    vcpu: VcpuState,
}

/// Everything a checkpoint holds besides guest RAM.
#[derive(Default)]
struct CpuState {
    vcpu: VcpuState,
    gic: Vec<u8>,
    /// Comma-separated parts `apply` leaves out (restore-side control only).
    skip: String,
}

enum Start {
    Fresh,
    Restore(Box<CpuState>),
}

struct Control {
    stop: AtomicBool,
    vcpu: AtomicU64,
    ready: AtomicBool,
}

pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: checkpoint_spike save|restore <dir> [run_ms] [--eager]";
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

/// Reads an optional numeric knob from the environment.
fn env_mb(name: &str, default: usize) -> Result<usize> {
    std::env::var(name).map_or(Ok(default), |v| Ok(v.parse()?))
}

fn save(dir: &Path, run_ms: u64) -> Result<()> {
    fs::create_dir_all(dir)?;
    // SPIKE_RAM_MB sizes guest RAM; SPIKE_FILL_MB pre-dirties that much of
    // it above the guest's own 4 MiB, standing in for memory a real guest
    // has touched.
    let ram_size = env_mb("SPIKE_RAM_MB", 64)? << 20;
    let fill = env_mb("SPIKE_FILL_MB", 0)? << 20;
    if fill + (4 << 20) > ram_size {
        return Err("SPIKE_FILL_MB must leave the guest's first 4 MiB".into());
    }
    let vm = HvVm::new()?;
    let ram = Ram::anon(ram_size)?;
    ram.bytes_mut()[..GUEST.len()].copy_from_slice(GUEST);
    for (i, word) in ram.bytes_mut()[4 << 20..(4 << 20) + fill]
        .chunks_exact_mut(8)
        .enumerate()
    {
        word.copy_from_slice(&(i as u64 | 1).to_le_bytes());
    }
    // SAFETY: `ram` outlives the VM's use of the mapping: the vCPU thread is
    // joined and `vm` dropped before `ram` at the end of this function.
    unsafe { vm.map_memory(ram.ptr(), RAM_BASE, ram_size, MemoryPermission::ALL)? };
    let gic = Gic::with_defaults()?;

    let (ctl, handle) = spawn_vcpu(Start::Fresh, gic.distributor_base());
    let handle = wait_ready(&ctl, handle)?;
    if let Err(e) = wait_until(|| ram.data(32) == 1, Duration::from_secs(1), "guest init") {
        stop(&vm, &ctl)?;
        let pc =
            handle.join().map_err(|_| "vCPU thread panicked")??.vcpu.gpr[reg::HV_REG_PC as usize];
        return Err(format!("{e} (guest pc {pc:#x})").into());
    }
    std::thread::sleep(Duration::from_millis(run_ms));

    let t = Instant::now();
    stop(&vm, &ctl)?;
    let cpu = handle.join().map_err(|_| "vCPU thread panicked")??;
    let paused_at = cpu.vcpu.captured_at;
    let t_cpu = t.elapsed();
    let (ticks, wakeups) = (ram.data(0), ram.data(8));

    let t = Instant::now();
    let (pages, allocated) = write_sparse(&dir.join("mem.bin"), ram.bytes())?;
    let t_mem = t.elapsed();
    fs::write(dir.join("gic.bin"), &cpu.gic)?;
    let saved = Saved {
        ticks,
        wakeups,
        vcpu: cpu.vcpu.clone().paused_at(paused_at),
    };
    fs::write(dir.join("state.json"), serde_json::to_string(&saved)?)?;
    drop(vm);

    println!(
        "saved: ticks={ticks} wakeups={wakeups} x19={}",
        cpu.vcpu.gpr[19]
    );
    println!(
        "  vcpu+gic capture {:.3} ms, gic blob {} B, memory write {:.3} ms",
        ms(t_cpu),
        cpu.gic.len(),
        ms(t_mem)
    );
    println!(
        "  mem.bin: {pages} non-zero 16 KiB pages of {}, {} KiB allocated",
        ram_size / PAGE,
        allocated / 1024
    );
    Ok(())
}

fn restore(dir: &Path, run_ms: u64, eager: bool) -> Result<()> {
    let t0 = Instant::now();
    let saved: Saved = serde_json::from_slice(&fs::read(dir.join("state.json"))?)?;
    let (ticks0, wakeups0) = (saved.ticks, saved.wakeups);
    let cpu = CpuState {
        vcpu: saved.vcpu,
        gic: fs::read(dir.join("gic.bin"))?,
        // SPIKE_SKIP leaves parts of the checkpoint out, as negative controls.
        skip: std::env::var("SPIKE_SKIP").unwrap_or_default(),
    };
    let mem = dir.join("mem.bin");
    let ram_size = usize::try_from(fs::metadata(&mem)?.len())?;

    let vm = HvVm::new()?;
    let ram = if eager {
        Ram::read_from(&mem, ram_size)?
    } else {
        Ram::map_private(&mem, ram_size)?
    };
    // SAFETY: as in `save`: `ram` outlives the VM, which is dropped first.
    unsafe { vm.map_memory(ram.ptr(), RAM_BASE, ram_size, MemoryPermission::ALL)? };
    let t_mapped = t0.elapsed();
    let last_tick_cntvct = ram.data(16);
    let gic = Gic::with_defaults()?;

    let (ctl, handle) = spawn_vcpu(Start::Restore(Box::new(cpu)), gic.distributor_base());
    let handle = wait_ready(&ctl, handle)?;
    let t_running = t0.elapsed();
    if let Err(e) = wait_until(
        || ram.data(0) > ticks0,
        Duration::from_secs(1),
        "first tick",
    ) {
        stop(&vm, &ctl)?;
        let st = handle.join().map_err(|_| "vCPU thread panicked")??.vcpu;
        return Err(format!(
            "{e}: pc {:#x} cpsr {:#x} wakeups {} CNTV_CTL {:#x} CNTV_CVAL {:#x} cntvct {:#x} masked {}",
            st.gpr[reg::HV_REG_PC as usize],
            st.gpr[reg::HV_REG_CPSR as usize],
            ram.data(8),
            st.sys_reg("CNTV_CTL_EL0"),
            st.sys_reg("CNTV_CVAL_EL0"),
            st.cntvct(mach_time()),
            st.vtimer_masked
        )
        .into());
    }
    let t_tick = t0.elapsed();
    if let Err(e) = wait_until(
        || ram.data(8) > wakeups0,
        Duration::from_secs(1),
        "first wakeup",
    ) {
        stop(&vm, &ctl)?;
        handle.join().map_err(|_| "vCPU thread panicked")??;
        return Err(e);
    }
    let t_wakeup = t0.elapsed();
    let first_tick_cntvct = ram.data(16);

    let ticks_a = ram.data(0);
    let t = Instant::now();
    std::thread::sleep(Duration::from_millis(run_ms));
    let ticks_b = ram.data(0);
    let rate = (ticks_b - ticks_a) as f64 / t.elapsed().as_secs_f64() / 1000.0;
    stop(&vm, &ctl)?;
    let after = handle.join().map_err(|_| "vCPU thread panicked")??;
    drop(vm);

    let freq = cntfrq();
    println!(
        "restored ({}): ticks {ticks0} -> {} wakeups {wakeups0} -> {} x19 {}",
        if eager { "eager copy" } else { "MAP_PRIVATE" },
        ram.data(0),
        ram.data(8),
        after.vcpu.gpr[19]
    );
    println!(
        "  memory mapped {:.3} ms, vCPU running {:.3} ms, first tick {:.3} ms, first wakeup {:.3} ms",
        ms(t_mapped),
        ms(t_running),
        ms(t_tick),
        ms(t_wakeup)
    );
    println!(
        "  guest time across the checkpoint: {:.3} ms between the last tick saved and the first resumed",
        (first_tick_cntvct.wrapping_sub(last_tick_cntvct)) as f64 * 1000.0 / freq as f64
    );
    println!("  tick rate after resume: {rate:.3} kHz (expect ~1)");
    println!("  peak process RSS: {} MiB", max_rss() >> 20);
    if !(0.8..1.2).contains(&rate) {
        return Err(format!("tick rate {rate:.3} kHz out of range").into());
    }
    Ok(())
}

fn spawn_vcpu(
    start: Start,
    gicd: u64,
) -> (Arc<Control>, std::thread::JoinHandle<Result<CpuState>>) {
    let ctl = Arc::new(Control {
        stop: AtomicBool::new(false),
        vcpu: AtomicU64::new(u64::MAX),
        ready: AtomicBool::new(false),
    });
    let thread_ctl = Arc::clone(&ctl);
    let handle = std::thread::spawn(move || vcpu_main(start, gicd, &thread_ctl));
    (ctl, handle)
}

fn vcpu_main(start: Start, gicd: u64, ctl: &Control) -> Result<CpuState> {
    let vcpu = HvVcpu::new()?;
    match start {
        Start::Fresh => {
            // The GIC binds a redistributor frame to the vCPU through its
            // MPIDR affinity, which must be set first (as the HV VMM does).
            vcpu.set_sys_reg(sys_reg::HV_SYS_REG_MPIDR_EL1, MPIDR_VCPU0)?;
            let mut gicr = 0;
            // SAFETY: `vcpu` is live on this thread; `gicr` is a valid out-pointer.
            check(unsafe { ffi::hv_gic_get_redistributor_base(vcpu.id(), &raw mut gicr) })?;
            vcpu.set_reg(reg::HV_REG_PC, RAM_BASE)?;
            vcpu.set_reg(reg::HV_REG_X0, gicd)?;
            vcpu.set_reg(reg::HV_REG_X1, gicr)?;
            vcpu.set_reg(reg::HV_REG_CPSR, CPSR_EL1H)?;
            vcpu.set_vtimer_mask(false)?;
        }
        Start::Restore(state) => apply(&vcpu, &state)?,
    }
    ctl.vcpu.store(vcpu.raw_handle(), Ordering::SeqCst);
    ctl.ready.store(true, Ordering::SeqCst);

    loop {
        match vcpu.run()? {
            VcpuExit::VtimerActivated => vcpu.set_vtimer_mask(false)?,
            // With one vCPU, its stop is also the "VM stopped" that
            // `hv_gic_state_create` requires.
            VcpuExit::Canceled if ctl.stop.load(Ordering::SeqCst) => {
                return Ok(CpuState {
                    vcpu: VcpuState::capture(&vcpu)?,
                    gic: gic_capture()?,
                    skip: String::new(),
                });
            }
            VcpuExit::Canceled => {}
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(info),
                ..
            } if info.address == FAIL_MMIO || info.address == TRAP_MMIO => {
                let value = vcpu.get_reg(u32::from(info.register))?;
                let pc = vcpu.get_reg(reg::HV_REG_PC)?;
                return Err(format!(
                    "guest reported {} {value:#x} at pc {pc:#x}",
                    if info.address == FAIL_MMIO {
                        "broken invariant, x19 ="
                    } else {
                        "unexpected exception, ESR ="
                    }
                )
                .into());
            }
            // Apple's GIC leaves a few registers to the VMM (GICR_WAKER among
            // them); treat them as RAZ/WI like the HV VMM's MMIO bus does.
            VcpuExit::Exception {
                class: ExceptionClass::DataAbort(info),
                ..
            } if (GIC_MMIO).contains(&info.address) && info.isv => {
                if !info.is_write && info.register < 31 {
                    vcpu.set_reg(u32::from(info.register), 0)?;
                }
                let pc = vcpu.get_reg(reg::HV_REG_PC)?;
                vcpu.set_reg(reg::HV_REG_PC, pc + 4)?;
            }
            other => return Err(format!("unexpected exit {other:?}").into()),
        }
    }
}

/// Writes a checkpoint into a fresh vCPU before its first run.
///
/// `s.skip` names parts to leave out: `gic`, `icc`, `simd`, `vtimer`, or
/// `sysfirst`, which restores the GIC before the system registers.
fn apply(vcpu: &HvVcpu, s: &CpuState) -> Result<()> {
    let skip = |part: &str| s.skip.split(',').any(|p| p == part);
    // System registers first: MPIDR binds the vCPU to its redistributor
    // frame, which the GIC state restored next is addressed by.
    if !skip("sysfirst") {
        s.vcpu.set_sys(vcpu)?;
    }
    if !skip("gic") {
        gic_restore(&s.gic)?;
    }
    if skip("sysfirst") {
        s.vcpu.set_sys(vcpu)?;
    }
    if !skip("icc") {
        s.vcpu.set_icc(vcpu)?;
    }
    s.vcpu.set_gprs(vcpu)?;
    if !skip("simd") {
        s.vcpu.set_simd(vcpu)?;
    }
    // Resume the guest's virtual counter where it stopped.
    let resume_at = (!skip("vtimer")).then(mach_time);
    s.vcpu.set_vtimer(vcpu, resume_at)?;
    Ok(())
}

fn stop(vm: &HvVm, ctl: &Control) -> Result<()> {
    ctl.stop.store(true, Ordering::SeqCst);
    vm.exit_vcpus(&[ctl.vcpu.load(Ordering::SeqCst)])?;
    Ok(())
}

type VcpuThread = std::thread::JoinHandle<Result<CpuState>>;

/// Waits for the vCPU thread to enter its run loop, surfacing its error if
/// it stopped first.
fn wait_ready(ctl: &Control, handle: VcpuThread) -> Result<VcpuThread> {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !ctl.ready.load(Ordering::SeqCst) {
        if handle.is_finished() {
            handle.join().map_err(|_| "vCPU thread panicked")??;
            return Err("vCPU thread exited before running".into());
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for vCPU start".into());
        }
        std::thread::sleep(Duration::from_micros(20));
    }
    Ok(handle)
}
