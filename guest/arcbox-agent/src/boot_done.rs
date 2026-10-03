//! The guest's own declaration that the distro's init has finished booting.
//!
//! A Machine runs an upstream distro image whose init starts *after* the
//! agent — the boot shim runs `machine-init`, backgrounds the agent, then
//! `exec`s `/sbin/init` — and that init typically reconfigures the network
//! from scratch, flushing the interface the shim already configured. The host
//! gates machine readiness on this signal so `Start` does not return into
//! that window (CORE-66).
//!
//! The signal is a sentinel written by a hook ordered at the end of the
//! distro's own boot sequence, not an inspection of the init system's runtime
//! state. That follows what comparable runtimes do:
//!
//! - **Lima** polls `/run/lima-boot-done` for the instance id, written by the
//!   boot scripts it injects (`pkg/hostagent/requirements.go`).
//! - **Multipass** waits on cloud-init's `/var/lib/cloud/instance/boot-finished`
//!   (`base_virtual_machine.cpp`, `wait_for_cloud_init`).
//! - **Incus** has the guest declare itself: `PATCH /1.0 {"state":"Ready"}` on
//!   the devIncus socket, recorded as `volatile.last_state.ready` — documented
//!   as "Instance marked itself as ready".
//!
//! None of them read the init system's internals from outside, and the reason
//! shows up immediately when you try: `/run/openrc/rc.starting` is absent both
//! *before* openrc runs and *after* it finishes, so the obvious check reports
//! "settled" during exactly the window it exists to catch. Unit ordering is a
//! public contract; a runtime directory's layout is not.
//!
//! cloud-init would be the standard vehicle, but the images ArcBox mirrors are
//! the linuxcontainers `default` variant, which does not ship it (verified: an
//! `ubuntu-noble` machine boots with no cloud-init at all). The hook is
//! installed directly instead, which the writable overlay already allows —
//! `machine-init` writes `/etc/resolv.conf` and the DHCP script the same way.
//!
//! The hook is one shell script ([`HOOK_SCRIPT`]) that each init runs at the
//! end of its boot. Besides the sentinel it finishes what `machine-init`
//! could only start: the provisional default route the shim installs so the
//! agent has egress before the distro's network manager runs is removed once
//! that manager has installed its own ([`hook_script_body`]).

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use arcbox_constants::cmdline::AGENT_DHCP_ROUTE_PROTO;

/// Sentinel carrying the boot it was written for. Lives on `/run` (tmpfs, so
/// it starts each boot empty); the boot id is compared anyway, so a `/run`
/// that persists cannot make a previous boot's sentinel look current.
pub const SENTINEL: &str = "/run/arcbox-boot-done";

/// The script every init adapter runs; see [`hook_script_body`].
pub const HOOK_SCRIPT: &str = "/etc/arcbox/boot-done.sh";

const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const SYSTEMD_UNIT: &str = "/etc/systemd/system/arcbox-boot-done.service";
const SYSTEMD_TARGET_DROP_IN: &str =
    "/etc/systemd/system/multi-user.target.d/arcbox-boot-done.conf";
const OPENRC_SERVICE: &str = "/etc/init.d/arcbox-boot-done";
const OPENRC_RUNLEVEL: &str = "/etc/runlevels/default/arcbox-boot-done";
const SYSVINIT_INITTAB: &str = "/etc/inittab";
const SYSVINIT_RC: &str = "/etc/init.d/rc";
/// The id field of the hook's inittab entry, which is how an install on a
/// later boot finds it already there.
const SYSVINIT_ENTRY_ID: &str = "abd";

/// What the hook does before writing the sentinel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hook {
    /// The uplink interface `machine-init` put its provisional default route
    /// on; `None` when it configured none, and the hook then only writes
    /// the sentinel.
    pub primary_interface: Option<String>,
}

/// The hook script.
///
/// `machine-init` installs a default route tagged [`AGENT_DHCP_ROUTE_PROTO`]
/// so the agent has egress before the distro's init runs. Network managers
/// that reconfigure the interface from scratch (networkd, dhclient's
/// `replace`) remove it on their own; one that adds its route next to ours
/// (alpine's dhcpcd, measured 2026-09-27: two defaults via eth0) leaves two,
/// so the hook drops ours once a second default route on that interface
/// exists. It never drops ours while it is the only one: a distro that
/// configured no network at all would otherwise lose its egress. The
/// sentinel write comes last, so readiness sees a settled routing table.
///
/// `/bin/busybox` is the distro's own on BusyBox images and the static one
/// the shim copies in everywhere else, so the script runs the same way on
/// every init.
fn hook_script_body(hook: &Hook) -> String {
    let route_cleanup = hook.primary_interface.as_deref().map_or_else(String::new, |iface| {
        format!(
            "if [ \"$($bb ip -4 route show default dev {iface} | $bb wc -l)\" -gt 1 ]; then\n  $bb ip route del default dev {iface} proto {AGENT_DHCP_ROUTE_PROTO}\nfi\n"
        )
    });
    format!(
        "#!/bin/sh\n# ArcBox boot-completion hook, rewritten by `arcbox-agent machine-init` on\n# every boot. Runs once the distro's own init has finished booting.\nbb=/bin/busybox\n{route_cleanup}$bb cat {BOOT_ID} > {SENTINEL}\n"
    )
}

/// The unit that runs the hook.
///
/// [`SYSTEMD_TARGET_DROP_IN`] pulls it into the boot; `After=` on the same
/// target orders it after that target is *reached*. Be precise about what
/// that buys: `Wants=` adds no ordering of its own, so the target is reached
/// once the units that declare `Before=multi-user.target` are done —
/// conventional for distro service units, but not something `Wants=`
/// guarantees. `After=network-online.target` is listed as well and costs
/// nothing: without a matching `Wants=` it constrains ordering only on images
/// where something else already activates that target, and imposes nothing
/// where nothing does. Deliberately no `Wants=network-online.target` — on an
/// image with no wait-online provider the target never activates, and the
/// hook would never run.
///
/// Deliberately no `[Install]` section either, which makes the unit static:
/// the first boot of an image with an uninitialized machine id applies the
/// preset policy, and on a `disable *` distro (Fedora, the RHEL family) that
/// removed a `multi-user.target.wants` link before the unit ever ran —
/// readiness then waited out its whole timeout. Presets act on `[Install]`
/// sections, never on a target's drop-in.
fn systemd_unit_body() -> String {
    format!(
        "[Unit]
Description=ArcBox boot-completion sentinel
After=multi-user.target
After=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={HOOK_SCRIPT}
"
    )
}

/// The drop-in through which `multi-user.target` wants the unit.
const SYSTEMD_TARGET_DROP_IN_BODY: &str = "[Unit]\nWants=arcbox-boot-done.service\n";

/// `after *` orders this behind every other service in the runlevel, which
/// is openrc's own way to say "last".
fn openrc_service_body() -> String {
    format!(
        "#!/sbin/openrc-run
description=\"ArcBox boot-completion sentinel\"

depend() {{
    after *
}}

start() {{
    {HOOK_SCRIPT}
}}

stop() {{
    return 0
}}
"
    )
}

/// The inittab entry that runs the hook.
///
/// sysvinit runs a runlevel's `wait` entries one at a time, in file order, so
/// an entry appended after the `rc N` lines runs once every script of the
/// runlevel has finished — sysvinit's way to say "last". Devuan needs it: its
/// image ships the build host's `resolv.conf` (`nameserver 127.0.0.53`, with
/// no resolver behind it), and until `rc 2` has run dhclient, a started
/// machine could not resolve anything.
fn sysvinit_entry() -> String {
    format!("{SYSVINIT_ENTRY_ID}:2345:wait:{HOOK_SCRIPT}")
}

fn is_sysvinit_entry(line: &str) -> bool {
    line.split(':').next() == Some(SYSVINIT_ENTRY_ID)
}

fn has_sysvinit_entry(inittab: &str) -> bool {
    inittab.lines().any(is_sysvinit_entry)
}

/// `inittab` with the hook's entry current: an earlier boot's entry (an
/// older agent wrote the sentinel inline) is replaced in place, a missing
/// one is appended after the `rc N` lines.
fn with_sysvinit_entry(inittab: &str) -> String {
    let entry = sysvinit_entry();
    let mut lines: Vec<&str> = inittab.lines().collect();
    match lines.iter().position(|line| is_sysvinit_entry(line)) {
        Some(i) => lines[i] = &entry,
        None => lines.push(&entry),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// The filesystem the hook is installed into.
///
/// Rooted rather than hardcoded so the install path — the half with the
/// rollbacks — can be exercised against a temporary directory instead of the
/// running host's own `/etc`. Production is always [`Layout::guest`].
///
/// Paths *written into* the hook bodies stay absolute: they are resolved by
/// the guest's init at boot, not through this root.
struct Layout {
    root: PathBuf,
}

impl Layout {
    /// The guest's own filesystem. Only the in-guest (Linux) build has one;
    /// a host build compiles this module for its tests alone, which root
    /// themselves in a temporary directory.
    #[cfg(target_os = "linux")]
    fn guest() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    /// Whether a hook is installed, i.e. whether a sentinel is coming at all.
    ///
    /// This is what tells readiness to wait: an image whose init we do not
    /// recognize gets no hook, and readiness must not block on a signal
    /// nothing will ever send. The hook itself *is* the marker — there is no
    /// second piece of state to keep in sync with it.
    fn hook_installed(&self) -> bool {
        self.path(SYSTEMD_UNIT).exists()
            || self.path(OPENRC_SERVICE).exists()
            || fs::read_to_string(self.path(SYSVINIT_INITTAB))
                .is_ok_and(|inittab| has_sysvinit_entry(&inittab))
    }

    /// Whether the sentinel was written by the boot that is running now.
    fn boot_complete(&self) -> bool {
        let (Ok(sentinel), Ok(boot_id)) = (
            fs::read_to_string(self.path(SENTINEL)),
            fs::read_to_string(self.path(BOOT_ID)),
        ) else {
            return false;
        };
        sentinel_matches(&sentinel, &boot_id)
    }
}

/// Whether a hook is installed in the guest; see [`Layout::hook_installed`].
#[cfg(target_os = "linux")]
#[must_use]
pub fn hook_installed() -> bool {
    Layout::guest().hook_installed()
}

/// Whether the guest's sentinel names the running boot; see
/// [`Layout::boot_complete`].
#[cfg(target_os = "linux")]
#[must_use]
pub fn boot_complete() -> bool {
    Layout::guest().boot_complete()
}

/// Whether `sentinel` names the boot `boot_id` identifies.
///
/// The boot id is what makes the sentinel self-invalidating: `/run` is tmpfs
/// on every image seen so far, but an image where it is not would otherwise
/// carry a previous boot's sentinel into the window this is meant to catch.
/// An empty boot id (unreadable `/proc`) matches nothing rather than
/// everything.
fn sentinel_matches(sentinel: &str, boot_id: &str) -> bool {
    let boot_id = boot_id.trim();
    !boot_id.is_empty() && sentinel.trim() == boot_id
}

/// Installs the boot-completion hook for the distro's init, if recognized.
///
/// Best-effort by design: a failure here means readiness falls back to not
/// waiting (`hook_installed` stays false), which is the behavior that
/// predates the hook — strictly better than a half-installed hook that never
/// fires and burns the readiness timeout instead.
#[cfg(target_os = "linux")]
pub fn install(hook: &Hook) -> bool {
    Layout::guest().install(hook)
}

impl Layout {
    /// Installs the hook for whichever init this image ships, if recognized.
    ///
    /// The script is written first and only then wired into the init, so
    /// an init adapter never points at a script that is not there.
    fn install(&self, hook: &Hook) -> bool {
        if let Err(e) = write_file(&self.path(HOOK_SCRIPT), &hook_script_body(hook), 0o755) {
            tracing::warn!(error = %e, "failed to write the boot-done hook script");
            return false;
        }
        if self.path("/usr/lib/systemd/systemd").exists()
            || self.path("/lib/systemd/systemd").exists()
        {
            return self.install_systemd();
        }
        if self.path("/sbin/openrc").exists() || self.path("/usr/libexec/rc").is_dir() {
            return self.install_openrc();
        }
        // `/etc/init.d/rc` tells sysv-rc apart from BusyBox init, which reads
        // an inittab too but has neither runlevels nor that script.
        if self.path(SYSVINIT_INITTAB).is_file() && self.path(SYSVINIT_RC).is_file() {
            return self.install_sysvinit();
        }
        tracing::info!(
            "no recognized distro init; machine readiness will not wait for boot to settle"
        );
        false
    }

    fn install_systemd(&self) -> bool {
        let unit = self.path(SYSTEMD_UNIT);
        if let Err(e) = write_file(&unit, &systemd_unit_body(), 0o644) {
            tracing::warn!(error = %e, "failed to write the systemd boot-done unit");
            return false;
        }
        // Wired in by file rather than `systemctl`: systemd is not running
        // yet at this point in the boot shim, so there is nothing to ask.
        let drop_in = self.path(SYSTEMD_TARGET_DROP_IN);
        if let Err(e) = write_file(&drop_in, SYSTEMD_TARGET_DROP_IN_BODY, 0o644) {
            tracing::warn!(error = %e, "failed to wire the systemd boot-done unit into the boot");
            let _ = fs::remove_file(&unit);
            return false;
        }
        tracing::info!("installed the systemd boot-completion hook");
        true
    }

    fn install_openrc(&self) -> bool {
        let service = self.path(OPENRC_SERVICE);
        if let Err(e) = write_file(&service, &openrc_service_body(), 0o755) {
            tracing::warn!(error = %e, "failed to write the openrc boot-done service");
            return false;
        }
        // `rc-update add` would work, but openrc is not running yet either;
        // the runlevel is just a directory of symlinks.
        let runlevel = self.path(OPENRC_RUNLEVEL);
        if let Some(parent) = runlevel.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            tracing::warn!(error = %e, "failed to create the openrc default runlevel dir");
            let _ = fs::remove_file(&service);
            return false;
        }
        let _ = fs::remove_file(&runlevel);
        if let Err(e) = std::os::unix::fs::symlink(OPENRC_SERVICE, &runlevel) {
            tracing::warn!(error = %e, "failed to add the openrc boot-done service to the runlevel");
            let _ = fs::remove_file(&service);
            return false;
        }
        tracing::info!("installed the openrc boot-completion hook");
        true
    }

    /// Keeps one entry current; the overlay keeps it for later boots, and a
    /// boot that finds it already right leaves `/etc/inittab` untouched.
    fn install_sysvinit(&self) -> bool {
        let path = self.path(SYSVINIT_INITTAB);
        let inittab = match fs::read_to_string(&path) {
            Ok(inittab) => inittab,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read /etc/inittab");
                return false;
            }
        };
        let wanted = with_sysvinit_entry(&inittab);
        if wanted != inittab
            && let Err(e) = write_file(&path, &wanted, 0o644)
        {
            tracing::warn!(error = %e, "failed to add the boot-done entry to /etc/inittab");
            return false;
        }
        tracing::info!("installed the sysvinit boot-completion hook");
        true
    }
}

/// Writes the hook body, staged then renamed.
///
/// The final path must never hold a partial file: [`Layout::hook_installed`]
/// keys on its existence, so a truncated body or a service left non-executable
/// would still read as "a sentinel is coming" — and readiness would wait out
/// its full 60 s timeout instead of falling back to not waiting at all, which
/// is the expensive half of the failure space this module is built to avoid.
/// Staging makes that state unreachable rather than merely unlikely: the mode
/// is set before the rename, and the rename is atomic.
fn write_file(path: &Path, body: &str, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged = path.with_extension("arcbox-tmp");
    let result = stage(&staged, body, mode).and_then(|()| fs::rename(&staged, path));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn stage(staged: &Path, body: &str, mode: u32) -> std::io::Result<()> {
    let mut file = fs::File::create(staged)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(staged, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty image rooted in a temp dir: no init, no hook, no sentinel.
    fn image() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout {
            root: dir.path().to_path_buf(),
        };
        (dir, layout)
    }

    fn touch(layout: &Layout, absolute: &str) {
        let path = layout.path(absolute);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "").expect("write");
    }

    fn hook() -> Hook {
        Hook {
            primary_interface: Some("eth0".to_owned()),
        }
    }

    /// An image whose init we do not recognize gets no hook, and readiness
    /// must then not wait — `hook_installed` staying false is what encodes
    /// "no signal is coming".
    #[test]
    fn an_unrecognized_init_installs_nothing_and_promises_nothing() {
        let (_dir, layout) = image();
        assert!(!layout.install(&hook()));
        assert!(!layout.hook_installed());
    }

    /// The route cleanup is the half of the hook a wrong line in would make
    /// a machine lose its egress: it must name the interface and the tag
    /// `machine-init` used, and it must only ever run behind a count that
    /// found a second default route.
    #[test]
    fn the_script_removes_only_a_duplicated_provisional_route() {
        let body = hook_script_body(&hook());
        assert!(body.starts_with("#!/bin/sh\n"), "{body}");
        assert!(
            body.contains("ip -4 route show default dev eth0 | $bb wc -l)\" -gt 1 ]"),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "ip route del default dev eth0 proto {AGENT_DHCP_ROUTE_PROTO}"
            )),
            "{body}"
        );
        // The sentinel is the last thing the hook does.
        assert!(
            body.trim_end()
                .ends_with(&format!("{BOOT_ID} > {SENTINEL}")),
            "{body}"
        );

        let without = hook_script_body(&Hook::default());
        assert!(!without.contains("route"), "{without}");
        assert!(without.contains(SENTINEL), "{without}");
    }

    /// The unit must be pulled in by the target's drop-in and carry no
    /// `[Install]` section: a first boot applies the distro's preset policy,
    /// and `disable *` (Fedora, the RHEL family) removed the `.wants` link
    /// this used to rely on, leaving an inert unit and a readiness timeout.
    #[test]
    fn a_systemd_image_gets_a_unit_presets_cannot_disable() {
        let (_dir, layout) = image();
        touch(&layout, "/usr/lib/systemd/systemd");

        assert!(layout.install(&hook()));
        assert!(layout.hook_installed());
        let drop_in = fs::read_to_string(layout.path(SYSTEMD_TARGET_DROP_IN)).expect("drop-in");
        assert!(
            drop_in.contains("Wants=arcbox-boot-done.service"),
            "{drop_in}"
        );
        let unit = fs::read_to_string(layout.path(SYSTEMD_UNIT)).expect("unit");
        assert!(!unit.contains("[Install]"), "{unit}");
        assert!(unit.contains(&format!("ExecStart={HOOK_SCRIPT}")), "{unit}");
        let mode = fs::metadata(layout.path(HOOK_SCRIPT))
            .expect("script")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "every init execs the script directly");
    }

    #[test]
    fn an_openrc_image_gets_a_service_in_the_default_runlevel() {
        let (_dir, layout) = image();
        touch(&layout, "/sbin/openrc");

        assert!(layout.install(&hook()));
        assert!(layout.hook_installed());
        let runlevel = layout.path(OPENRC_RUNLEVEL);
        assert_eq!(
            fs::read_link(&runlevel).expect("symlink"),
            Path::new(OPENRC_SERVICE)
        );
        let mode = fs::metadata(layout.path(OPENRC_SERVICE))
            .expect("service")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "openrc runs the service as a program");
    }

    /// The entry must land after the `rc N` lines, since sysvinit runs `wait`
    /// entries in file order, and a later boot must find it rather than
    /// append a second one.
    #[test]
    fn a_sysvinit_image_gets_one_inittab_entry_after_the_runlevels() {
        let (_dir, layout) = image();
        touch(&layout, SYSVINIT_RC);
        let inittab = layout.path(SYSVINIT_INITTAB);
        fs::write(
            &inittab,
            "id:2:initdefault:\nl2:2:wait:/etc/init.d/rc 2\nl3:3:wait:/etc/init.d/rc 3",
        )
        .expect("write");

        assert!(layout.install(&hook()));
        assert!(layout.install(&hook()));
        assert!(layout.hook_installed());
        let lines: Vec<String> = fs::read_to_string(&inittab)
            .expect("inittab")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[2], "l3:3:wait:/etc/init.d/rc 3");
        assert_eq!(lines[3], sysvinit_entry());
    }

    /// An inittab from a machine first booted by an older agent carries the
    /// entry in that agent's shape; the install must bring it up to date
    /// rather than leave the old command running forever.
    #[test]
    fn a_stale_sysvinit_entry_is_replaced_in_place() {
        let (_dir, layout) = image();
        touch(&layout, SYSVINIT_RC);
        let inittab = layout.path(SYSVINIT_INITTAB);
        fs::write(
            &inittab,
            "l2:2:wait:/etc/init.d/rc 2\nabd:2345:wait:/bin/sh -c 'cat x > y'\nz6:6:respawn:/sbin/sulogin\n",
        )
        .expect("write");

        assert!(layout.install(&hook()));
        let lines: Vec<String> = fs::read_to_string(&inittab)
            .expect("inittab")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[1], sysvinit_entry());
        assert_eq!(lines[2], "z6:6:respawn:/sbin/sulogin");
    }

    /// BusyBox init reads an inittab too, but has no runlevels to order
    /// against; without sysv-rc's `rc` script the image gets no hook.
    #[test]
    fn a_busybox_inittab_alone_is_not_sysvinit() {
        let (_dir, layout) = image();
        touch(&layout, SYSVINIT_INITTAB);
        assert!(!layout.install(&hook()));
        assert!(!layout.hook_installed());
    }

    /// A half-installed hook is the expensive failure: it never fires and
    /// burns the readiness timeout. A failed drop-in must therefore roll the
    /// unit back, leaving `hook_installed` false — the pre-hook behaviour.
    #[test]
    fn a_failed_enable_rolls_the_unit_back() {
        let (_dir, layout) = image();
        touch(&layout, "/usr/lib/systemd/systemd");
        // A regular file where the drop-in directory must go: create_dir_all
        // fails, so the target can never want the unit.
        touch(&layout, "/etc/systemd/system/multi-user.target.d");

        assert!(!layout.install_systemd());
        assert!(!layout.path(SYSTEMD_UNIT).exists());
        assert!(!layout.hook_installed());
    }

    /// A write that cannot complete must leave nothing at the destination.
    /// `hook_installed` keys on existence, so a partial hook would promise a
    /// sentinel it can never write, and readiness would burn its full timeout
    /// instead of falling back to not waiting.
    #[test]
    fn a_failed_write_leaves_no_hook_behind() {
        let (_dir, layout) = image();
        touch(&layout, "/usr/lib/systemd/systemd");
        // A directory where the staged file must go, so `File::create` fails
        // partway through what would otherwise be a successful install.
        let staged = layout.path(SYSTEMD_UNIT).with_extension("arcbox-tmp");
        fs::create_dir_all(&staged).expect("mkdir");

        assert!(!layout.install(&hook()));
        assert!(!layout.path(SYSTEMD_UNIT).exists());
        assert!(!layout.hook_installed());
    }

    /// The sentinel is only meaningful against the running boot.
    #[test]
    fn boot_complete_reads_the_sentinel_against_this_boot() {
        let (_dir, layout) = image();
        fs::create_dir_all(layout.path(SENTINEL).parent().expect("parent")).expect("mkdir");
        fs::create_dir_all(layout.path(BOOT_ID).parent().expect("parent")).expect("mkdir");
        fs::write(layout.path(BOOT_ID), "boot-2\n").expect("write");

        assert!(!layout.boot_complete(), "no sentinel yet");

        fs::write(layout.path(SENTINEL), "boot-1\n").expect("write");
        assert!(!layout.boot_complete(), "sentinel from the previous boot");

        fs::write(layout.path(SENTINEL), "boot-2\n").expect("write");
        assert!(layout.boot_complete());
    }

    /// Generating the bodies with `format!` means the shell braces in the
    /// openrc service have to be escaped, and getting that wrong yields a
    /// file openrc cannot parse — a hook that installs and never fires,
    /// which is the one failure mode that costs a readiness timeout rather
    /// than degrading safely.
    #[test]
    fn the_openrc_body_survives_format_escaping() {
        let body = openrc_service_body();
        assert!(body.contains("depend() {\n    after *\n}"), "{body}");
        assert!(body.contains("start() {\n"), "{body}");
        assert!(!body.contains("{{") && !body.contains("}}"), "{body}");
    }

    /// A leftover sentinel from a previous boot must not read as complete —
    /// the case a bare "does the file exist" check would get wrong.
    #[test]
    fn a_sentinel_from_another_boot_does_not_count() {
        assert!(!sentinel_matches("stale-boot-id", "current-boot-id"));
    }

    /// The hook writes with `cat`, so the sentinel carries the trailing
    /// newline `/proc/sys/kernel/random/boot_id` has.
    #[test]
    fn the_trailing_newline_the_hook_writes_is_tolerated() {
        assert!(sentinel_matches("boot-id\n", "boot-id\n"));
    }

    /// An unreadable boot id must match nothing rather than everything.
    #[test]
    fn an_empty_boot_id_never_matches() {
        assert!(!sentinel_matches("", ""));
        assert!(!sentinel_matches("anything", ""));
    }
}
