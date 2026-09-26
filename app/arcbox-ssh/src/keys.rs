//! The server's host key and the one client key it lets in: generated on
//! first start, then reused, so clients' known hosts and configs stay valid.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};

/// Host key file inside the SSH directory.
pub const HOST_KEY_FILE: &str = "ssh_host_ed25519_key";
/// Client private key file inside the SSH directory — the `IdentityFile`
/// clients use.
pub const CLIENT_KEY_FILE: &str = "id_ed25519";

/// Mode of every private key file: `ssh` refuses a key others can read.
const PRIVATE_KEY_MODE: u32 = 0o600;
/// Mode of the SSH directory itself.
const SSH_DIR_MODE: u32 = 0o700;

/// Why the keys could not be loaded or created.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Key {
        path: PathBuf,
        #[source]
        source: russh::keys::ssh_key::Error,
    },
    #[error(transparent)]
    Write(#[from] arcbox_atomic_file::AtomicWriteError),
}

/// The key material the server runs with.
pub struct SshKeys {
    /// Proves the server's identity to clients.
    pub host: PrivateKey,
    /// The only key the server authenticates.
    pub client: PublicKey,
    /// Where the client's private key lives, for the client config.
    pub client_key_path: PathBuf,
}

impl SshKeys {
    /// Loads the keys under `dir` (the data dir's `ssh/`), generating any
    /// that are missing.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory or a key cannot be read, parsed or
    /// written.
    pub fn load_or_generate(dir: &Path) -> Result<Self, KeyError> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(SSH_DIR_MODE)
            .create(dir)
            .map_err(|source| KeyError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        let host = load_or_generate_key(&dir.join(HOST_KEY_FILE), "arcbox host key")?;
        let client_key_path = dir.join(CLIENT_KEY_FILE);
        let client = load_or_generate_key(&client_key_path, "arcbox")?;
        Ok(Self {
            host,
            client: client.public_key().clone(),
            client_key_path,
        })
    }
}

/// Reads the OpenSSH private key at `path`, or creates it. Either way the
/// file ends up 0600: it is the daemon's own, and `ssh` rejects a looser one.
fn load_or_generate_key(path: &Path, comment: &str) -> Result<PrivateKey, KeyError> {
    let io_error = |source| KeyError::Io {
        path: path.to_path_buf(),
        source,
    };
    let key_error = |source| KeyError::Key {
        path: path.to_path_buf(),
        source,
    };
    let key = match std::fs::read_to_string(path) {
        Ok(pem) => PrivateKey::from_openssh(pem).map_err(key_error)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut key =
                PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(key_error)?;
            key.set_comment(comment);
            let pem = key.to_openssh(LineEnding::LF).map_err(key_error)?;
            match arcbox_atomic_file::write(path, pem.as_bytes()) {
                Ok(()) => {}
                // The key is in place; only its survival across a power loss
                // is unconfirmed, and a lost key is simply generated again.
                Err(e @ arcbox_atomic_file::AtomicWriteError::DurabilityUncertain { .. }) => {
                    tracing::warn!(error = %e, "ssh key written without a durable rename");
                }
                Err(e) => return Err(e.into()),
            }
            tracing::info!(path = %path.display(), "generated ssh key");
            key
        }
        Err(e) => return Err(io_error(e)),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_KEY_MODE))
        .map_err(io_error)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn keys_are_generated_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join("ssh");

        let first = SshKeys::load_or_generate(&ssh_dir).unwrap();
        let second = SshKeys::load_or_generate(&ssh_dir).unwrap();

        assert_eq!(first.client, second.client);
        assert_eq!(first.host.public_key(), second.host.public_key());
        assert_ne!(first.host.public_key(), &first.client);
        assert_eq!(first.client_key_path, ssh_dir.join(CLIENT_KEY_FILE));
    }

    #[test]
    fn key_files_are_private_even_if_loosened() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join("ssh");
        SshKeys::load_or_generate(&ssh_dir).unwrap();
        assert_eq!(mode(&ssh_dir), SSH_DIR_MODE);
        assert_eq!(mode(&ssh_dir.join(HOST_KEY_FILE)), PRIVATE_KEY_MODE);

        let client = ssh_dir.join(CLIENT_KEY_FILE);
        std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o644)).unwrap();
        SshKeys::load_or_generate(&ssh_dir).unwrap();
        assert_eq!(mode(&client), PRIVATE_KEY_MODE);
    }

    #[test]
    fn a_corrupt_key_fails_instead_of_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(HOST_KEY_FILE), "not a key").unwrap();

        let err = SshKeys::load_or_generate(dir.path()).err().unwrap();
        assert!(matches!(err, KeyError::Key { .. }), "{err}");
    }
}
