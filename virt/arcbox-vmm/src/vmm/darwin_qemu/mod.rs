//! macOS QEMU TCG backend: the System VM in a `qemu-system-aarch64` child,
//! for Apple silicon Macs without hardware virtualization.
//!
//! QEMU owns the vCPUs, guest RAM and the virtio transport; the daemon keeps
//! what it owns on every backend. NIC1 is the same frame socketpair VZ
//! attaches (`darwin_net`); `hvc0` and `hvc1` are `virtconsole` ports whose
//! socketpairs feed the engine's serial drain; QMP drives the lifecycle.
//! Every descriptor QEMU needs is inherited at spawn (`process`), so it
//! opens no socket of its own.
//!
//! virtio-fs and vsock come from vhost-user back-ends in the daemon. QEMU
//! blocks in device realize until a back-end answers, so a device is only
//! put on the command line together with the back-end serving it — see
//! [`QemuMachine::initialize`].

mod args;
mod process;
mod qmp;

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::darwin_net;
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
    shared_dns: Option<arcbox_net::dns::SharedDns>,
    proxy_env: Option<arcbox_fakeip::proxy_detect::ProxyEnvironment>,
    /// This run's pidfile, under [`QemuHost::run_dir`].
    pidfile: PathBuf,
    /// Built by `initialize`, spent by `start`.
    launch: Option<Launch>,
    /// The host ends of `hvc0` and `hvc1`.
    consoles: Option<[OwnedFd; 2]>,
    net_cancel: Option<tokio_util::sync::CancellationToken>,
    inbound_listener_manager: Option<arcbox_net::darwin::inbound_relay::InboundListenerManager>,
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
            shared_dns: None,
            proxy_env: None,
            pidfile,
            launch: None,
            consoles: None,
            net_cancel: None,
            inbound_listener_manager: None,
            process: None,
            qmp: None,
            poweroff_rx: None,
            poweroff_observed: false,
        })
    }

    /// Sets the shared DNS policy. Must be called before [`Self::initialize`].
    pub fn set_shared_dns(&mut self, dns: arcbox_net::dns::SharedDns) {
        self.shared_dns = Some(dns);
    }

    /// Sets the egress proxy. Must be called before [`Self::initialize`].
    pub fn set_proxy_env(&mut self, env: arcbox_fakeip::proxy_detect::ProxyEnvironment) {
        self.proxy_env = Some(env);
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

        let net_fd = if self.config.networking {
            // Untagged frames at the default MTU; QEMU's dgram netdev
            // carries no virtio-net header, as VZ's file handle does not.
            let nic =
                darwin_net::GuestNic::new(1500, self.shared_dns.as_ref(), self.proxy_env.clone())?;
            self.net_cancel = Some(nic.cancel);
            self.inbound_listener_manager = Some(nic.inbound);
            let fd = nic.guest_fd.as_raw_fd();
            inherited.push(nic.guest_fd);
            Some(fd)
        } else {
            None
        };

        // vhost-user attach point. Each back-end gets one end of a
        // `stream_pair()` and starts serving it before `start` spawns QEMU,
        // which blocks in device realize until the back-end answers: one
        // virtio-fs back-end per `config.shared_dirs` entry
        // (`arcbox_vhost_user::fs::FsShare::new(tag, path).spawn(stream)`),
        // one vsock back-end for the guest CID. QEMU's end goes into
        // `inherited` and its number into `shares` (an `args::FsShare`) or
        // `vsock_fd`; the machine keeps each back-end's handle and stops it
        // after QEMU. Neither back-end is wired yet, so the guest boots
        // without them.
        let shares: Vec<args::FsShare> = Vec::new();
        let vsock_fd: Option<RawFd> = None;
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
            net_fd,
            disks: &self.config.block_devices,
            vsock_fd,
            shares: &shares,
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
                let stderr = match process.terminate(|| {}) {
                    Ok(stderr) => stderr,
                    Err(process) => {
                        // `stop` tries again; until it succeeds the run
                        // keeps its pidfile.
                        self.process = Some(process);
                        "QEMU survived SIGKILL".to_string()
                    }
                };
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
    ///
    /// # Errors
    ///
    /// Returns an error when QEMU outlives SIGKILL, stuck in host I/O. The
    /// machine then keeps the process, its QMP session and its pidfile, so
    /// a later stop tries again.
    pub fn stop(&mut self) -> Result<()> {
        let qmp = self.qmp.take();
        if let Some(process) = self.process.take() {
            let terminated = process.terminate(|| {
                if let Some(qmp) = &qmp {
                    let _ = qmp.execute("quit");
                }
            });
            if let Err(process) = terminated {
                self.process = Some(process);
                self.qmp = qmp;
                return Err(VmmError::Device("QEMU survived SIGKILL".into()));
            }
        }
        if let Some(qmp) = qmp {
            qmp.join();
        }
        self.running.store(false, Ordering::SeqCst);
        // QEMU removes its pidfile on a clean exit only.
        let _ = std::fs::remove_file(&self.pidfile);
        if let Some(cancel) = self.net_cancel.take() {
            cancel.cancel();
        }
        self.consoles = None;
        self.launch = None;
        Ok(())
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

    /// The port-forwarding listener manager, while the machine owns it.
    pub const fn inbound_listener_manager(
        &mut self,
    ) -> Option<&mut arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.as_mut()
    }

    /// Takes the port-forwarding listener manager.
    pub const fn take_inbound_listener_manager(
        &mut self,
    ) -> Option<arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.take()
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
