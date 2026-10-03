//! Mounts every running distro machine's root filesystem on the host.
//!
//! A machine that reaches readiness is asked (`EnsureMachineExport`) to
//! serve its root over NFSv3 on its bridge NIC, and the endpoint is mounted
//! read-write at `<root>/<name>`: `~/ArcBoxMachines/<name>` unless
//! `ARCBOX_MACHINE_MOUNT_DIR` moves the root, which the e2e harness and the
//! dev daemons do to stay inside their data dir. The mount is released when
//! the machine begins to stop, so the unmount still reaches a live server,
//! and swept again once it has stopped or been removed; daemon shutdown
//! unmounts everything before it stops the machines, for the same reason.
//!
//! The loop follows the runtime's event bus the way `machine_dns` does and
//! re-derives what should be mounted from each machine's record, so a
//! lagged receiver is repaired by one pass. [`mount_machine`] and
//! [`unmount_machine`] are the two operations. Clone and export never meet
//! a mount: both refuse a running machine, and a clone or an imported
//! machine is mounted like any other once it starts.
//!
//! A release can fail: a machine stopped moments after its mount came up
//! still has the client's first requests in flight, and `umount`, forced
//! or not, waits out its timeout on them (seen 2026-10-04). The entry then
//! stays recorded and a background task keeps retrying until the mount is
//! gone, so the mount point disappears with the machine rather than
//! lingering, empty, after the NFS client gives the dead server up on its
//! own (`deadtimeout`).
//!
//! Why not `~/ArcBox/machines/<name>`: `~/ArcBox` is itself the read-only
//! NFS mount of the System VM's docker data, so nothing can be created
//! inside it. Moving that export to `~/ArcBox/docker` would free the name,
//! but the desktop app maps guest paths onto `~/ArcBox/<rest>` and would
//! have to move with it; that layout change is a decision of its own.

mod export;
mod registry;

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use arcbox_connect::v1::EnsureMachineExportRequest;
use arcbox_core::Runtime;
use arcbox_core::event::Event;
use arcbox_core::machine::{MachineInfo, MachineState};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use self::export::{host_addresses_on_link, is_machine_export, release};
use self::registry::{Entry, Mounts};
use crate::context::DaemonContext;
use crate::host_mount;

/// Names the directory the machine mounts live under.
const MOUNT_ROOT_ENV: &str = "ARCBOX_MACHINE_MOUNT_DIR";
/// The directory under the home directory they live under by default.
const DEFAULT_MOUNT_ROOT: &str = "ArcBoxMachines";
/// The guest account the host user stands in for. Machines have no default
/// user yet — `abctl machine exec` and `ssh <machine>@arcbox` run as root —
/// so files the host creates belong to root, like everything else it does
/// in a machine.
const GUEST_OWNER: (u32, u32) = (0, 0);
/// The mounts this daemon made; see [`registry`].
static MOUNTED: Mutex<Mounts> = Mutex::new(Mounts::new());
/// How often a failed release is retried, and for how long. The client gives
/// a dead server up after the mount's `deadtimeout` (60 s), so the budget
/// reaches past it: by then either the retry or the client has unmounted.
const UNMOUNT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const UNMOUNT_RETRY_BUDGET: Duration = Duration::from_secs(90);

/// Spawns the loop for the daemon's lifetime.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let events = runtime.event_bus().subscribe();
    let shutdown = ctx.shutdown.clone();
    let runtime = Arc::clone(runtime);
    drop(tokio::spawn(async move {
        run(&runtime, events, shutdown).await;
    }));
}

/// Unmounts every machine root this daemon mounted. Shutdown calls it twice:
/// before the machines stop, while their exports still answer, and after,
/// for a mount that raced the first pass or whose release failed.
pub async fn cleanup() {
    let entries = mounted().entries();
    for (name, entry) in entries {
        if unmount_path(&name, &entry.path).await {
            mounted().forget(&name, entry.generation);
        } else {
            mounted().mark_stale(&name, entry.generation);
        }
    }
}

async fn run(
    runtime: &Arc<Runtime>,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) {
    sync_all(runtime).await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            event = events.recv() => match event {
                Ok(Event::MachineStarted { name }) => sync(runtime, &name).await,
                Ok(
                    Event::MachineStopping { name }
                    | Event::MachineStopped { name }
                    | Event::MachineRemoved { name },
                ) => unmount_machine(&name).await,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => sync_all(runtime).await,
                Err(RecvError::Closed) => break,
            },
        }
    }
}

/// Brings `name`'s mount in line with its record: present while the machine
/// runs with a bridge address, absent otherwise.
async fn sync(runtime: &Arc<Runtime>, name: &str) {
    match runtime.machine_manager().get(name) {
        Some(machine) if exports_its_root(&machine) => {
            if let Err(e) = mount_machine(runtime, &machine).await {
                warn!(machine = name, error = %e, "could not mount the machine's root on the host");
            }
        }
        _ => unmount_machine(name).await,
    }
}

/// Every machine the daemon knows or still has mounted, for the lagged
/// case: a machine removed during the lag is only in the second set.
async fn sync_all(runtime: &Arc<Runtime>) {
    let mut names: Vec<String> = runtime
        .machine_manager()
        .list()
        .into_iter()
        .map(|machine| machine.name)
        .collect();
    names.extend(mounted().names());
    names.sort_unstable();
    names.dedup();
    for name in names {
        sync(runtime, &name).await;
    }
}

/// Whether a machine's root is mounted while it runs: it is a distro
/// machine (the System VM's data is the `~/ArcBox` export) with a bridge
/// address the host can reach.
fn exports_its_root(machine: &MachineInfo) -> bool {
    machine.state == MachineState::Running
        && machine.distro.is_some()
        && machine.bridge_ip_address.is_some()
}

/// Mounts a running machine's root at its mount point and records it.
/// Idempotent: a machine this daemon already mounted is left as it is.
///
/// # Errors
///
/// Returns an error when the host has no address on the machine's bridge
/// network, the mount point is held by a mount this daemon did not create,
/// the agent refuses the export, or `mount_nfs` keeps failing.
pub async fn mount_machine(runtime: &Arc<Runtime>, machine: &MachineInfo) -> Result<PathBuf> {
    let mount_path = mount_root()?.join(&machine.name);
    let already = mounted()
        .get(&machine.name)
        .is_some_and(|entry| entry.path == mount_path && !entry.stale);
    if already
        && host_mount::current_mount_info(&mount_path).is_some_and(|info| is_machine_export(&info))
    {
        return Ok(mount_path);
    }

    let bridge: Ipv4Addr = machine
        .bridge_ip_address
        .as_deref()
        .context("machine has no bridge address")?
        .parse()
        .context("machine bridge address")?;
    let clients = host_addresses_on_link(bridge);
    if clients.is_empty() {
        bail!(
            "the host has no address on the machine's bridge network ({bridge}), so it could not reach the export"
        );
    }

    match host_mount::current_mount_info(&mount_path) {
        None => {}
        Some(info) if is_machine_export(&info) => {
            info!(path = %mount_path.display(), source = %info.source, "replacing a stale machine mount");
            release(&mount_path).await?;
        }
        Some(info) => bail!(
            "mount point {} is occupied by {} ({})",
            mount_path.display(),
            info.source,
            info.fstype
        ),
    }
    std::fs::create_dir_all(&mount_path)
        .with_context(|| format!("creating {}", mount_path.display()))?;

    let request = EnsureMachineExportRequest {
        client_addresses: clients.iter().map(ToString::to_string).collect(),
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        host_uid: unsafe { libc::getuid() },
        host_gid: unsafe { libc::getgid() },
        guest_uid: GUEST_OWNER.0,
        guest_gid: GUEST_OWNER.1,
        ..Default::default()
    };
    let endpoint = runtime
        .ensure_machine_export(&machine.name, request)
        .await
        .context("the machine's agent did not start the export")?;
    let port = u16::try_from(endpoint.port).context("export port out of range")?;
    export::mount(&endpoint.address, port, &mount_path).await?;

    mounted().record(&machine.name, mount_path.clone());
    info!(
        machine = %machine.name,
        path = %mount_path.display(),
        address = %endpoint.address,
        port,
        "mounted the machine's root on the host (NFSv3, read-write)"
    );
    Ok(mount_path)
}

/// Unmounts `name`'s root if this daemon mounted it and removes the empty
/// mount point. A machine that was never mounted is a no-op; a release that
/// fails is retried in the background (see the module docs).
pub async fn unmount_machine(name: &str) {
    let Some(Entry {
        path, generation, ..
    }) = mounted().get(name).cloned()
    else {
        return;
    };
    if unmount_path(name, &path).await {
        mounted().forget(name, generation);
        return;
    }
    mounted().mark_stale(name, generation);
    drop(tokio::spawn(retry_unmount(
        name.to_owned(),
        path,
        generation,
    )));
}

/// Retries a failed release every [`UNMOUNT_RETRY_INTERVAL`] until the mount
/// is gone, the name is mounted anew (a later generation owns the path), or
/// [`UNMOUNT_RETRY_BUDGET`] runs out.
async fn retry_unmount(name: String, path: PathBuf, generation: u64) {
    let deadline = tokio::time::Instant::now() + UNMOUNT_RETRY_BUDGET;
    loop {
        tokio::time::sleep(UNMOUNT_RETRY_INTERVAL).await;
        if !mounted().is_current(&name, generation) {
            return;
        }
        if unmount_path(&name, &path).await {
            mounted().forget(&name, generation);
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(machine = %name, path = %path.display(), "giving up on unmounting the machine's root");
            mounted().forget(&name, generation);
            return;
        }
    }
}

/// Releases the machine export at `path`, if one is there, and removes the
/// empty mount point. Returns whether nothing of ours is mounted there any
/// more: `false` only when the release failed and is worth retrying.
async fn unmount_path(name: &str, path: &Path) -> bool {
    match host_mount::current_mount_info(path) {
        None => {}
        Some(info) if is_machine_export(&info) => match release(path).await {
            Ok(()) => info!(machine = name, path = %path.display(), "unmounted the machine's root"),
            Err(e) => {
                warn!(machine = name, path = %path.display(), error = %e, "failed to unmount the machine's root");
                return false;
            }
        },
        Some(info) => {
            warn!(machine = name, path = %path.display(), source = %info.source, "mount point holds a mount this daemon did not create; leaving it");
            return true;
        }
    }
    // Keep the root tidy: only running machines have a directory there.
    if let Err(e) = std::fs::remove_dir(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        debug!(path = %path.display(), error = %e, "mount point not removed");
    }
    true
}

/// The directory the machine mounts live under.
fn mount_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(MOUNT_ROOT_ENV) {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|home| home.join(DEFAULT_MOUNT_ROOT))
        .context("could not determine the home directory for the machine mounts")
}

fn mounted() -> std::sync::MutexGuard<'static, Mounts> {
    // A panic while holding the lock leaves a plain map; keep serving.
    MOUNTED.lock().unwrap_or_else(PoisonError::into_inner)
}
