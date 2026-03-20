//! arcbox-helper shared types and client.
//!
//! This library defines the tarpc service interface for the privileged helper
//! daemon and provides a high-level [`client::Client`] for consumers
//! (arcbox-core, arcbox-daemon).

pub mod client;
pub mod validate;

/// Unix socket path where the helper daemon listens.
pub const HELPER_SOCKET: &str = "/var/run/arcbox-helper.sock";

/// Override the socket path for development/testing.
pub const HELPER_SOCKET_ENV: &str = "ARCBOX_HELPER_SOCKET";

/// Returns the effective socket path, checking the env override first.
pub fn socket_path() -> String {
    std::env::var(HELPER_SOCKET_ENV).unwrap_or_else(|_| HELPER_SOCKET.to_string())
}

/// The tarpc service definition for privileged host mutations.
///
/// All methods perform input validation server-side before executing
/// any privileged operation. Results carry error strings on failure.
#[tarpc::service]
pub trait HelperService {
    /// Adds a host route for `subnet` via `iface`.
    /// Idempotent: returns Ok if the route already exists.
    async fn route_add(subnet: String, iface: String) -> Result<(), String>;

    /// Removes the host route for `subnet`.
    /// Idempotent: returns Ok if the route is already absent.
    async fn route_remove(subnet: String) -> Result<(), String>;

    /// Installs a DNS resolver file for `domain` pointing to `127.0.0.1:port`.
    async fn dns_install(domain: String, port: u16) -> Result<(), String>;

    /// Removes the DNS resolver file for `domain`.
    async fn dns_uninstall(domain: String) -> Result<(), String>;

    /// Checks if a DNS resolver file is installed for `domain`.
    async fn dns_status(domain: String) -> Result<bool, String>;

    /// Creates `/var/run/docker.sock` symlink pointing to `target`.
    async fn socket_link(target: String) -> Result<(), String>;

    /// Removes the `/var/run/docker.sock` symlink.
    async fn socket_unlink() -> Result<(), String>;

    /// Returns the helper version string.
    async fn version() -> String;
}

/// Low-level connect — use [`client::Client::connect()`] instead.
pub(crate) async fn connect() -> Result<HelperServiceClient, std::io::Error> {
    let path = socket_path();
    let transport =
        tarpc::serde_transport::unix::connect(&path, tarpc::tokio_serde::formats::Bincode::default)
            .await?;
    Ok(HelperServiceClient::new(tarpc::client::Config::default(), transport).spawn())
}
