/// The domain ArcBox names containers under (`<container>.arcbox.local`,
/// `<service>.<project>.arcbox.local`), and the only one its local CA
/// signs for.
pub const LOCAL_DOMAIN: &str = "arcbox.local";

/// Subject common name of the local CA behind `https://*.arcbox.local`,
/// which `abctl tls trust` adds to the login keychain and uninstall removes.
pub const LOCAL_CA_COMMON_NAME: &str = "ArcBox Local CA";
