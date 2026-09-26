//! The OpenSSH client config the daemon writes next to its keys: with it,
//! `ssh -F <data_dir>/ssh/config <machine>@arcbox` — or plain `ssh` once
//! `~/.ssh/config` includes it — reaches the server with the right key and
//! a pinned host key.

use std::path::Path;

use arcbox_constants::paths::host::SSH_CONFIG;
use russh::keys::ssh_key::PublicKey;

use crate::error::SetupError;
use crate::keys::SshKeys;

/// Known-hosts file pinning the server's host key, inside the SSH directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// Writes `config` and `known_hosts` into `dir` for a server listening on
/// loopback `port`, reached as host `alias` (`arcbox`). Rewritten on every
/// start, since the port can change.
///
/// # Errors
///
/// Returns an error if a path cannot be written into the config or a file
/// cannot be written.
pub fn write_client_config(
    dir: &Path,
    alias: &str,
    port: u16,
    keys: &SshKeys,
) -> Result<(), SetupError> {
    let known_hosts = dir.join(KNOWN_HOSTS_FILE);
    let config = render_config(alias, port, &keys.client_key_path, &known_hosts)?;
    let pin =
        render_known_hosts(alias, keys.host.public_key()).map_err(|source| SetupError::Key {
            path: known_hosts.clone(),
            source,
        })?;
    write(&known_hosts, &pin)?;
    write(&dir.join(SSH_CONFIG), &config)
}

/// Removes the client config, so a client finds no host rather than an
/// endpoint this daemon does not serve.
///
/// # Errors
///
/// Returns an error if an existing file cannot be removed.
pub fn remove_client_config(dir: &Path) -> Result<(), SetupError> {
    for name in [SSH_CONFIG, KNOWN_HOSTS_FILE] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(SetupError::Io { path, source: e });
            }
            _ => {}
        }
    }
    Ok(())
}

fn render_config(
    alias: &str,
    port: u16,
    identity: &Path,
    known_hosts: &Path,
) -> Result<String, SetupError> {
    let identity = quoted(identity)?;
    let known_hosts = quoted(known_hosts)?;
    Ok(format!(
        "# Written by the ArcBox daemon on every start; edits are overwritten.\n\
         # `abctl ssh install` includes it from ~/.ssh/config. Then:\n\
         #   ssh <machine>@{alias}          root in <machine>\n\
         #   ssh <user>@<machine>@{alias}   <user> in <machine>\n\
         Host {alias}\n\
         \x20 HostName 127.0.0.1\n\
         \x20 Port {port}\n\
         \x20 IdentityFile {identity}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 HostKeyAlias {alias}\n\
         \x20 UserKnownHostsFile {known_hosts}\n\
         \x20 StrictHostKeyChecking yes\n"
    ))
}

fn render_known_hosts(
    alias: &str,
    host_key: &PublicKey,
) -> Result<String, russh::keys::ssh_key::Error> {
    Ok(format!("{alias} {}\n", host_key.to_openssh()?))
}

/// A path as a double-quoted ssh_config argument, which keeps spaces in it.
/// A quote or a control character cannot be written into one — and a
/// newline would start a directive of its own — so those are refused.
fn quoted(path: &Path) -> Result<String, SetupError> {
    let text = path.to_string_lossy();
    if text.contains(|c: char| c == '"' || c.is_control()) {
        return Err(SetupError::UnquotablePath(path.to_path_buf()));
    }
    Ok(format!("\"{text}\""))
}

fn write(path: &Path, content: &str) -> Result<(), SetupError> {
    match arcbox_atomic_file::write(path, content.as_bytes()) {
        // Visible already; only its survival across a power loss is
        // unconfirmed, and the next start rewrites it anyway.
        Ok(()) | Err(arcbox_atomic_file::AtomicWriteError::DurabilityUncertain { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_host_block_pins_key_port_and_host_key() {
        let config = render_config(
            "arcbox",
            16022,
            Path::new("/Users/me/.arcbox/ssh/id_ed25519"),
            Path::new("/Users/me/.arcbox/ssh/known_hosts"),
        )
        .unwrap();

        let directives: Vec<&str> = config
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(str::trim)
            .collect();
        assert_eq!(
            directives,
            [
                "Host arcbox",
                "HostName 127.0.0.1",
                "Port 16022",
                r#"IdentityFile "/Users/me/.arcbox/ssh/id_ed25519""#,
                "IdentitiesOnly yes",
                "HostKeyAlias arcbox",
                r#"UserKnownHostsFile "/Users/me/.arcbox/ssh/known_hosts""#,
                "StrictHostKeyChecking yes",
            ]
        );
    }

    #[test]
    fn a_path_that_would_break_the_config_is_refused() {
        for bad in ["/tmp/a\"b", "/tmp/a\nHost *"] {
            assert!(quoted(&PathBuf::from(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(
            quoted(Path::new("/Users/John Doe/.arcbox")).unwrap(),
            r#""/Users/John Doe/.arcbox""#
        );
    }

    #[test]
    fn written_files_pin_the_generated_host_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys = SshKeys::load_or_generate(dir.path()).unwrap();
        write_client_config(dir.path(), "arcbox", 2222, &keys).unwrap();

        let pin = std::fs::read_to_string(dir.path().join(KNOWN_HOSTS_FILE)).unwrap();
        let expected = keys.host.public_key().to_openssh().unwrap();
        assert_eq!(pin, format!("arcbox {expected}\n"));

        remove_client_config(dir.path()).unwrap();
        assert!(!dir.path().join(SSH_CONFIG).exists());
        remove_client_config(dir.path()).unwrap();
    }
}
