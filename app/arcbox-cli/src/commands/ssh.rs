//! `abctl ssh`: opt-in wiring of the daemon's SSH server into the user's
//! OpenSSH client.
//!
//! The daemon writes `<data_dir>/ssh/config` (a `Host arcbox` block) on every
//! start. `install` adds the one `Include` line that makes plain `ssh`,
//! `scp`, VS Code Remote-SSH and anything else reading `~/.ssh/config` see
//! it; nothing touches `~/.ssh/config` unless asked.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arcbox_constants::paths::{ArcboxProfile, HostLayout};
use clap::Subcommand;

/// Comment written above the `Include` line, which `uninstall` removes too.
const MARKER: &str = "# Added by `abctl ssh install`: ssh <machine>@arcbox";
/// Mode of a `~/.ssh/config` this command creates.
const NEW_CONFIG_MODE: u32 = 0o600;

/// SSH subcommands.
#[derive(Subcommand)]
pub enum SshCommands {
    /// Include ArcBox's SSH config from ~/.ssh/config, so `ssh <machine>@arcbox` works everywhere
    Install,
    /// Remove the Include line `install` added to ~/.ssh/config
    Uninstall,
}

/// Executes an SSH subcommand.
pub async fn execute(cmd: SshCommands) -> Result<()> {
    let home = dirs::home_dir().context("Failed to find the home directory")?;
    let user_config = home.join(".ssh").join("config");
    let config = HostLayout::from_env_or_default().ssh_config;
    let host = ArcboxProfile::from_env_or_default().ssh_host();
    match cmd {
        SshCommands::Install => {
            if install(&user_config, &config, &home)? {
                println!(
                    "Added `Include {}` to {}",
                    include_argument(&config, &home),
                    user_config.display()
                );
            } else {
                println!(
                    "{} already includes {}",
                    user_config.display(),
                    config.display()
                );
            }
            println!("Connect with: ssh <machine>@{host}");
        }
        SshCommands::Uninstall => {
            if uninstall(&user_config, &config, &home)? {
                println!("Removed the ArcBox Include from {}", user_config.display());
            } else {
                println!(
                    "{} does not include {}",
                    user_config.display(),
                    config.display()
                );
            }
        }
    }
    Ok(())
}

/// Adds the `Include` line for `config` to `user_config`, keeping the rest
/// of the file — and its mode — as it was. Returns whether the file changed.
fn install(user_config: &Path, config: &Path, home: &Path) -> Result<bool> {
    let file = UserConfig::read(user_config)?;
    if includes(&file.content, config, home) {
        return Ok(false);
    }
    file.write(&with_include(
        &file.content,
        &include_argument(config, home),
    ))?;
    Ok(true)
}

/// Removes what [`install`] added. Returns whether the file changed.
fn uninstall(user_config: &Path, config: &Path, home: &Path) -> Result<bool> {
    let file = UserConfig::read(user_config)?;
    match without_include(&file.content, config, home) {
        Some(content) => {
            file.write(&content)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// `~/.ssh/config` as found: the file a symlink points at, and its mode.
struct UserConfig {
    path: PathBuf,
    content: String,
    /// `None` when the file does not exist yet.
    mode: Option<u32>,
}

impl UserConfig {
    fn read(path: &Path) -> Result<Self> {
        match std::fs::canonicalize(path) {
            Ok(target) => {
                let content = std::fs::read_to_string(&target)
                    .with_context(|| format!("Failed to read {}", target.display()))?;
                let mode = std::fs::metadata(&target)?.permissions().mode() & 0o7777;
                Ok(Self {
                    path: target,
                    content,
                    mode: Some(mode),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(dir)
                        .with_context(|| format!("Failed to create {}", dir.display()))?;
                }
                Ok(Self {
                    path: path.to_path_buf(),
                    content: String::new(),
                    mode: None,
                })
            }
            Err(e) => Err(e).with_context(|| format!("Failed to resolve {}", path.display())),
        }
    }

    /// Replaces the file atomically, then gives it back its mode.
    fn write(&self, content: &str) -> Result<()> {
        match arcbox_atomic_file::write(&self.path, content.as_bytes()) {
            Ok(()) | Err(arcbox_atomic_file::AtomicWriteError::DurabilityUncertain { .. }) => {}
            Err(e) => return Err(e).context("Failed to update the SSH config"),
        }
        let mode = self.mode.unwrap_or(NEW_CONFIG_MODE);
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("Failed to restore the mode of {}", self.path.display()))
    }
}

/// How the `Include` line names `config`: `~/…` under the home directory,
/// which keeps a synced `~/.ssh/config` valid on another Mac.
fn include_argument(config: &Path, home: &Path) -> String {
    let path = config.strip_prefix(home).map_or_else(
        |_| config.display().to_string(),
        |relative| format!("~/{}", relative.display()),
    );
    if path.contains(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path
    }
}

/// `content` with the `Include` line (and its marker) at the very top:
/// ssh reads its config in order and an `Include` after a `Host` or `Match`
/// line only applies inside that block.
fn with_include(content: &str, argument: &str) -> String {
    format!("{MARKER}\nInclude {argument}\n\n{content}")
}

/// `content` without any `Include` of `config`, nor the marker above it and
/// the blank line after it; `None` when there is none.
fn without_include(content: &str, config: &Path, home: &Path) -> Option<String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut removed = false;
    let mut skip_blank = false;
    for line in lines {
        if included_paths(line, home).any(|path| path == config) {
            if kept.last().is_some_and(|last| last.trim_end() == MARKER) {
                kept.pop();
            }
            removed = true;
            skip_blank = true;
            continue;
        }
        if !(skip_blank && line.trim().is_empty()) {
            kept.push(line);
        }
        skip_blank = false;
    }
    removed.then(|| kept.concat())
}

/// Whether `content` already includes `config`.
fn includes(content: &str, config: &Path, home: &Path) -> bool {
    content
        .lines()
        .any(|line| included_paths(line, home).any(|path| path == config))
}

/// The paths an `Include` directive on `line` names, resolved the way ssh
/// resolves them in a user config: `~/` against the home directory and a
/// relative path against `~/.ssh`.
fn included_paths<'a>(line: &'a str, home: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
    let line = line.trim_start();
    let keyword_end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(keyword_end);
    let arguments = if keyword.eq_ignore_ascii_case("include") {
        rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=')
    } else {
        ""
    };
    split_arguments(arguments).map(move |argument| {
        if let Some(relative) = argument.strip_prefix("~/") {
            home.join(relative)
        } else if Path::new(&argument).is_absolute() {
            PathBuf::from(argument)
        } else {
            home.join(".ssh").join(argument)
        }
    })
}

/// Splits ssh_config arguments on whitespace, honouring double quotes.
fn split_arguments(arguments: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest = arguments.trim();
    std::iter::from_fn(move || {
        rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            return None;
        }
        let (argument, remainder) = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.split_once('"').unwrap_or((quoted, ""))
        } else {
            rest.split_once(char::is_whitespace).unwrap_or((rest, ""))
        };
        rest = remainder;
        Some(argument.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/me";

    fn config() -> PathBuf {
        PathBuf::from("/Users/me/.arcbox/ssh/config")
    }

    #[test]
    fn the_include_names_the_config_relative_to_home() {
        let home = Path::new(HOME);
        assert_eq!(include_argument(&config(), home), "~/.arcbox/ssh/config");
        assert_eq!(
            include_argument(Path::new("/opt/arc box/ssh/config"), home),
            "\"/opt/arc box/ssh/config\""
        );
    }

    #[test]
    fn every_spelling_of_the_include_is_recognized() {
        let home = Path::new(HOME);
        for line in [
            "Include ~/.arcbox/ssh/config",
            "  include \"/Users/me/.arcbox/ssh/config\"",
            "Include=~/.arcbox/ssh/config",
            "Include ~/other ~/.arcbox/ssh/config",
        ] {
            assert!(includes(line, &config(), home), "{line}");
        }
        // A relative path is relative to ~/.ssh, as ssh reads it.
        let beside = Path::new("/Users/me/.ssh/arcbox/config");
        assert!(includes("Include arcbox/config", beside, home));
        for line in [
            "# Include ~/.arcbox/ssh/config",
            "Include ~/.arcbox-dev/ssh/config",
            "IncludeX ~/.arcbox/ssh/config",
            "Host arcbox",
        ] {
            assert!(!includes(line, &config(), home), "{line}");
        }
    }

    #[test]
    fn install_goes_first_and_uninstall_restores_the_file() {
        let home = Path::new(HOME);
        let original = "Host *\n  ServerAliveInterval 60\n";
        let installed = with_include(original, "~/.arcbox/ssh/config");
        assert!(installed.starts_with(&format!("{MARKER}\nInclude ~/.arcbox/ssh/config\n\n")));
        assert!(installed.ends_with(original));

        assert_eq!(
            without_include(&installed, &config(), home).as_deref(),
            Some(original)
        );
        assert_eq!(without_include(original, &config(), home), None);
    }

    #[test]
    fn install_is_idempotent_and_keeps_the_file_and_its_mode() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".arcbox/ssh/config");
        let user_config = home.path().join(".ssh/config");

        // A missing ~/.ssh/config is created private.
        assert!(install(&user_config, &config, home.path()).unwrap());
        assert!(!install(&user_config, &config, home.path()).unwrap());
        let mode = std::fs::metadata(&user_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        // An existing one, reached through a symlink, keeps its content and
        // mode, and stays a symlink.
        let dotfiles = home.path().join("dotfiles/ssh_config");
        std::fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
        std::fs::write(&dotfiles, "Host *\n  User me\n").unwrap();
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_file(&user_config).unwrap();
        std::os::unix::fs::symlink(&dotfiles, &user_config).unwrap();

        assert!(install(&user_config, &config, home.path()).unwrap());
        assert!(
            std::fs::symlink_metadata(&user_config)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let content = std::fs::read_to_string(&dotfiles).unwrap();
        assert!(content.contains("Include ~/.arcbox/ssh/config"));
        assert!(content.ends_with("Host *\n  User me\n"));
        let mode = std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);

        assert!(uninstall(&user_config, &config, home.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(&dotfiles).unwrap(),
            "Host *\n  User me\n"
        );
        assert!(!uninstall(&user_config, &config, home.path()).unwrap());
    }
}
