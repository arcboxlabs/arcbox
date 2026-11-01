//! The `arcbox-hv-vmm` process: one Hypervisor.framework VM, served to the
//! driver over a Unix socket.
//!
//! The process starts with no VM and waits for `Boot` or `Restore`; that
//! split is the driver's `Prepare`. The VM itself is
//! [`HvMachineDriver`]'s, in this process, and every request maps onto one
//! of its handle's capabilities. When the guest exits, every watcher gets
//! [`Event::Exited`] and the process exits with the guest's code. The
//! driver stops a VM by killing the process: there is nothing in it worth a
//! cleanup that the kernel does not do.
//!
//! Connections are served one thread each; the VM's async calls run on the
//! process's Tokio runtime through its handle.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arcbox_vm_driver::{
    AfterCheckpoint, CheckpointKind, CheckpointOptions, ConsoleSpec, Error, PortForward, Result,
    VmDriver, VmEvent, VmHandle, VmId, VmSpec,
};
use arcbox_vmm::hv_driver::HvMachineDriver;
use tokio::runtime::Handle;

use crate::proto::{Booted, Event, Request, Response, WireError};
use crate::wire;

pub mod console;

/// What the process was started with.
pub struct Args {
    /// The control socket to bind.
    pub socket: PathBuf,
    /// The VM's id; a spec for another id is refused.
    pub id: VmId,
    /// Tunnel guest egress through the host's configured proxy.
    pub detect_proxy: bool,
}

struct Server {
    id: VmId,
    /// The directory the socket is in: the VM's runtime dir.
    dir: PathBuf,
    driver: HvMachineDriver,
    runtime: Handle,
    /// Set by the first `Boot` or `Restore`, cleared if it fails.
    claimed: AtomicBool,
    /// The VM's console, which its checkpoints record.
    console: Mutex<ConsoleSpec>,
    vm: Mutex<Option<Vm>>,
    watchers: Mutex<Vec<UnixStream>>,
}

struct Vm {
    handle: Arc<dyn VmHandle>,
    booted: Booted,
    forward: Option<Arc<tokio::sync::Mutex<Box<dyn PortForward>>>>,
}

/// Serves `args.socket` until the VM exits. Never returns on success: the
/// process exits with the guest's code.
pub fn run(args: Args, runtime: &tokio::runtime::Runtime) -> std::io::Result<()> {
    // The socket is this process's own; a stale one is from a dead
    // predecessor under the same runtime dir.
    let _ = std::fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)?;
    let proxy = args
        .detect_proxy
        .then(arcbox_fakeip::proxy_detect::ProxyEnvironment::detect);
    let server = Arc::new(Server {
        id: args.id,
        dir: args.socket.parent().map(PathBuf::from).unwrap_or_default(),
        driver: HvMachineDriver::new(None, proxy),
        runtime: runtime.handle().clone(),
        claimed: AtomicBool::new(false),
        console: Mutex::new(ConsoleSpec::Off),
        vm: Mutex::new(None),
        watchers: Mutex::new(Vec::new()),
    });
    tracing::info!(vm = %server.id, socket = %args.socket.display(), "arcbox-hv-vmm ready");
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        let server = Arc::clone(&server);
        std::thread::Builder::new()
            .name("hv-vmm-conn".into())
            .spawn(move || server.serve(conn))?;
    }
    Ok(())
}

impl Server {
    fn serve(self: &Arc<Self>, conn: UnixStream) {
        let request = match wire::recv::<Request>(&conn) {
            Ok((request, _)) => request,
            Err(e) => {
                tracing::debug!("unreadable request: {e}");
                return;
            }
        };
        if matches!(request, Request::Watch) {
            lock(&self.watchers).push(conn);
            return;
        }
        let (response, fd) = match self.handle(request) {
            Ok(answer) => answer,
            Err(e) => (Response::Error(WireError::from(&e)), None),
        };
        if let Err(e) = wire::send(&conn, &response, fd.as_ref().map(AsFd::as_fd)) {
            tracing::debug!("reply failed: {e}");
        }
    }

    fn handle(self: &Arc<Self>, request: Request) -> Result<(Response, Option<OwnedFd>)> {
        let plain = |r| Ok((r, None));
        match request {
            Request::Hello => plain(Response::Hello {
                pid: std::process::id(),
                id: self.id.clone(),
                vm: lock(&self.vm).as_ref().map(|vm| Booted {
                    state: vm.handle.state(),
                    ..vm.booted
                }),
            }),
            Request::Boot(spec) => plain(Response::Booted(self.boot(*spec)?)),
            Request::Restore { image, spec } => {
                self.claim(&spec.id)?;
                let console = read_console(&image.dir);
                let handle = self.open_console(&console).and_then(|()| {
                    self.runtime
                        .block_on(self.driver.restore(&image, spec, &self.dir))
                });
                plain(Response::Booted(self.install(handle, console)?))
            }
            Request::Watch => unreachable!("served by `serve`"),
            Request::Dial(port) => {
                let handle = self.vm()?;
                let vsock = handle.vsock().ok_or_else(|| absent("vsock"))?;
                let conn = self.runtime.block_on(vsock.dial(port))?;
                Ok((Response::Fd, Some(conn.fd)))
            }
            Request::Checkpoint { dir, hold } => {
                let handle = self.vm()?;
                let checkpoint = handle.checkpoint().ok_or_else(|| absent("checkpoint"))?;
                let opts = CheckpointOptions {
                    after: if hold {
                        AfterCheckpoint::HoldQuiesced
                    } else {
                        AfterCheckpoint::Resume
                    },
                    kind: CheckpointKind::Full,
                };
                let image = self.runtime.block_on(checkpoint.checkpoint(&dir, opts))?;
                write_console(&dir, &lock(&self.console))?;
                plain(Response::Image(image))
            }
            Request::BalloonTarget(bytes) => {
                let handle = self.vm()?;
                let balloon = handle.balloon().ok_or_else(|| absent("balloon"))?;
                self.runtime.block_on(balloon.set_target(bytes))?;
                plain(Response::Ok)
            }
            Request::BalloonStats => {
                let handle = self.vm()?;
                let balloon = handle.balloon().ok_or_else(|| absent("balloon"))?;
                let stats = self.runtime.block_on(balloon.stats())?;
                plain(Response::Balloon {
                    target: stats.target_bytes,
                    current: stats.current_bytes,
                })
            }
            Request::Debug => {
                let handle = self.vm()?;
                let debug = handle.debug().ok_or_else(|| absent("debug"))?;
                plain(Response::Debug(debug.snapshot()))
            }
            Request::Console => {
                let reader = console::reader().ok_or_else(|| absent("console"))?;
                Ok((Response::Fd, Some(reader.try_clone_to_owned()?)))
            }
            Request::ForwardAdd(rule) => {
                let forward = self.forward()?;
                let port = self
                    .runtime
                    .block_on(async { forward.lock().await.add(rule.into()).await })?;
                plain(Response::Port(port))
            }
            Request::ForwardRemove(rule) => {
                let forward = self.forward()?;
                self.runtime
                    .block_on(async { forward.lock().await.remove(rule.into()).await });
                plain(Response::Ok)
            }
            Request::ForwardClear => {
                let forward = self.forward()?;
                self.runtime
                    .block_on(async { forward.lock().await.clear().await });
                plain(Response::Ok)
            }
        }
    }

    /// Boots `spec`. Its console is this process's: the machine itself
    /// boots without one.
    fn boot(self: &Arc<Self>, mut spec: VmSpec) -> Result<Booted> {
        self.claim(&spec.id)?;
        let console = std::mem::take(&mut spec.console);
        let handle = self
            .open_console(&console)
            .and_then(|()| self.runtime.block_on(self.driver.boot(spec, &self.dir)));
        self.install(handle, console)
    }

    fn open_console(&self, console: &ConsoleSpec) -> Result<()> {
        let opened = match console {
            ConsoleSpec::Off => Ok(()),
            ConsoleSpec::File(path) => console::to_file(path),
            ConsoleSpec::Pipe => console::to_pipe(),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "only Off, File and Pipe consoles are available",
            )),
        };
        opened.map_err(|e| Error::InvalidSpec(format!("hv: console: {e}")))
    }

    /// Refuses a spec for another VM, and a second VM in this process.
    fn claim(&self, id: &VmId) -> Result<()> {
        if *id != self.id {
            return Err(Error::InvalidSpec(format!("this process runs {}", self.id)));
        }
        if self.claimed.swap(true, Ordering::SeqCst) {
            return Err(Error::InvalidSpec(
                "this process already runs its VM".into(),
            ));
        }
        Ok(())
    }

    /// Keeps a booted VM, or releases the claim of a boot that failed.
    fn install(
        self: &Arc<Self>,
        handle: Result<Box<dyn VmHandle>>,
        console: ConsoleSpec,
    ) -> Result<Booted> {
        let handle: Arc<dyn VmHandle> = match handle {
            Ok(handle) => Arc::from(handle),
            Err(e) => {
                self.claimed.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let forward = handle
            .take_port_forward()
            .map(|f| Arc::new(tokio::sync::Mutex::new(f)));
        let booted = Booted {
            state: handle.state(),
            vsock: handle.vsock().is_some(),
            balloon: handle.balloon().is_some(),
            console: console == ConsoleSpec::Pipe,
            port_forward: forward.is_some(),
        };
        *lock(&self.console) = console;
        let events = handle.events();
        *lock(&self.vm) = Some(Vm {
            handle,
            booted,
            forward,
        });
        let server = Arc::clone(self);
        self.runtime.spawn(server.relay(events));
        Ok(booted)
    }

    /// Forwards the VM's events to every watcher, and ends the process
    /// once the guest has exited.
    async fn relay(self: Arc<Self>, mut events: tokio::sync::broadcast::Receiver<VmEvent>) {
        loop {
            let event = match events.recv().await {
                Ok(VmEvent::ResetRequested) => Event::ResetRequested,
                Ok(VmEvent::Exited(status)) => Event::Exited(status),
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            lock(&self.watchers).retain(|w| wire::send(w, &event, None).is_ok());
            if let Event::Exited(status) = event {
                tracing::info!(vm = %self.id, %status, "guest exited; arcbox-hv-vmm exits");
                std::process::exit(status.code.unwrap_or(1));
            }
        }
    }

    fn vm(&self) -> Result<Arc<dyn VmHandle>> {
        lock(&self.vm)
            .as_ref()
            .map(|vm| Arc::clone(&vm.handle))
            .ok_or_else(|| failed("no VM has booted in this process"))
    }

    fn forward(&self) -> Result<Arc<tokio::sync::Mutex<Box<dyn PortForward>>>> {
        lock(&self.vm)
            .as_ref()
            .and_then(|vm| vm.forward.clone())
            .ok_or_else(|| absent("port forwarding"))
    }
}

/// The console a checkpoint's VM had, which the machine's own image does
/// not hold: the machine boots without one.
const CONSOLE_FILE: &str = "console.json";

fn write_console(dir: &Path, console: &ConsoleSpec) -> Result<()> {
    let json = serde_json::to_vec(console).map_err(std::io::Error::other)?;
    std::fs::write(dir.join(CONSOLE_FILE), json)?;
    Ok(())
}

/// An image without the record restores without a console.
fn read_console(dir: &Path) -> ConsoleSpec {
    std::fs::read(dir.join(CONSOLE_FILE))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

/// A capability the VM was not built with; the driver's accessors do not
/// offer it, so only a confused client asks.
fn absent(what: &str) -> Error {
    failed(&format!("this VM has no {what}"))
}

fn failed(message: &str) -> Error {
    Error::Driver {
        driver: crate::NAME,
        message: message.to_owned(),
        source: None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
