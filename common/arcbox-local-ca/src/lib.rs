//! The local certificate authority behind `https://<name>.arcbox.local`.
//!
//! The daemon generates the CA once into the data directory ([`ensure`]):
//! `tls/ca.pem` and `tls/ca-key.pem`, the key mode 0600. The data directory
//! is the System VM's `/arcbox` share, so the guest agent loads the same
//! pair ([`LeafMinter::load`]) and mints a short-lived certificate per
//! server name as TLS clients ask for it. Users trust `ca.pem` once
//! (`abctl tls trust`).
//!
//! # Trust model
//!
//! - **Who can read the key.** On the Mac, the user and root, who can
//!   already read everything in the data directory, the VM's disks
//!   included. Inside the VM, anything that reads `/arcbox` as root: the
//!   agent, dockerd, and any container given the share (a bind mount of
//!   `/arcbox`, or a privileged container that enters the VM's namespaces).
//!   Starting such a container takes the Docker socket, which is already
//!   root in the VM, so the key's audience is the user plus whoever holds
//!   that socket.
//! - **What the key can sign for.** The CA carries critical name
//!   constraints: DNS names under [`LOCAL_DOMAIN`] only, and no IP address
//!   at all; it can sign no other CA (path length 0). macOS applies a
//!   trusted root's constraints to the chain (trustd seeds its permitted
//!   subtrees from the anchor), as do OpenSSL, NSS and Chrome, so a leaked
//!   key impersonates only `*.arcbox.local` names, whose traffic reaches
//!   containers the key holder could already replace.
//! - **What trusting it means.** `abctl tls trust` adds the certificate to
//!   the user's login keychain with trust limited to TLS servers, behind
//!   the macOS authorization prompt. Leaf keys never leave guest memory.
//! - **Rotation.** Delete `tls/` in the data directory: the daemon writes a
//!   new CA on its next start, and it must be trusted again.

mod authority;
#[cfg(target_os = "macos")]
mod keychain;
mod minter;
mod request;

use arcbox_atomic_file::AtomicWriteError;
pub use arcbox_constants::dns::LOCAL_DOMAIN;

pub use self::authority::ensure;
#[cfg(target_os = "macos")]
pub use self::keychain::KeychainAuthority;
pub use self::minter::LeafMinter;
pub use self::request::{CertificateRequest, MAX_CERTIFICATE_SIZE};

/// Why the CA could not be created, loaded, or used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a CA file failed.
    #[error("{}: {source}", .path.display())]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Replacing a CA file failed.
    #[error(transparent)]
    Write(#[from] AtomicWriteError),
    /// Generating or parsing a certificate or key failed.
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    /// rustls refused a minted key.
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    /// A server name the CA does not sign for.
    #[error("{0:?} is not a name under {LOCAL_DOMAIN}")]
    ForeignName(String),
    /// A request violates the host's certificate policy.
    #[error("invalid certificate request: {0}")]
    InvalidRequest(&'static str),
    /// A Keychain operation failed. No file-backed key is substituted.
    #[cfg(target_os = "macos")]
    #[error("keychain: {0}")]
    Keychain(#[from] security_framework::base::Error),
    /// Security.framework could not perform a cryptographic operation.
    #[cfg(target_os = "macos")]
    #[error("security framework: {0}")]
    Security(String),
    /// A persistent keychain item has an unexpected representation.
    #[error("invalid keychain CA: {0}")]
    InvalidAuthority(&'static str),
}

impl Error {
    fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(target_os = "macos")]
impl From<core_foundation::error::CFError> for Error {
    fn from(error: core_foundation::error::CFError) -> Self {
        Self::Security(error.to_string())
    }
}
