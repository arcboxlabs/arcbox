//! QEMU processes a daemon that crashed left behind.
//!
//! QEMU writes its pid to a pidfile under the run directory and holds a
//! `F_SETLK` write lock on it until it exits. A daemon that died leaves
//! its QEMU running with `docker.img` open; the next daemon finds the
//! holder through `F_GETLK`, which names the locking process itself, so a
//! recycled pid in a stale file can never be signalled.

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long QEMU gets to exit after each signal.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The pid of the process holding QEMU's lock on `pidfile`, if any.
fn lock_holder(pidfile: &Path) -> Option<libc::pid_t> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pidfile)
        .ok()?;
    let mut lock = libc::flock {
        l_start: 0,
        l_len: 0,
        l_pid: 0,
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
    };
    // SAFETY: F_GETLK on an open descriptor with a valid `flock`.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &raw mut lock) } != 0 {
        return None;
    }
    (lock.l_type != libc::F_UNLCK as libc::c_short).then_some(lock.l_pid)
}

/// Stops every QEMU a previous daemon left running from `run_dir`, then
/// removes the pidfiles. Call it at startup, before anything waits for
/// `docker.img` to be released. Returns how many it stopped.
///
/// The VM's guest state is lost, as at a power cut; its disks were last
/// written by a QEMU that nothing could reach any more.
#[must_use]
pub fn reap_orphaned_qemu(run_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(run_dir) else {
        return 0;
    };
    let mut reaped = 0;
    for entry in entries.flatten() {
        let pidfile = entry.path();
        if pidfile.extension().is_none_or(|ext| ext != "pid") {
            continue;
        }
        if let Some(pid) = lock_holder(&pidfile) {
            tracing::warn!(pid, pidfile = %pidfile.display(), "stopping QEMU left by a previous daemon");
            reaped += 1;
            for signal in [libc::SIGTERM, libc::SIGKILL] {
                // SAFETY: `pid` holds the lock right now; a lock is
                // released when its holder exits, so the holder is alive.
                unsafe { libc::kill(pid, signal) };
                let deadline = Instant::now() + EXIT_GRACE;
                while lock_holder(&pidfile).is_some() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if lock_holder(&pidfile).is_none() {
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(&pidfile);
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    use crate::error::Result;

    fn pipe() -> Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: pipe(2) returned two fresh descriptors we now own.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&read, &write] {
            // SAFETY: F_SETFD on a descriptor we own.
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        Ok((read, write))
    }

    /// Reaps `pid`, returning its raw wait status.
    fn wait(pid: libc::pid_t) -> i32 {
        let mut status = 0;
        loop {
            // SAFETY: waitpid on our own child with a valid status pointer.
            let ret = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            if ret == pid {
                return status;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return -1;
            }
        }
    }

    /// A stale pidfile — its QEMU died without removing it — names a pid
    /// that may now belong to anything. Nobody holds its lock, so it is
    /// removed and nothing is signalled.
    #[test]
    fn stale_pidfiles_are_removed_without_signalling() {
        let dir = tempfile::tempdir().unwrap();
        // Our own pid: signalling it would kill the test.
        std::fs::write(
            dir.path().join("vm.pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        std::fs::write(dir.path().join("other.txt"), "kept").unwrap();
        assert_eq!(reap_orphaned_qemu(dir.path()), 0);
        assert!(!dir.path().join("vm.pid").exists());
        assert!(dir.path().join("other.txt").exists());
    }

    /// A process holding the lock the way QEMU does is stopped and its file
    /// removed; the lock's holder, not the file's contents, names it.
    #[test]
    fn a_locked_pidfile_names_the_process_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("vm.pid");
        let path = CString::new(pidfile.as_os_str().as_bytes()).unwrap();
        let (ready_read, ready_write) = pipe().unwrap();
        // SAFETY: the child calls only async-signal-safe functions (open,
        // write, fcntl, pause) before SIGTERM ends it.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: as above; the child never returns into the test.
            unsafe {
                let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600);
                // The file names a pid that is not the holder.
                libc::write(fd, b"1\n".as_ptr().cast(), 2);
                let lock = libc::flock {
                    l_start: 0,
                    l_len: 0,
                    l_pid: 0,
                    l_type: libc::F_WRLCK as libc::c_short,
                    l_whence: libc::SEEK_SET as libc::c_short,
                };
                libc::fcntl(fd, libc::F_SETLK, &raw const lock);
                libc::write(ready_write.as_raw_fd(), b"x".as_ptr().cast(), 1);
                loop {
                    libc::pause();
                }
            }
        }
        drop(ready_write);
        let mut byte = [0u8];
        std::io::Read::read_exact(&mut std::fs::File::from(ready_read), &mut byte).unwrap();
        assert_eq!(lock_holder(&pidfile), Some(pid));

        assert_eq!(reap_orphaned_qemu(dir.path()), 1);
        let status = wait(pid);
        assert!(
            libc::WIFSIGNALED(status),
            "the holder was signalled: {status:#x}"
        );
        assert!(!pidfile.exists());
    }
}
