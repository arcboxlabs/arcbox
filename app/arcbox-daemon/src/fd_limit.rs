//! Raises the daemon's open-file soft limit at startup.
//!
//! launchd starts a job with the soft `RLIMIT_NOFILE` of `launchctl limit
//! maxfiles`, 256 on a stock Mac, and neither the Desktop's LaunchDaemon
//! plist nor the `abctl _install` agent sets `SoftResourceLimits`. A daemon
//! started from a shell inherits the shell's, also 256 by default. Every
//! Docker client costs the daemon a host socket and a vsock stream to the
//! guest, so 256 runs out in ordinary use. Raising the limit in the process
//! covers every way the daemon is started.

use std::io;

/// The soft limit before and after [`raise`].
#[derive(Debug, Clone, Copy)]
pub struct Raised {
    pub from: libc::rlim_t,
    pub to: libc::rlim_t,
}

/// Raises the soft `RLIMIT_NOFILE` to the most descriptors the kernel lets
/// this process open: the hard limit, and on macOS no more than
/// `kern.maxfilesperproc`. Never lowers it.
pub fn raise() -> io::Result<Raised> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one `rlimit` through a pointer that is valid
    // for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let from = limit.rlim_cur;
    let target = limit.rlim_max.min(per_process_ceiling());
    if from >= target {
        return Ok(Raised { from, to: from });
    }
    limit.rlim_cur = target;
    // SAFETY: setrlimit reads one `rlimit` through a pointer that is valid
    // for the duration of the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Raised { from, to: target })
}

/// The kernel's per-process descriptor ceiling, whatever the soft limit says.
///
/// macOS refuses a descriptor at or above `kern.maxfilesperproc` even under a
/// higher soft limit. setrlimit(2) documents `EINVAL` for a soft limit above
/// it; macOS 26.3 accepts and stores one, `RLIM_INFINITY` included, which then
/// misreports the real ceiling to whatever reads the limit back, child
/// processes included. Hence the clamp, with the man page's `OPEN_MAX` when
/// the sysctl cannot be read.
#[cfg(target_os = "macos")]
fn per_process_ceiling() -> libc::rlim_t {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, `value`/`len` describe a writable
    // buffer of exactly `len` bytes, and no new value is passed in.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && value > 0 {
        value as libc::rlim_t
    } else {
        OPEN_MAX
    }
}

/// `OPEN_MAX` from `<sys/syslimits.h>`, which the `libc` crate does not export.
#[cfg(target_os = "macos")]
const OPEN_MAX: libc::rlim_t = 10240;

/// Linux already bounds the hard limit by `fs.nr_open`.
#[cfg(not(target_os = "macos"))]
fn per_process_ceiling() -> libc::rlim_t {
    libc::RLIM_INFINITY
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soft_and_hard() -> (libc::rlim_t, libc::rlim_t) {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: as in `raise`.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
            0
        );
        (limit.rlim_cur, limit.rlim_max)
    }

    #[test]
    fn raises_the_soft_limit_to_the_kernel_ceiling_and_never_lowers_it() {
        let (_, hard) = soft_and_hard();
        // Read the ceiling the way an operator would, not through the code
        // under test.
        #[cfg(target_os = "macos")]
        let ceiling: libc::rlim_t = {
            let out = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.maxfilesperproc"])
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        #[cfg(not(target_os = "macos"))]
        let ceiling = hard;

        let raised = raise().unwrap();
        assert_eq!(raised.to, raised.from.max(hard.min(ceiling)));
        assert_eq!(soft_and_hard().0, raised.to);
    }
}
