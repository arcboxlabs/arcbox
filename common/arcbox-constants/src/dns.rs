#[cfg(feature = "std")]
mod domain;
#[cfg(feature = "std")]
pub use domain::Domain;

/// The default DNS suffix for ArcBox resources.
pub const LOCAL_DOMAIN: &str = "arcbox.local";

/// The critical DNS constraint on new local roots; the leading dot excludes `local` itself.
pub const LOCAL_CA_DNS_SUBTREE: &str = ".local";

/// Subject common name of the local CA for managed `.local` services.
///
/// The helper stores the public certificate in the System Keychain.
/// A login-session process requests native trust; the private identity stays in the login Keychain.
pub const LOCAL_CA_COMMON_NAME: &str = "ArcBox Local CA";
