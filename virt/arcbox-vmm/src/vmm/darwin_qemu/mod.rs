//! macOS QEMU TCG backend: the System VM in a `qemu-system-aarch64` child,
//! for Macs without hardware virtualization.
//!
//! QEMU owns the vCPUs, guest RAM and the virtio transport; the daemon keeps
//! what it owns on every backend. `hvc0` and `hvc1` are `virtconsole` ports
//! whose socketpairs feed the engine's serial drain; QMP drives the
//! lifecycle.
//! Every descriptor QEMU needs is inherited at spawn (`process`), so it
//! opens no socket of its own.
//!
//! NIC1, virtio-fs and vsock are not attached yet: the guest boots without
//! a network or its shares, and the host cannot reach the agent.

mod args;
mod process;
mod qmp;

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::{Result, VmmError};
use crate::vmm::{QemuHost, VmmConfig};

pub use process::reap_orphaned_qemu;

/// A QEMU machine: the command line it was given, the process once
/// started, and the host ends of every channel into it.
pub struct QemuMachine {
    config: VmmConfig,
    host: QemuHost,
    /// The owner's run flag; cleared when the guest goes down.
    running: Arc<AtomicBool>,
    reset_requested: Arc<AtomicBool>,
    /// This run's pidfile, under [`QemuHost::run_dir`].
    pidfile: PathBuf,
    /// Built by `initialize`, spent by `start`.
    launch: Option<Launch>,
    /// The host ends of `hvc0` and `hvc1`.
    consoles: Option<[OwnedFd; 2]>,
    process: Option<process::QemuProcess>,
    qmp: Option<qmp::Qmp>,
    /// Completed by the guest's power-off; see [`Self::wait_for_poweroff`].
    poweroff_rx: Option<crossbeam_channel::Receiver<()>>,
    poweroff_observed: bool,
}

/// What `start` hands QEMU.
struct Launch {
    argv: Vec<std::ffi::OsString>,
    /// QEMU's ends, inherited at spawn and closed here right after.
    inherited: Vec<OwnedFd>,
    qmp: UnixStream,
}

impl QemuMachine {
    /// Creates an unstarted machine for `config`.
    ///
    /// # Errors
    ///
    /// Returns an error when the config names no QEMU binary.
    pub fn new(config: VmmConfig, running: Arc<AtomicBool>) -> Result<Self> {
        let host = config.qemu.clone().ok_or_else(|| {
            VmmError::config("the QEMU backend needs a QEMU binary, and none is configured")
        })?;
        let pidfile = host
            .run_dir
            .join(format!("{}.pid", uuid::Uuid::new_v4().simple()));
        Ok(Self {
            config,
            host,
            running,
            reset_requested: Arc::new(AtomicBool::new(false)),
            pidfile,
            launch: None,
            consoles: None,
            process: None,
            qmp: None,
            poweroff_rx: None,
            poweroff_observed: false,
        })
    }

    /// Creates every channel into the guest and renders the command line.
    pub fn initialize(&mut self) -> Result<()> {
        if !self.host.binary.is_file() {
            return Err(VmmError::config(format!(
                "QEMU binary not found at {}",
                self.host.binary.display()
            )));
        }
        std::fs::create_dir_all(&self.host.run_dir)?;
        let mut inherited = Vec::new();

        let (qmp, qmp_theirs) = stream_pair()?;
        let qmp_fd = qmp_theirs.as_raw_fd();
        inherited.push(qmp_theirs);

        let mut console_fds = [0; 2];
        let mut consoles = Vec::with_capacity(2);
        for fd in &mut console_fds {
            let (ours, theirs) = stream_pair()?;
            // The serial drain watches these with a non-blocking reactor.
            set_nonblocking(ours.as_raw_fd())?;
            *fd = theirs.as_raw_fd();
            inherited.push(theirs);
            consoles.push(OwnedFd::from(ours));
        }

        if self.config.networking {
            tracing::warn!("QEMU backend: no NIC yet; booting without networking");
        }
        if !self.config.shared_dirs.is_empty() || self.config.vsock {
            tracing::warn!(
                shares = self.config.shared_dirs.len(),
                vsock = self.config.vsock,
                "QEMU backend: no vhost-user back-ends yet; booting without virtio-fs and vsock"
            );
        }

        let argv = args::qemu_args(&args::QemuArgs {
            kernel: &self.config.kernel_path,
            initrd: self.config.initrd_path.as_deref(),
            cmdline: &self.config.kernel_cmdline,
            vcpus: self.config.vcpu_count,
            memory_mib: self.config.memory_size / (1024 * 1024),
            pidfile: &self.pidfile,
            qmp_fd,
            console_fds,
            net_fd: None,
            disks: &self.config.block_devices,
        })
        .map_err(VmmError::config)?;

        let [console, agent_log] = <[OwnedFd; 2]>::try_from(consoles)
            .map_err(|_| VmmError::Device("expected two console channels".into()))?;
        self.consoles = Some([console, agent_log]);
        self.launch = Some(Launch {
            argv,
            inherited,
            qmp,
        });
        Ok(())
    }

    /// Spawns QEMU and negotiates QMP.
    ///
    /// # Errors
    ///
    /// Returns an error, with QEMU's stderr, when it cannot be spawned or
    /// exits before QMP answers.
    pub fn start(&mut self) -> Result<()> {
        let launch = self
            .launch
            .take()
            .ok_or_else(|| VmmError::invalid_state("QEMU machine is not initialized"))?;
        let (poweroff_tx, poweroff_rx) = crossbeam_channel::unbounded();
        self.poweroff_rx = Some(poweroff_rx);
        self.poweroff_observed = false;
        self.running.store(true, Ordering::SeqCst);
        let fds: Vec<RawFd> = launch.inherited.iter().map(AsRawFd::as_raw_fd).collect();
        tracing::info!(argv = ?launch.argv, "starting QEMU");
        let running = Arc::clone(&self.running);
        let process =
            process::QemuProcess::spawn(&self.host.binary, &launch.argv, &fds, move || {
                // Exiting without a SHUTDOWN event (a crash) stops the VM too.
                running.store(false, Ordering::SeqCst);
            });
        // QEMU holds its own copies now.
        drop(launch.inherited);
        let process = process.inspect_err(|_| self.running.store(false, Ordering::SeqCst))?;

        let events = qmp::GuestEvents {
            running: Arc::clone(&self.running),
            reset_requested: Arc::clone(&self.reset_requested),
            poweroff: poweroff_tx,
        };
        match qmp::Qmp::connect(launch.qmp, events) {
            Ok(qmp) => {
                self.qmp = Some(qmp);
                self.process = Some(process);
                Ok(())
            }
            Err(e) => {
                let stderr = process.terminate(|| {});
                self.running.store(false, Ordering::SeqCst);
                Err(VmmError::Device(format!(
                    "QEMU failed to start ({e}): {stderr}"
                )))
            }
        }
    }

    /// Pauses the vCPUs.
    pub fn pause(&self) -> Result<()> {
        self.command("stop").map(drop)
    }

    /// Resumes the vCPUs.
    pub fn resume(&self) -> Result<()> {
        self.command("cont").map(drop)
    }

    fn command(&self, command: &str) -> Result<serde_json::Value> {
        self.qmp
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("QEMU is not running"))?
            .execute(command)
    }

    /// Ends QEMU — `quit`, then SIGTERM, then SIGKILL — and releases every
    /// host resource of the run. A guest that already shut down leaves an
    /// exited process, which only needs reaping.
    pub fn stop(&mut self) {
        let qmp = self.qmp.take();
        if let Some(process) = self.process.take() {
            let _ = process.terminate(|| {
                if let Some(qmp) = &qmp {
                    let _ = qmp.execute("quit");
                }
            });
        }
        if let Some(qmp) = qmp {
            qmp.join();
        }
        self.running.store(false, Ordering::SeqCst);
        // QEMU removes its pidfile on a clean exit only.
        let _ = std::fs::remove_file(&self.pidfile);
        self.consoles = None;
        self.launch = None;
    }

    /// Waits for the guest's power-off. `running` is whether the owner
    /// considers the VM running; a completion already observed answers
    /// `true` regardless.
    pub fn wait_for_poweroff(&mut self, timeout: Duration, running: bool) -> Result<bool> {
        if self.poweroff_observed {
            return Ok(true);
        }
        if !running {
            return Ok(false);
        }
        let receiver = self
            .poweroff_rx
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("QEMU is not running"))?;
        match receiver.recv_timeout(timeout) {
            Ok(()) => {
                self.poweroff_observed = true;
                Ok(true)
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(false),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(VmmError::Vcpu(
                "QEMU exited without the guest powering off".into(),
            )),
        }
    }

    /// Whether the guest asked for a reset.
    pub fn reset_requested(&self) -> bool {
        self.reset_requested.load(Ordering::SeqCst)
    }

    /// Clears the reset request before a reboot.
    pub fn clear_reset_requested(&self) {
        self.reset_requested.store(false, Ordering::SeqCst);
    }

    /// Opens a host-initiated vsock connection to `port` in the guest.
    ///
    /// # Errors
    ///
    /// Always, until the vhost-user vsock back-end is wired in.
    pub fn connect_vsock(&self, port: u32) -> Result<RawFd> {
        Err(VmmError::Unsupported(format!(
            "vsock port {port}: the QEMU backend has no vsock device yet"
        )))
    }

    /// Duplicates the host ends of `hvc0` and `hvc1` for the serial drain.
    pub fn dup_serial_readers(&self) -> Result<arcbox_hypervisor::darwin::SerialReaders> {
        let [console, agent_log] = self
            .consoles
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("QEMU consoles are not open"))?;
        Ok(arcbox_hypervisor::darwin::SerialReaders {
            console: console.try_clone()?,
            agent_log: agent_log.try_clone()?,
        })
    }
}

/// A connected `SOCK_STREAM` pair, both ends close-on-exec.
fn stream_pair() -> Result<(UnixStream, OwnedFd)> {
    let (ours, theirs) = UnixStream::pair()?;
    // `UnixStream::pair` marks both ends close-on-exec on macOS too.
    Ok((ours, OwnedFd::from(theirs)))
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: F_GETFL/F_SETFL on a descriptor the caller owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
