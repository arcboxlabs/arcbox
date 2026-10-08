//! The local certificate authority behind `https://<name>.arcbox.local`.
//!
//! On macOS, the daemon creates a per-profile CA in the user's Keychain.
//! Only the public certificate is published as `tls/ca.pem` on the guest
//! share. The guest keeps leaf keys in memory and sends signed CSRs to
//! the host. The privileged helper stores the public root in the System Keychain.
//! A login-session process requests Admin trust through native authorization.
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
//! - **What trusting it means.** [`PublicAuthority`] validates the root before
//!   storage or authorization. TLS-server-only Admin trust requires macOS
//!   authorization in the login session. The private key stays in the user's
//!   Keychain. Leaf keys stay in guest memory.
//! - **Migration.** A new Keychain CA replaces the old shared CA. Before
//!   publication, the caller must revoke the old root's User and Admin trust.
//!   Publication retires the shared key without requesting authorization.
//!   A trusted previous root remains published until explicit revocation.
//! - **Persistence.** Removing `tls/ca.pem` does not rotate the Keychain
//!   CA. The daemon republishes the same public certificate on startup.
//! - **Validity.** Root metadata remains readable after expiry for status and
//!   revocation. Signing requires a valid root and caps the leaf's validity at
//!   the root's bounds. The guest verifies each leaf against its requested name
//!   and the published root before serving it. Root replacement invalidates cached leaves.
//! - **Name authorization.** The signer verifies each CSR before checking its DNS name
//!   against current host container ownership. Ownership must still match after signing.

#[cfg(any(target_os = "macos", test))]
mod authority;
#[cfg(any(target_os = "macos", test))]
mod journal;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "macos")]
mod migration;
#[cfg(target_os = "macos")]
mod operation;
mod public;
mod request;
#[cfg(target_os = "macos")]
mod rotation;
#[cfg(target_os = "macos")]
mod trust;
mod validity;

use arcbox_atomic_file::AtomicWriteError;
pub use arcbox_constants::dns::LOCAL_DOMAIN;

#[cfg(target_os = "macos")]
pub use self::keychain::KeychainAuthority;
#[cfg(target_os = "macos")]
pub use self::migration::{
    Publication, remove_shared_private_key, remove_trust, remove_user_trust_der,
};
#[cfg(target_os = "macos")]
pub use self::operation::lock_profile;
pub use self::public::PublicAuthority;
#[cfg(any(target_os = "macos", test))]
pub use self::request::VerifiedCertificateRequest;
pub use self::request::{CertificateRequest, MAX_CERTIFICATE_SIZE};
#[cfg(target_os = "macos")]
pub use self::rotation::Rotation;
pub use self::validity::CertificateValidity;

/// Why the CA could not be created, loaded, or used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Another process holds this profile's native authorization or identity mutation lock.
    #[error(
        "another local HTTPS operation is in progress for this profile; finish or cancel its authorization first"
    )]
    OperationInProgress,
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
    /// The persisted CA rotation state could not be encoded or decoded.
    #[error("invalid local CA journal {}: {source}", .path.display())]
    Journal {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Generating or parsing a certificate or key failed.
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    /// rustls refused a minted key.
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    /// The public root could not initialize TLS certificate verification.
    #[error("certificate verifier: {0}")]
    Verifier(#[from] rustls::client::VerifierBuilderError),
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
    /// A CA violates the expected identity, constraints, or Keychain representation.
    #[error("invalid local CA: {0}")]
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
