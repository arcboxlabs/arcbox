//! The local certificate authority behind `https://<name>.arcbox.local`.
//!
//! On macOS, the daemon creates a per-profile CA in the user's Keychain.
//! Only the public certificate is published as `tls/ca.pem` on the guest
//! share. The guest keeps leaf keys in memory and sends signed CSRs to
//! the host. Users trust the root with `abctl tls trust`.
//!
//! # Trust model
//!
//! - **Key access.** The macOS file Keychain controls access through the
//!   creating application's ACL. ArcBox never exports the private key.
//!   This does not promise Secure Enclave storage or protection from a
//!   compromised host account. A guest can request ArcBox certificates
//!   while connected, but cannot copy the root key from the share.
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
//! - **Migration.** A new Keychain CA replaces the old shared CA. The
//!   daemon removes the shared key and revokes the old root's user trust
//!   before publishing the new root. Users must trust the new root.
//! - **Persistence.** Removing `tls/ca.pem` does not rotate the Keychain
//!   CA. The daemon republishes the same public certificate on startup.

#[cfg(any(target_os = "macos", test))]
mod authority;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "macos")]
mod migration;
mod request;

use arcbox_atomic_file::AtomicWriteError;
pub use arcbox_constants::dns::LOCAL_DOMAIN;

#[cfg(target_os = "macos")]
pub use self::keychain::KeychainAuthority;
#[cfg(target_os = "macos")]
pub use self::migration::remove_trust;
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
    #[cfg(target_os = "macos")]
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
