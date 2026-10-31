//! vCPU and GIC state (from `arcbox_hv::snapshot`) plus the helpers the HV
//! checkpoint spikes share.

use std::error::Error;
use std::time::{Duration, Instant};

pub use arcbox_hv::snapshot::{VcpuState, gic_capture, gic_restore, mach_time};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub fn check(ret: arcbox_hv::ffi::hv_return_t) -> Result<()> {
    arcbox_hv::check(ret).map_err(Into::into)
}

pub fn hex(s: &str) -> std::result::Result<u64, std::num::ParseIntError> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
}

pub fn cntfrq() -> u64 {
    let f: u64;
    // SAFETY: reading the architected counter frequency has no side effects.
    unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    f
}

/// Peak resident set of this process: guest pages the VM actually faulted
/// in, plus the process's own. Untouched `MAP_PRIVATE` file pages do not count.
pub fn max_rss() -> u64 {
    // SAFETY: `rusage` is plain data that `getrusage` fills in.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer for the calling process.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut ru) };
    // macOS reports `ru_maxrss` in bytes.
    u64::try_from(ru.ru_maxrss).unwrap_or(0)
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !f() {
        if Instant::now() > deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::sleep(Duration::from_micros(20));
    }
    Ok(())
}
