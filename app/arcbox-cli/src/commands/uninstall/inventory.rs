//! Everything ArcBox writes to a Mac, and which of it is present.
//!
//! This list is the contract behind `abctl uninstall`; the "Uninstall"
//! section of `docs/data-directories.md` restates it for users. Change the
//! two together.
//!
//! Privileged paths are checked for ownership before they are listed, with
//! the same rules the helper applies when it creates them: a `/usr/local/bin`
//! link is ours when it points into an ArcBox bundle's `xbin/`
//! (`is_arcbox_owned`), `/var/run/docker.sock` when it points at a socket
//! under `~/.arcbox`, `/etc/resolver/arcbox.local` when it carries the
//! ArcBox marker. Whatever another tool owns is left alone and never shown.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arcbox_constants::paths::{
    ArcboxProfile, DOCKER_CLI_TOOLS, HostLayout, guest, is_arcbox_owned, labels, privileged,
    privileged_log,
};
use arcbox_helper::validate::SocketTarget;

/// Marker prefix shared by the resolver files `abctl dns install`
/// (`# managed by arcbox`) and the helper (`# managed by arcbox-helper`) write.
const RESOLVER_MARKER: &str = "# managed by arcbox";

/// Marker on the one `/etc/hosts` line the helper manages.
pub(super) const HOSTS_MARKER: &str = "# managed by arcbox-helper";

/// Where this Mac keeps ArcBox state.
pub(super) struct Roots {
    pub(super) profile: ArcboxProfile,
    /// The user's home directory.
    pub(super) home: PathBuf,
    /// The profile's data directory (`~/.arcbox`).
    pub(super) data_dir: PathBuf,
    /// The Docker CLI config directory (`~/.docker`, or `$DOCKER_CONFIG`).
    pub(super) docker_config: PathBuf,
    /// The Docker context this install created (`arcbox`).
    pub(super) docker_context: String,
    /// Root of the privileged paths: `/`. Tests point it at a directory.
    pub(super) system: PathBuf,
}

impl Roots {
    pub(super) fn from_env() -> Result<Self> {
        let profile = ArcboxProfile::from_env_or_default();
        let docker_context = std::env::var(arcbox_constants::env::DOCKER_CONTEXT)
            .ok()
            .filter(|context| !context.is_empty())
            .unwrap_or_else(|| profile.docker_context_name().to_owned());
        Ok(Self {
            profile,
            home: dirs::home_dir().context("cannot determine home directory")?,
            data_dir: HostLayout::from_env_or_default().data_dir,
            docker_config: crate::commands::cli_plugins::default_docker_config_dir()?,
            docker_context,
            system: PathBuf::from("/"),
        })
    }

    /// `absolute` relocated under [`Self::system`].
    pub(super) fn system_path(&self, absolute: &str) -> PathBuf {
        self.system.join(absolute.trim_start_matches('/'))
    }

    pub(super) fn launch_agent(&self, label: &str) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
    }

    pub(super) fn app_bundle(&self) -> PathBuf {
        self.system_path("/Applications")
            .join(format!("{}.app", self.profile.app_name()))
    }

    pub(super) fn hosts(&self) -> PathBuf {
        self.system_path("/etc/hosts")
    }

    /// The host-side mount of the guest's Docker data.
    pub(super) fn data_export_mount(&self) -> PathBuf {
        self.home.join("ArcBox")
    }

    /// The directory the daemon mounts each running machine's root under.
    pub(super) fn machine_mount_root(&self) -> PathBuf {
        self.home.join("ArcBoxMachines")
    }

    pub(super) fn login_keychain(&self) -> PathBuf {
        self.home.join("Library/Keychains/login.keychain-db")
    }

    pub(super) fn local_ca(&self) -> PathBuf {
        self.data_dir.join(guest::TLS).join(guest::TLS_CA_CERT)
    }

    /// The Desktop app's preferences, which `cfprefsd` caches: removed with
    /// `defaults delete`, not by unlinking the file.
    pub(super) fn preferences(&self) -> PathBuf {
        self.home
            .join("Library/Preferences")
            .join(format!("{}.plist", self.profile.app_bundle_id()))
    }

    /// Whether a Homebrew cask installed the app. Then the app and the
    /// `abctl` link in Homebrew's bin are brew's to remove: deleting the
    /// bundle here would make `brew uninstall --cask arcbox` fail on the
    /// missing app.
    pub(super) fn homebrew_manages_the_app(&self) -> bool {
        ["/opt/homebrew/Caskroom", "/usr/local/Caskroom"]
            .into_iter()
            .map(|caskroom| self.system_path(caskroom))
            .any(|caskroom| {
                ["arcbox", "arcbox@latest"]
                    .into_iter()
                    .any(|cask| caskroom.join(cask).is_dir())
            })
    }
}

/// A file or directory ArcBox left behind.
pub(super) struct Found {
    pub(super) what: &'static str,
    pub(super) path: PathBuf,
    /// Removing it needs root in a real install.
    pub(super) needs_root: bool,
}

/// What is present on this Mac out of everything ArcBox writes.
pub(super) struct Residue {
    pub(super) files: Vec<Found>,
    /// `/etc/hosts` carries the managed `ArcBox` alias line.
    pub(super) hosts_alias: bool,
    /// The helper LaunchDaemon plist is installed, so launchd has the job.
    pub(super) helper_registered: bool,
    /// The Desktop app's preferences domain exists.
    pub(super) preferences: bool,
    /// The app bundle exists but Homebrew owns it.
    pub(super) app_left_to_homebrew: bool,
}

impl Residue {
    pub(super) fn needs_root(&self) -> bool {
        self.hosts_alias || self.helper_registered || self.files.iter().any(|f| f.needs_root)
    }
}

/// Lists what is present. Read-only; ownership checks read links and files
/// as the current user.
pub(super) fn scan(roots: &Roots, keep_data: bool) -> Residue {
    let mut files = Vec::new();
    let mut found = |what, path: PathBuf, needs_root| {
        if path.symlink_metadata().is_ok() {
            files.push(Found {
                what,
                path,
                needs_root,
            });
        }
    };

    // Privileged, by path.
    found(
        "helper binary",
        roots.system_path(privileged::HELPER_BINARY),
        true,
    );
    found(
        "helper LaunchDaemon",
        roots.system_path(privileged::HELPER_PLIST),
        true,
    );
    found(
        "helper socket",
        roots.system_path(privileged::HELPER_SOCKET),
        true,
    );
    found(
        "helper log directory",
        roots.system_path(privileged_log::HELPER_LOG_DIR),
        true,
    );
    let bin = roots.system_path("/usr/local/bin");
    for name in ["abctl", "arcbox-daemon"] {
        found("command-line binary", bin.join(name), true);
    }

    // Privileged, by ownership.
    let resolver = roots.system_path(&format!(
        "/etc/resolver/{}",
        arcbox_constants::dns::LOCAL_DOMAIN
    ));
    if is_managed_resolver(&resolver) {
        found("DNS resolver", resolver, true);
    }
    let docker_socket = roots.system_path(privileged::DOCKER_SOCKET);
    if is_arcbox_socket_link(&docker_socket) {
        found("Docker socket link", docker_socket, true);
    }
    for name in DOCKER_CLI_TOOLS {
        let link = bin.join(name);
        if std::fs::read_link(&link).is_ok_and(|target| is_arcbox_owned(&target)) {
            found("Docker CLI link", link, true);
        }
    }
    let hosts_alias = std::fs::read_to_string(roots.hosts())
        .is_ok_and(|content| content.lines().any(|line| line.contains(HOSTS_MARKER)));
    let helper_registered = roots.system_path(privileged::HELPER_PLIST).exists();

    // The user's own.
    for label in [roots.profile.daemon_label(), labels::LEGACY_SCRIPT_DAEMON] {
        found("daemon LaunchAgent", roots.launch_agent(label), false);
    }
    let library = roots.home.join("Library");
    let bundle_id = roots.profile.app_bundle_id();
    for (what, path) in [
        (
            "Desktop app support",
            library.join("Application Support").join(bundle_id),
        ),
        ("Desktop app cache", library.join("Caches").join(bundle_id)),
        (
            "Desktop app HTTP storage",
            library.join("HTTPStorages").join(bundle_id),
        ),
        (
            "Desktop app window state",
            library
                .join("Saved Application State")
                .join(format!("{bundle_id}.savedState")),
        ),
        ("log directory", library.join("Logs").join("arcbox")),
    ] {
        found(what, path, false);
    }
    let preferences = roots.preferences().exists();

    if keep_data {
        if let Ok(entries) = std::fs::read_dir(&roots.data_dir) {
            for entry in entries.flatten() {
                if entry.file_name() != "data" {
                    found("data directory entry", entry.path(), false);
                }
            }
        }
    } else {
        found("data directory", roots.data_dir.clone(), false);
    }

    let app = roots.app_bundle();
    let app_left_to_homebrew = app.exists() && roots.homebrew_manages_the_app();
    if !app_left_to_homebrew {
        found("app bundle", app, false);
    }

    Residue {
        files,
        hosts_alias,
        helper_registered,
        preferences,
        app_left_to_homebrew,
    }
}

fn is_managed_resolver(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|content| {
        content
            .lines()
            .any(|line| line.trim_start().starts_with(RESOLVER_MARKER))
    })
}

fn is_arcbox_socket_link(path: &Path) -> bool {
    std::fs::read_link(path).is_ok_and(|target| {
        target
            .to_str()
            .is_some_and(|target| target.parse::<SocketTarget>().is_ok())
    })
}
