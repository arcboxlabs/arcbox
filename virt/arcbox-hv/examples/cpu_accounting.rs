//! Compare Hypervisor execution time with host thread CPU time and elapsed time.
//! Run with `busy`, `wfi`, or `host-sleep`. Each case lasts three seconds.

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "gic"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use arcbox_hv::{Gic, HvVcpu, HvVm, MemoryPermission, VcpuExit, check, ffi, reg};
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn execution_ticks(cpu: &HvVcpu) -> Result<u64, arcbox_hv::HvError> {
        let mut value = 0;
        // SAFETY: The calling thread owns the vCPU and the output pointer is valid.
        check(unsafe { ffi::hv_vcpu_get_exec_time(cpu.id(), &raw mut value) })?;
        Ok(value)
    }

    fn thread_seconds() -> Result<f64, std::io::Error> {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: The output pointer references an initialized timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut value) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(value.tv_sec as f64 + value.tv_nsec as f64 / 1e9)
    }

    let mode = std::env::args()
        .nth(1)
        .ok_or("expected busy, wfi, or host-sleep")?;
    if !matches!(mode.as_str(), "busy" | "wfi" | "host-sleep") {
        return Err("expected busy, wfi, or host-sleep".into());
    }
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut timebase = Timebase { numer: 0, denom: 0 };
    // SAFETY: The output pointer references an initialized timebase structure.
    let result = unsafe { mach_timebase_info(&raw mut timebase) };
    if result != 0 {
        return Err(format!("mach_timebase_info: {result}").into());
    }

    #[repr(C, align(16384))]
    struct Page([u32; 4096]);
    let mut page = Box::new(Page([0; 4096]));
    // ADD X0, X0, #1 or WFI; B back one instruction. Only the host can end the run.
    page.0[..2].copy_from_slice(&[
        if mode == "wfi" {
            0xd503_207f
        } else {
            0x9100_0400
        },
        0x17ff_ffff,
    ]);
    let vm = HvVm::new()?;
    let _gic = Gic::with_defaults()?;
    const GPA: u64 = 0x4000_0000;
    // SAFETY: The aligned page remains allocated until after the VM is destroyed.
    unsafe {
        vm.map_memory(
            page.0.as_mut_ptr().cast(),
            GPA,
            16384,
            MemoryPermission::ALL,
        )?;
    }
    let cpu = HvVcpu::new()?;
    cpu.set_reg(reg::HV_REG_PC, GPA)?;
    cpu.set_reg(reg::HV_REG_CPSR, 0x3c5)?;
    let ticks_before = execution_ticks(&cpu)?;
    let thread_before = thread_seconds()?;
    let started = Instant::now();
    println!(
        "{{\"ready\":true,\"pid\":{},\"mode\":\"{mode}\"}}",
        std::process::id()
    );
    std::io::stdout().flush()?;
    let mut exits = 0;
    if mode == "host-sleep" {
        std::thread::sleep(Duration::from_secs(3));
    } else {
        let mut id = cpu.raw_handle();
        let kicker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            // SAFETY: The owner joins this thread before destroying the vCPU.
            check(unsafe { ffi::hv_vcpus_exit(&raw mut id, 1) })
        });
        let exit = cpu.run();
        // Join before dropping the vCPU, including when the run fails.
        kicker.join().map_err(|_| "kicker panicked")??;
        if !matches!(exit?, VcpuExit::Canceled) {
            return Err("guest returned before the host cancellation".into());
        }
        exits = 1;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let thread_cpu = thread_seconds()? - thread_before;
    let ticks = execution_ticks(&cpu)? - ticks_before;
    let execution = ticks as f64 * f64::from(timebase.numer) / f64::from(timebase.denom) / 1e9;
    println!(
        "{{\"mode\":\"{mode}\",\"wall_seconds\":{elapsed:.9},\"thread_cpu_seconds\":{thread_cpu:.9},\"hv_execution_seconds\":{execution:.9},\"hv_raw_ticks\":{ticks},\"timebase_numer\":{},\"timebase_denom\":{},\"userspace_exits\":{exits}}}",
        timebase.numer, timebase.denom
    );
    drop(cpu);
    vm.unmap_memory(GPA, 16384)?;
    drop(vm);
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64", feature = "gic")))]
fn main() {
    panic!("requires Apple Silicon macOS and --features gic");
}
