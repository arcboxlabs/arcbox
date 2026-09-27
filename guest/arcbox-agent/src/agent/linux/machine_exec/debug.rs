//! Container debug sessions (guest side).
//!
//! `abctl debug <container>` runs a shell from the *agent* root filesystem — so
//! it works even against a shell-less image such as `gcr.io/distroless/static`
//! — while giving that shell the target container's view of the system.
//!
//! ## Why nsenter-style, not a sidecar container?
//!
//! A sidecar (`docker run --pid=container:… --network=container:…`) would need
//! a debug image with the tools baked in, pulled from a registry — but the
//! ArcBox CDN is unreachable from this environment, so there is no offline
//! tools image to run. The agent root filesystem already ships a musl busybox,
//! so the session execs *that* shell after entering the target's namespaces:
//! no external artifact, and the container image is never modified.
//!
//! ## Entering the namespaces
//!
//! The entry happens in the child that [`std::process::Command`] forks, in the
//! pre-exec step (see [`NsEnter::apply`]):
//!
//! - the container's **network, IPC and UTS** namespaces are entered with
//!   `setns` on the calling task, so `ip addr` and `hostname` reflect the
//!   container;
//! - the container's **PID namespace** is entered as well. `setns(CLONE_NEWPID)`
//!   moves only a task's *children* into the namespace, so the shell must be one
//!   fork deeper than the process `Command` spawned. Being a real member of the
//!   PID namespace — not merely a reader of its procfs — is what lets `ps`,
//!   `kill <pid>` and `/proc/<pid>/…` all speak the container's PIDs;
//! - a **fresh procfs** is mounted over `/proc` from inside the PID namespace,
//!   so `/proc` lists the container's processes (a procfs reports the PID
//!   namespace it was mounted from, to any reader), and likewise a **fresh
//!   sysfs** over `/sys`, so `/sys/class/net` shows the container's interfaces
//!   rather than the agent's;
//! - the working directory is the container root (`/proc/<pid>/root`), so the
//!   shell lands in the container filesystem while its own binaries still
//!   resolve against the agent root — no chroot, so the tools keep working.
//!
//! The mounts happen in a private mount namespace (`CLONE_NEWNS` + a recursive
//! private remount of `/`), so nothing propagates back to the agent: the
//! session leaves no residue, and there is nothing to unmount when it ends.
//!
//! ## Tools
//!
//! The agent rootfs links only the handful of busybox applets its boot
//! sequence runs, so the session puts a link for every applet busybox was
//! built with last on the shell's `PATH` ([`tools_dir`]): `ps`, `grep`, `wget`
//! and the rest resolve by name, and nothing the agent root ships is
//! shadowed. The links grant nothing new — a root shell can run any applet as
//! `busybox <name>` anyway.
//!
//! ## The extra fork and `Command`'s spawn protocol
//!
//! `Command::spawn` forks a child that reports exec success or failure to the
//! parent by closing (on exec) or writing to a `CLOEXEC` sync pipe. The debug
//! shell has to run one fork deeper, so [`NsEnter::apply`] forks again from
//! inside pre-exec: the **grandchild** returns and goes on to exec the shell
//! (it keeps the authoritative sync-pipe end, so `spawn()` still returns
//! exactly when the shell execs, or with the grandchild's pre-exec errno), and
//! the **intermediate** drops its inherited copy of that pipe with `close_range`
//! — otherwise `spawn()` would block forever waiting for an end that only closes
//! when the session ends — then waits for the grandchild and mirrors its exit
//! status, so the machine-exec session loop (`session.rs`) sees an ordinary
//! child with the right exit code or signal. A parent-death signal ties the
//! grandchild's life to the intermediate, so killing the intermediate's process
//! group (what the session does when the host goes away) takes the shell with
//! it.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use anyhow::Context as _;

use crate::rpc::ErrorResponse;

/// Where [`tools_dir`] links the applets: tmpfs, so they go with the machine.
const TOOLS_DIR: &str = "/run/arcbox/debug-tools";

/// The agent root's multi-call busybox.
const BUSYBOX: &str = "/bin/busybox";

/// A resolved debug target: open handles to the container's namespaces and its
/// root directory. Everything the async-signal-unsafe pre-exec closure needs is
/// prepared here, before the fork — no allocation happens in [`Self::apply`].
pub(super) struct NsEnter {
    net: OwnedFd,
    ipc: OwnedFd,
    uts: OwnedFd,
    /// The container's PID namespace; entered so children join it.
    pid: OwnedFd,
    /// The container root (`/proc/<pid>/root`), `fchdir`'d to as the shell's cwd.
    root: OwnedFd,
}

impl NsEnter {
    /// Resolves `container` to its init PID via the Docker Engine API and opens
    /// handles to the namespaces the session enters and to the container root.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorResponse`] if the container is unknown or not running,
    /// or if a handle cannot be opened.
    pub(super) async fn resolve(container: &str) -> Result<Self, ErrorResponse> {
        let pid = container_init_pid(container).await?;
        let open = |path: String| -> Result<OwnedFd, ErrorResponse> {
            std::fs::File::open(&path)
                .map(OwnedFd::from)
                .map_err(|e| ErrorResponse::new(500, format!("open {path}: {e}")))
        };
        Ok(Self {
            net: open(format!("/proc/{pid}/ns/net"))?,
            ipc: open(format!("/proc/{pid}/ns/ipc"))?,
            uts: open(format!("/proc/{pid}/ns/uts"))?,
            pid: open(format!("/proc/{pid}/ns/pid"))?,
            root: open(format!("/proc/{pid}/root"))?,
        })
    }

    /// Enters the target namespaces and sets up the container view, returning in
    /// the grandchild that goes on to exec the shell.
    ///
    /// Runs post-fork / pre-exec, so it is async-signal-safe: only raw syscalls
    /// on descriptors prepared before the fork, no allocation. The intermediate
    /// process never returns from here — it waits for the grandchild and exits
    /// in its place.
    pub(super) fn apply(&self) -> io::Result<()> {
        // Network, IPC and UTS entry take effect on the calling task; the PID
        // namespace takes effect on its children, i.e. the fork below.
        setns(self.net.as_raw_fd(), libc::CLONE_NEWNET)?;
        setns(self.ipc.as_raw_fd(), libc::CLONE_NEWIPC)?;
        setns(self.uts.as_raw_fd(), libc::CLONE_NEWUTS)?;
        setns(self.pid.as_raw_fd(), libc::CLONE_NEWPID)?;

        match unsafe { libc::fork() } {
            -1 => Err(io::Error::last_os_error()),
            0 => self.setup_child(),
            grandchild => reap_and_exit(grandchild),
        }
    }

    /// The grandchild's setup: a private mount namespace with a fresh procfs for
    /// the container's PID namespace, and the container root as the cwd. Returns
    /// `Ok` so the caller goes on to set up the terminal and exec the shell.
    fn setup_child(&self) -> io::Result<()> {
        // Die if the intermediate does, so a killed session takes the shell
        // with it even after the shell leaves the intermediate's process group.
        cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
        // A private mount namespace so the procfs mount never reaches the agent.
        cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
        cvt(unsafe {
            libc::mount(
                c"none".as_ptr(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        })?;
        // A fresh procfs reports the mounter's PID namespace — the container's,
        // since this task is now a member — so `/proc` lists its processes.
        cvt(unsafe {
            libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                0,
                std::ptr::null(),
            )
        })?;
        // Likewise a fresh sysfs reports the entered network namespace, so
        // `/sys/class/net` lists the container's interfaces and not the agent's.
        cvt(unsafe {
            libc::mount(
                c"sysfs".as_ptr(),
                c"/sys".as_ptr(),
                c"sysfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        })?;
        // Land in the container filesystem (its binaries still resolve against
        // the agent root, which stays this task's `/`).
        cvt(unsafe { libc::fchdir(self.root.as_raw_fd()) })?;
        Ok(())
    }
}

/// Waits for the debug shell (`grandchild`) and exits in its place, mirroring
/// how it ended so the session loop reports the right exit code or signal.
///
/// First drops every inherited descriptor above stdio — most importantly this
/// process's copy of `Command`'s `CLOEXEC` sync pipe, which the grandchild still
/// holds: until this copy closes, the parent's `spawn()` blocks waiting for an
/// end that would otherwise stay open for the whole session. Then it leads its
/// own process group, so the session's group-kill (host gone) reaches it and,
/// through the parent-death signal, the shell.
fn reap_and_exit(grandchild: libc::pid_t) -> ! {
    // `close_range` needs Linux 5.9+; fall back to a bounded loop on older
    // kernels. `libc::syscall` reads the variadic args as `c_long`.
    let (first, last, flags) = (
        3 as libc::c_long,
        libc::c_uint::MAX as libc::c_long,
        0 as libc::c_long,
    );
    if unsafe { libc::syscall(libc::SYS_close_range, first, last, flags) } == -1 {
        for fd in 3..1024 {
            unsafe { libc::close(fd) };
        }
    }
    unsafe { libc::setsid() };

    let mut status: libc::c_int = 0;
    while unsafe { libc::waitpid(grandchild, std::ptr::addr_of_mut!(status), 0) } == -1 {
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }

    if libc::WIFSIGNALED(status) {
        // Re-raise so the parent sees the same signal death the shell had.
        let signal = libc::WTERMSIG(status);
        unsafe { libc::signal(signal, libc::SIG_DFL) };
        unsafe { libc::raise(signal) };
        unsafe { libc::_exit(128 + signal) };
    }
    unsafe { libc::_exit(libc::WEXITSTATUS(status)) }
}

/// The directory of busybox applet links for the shell's `PATH`, populated by
/// the agent's first debug session.
pub(super) async fn tools_dir() -> Result<&'static str, ErrorResponse> {
    static INSTALLED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INSTALLED
        .get_or_try_init(|| install_tools(Path::new(BUSYBOX), Path::new(TOOLS_DIR)))
        .await
        .map_err(|e| ErrorResponse::new(500, format!("install debug tools: {e:#}")))?;
    Ok(TOOLS_DIR)
}

/// Links every applet `busybox --list` reports into `dir`, keeping the links
/// an earlier agent left on the same boot.
async fn install_tools(busybox: &Path, dir: &Path) -> anyhow::Result<()> {
    let list = tokio::process::Command::new(busybox)
        .arg("--list")
        .output()
        .await
        .with_context(|| format!("run {} --list", busybox.display()))?;
    anyhow::ensure!(
        list.status.success(),
        "{} --list: {}",
        busybox.display(),
        list.status
    );
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for applet in String::from_utf8_lossy(&list.stdout).lines() {
        match std::os::unix::fs::symlink(busybox, dir.join(applet)) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(e).with_context(|| format!("link {applet}"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Inspects `container` over the Docker Engine API and returns its init PID.
async fn container_init_pid(container: &str) -> Result<u32, ErrorResponse> {
    let info = crate::docker_events::docker_get(&format!("/containers/{container}/json"))
        .await
        .map_err(|e| ErrorResponse::new(502, format!("docker inspect {container}: {e}")))?;

    let running = info
        .pointer("/State/Running")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let pid = info
        .pointer("/State/Pid")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    if running && pid != 0 {
        return u32::try_from(pid)
            .map_err(|_| ErrorResponse::new(500, "container init PID out of range"));
    }

    // A 404 body carries `{"message": "No such container: …"}` — a missing
    // container, distinct from one that exists but is stopped
    // (`State.Running == false`). They map to different Connect codes (404 →
    // NotFound, 412 → FailedPrecondition) so the CLI can tell the user to
    // check the name versus start the container.
    if let Some(message) = info.pointer("/message").and_then(serde_json::Value::as_str) {
        return Err(ErrorResponse::new(404, message.to_owned()));
    }
    Err(ErrorResponse::new(
        412,
        format!("container '{container}' is not running"),
    ))
}

fn setns(fd: libc::c_int, nstype: libc::c_int) -> io::Result<()> {
    cvt(unsafe { libc::setns(fd, nstype) })
}

fn cvt(ret: libc::c_int) -> io::Result<()> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[tokio::test]
    async fn every_listed_applet_is_linked_again_after_an_agent_restart() {
        let root = tempfile::tempdir().unwrap();
        let busybox = root.path().join("busybox");
        std::fs::write(&busybox, "#!/bin/sh\nprintf 'ps\\ngrep\\n'\n").unwrap();
        std::fs::set_permissions(&busybox, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.path().join("tools");

        install_tools(&busybox, &dir).await.unwrap();
        // A respawned agent finds the links its predecessor made.
        install_tools(&busybox, &dir).await.unwrap();

        for applet in ["ps", "grep"] {
            assert_eq!(std::fs::read_link(dir.join(applet)).unwrap(), busybox);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }
}
