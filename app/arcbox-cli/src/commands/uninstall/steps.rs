//! The actions `abctl uninstall` takes, one per kind of residue.
//!
//! Every action reports what it did, so the run can show the user what was
//! removed, what was not there, and what failed — never a checkmark for a
//! step whose work did not happen (#716).

use std::ffi::{CStr, OsStr};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_constants::dns::LOCAL_CA_COMMON_NAME;
use arcbox_constants::paths::{DOCKER_CLI_TOOLS, HostLayout, is_arcbox_owned, labels};

use super::host::{Host, run_checked, sudo};
use super::inventory::{HOSTS_MARKER, Roots};
use crate::commands::daemon;

/// What one action did.
pub(super) enum Outcome {
    Done,
    /// Nothing to do, with the reason shown to the user.
    Skipped(String),
}

fn skipped(reason: impl Into<String>) -> Outcome {
    Outcome::Skipped(reason.into())
}

/// Asks the still-registered helper to remove our `/usr/local/bin/docker*`
/// links, without `sudo`. Best-effort: a missing or incompatible helper
/// leaves the links for `abctl uninstall`, which removes them with `sudo`.
///
/// Ownership is checked here as well as in the helper: a helper built before
/// #715 still deletes OrbStack's links, which share our `xbin` layout.
pub(super) async fn unlink_cli_tools_through_helper(roots: &Roots) {
    let Ok(client) = arcbox_helper::client::Client::connect().await else {
        return;
    };
    let bin = roots.system_path("/usr/local/bin");
    for name in DOCKER_CLI_TOOLS {
        if std::fs::read_link(bin.join(name)).is_ok_and(|target| is_arcbox_owned(&target)) {
            let _ = client.cli_unlink(name).await;
        }
    }
}

/// How long the Desktop app gets to quit. Its termination handler stops the
/// daemon and unregisters the LaunchAgent, so this covers a VM shutdown.
const QUIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Asks the Desktop app to quit and waits until it has.
///
/// Quitting matters beyond the window: the app's termination handler
/// unregisters its `SMAppService` daemon, which is what clears the Login
/// Items entry.
pub(super) fn quit_app(host: &dyn Host, bundle_id: &str) -> Result<Outcome> {
    if !app_is_running(host, bundle_id)? {
        return Ok(skipped("not running"));
    }
    let script = format!("tell application id \"{bundle_id}\" to quit");
    run_checked(host, "osascript", &[OsStr::new("-e"), OsStr::new(&script)])?;
    let deadline = Instant::now() + QUIT_TIMEOUT;
    while app_is_running(host, bundle_id)? {
        if Instant::now() >= deadline {
            bail!("the app did not quit within {}s", QUIT_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(Outcome::Done)
}

fn app_is_running(host: &dyn Host, bundle_id: &str) -> Result<bool> {
    let script = format!("application id \"{bundle_id}\" is running");
    let output = host.run("osascript", &[OsStr::new("-e"), OsStr::new(&script)])?;
    // An error means no app with that identifier is installed.
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

/// Stops the daemon: through launchd when it manages one under `labels`,
/// and through the PID in `daemon.lock` for one started by `abctl daemon
/// start`. Waits for the lock to be released either way.
///
/// The daemon stops its own System VM and unmounts `~/ArcBox` on SIGTERM.
/// Nothing here kills by process name: `pkill -f
/// com.apple.Virtualization.VirtualMachine` stopped every VZ guest on the
/// Mac, UTM's and Lima's included (#716).
pub(super) async fn stop_daemon(
    host: &dyn Host,
    layout: &HostLayout,
    labels: &[&str],
) -> Result<Outcome> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut booted_out = false;
    for label in labels {
        booted_out |= bootout(host, &format!("gui/{uid}/{label}"), false)?;
    }

    if booted_out {
        if let Some(pid) = daemon::locked_pid(layout)? {
            daemon::await_exit(layout, pid).await?;
        }
        return Ok(Outcome::Done);
    }
    if daemon::daemon_is_alive(&layout.lock_file) {
        daemon::stop(layout).await?;
        return Ok(Outcome::Done);
    }
    Ok(skipped("not running"))
}

/// `launchctl bootout <target>`; `true` when launchd had the job.
///
/// launchd answers 3 (`ESRCH`) for a label it does not know, and prints
/// "Could not find service" for a domain with no such job.
fn bootout(host: &dyn Host, target: &str, privileged: bool) -> Result<bool> {
    let args = [
        OsStr::new("launchctl"),
        OsStr::new("bootout"),
        OsStr::new(target),
    ];
    let output = if privileged {
        host.run("sudo", &args)?
    } else {
        host.run("launchctl", &args[1..])?
    };
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(3)
        || stderr.contains("No such process")
        || stderr.contains("Could not find service")
    {
        return Ok(false);
    }
    bail!(
        "launchctl bootout {target} failed ({}): {}",
        output.status,
        stderr.trim()
    );
}

/// Unregisters the helper LaunchDaemon so launchd stops activating it.
pub(super) fn bootout_helper(host: &dyn Host) -> Result<Outcome> {
    if bootout(host, &format!("system/{}", labels::HELPER), true)? {
        Ok(Outcome::Done)
    } else {
        Ok(skipped("not loaded"))
    }
}

/// Removes a file, symlink or directory ArcBox owns. Tries as the user
/// first and escalates to `sudo rm` only when the path refuses, so a
/// user-writable prefix never prompts.
pub(super) fn remove_path(host: &dyn Host, path: &Path) -> Result<()> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("could not inspect {}", path.display())),
    };
    let is_dir = metadata.is_dir();
    let direct = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match direct {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let flags = if is_dir { "-rf" } else { "-f" };
            sudo(
                host,
                &[OsStr::new("rm"), OsStr::new(flags), path.as_os_str()],
            )
        }
        Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
    }
}

/// Drops the managed `127.0.0.1 ArcBox` line from `/etc/hosts`.
pub(super) fn remove_hosts_alias(host: &dyn Host, hosts: &Path) -> Result<Outcome> {
    let content = std::fs::read_to_string(hosts)
        .with_context(|| format!("could not read {}", hosts.display()))?;
    if !content.lines().any(|line| line.contains(HOSTS_MARKER)) {
        return Ok(skipped("no alias line"));
    }
    let mut kept: String = content
        .lines()
        .filter(|line| !line.contains(HOSTS_MARKER))
        .collect::<Vec<_>>()
        .join("\n");
    if !kept.is_empty() {
        kept.push('\n');
    }
    match std::fs::write(hosts, &kept) {
        Ok(()) => Ok(Outcome::Done),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let staged =
                tempfile::NamedTempFile::new().context("could not stage the new hosts file")?;
            std::fs::write(staged.path(), &kept).context("could not stage the new hosts file")?;
            sudo(
                host,
                &[
                    OsStr::new("cp"),
                    staged.path().as_os_str(),
                    hosts.as_os_str(),
                ],
            )?;
            Ok(Outcome::Done)
        }
        Err(e) => Err(e).with_context(|| format!("could not write {}", hosts.display())),
    }
}

/// Unmounts the guest data export the daemon left at `~/ArcBox`, if the
/// daemon did not, and removes the empty mount point. A mount of any other
/// shape, or a directory with the user's files in it, is left alone.
pub(super) fn remove_data_export(host: &dyn Host, mount_point: &Path) -> Result<Outcome> {
    if !mount_point.exists() {
        return Ok(skipped("absent"));
    }
    match mount_at(mount_point) {
        Some(mount) if mount.is_arcbox_export() => {
            run_checked(host, "/sbin/umount", &[mount_point.as_os_str()])?;
        }
        Some(mount) => {
            return Ok(skipped(format!(
                "left alone: {} ({}) is mounted there",
                mount.source, mount.fstype
            )));
        }
        None => {}
    }
    let empty = std::fs::read_dir(mount_point)
        .with_context(|| format!("could not read {}", mount_point.display()))?
        .next()
        .is_none();
    if !empty {
        return Ok(skipped("left alone: not empty"));
    }
    std::fs::remove_dir(mount_point)
        .with_context(|| format!("could not remove {}", mount_point.display()))?;
    Ok(Outcome::Done)
}

/// Unmounts the machine roots a daemon left under `~/ArcBoxMachines`, then
/// removes the empty mount points and the root. The daemon is already
/// stopped, so the machines serving those mounts are gone: the unmount is
/// forced, or the NFS client would wait for servers that never answer. A
/// mount of another shape, or a directory with the user's files in it, is
/// left alone, and the root stays with it.
pub(super) fn remove_machine_exports(host: &dyn Host, root: &Path) -> Result<Outcome> {
    if !root.exists() {
        return Ok(skipped("absent"));
    }
    let mut kept = Vec::new();
    for entry in
        std::fs::read_dir(root).with_context(|| format!("could not read {}", root.display()))?
    {
        let path = entry?.path();
        match mount_at(&path) {
            Some(mount) if mount.is_machine_export() => {
                run_checked(host, "/sbin/umount", &[OsStr::new("-f"), path.as_os_str()])?;
            }
            Some(mount) => {
                kept.push(format!("{} ({})", mount.source, mount.fstype));
                continue;
            }
            None => {}
        }
        match std::fs::remove_dir(&path) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => {
                kept.push(path.display().to_string());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("could not remove {}", path.display()));
            }
        }
    }
    if !kept.is_empty() {
        return Ok(skipped(format!("left alone: {}", kept.join(", "))));
    }
    std::fs::remove_dir(root).with_context(|| format!("could not remove {}", root.display()))?;
    Ok(Outcome::Done)
}

struct Mount {
    source: String,
    fstype: String,
}

impl Mount {
    /// The shape the daemon creates: NFS from the loopback proxy, named
    /// either by address or by the `ArcBox` hosts alias.
    fn is_arcbox_export(&self) -> bool {
        self.fstype == "nfs"
            && (self.source == "127.0.0.1:/"
                || self.source == format!("{}:/", arcbox_helper::HOSTS_ALIAS_NAME))
    }

    /// The shape of a machine root mount: NFS from the root of a server
    /// named by its bridge address (never loopback, which is the docker
    /// export's proxy).
    fn is_machine_export(&self) -> bool {
        self.fstype == "nfs"
            && self
                .source
                .strip_suffix(":/")
                .and_then(|host| host.parse::<std::net::Ipv4Addr>().ok())
                .is_some_and(|host| !host.is_loopback())
    }
}

/// The mount whose mount point is exactly `path`, if `path` is one.
fn mount_at(path: &Path) -> Option<Mount> {
    use std::os::unix::ffi::OsStrExt as _;

    let canonical = std::fs::canonicalize(path).ok()?;
    let c_path = std::ffi::CString::new(canonical.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs fills the zeroed out-parameter for a valid NUL-terminated path.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    if unsafe { libc::statfs(c_path.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    // SAFETY: statfs NUL-terminates these fixed-size name buffers.
    let field = |bytes: &[libc::c_char]| {
        unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    (Path::new(&field(&stat.f_mntonname)) == canonical).then(|| Mount {
        source: field(&stat.f_mntfromname),
        fstype: field(&stat.f_fstypename),
    })
}

/// Removes trust in, and then the copy of, the ArcBox local CA that
/// `abctl tls trust` added to the login keychain. `security` asks for the
/// user's password, as it did when trust was granted.
pub(super) fn untrust_local_ca(host: &dyn Host, roots: &Roots) -> Result<Outcome> {
    let keychain = roots.login_keychain();
    let found = host.run(
        "security",
        &[
            OsStr::new("find-certificate"),
            OsStr::new("-c"),
            OsStr::new(LOCAL_CA_COMMON_NAME),
            keychain.as_os_str(),
        ],
    )?;
    if !found.status.success() {
        return Ok(skipped("not trusted"));
    }
    let ca = roots.local_ca();
    if ca.is_file() {
        run_checked(
            host,
            "security",
            &[OsStr::new("remove-trusted-cert"), ca.as_os_str()],
        )?;
    }
    run_checked(
        host,
        "security",
        &[
            OsStr::new("delete-certificate"),
            OsStr::new("-c"),
            OsStr::new(LOCAL_CA_COMMON_NAME),
            keychain.as_os_str(),
        ],
    )?;
    Ok(Outcome::Done)
}

/// Deletes the Desktop app's preferences domain through `defaults`, so
/// `cfprefsd` forgets them too, then the file if one is still there.
pub(super) fn delete_preferences(host: &dyn Host, roots: &Roots) -> Result<Outcome> {
    let plist = roots.preferences();
    if !plist.exists() {
        return Ok(skipped("absent"));
    }
    run_checked(
        host,
        "defaults",
        &[
            OsStr::new("delete"),
            OsStr::new(roots.profile.app_bundle_id()),
        ],
    )?;
    remove_path(host, &plist)?;
    Ok(Outcome::Done)
}
