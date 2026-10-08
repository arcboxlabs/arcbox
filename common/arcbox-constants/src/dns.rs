#[cfg(feature = "std")]
mod domain;
#[cfg(feature = "std")]
pub use domain::Domain;

/// The default DNS suffix and fixed namespace allowed for local CA signing.
pub const LOCAL_DOMAIN: &str = "arcbox.local";

/// Subject common name of the local CA behind `https://*.arcbox.local`.
///
/// `abctl tls trust` uses the helper to trust the public certificate in the
/// System Keychain. The private identity remains in the login keychain.
pub const LOCAL_CA_COMMON_NAME: &str = "ArcBox Local CA";
