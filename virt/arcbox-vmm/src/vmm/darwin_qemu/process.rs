//! The QEMU child process: spawn, exit, stop, and orphans of a daemon that
//! crashed.
//!
//! QEMU starts with `posix_spawn` and `POSIX_SPAWN_CLOEXEC_DEFAULT`, so it
//! inherits exactly the descriptors the command line names: macOS has no
//! `SOCK_CLOEXEC`, and a daemon socket that leaked into QEMU would outlive
//! the daemon. It runs in its own process group, so a terminal's Ctrl-C
//! reaches the daemon, which stops the guest in order, and not QEMU.
//!
//! QEMU writes its pid to a pidfile under the run directory and holds a
//! `F_SETLK` write lock on it until it exits. A daemon that died leaves
//! its QEMU running with `docker.img` open; the next daemon finds the
//! holder through `F_GETLK`, which names the locking process itself, so a
//! recycled pid in a stale file can never be signalled.

use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Result, VmmError};

/// How long QEMU gets to exit after each escalation step.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Lines of QEMU's stderr kept for error messages.
const STDERR_TAIL: usize = 20;

unsafe extern "C" {
    /// `<spawn.h>`: keep `fd` open in the child despite
    /// `POSIX_SPAWN_CLOEXEC_DEFAULT`, clearing its close-on-exec flag.
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

/// A running QEMU and the thread that reaps it.
pub(super) struct QemuProcess {
    pid: libc::pid_t,
    exit: Arc<(Mutex<Option<i32>>, Condvar)>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl QemuProcess {
    /// Spawns `binary` with `args`, inheriting `fds` at their numbers.
    /// `on_exit` runs on the reaper thread once QEMU has exited.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be spawned.
    pub(super) fn spawn(
        binary: &Path,
        args: &[OsString],
        fds: &[RawFd],
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        let (stderr_read, stderr_write) = pipe()?;
        let pid = posix_spawn(binary, args, fds, stderr_write.as_raw_fd())?;
        drop(stderr_write);
        tracing::info!(pid, binary = %binary.display(), "QEMU started");

        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr);
        let logger = std::thread::Builder::new()
            .name("qemu-stderr".into())
            .spawn(move || {
                for line in BufReader::new(std::fs::File::from(stderr_read)).lines() {
                    let Ok(line) = line else { break };
                    tracing::warn!("qemu: {line}");
                    if let Ok(mut tail) = tail.lock() {
                        if tail.len() == STDERR_TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            })?;

        let exit = Arc::new((Mutex::new(None), Condvar::new()));
        let exited = Arc::clone(&exit);
        let reaper = std::thread::Builder::new()
            .name("qemu-wait".into())
            .spawn(move || {
                let status = wait(pid);
                tracing::info!(pid, status, "QEMU exited");
                on_exit();
                let (lock, cvar) = &*exited;
                if let Ok(mut slot) = lock.lock() {
                    *slot = Some(status);
                }
                cvar.notify_all();
            })?;

        Ok(Self {
            pid,
            exit,
            stderr,
            threads: vec![logger, reaper],
        })
    }

    /// QEMU's last stderr lines, oldest first.
    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    /// Whether QEMU has exited.
    pub(super) fn has_exited(&self) -> bool {
        self.wait_exit(Some(Duration::ZERO))
    }

    /// Waits up to `timeout` (`None`: for good) for QEMU to exit; `true`
    /// once it has.
    pub(super) fn wait_exit(&self, timeout: Option<Duration>) -> bool {
        let (lock, cvar) = &*self.exit;
        let Ok(slot) = lock.lock() else { return true };
        let running = |status: &mut Option<i32>| status.is_none();
        match timeout {
            Some(timeout) => cvar
                .wait_timeout_while(slot, timeout, running)
                .map_or(true, |(slot, _)| slot.is_some()),
            None => cvar.wait_while(slot, running).is_ok(),
        }
    }

    /// Ends QEMU: `quit` (the caller's closure, over QMP) first, then
    /// SIGTERM, then SIGKILL, each given [`EXIT_GRACE`]. Returns QEMU's
    /// last stderr lines once the process is gone and its threads joined.
    pub(super) fn terminate(mut self, quit: impl FnOnce()) -> String {
        if !self.has_exited() {
            quit();
            for signal in [libc::SIGTERM, libc::SIGKILL] {
                if self.wait_exit(Some(EXIT_GRACE)) {
                    break;
                }
                tracing::warn!(pid = self.pid, signal, "QEMU still running; signalling it");
                // SAFETY: `pid` is our unreaped child, so the number cannot
                // have been reused.
                unsafe { libc::kill(self.pid, signal) };
            }
            self.wait_exit(None);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.stderr_tail()
    }
}

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

fn c_string(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|_| VmmError::config("QEMU argument contains a NUL byte"))
}

/// `posix_spawn` with stdin and stdout on `/dev/null`, stderr on `stderr`,
/// `fds` inherited, every other descriptor closed, its own process group,
/// and default signal handling.
fn posix_spawn(binary: &Path, args: &[OsString], fds: &[RawFd], stderr: RawFd) -> Result<i32> {
    let path = c_string(binary.as_os_str().as_bytes())?;
    let argv: Vec<CString> = std::iter::once(Ok(path.clone()))
        .chain(args.iter().map(|a| c_string(a.as_bytes())))
        .collect::<Result<_>>()?;
    let envp: Vec<CString> = std::env::vars_os()
        .map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            c_string(&entry)
        })
        .collect::<Result<_>>()?;
    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut envp_ptrs: Vec<*mut libc::c_char> =
        envp.iter().map(|e| e.as_ptr().cast_mut()).collect();
    envp_ptrs.push(std::ptr::null_mut());
    let dev_null = c"/dev/null";

    let check = |ret: libc::c_int, what: &str| -> Result<()> {
        if ret == 0 {
            Ok(())
        } else {
            Err(VmmError::Device(format!(
                "{what}: {}",
                std::io::Error::from_raw_os_error(ret)
            )))
        }
    };

    // SAFETY: the attribute and file-action objects are initialized before
    // use and destroyed on every path below; every pointer handed to
    // posix_spawn (path, argv, envp, /dev/null) outlives the call, and the
    // vectors are NULL-terminated.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        check(
            libc::posix_spawnattr_init(&raw mut attr),
            "posix_spawnattr_init",
        )?;
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        if let Err(e) = check(
            libc::posix_spawn_file_actions_init(&raw mut actions),
            "posix_spawn_file_actions_init",
        ) {
            libc::posix_spawnattr_destroy(&raw mut attr);
            return Err(e);
        }
        let result = (|| {
            let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
                | libc::POSIX_SPAWN_SETPGROUP
                | libc::POSIX_SPAWN_SETSIGMASK
                | libc::POSIX_SPAWN_SETSIGDEF;
            check(
                libc::posix_spawnattr_setflags(&raw mut attr, flags as libc::c_short),
                "posix_spawnattr_setflags",
            )?;
            check(
                libc::posix_spawnattr_setpgroup(&raw mut attr, 0),
                "posix_spawnattr_setpgroup",
            )?;
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut mask);
            check(
                libc::posix_spawnattr_setsigmask(&raw mut attr, &raw const mask),
                "posix_spawnattr_setsigmask",
            )?;
            // The daemon ignores SIGPIPE, as every Rust program does; QEMU
            // decides for itself.
            let mut defaults: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut defaults);
            libc::sigaddset(&raw mut defaults, libc::SIGPIPE);
            check(
                libc::posix_spawnattr_setsigdefault(&raw mut attr, &raw const defaults),
                "posix_spawnattr_setsigdefault",
            )?;
            check(
                libc::posix_spawn_file_actions_addopen(
                    &raw mut actions,
                    0,
                    dev_null.as_ptr(),
                    libc::O_RDONLY,
                    0,
                ),
                "stdin",
            )?;
            check(
                libc::posix_spawn_file_actions_addopen(
                    &raw mut actions,
                    1,
                    dev_null.as_ptr(),
                    libc::O_WRONLY,
                    0,
                ),
                "stdout",
            )?;
            check(
                libc::posix_spawn_file_actions_adddup2(&raw mut actions, stderr, 2),
                "stderr",
            )?;
            for &fd in fds {
                check(
                    posix_spawn_file_actions_addinherit_np(&raw mut actions, fd),
                    "inherit",
                )?;
            }
            let mut pid = 0;
            check(
                libc::posix_spawn(
                    &raw mut pid,
                    path.as_ptr(),
                    &raw const actions,
                    &raw const attr,
                    argv_ptrs.as_ptr(),
                    envp_ptrs.as_ptr(),
                ),
                "posix_spawn QEMU",
            )?;
            Ok(pid)
        })();
        libc::posix_spawn_file_actions_destroy(&raw mut actions);
        libc::posix_spawnattr_destroy(&raw mut attr);
        result
    }
}

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
