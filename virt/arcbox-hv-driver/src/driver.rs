//! [`HvDriver`]: spawns, finds and boots `arcbox-hv-vmm` processes.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use arcbox_vm_driver::{
    Adopt, CheckpointImage, DriverCapabilities, Error, IsolationSpec, NestedVirt, Prepare,
    PreparedVm, ProcessRecord, RestoreSpec, Result, VmDriver, VmHandle, VmId, VmRecord, VmSpec,
};
use async_trait::async_trait;

use crate::control::{self, Control};
use crate::handle::HvHandle;
use crate::prepared::HvPrepared;
use crate::process::VmmProcess;
use crate::proto::{Request, Response};
use crate::{NAME, driver_error};

/// The control socket, inside the VM's runtime dir.
const SOCKET: &str = "vmm.sock";
/// The process's log, inside the VM's runtime dir.
const LOG: &str = "vmm.log";
/// `sun_path` on macOS, including the terminating NUL.
const SUN_PATH: usize = 104;
/// How long a spawned process gets to answer on its socket.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs each VM in its own `arcbox-hv-vmm` process.
pub struct HvDriver {
    binary: PathBuf,
    detect_proxy: bool,
}

impl HvDriver {
    /// A driver that spawns `binary`, an `arcbox-hv-vmm` signed with the
    /// hypervisor entitlement. With `detect_proxy`, each VM tunnels its
    /// egress through the host's configured proxy, as the System VM does.
    pub const fn new(binary: PathBuf, detect_proxy: bool) -> Self {
        Self {
            binary,
            detect_proxy,
        }
    }

    async fn spawn(&self, id: &VmId, runtime_dir: &Path) -> Result<HvPrepared> {
        let socket = runtime_dir.join(SOCKET);
        if socket.as_os_str().len() >= SUN_PATH {
            return Err(Error::InvalidSpec(format!(
                "{NAME}: the control socket {} is longer than a Unix socket path can be",
                socket.display()
            )));
        }
        let log = File::options()
            .create(true)
            .append(true)
            .open(runtime_dir.join(LOG))?;
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .arg("--socket")
            .arg(&socket)
            .arg("--id")
            .arg(id.as_str())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group: a signal to the daemon's group (a
            // terminal's ^C) must not reach VMs that outlive the daemon.
            .process_group(0)
            .kill_on_drop(false);
        if self.detect_proxy {
            command.arg("--detect-proxy");
        }
        let child = command
            .spawn()
            .map_err(|e| driver_error(format!("spawn {}", self.binary.display()), e))?;
        let process = VmmProcess::spawned(child, socket.clone())?;
        let prepared = HvPrepared::new(record(id, runtime_dir, &process), Arc::clone(&process));
        wait_ready(&process, id, runtime_dir).await?;
        process.watch()?;
        tracing::info!(vm = %id, pid = process.pid(), "arcbox-hv-vmm started");
        Ok(prepared)
    }
}

/// Waits until the process answers `Hello` as `id`.
async fn wait_ready(process: &VmmProcess, id: &VmId, runtime_dir: &Path) -> Result<()> {
    let control = Control::new(process.socket().to_path_buf());
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = process.exit_status() {
            return Err(driver_error(
                format!(
                    "arcbox-hv-vmm exited before it was ready; see {}",
                    runtime_dir.join(LOG).display()
                ),
                std::io::Error::other(status.to_string()),
            ));
        }
        if process.socket().exists() {
            match control.call(Request::Hello, control::QUICK).await {
                Ok((Response::Hello { pid, id: got, .. }, _))
                    if pid == process.pid() && got == *id =>
                {
                    return Ok(());
                }
                Ok((other, _)) => return Err(control::unexpected(&other)),
                Err(_) => {}
            }
        }
        if tokio::time::Instant::now() > deadline {
            process.signal_kill();
            return Err(driver_error(
                "arcbox-hv-vmm did not answer",
                std::io::Error::from(std::io::ErrorKind::TimedOut),
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn record(id: &VmId, runtime_dir: &Path, process: &VmmProcess) -> VmRecord {
    VmRecord {
        id: id.clone(),
        driver: NAME.to_owned(),
        runtime_dir: runtime_dir.to_path_buf(),
        process: Some(ProcessRecord {
            pid: process.pid(),
            api_socket: Some(process.socket().to_path_buf()),
            jail: None,
        }),
    }
}

fn unconfined(isolation: &IsolationSpec) -> Result<()> {
    if *isolation != IsolationSpec::None {
        return Err(Error::InvalidSpec(format!(
            "{NAME}: isolation is not available"
        )));
    }
    Ok(())
}

#[async_trait]
impl VmDriver for HvDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            vsock: true,
            checkpoint: true,
            adopt: true,
            prepare: true,
            balloon: true,
            console: true,
            port_forward: true,
            debug: true,
            nested_virt: NestedVirt::unsupported(
                "Hypervisor.framework guests cannot run their own hypervisor",
            ),
            ..DriverCapabilities::default()
        }
    }

    async fn boot(&self, spec: VmSpec, runtime_dir: &Path) -> Result<Box<dyn VmHandle>> {
        spec.validate()?;
        unconfined(&spec.isolation)?;
        let prepared = self.spawn(&spec.id, runtime_dir).await?;
        // A failed boot drops the prepared VM, which kills the process.
        prepared.boot(spec).await
    }

    async fn restore(
        &self,
        image: &CheckpointImage,
        spec: RestoreSpec,
        runtime_dir: &Path,
    ) -> Result<Box<dyn VmHandle>> {
        crate::own_format(image)?;
        unconfined(&spec.isolation)?;
        let prepared = self.spawn(&spec.id, runtime_dir).await?;
        prepared.restore(image, spec).await
    }

    fn adopt(&self) -> Option<&dyn Adopt> {
        Some(self)
    }

    fn prepare(&self) -> Option<&dyn Prepare> {
        Some(self)
    }
}

#[async_trait]
impl Prepare for HvDriver {
    async fn prepare(
        &self,
        id: &VmId,
        isolation: &IsolationSpec,
        runtime_dir: &Path,
    ) -> Result<Box<dyn PreparedVm>> {
        unconfined(isolation)?;
        Ok(Box::new(self.spawn(id, runtime_dir).await?))
    }
}

#[async_trait]
impl Adopt for HvDriver {
    /// Asks the process at the record's socket who it is. Only the process
    /// the record names, still running its VM, is adopted; one that never
    /// booted a VM is killed, since its owner is gone; a socket nobody
    /// answers is a VM that is gone.
    async fn adopt(&self, record: &VmRecord) -> Result<Option<Box<dyn VmHandle>>> {
        let Some(ProcessRecord {
            pid,
            api_socket: Some(socket),
            ..
        }) = &record.process
        else {
            return Ok(None);
        };
        let control = Control::new(socket.clone());
        let (got_pid, got_id, vm) = match control.call(Request::Hello, control::QUICK).await {
            Ok((Response::Hello { pid, id, vm }, _)) => (pid, id, vm),
            Ok((other, _)) => return Err(control::unexpected(&other)),
            Err(_) => return Ok(None),
        };
        if got_pid != *pid || got_id != record.id {
            return Ok(None);
        }
        let process = VmmProcess::adopted(*pid, socket.clone());
        process.watch()?;
        let Some(booted) = vm else {
            process.kill().await?;
            return Ok(None);
        };
        tracing::info!(vm = %record.id, pid, "adopted arcbox-hv-vmm");
        Ok(Some(Box::new(HvHandle::new(
            record.clone(),
            process,
            booted,
        ))))
    }

    /// The process keeps nothing outside the runtime dir its owner made,
    /// so there is only its socket to remove.
    async fn discard_area(&self, record: &VmRecord, _isolation: &IsolationSpec) -> Result<()> {
        match std::fs::remove_file(record.runtime_dir.join(SOCKET)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}
