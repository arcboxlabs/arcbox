//! `abctl uninstall`: remove ArcBox from this Mac.
//!
//! The inventory of what ArcBox writes lives in [`inventory`]; the actions
//! in [`steps`]. This file orders them and reports each one honestly: a
//! step prints `done`, `skipped (why)`, or `FAILED: why`, and the command
//! exits non-zero when any step failed (#716).
//!
//! Run as the user, not under `sudo`: the Docker context, shell profile,
//! kubeconfig and login keychain are the user's. Privileged paths are
//! removed through `sudo`, which asks once up front.

mod host;
mod inventory;
mod steps;

use std::io::Write as _;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::{ArcboxProfile, HostLayout, labels};
use arcbox_docker::DockerContextManager;
use clap::Args;

use self::host::{Host, Mac};
use self::inventory::{Residue, Roots};
use self::steps::Outcome;
use super::{kubernetes, setup, ssh};

/// Remove ArcBox from this Mac: daemon, helper, links, integrations, data.
#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Keep container, image and machine data (`~/.arcbox/data`).
    #[arg(long)]
    pub keep_data: bool,
}

pub async fn execute(args: UninstallArgs) -> Result<()> {
    let profile = ArcboxProfile::from_env_or_default();
    validate_uninstall_scope(profile)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        bail!(
            "run `abctl uninstall` as yourself, not under sudo: it removes your Docker context, shell integration and keychain trust, and asks for sudo itself"
        );
    }

    let roots = Roots::from_env()?;
    let residue = inventory::scan(&roots, args.keep_data);
    print_plan(&roots, &residue);

    if !args.yes {
        print!("Continue? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }
    println!();

    let host = Mac;
    if residue.needs_root() {
        // One password prompt, up front, instead of one per privileged step.
        // Inherits the terminal: sudo reads the password from it.
        let status = std::process::Command::new("sudo")
            .arg("-v")
            .status()
            .context("could not run sudo")?;
        if !status.success() {
            bail!("sudo authentication failed");
        }
    }

    let report = run(&host, &roots, &residue, &mut |step| println!("{step}")).await;
    println!();
    if report.iter().any(Step::failed) {
        bail!(
            "{} step(s) failed; fix the cause and run `abctl uninstall` again",
            report.iter().filter(|step| step.failed()).count()
        );
    }
    println!("ArcBox has been removed.");
    if args.keep_data {
        println!(
            "Container data kept at {}",
            roots.data_dir.join("data").display()
        );
    }
    if residue.app_left_to_homebrew {
        println!("The app was installed by Homebrew; finish with: brew uninstall --cask arcbox");
    }
    Ok(())
}

fn print_plan(roots: &Roots, residue: &Residue) {
    println!("This will remove ArcBox from this Mac:\n");
    println!("  • Quit the Desktop app and stop the daemon (and its VM)");
    for found in &residue.files {
        let root = if found.needs_root { "  [sudo]" } else { "" };
        println!("  • Remove {} {}{root}", found.what, found.path.display());
    }
    if residue.hosts_alias {
        println!("  • Remove the ArcBox line from /etc/hosts  [sudo]");
    }
    println!("  • Remove the Docker context, shell, kubectl and ssh integration");
    println!("  • Remove trust in the ArcBox local CA, if granted");
    if residue.preferences {
        println!("  • Remove the Desktop app's preferences");
    }
    if residue.app_left_to_homebrew {
        println!(
            "  • Leave {} to Homebrew (brew uninstall --cask arcbox)",
            roots.app_bundle().display()
        );
    }
    println!();
}

/// One step of the run, as shown to the user.
struct Step {
    label: String,
    outcome: Result<Outcome>,
}

impl Step {
    fn failed(&self) -> bool {
        self.outcome.is_err()
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:<60} ", self.label)?;
        match &self.outcome {
            Ok(Outcome::Done) => write!(f, "done"),
            Ok(Outcome::Skipped(reason)) => write!(f, "skipped ({reason})"),
            Err(error) => write!(f, "FAILED: {error:#}"),
        }
    }
}

/// Runs every step against `residue`, calling `report` as each finishes.
///
/// Order matters: the app quits before the daemon is stopped (its handler
/// stops the daemon itself); integrations that need binaries from the data
/// directory (kubectl, the CA certificate) run before the data directory is
/// removed; the helper is unregistered before its plist goes.
async fn run(
    host: &dyn Host,
    roots: &Roots,
    residue: &Residue,
    report: &mut (dyn FnMut(&Step) + Send),
) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut step = |label: String, outcome: Result<Outcome>| {
        let step = Step { label, outcome };
        report(&step);
        steps.push(step);
    };
    let layout = HostLayout::new(roots.data_dir.clone());

    step(
        format!("Quitting {}", roots.profile.app_name()),
        steps::quit_app(host, roots.profile.app_bundle_id()),
    );
    step(
        "Stopping the daemon".into(),
        steps::stop_daemon(
            host,
            &layout,
            &[roots.profile.daemon_label(), labels::LEGACY_SCRIPT_DAEMON],
        )
        .await,
    );
    if residue.helper_registered {
        step(
            "Unregistering the helper".into(),
            steps::bootout_helper(host),
        );
    }
    step(
        format!("Unmounting {}", roots.data_export_mount().display()),
        steps::remove_data_export(host, &roots.data_export_mount()),
    );
    step(
        format!(
            "Unmounting machines under {}",
            roots.machine_mount_root().display()
        ),
        steps::remove_machine_exports(host, &roots.machine_mount_root()),
    );
    step(
        "Removing the Docker context".into(),
        remove_docker_context(roots),
    );
    step(
        "Removing shell integration".into(),
        remove_shell_integration(roots).await,
    );
    step(
        "Removing kubectl integration".into(),
        remove_kubernetes_integration(roots).await,
    );
    step(
        "Removing the ssh config Include".into(),
        remove_ssh_include(roots, &layout),
    );
    step(
        "Removing trust in the ArcBox local CA".into(),
        steps::untrust_local_ca(host, roots),
    );
    if residue.hosts_alias {
        step(
            "Removing the ArcBox line from /etc/hosts".into(),
            steps::remove_hosts_alias(host, &roots.hosts()),
        );
    }
    for found in &residue.files {
        step(
            format!("Removing {} {}", found.what, found.path.display()),
            steps::remove_path(host, &found.path).map(|()| Outcome::Done),
        );
    }
    if residue.preferences {
        step(
            "Removing the Desktop app's preferences".into(),
            steps::delete_preferences(host, roots),
        );
    }
    steps
}

fn remove_docker_context(roots: &Roots) -> Result<Outcome> {
    let manager = DockerContextManager::with_context_name_and_config_dir(
        HostLayout::new(roots.data_dir.clone()).docker_socket,
        roots.docker_context.clone(),
        roots.docker_config.clone(),
    );
    if !manager.context_exists() {
        return Ok(Outcome::Skipped("absent".into()));
    }
    // Restores the previous current context before removing ours: `docker
    // context rm` refuses the context in use (#716).
    manager.remove_context()?;
    Ok(Outcome::Done)
}

async fn remove_shell_integration(roots: &Roots) -> Result<Outcome> {
    let integration =
        setup::Integration::under(&roots.home, &roots.data_dir, roots.docker_config.clone())
            .await?;
    let removed = setup::remove_integration(&integration).await?;
    if let Some(error) = removed.plugin_error {
        bail!("Docker CLI plugins: {error}");
    }
    Ok(Outcome::Done)
}

async fn remove_kubernetes_integration(roots: &Roots) -> Result<Outcome> {
    let paths = kubernetes::HostPaths {
        home: roots.home.clone(),
        data_dir: roots.data_dir.clone(),
    };
    if kubernetes::remove_host_integration(&paths).await? {
        Ok(Outcome::Done)
    } else {
        Ok(Outcome::Skipped("not enabled".into()))
    }
}

fn remove_ssh_include(roots: &Roots, layout: &HostLayout) -> Result<Outcome> {
    let user_config = roots.home.join(".ssh").join("config");
    if !user_config.exists() {
        return Ok(Outcome::Skipped("absent".into()));
    }
    if ssh::uninstall(&user_config, &layout.ssh_config, &roots.home)? {
        Ok(Outcome::Done)
    } else {
        Ok(Outcome::Skipped("not included".into()))
    }
}

fn validate_uninstall_scope(profile: ArcboxProfile) -> Result<()> {
    if profile == ArcboxProfile::Development {
        bail!(
            "Development instances must be removed by the Desktop app; refusing the machine-wide uninstall command."
        );
    }
    Ok(())
}

/// The user-level part of an uninstall, for the Homebrew cask's pre-uninstall
/// hook (`abctl _internal brew-uninstall`): the daemon, its LaunchAgent, the
/// Docker context, the shell integration, our `/usr/local/bin/docker*` links
/// (through the helper, which is still registered at this point), and the run
/// directory. The cask removes the app itself; `brew zap` covers the rest of
/// the user's data; `abctl uninstall` covers the privileged state.
///
/// Fails on the first step that fails: Homebrew shows the error and stops.
pub(super) async fn brew_hook() -> Result<()> {
    let roots = Roots::from_env()?;
    let host = Mac;
    let layout = HostLayout::new(roots.data_dir.clone());

    steps::stop_daemon(
        &host,
        &layout,
        &[roots.profile.daemon_label(), labels::LEGACY_SCRIPT_DAEMON],
    )
    .await?;
    for label in [roots.profile.daemon_label(), labels::LEGACY_SCRIPT_DAEMON] {
        steps::remove_path(&host, &roots.launch_agent(label))?;
    }
    remove_docker_context(&roots)?;
    remove_shell_integration(&roots).await?;
    steps::unlink_cli_tools_through_helper(&roots).await;
    steps::remove_path(&host, &layout.run_dir)
}

#[cfg(test)]
mod tests;
