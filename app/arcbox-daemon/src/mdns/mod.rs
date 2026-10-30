//! Registers the daemon's granted concrete `.local` names with mDNSResponder.
//!
//! RFC 6762 assigns `.local` to mDNS, so macOS resolves these names
//! through mDNSResponder. The unicast server on 127.0.0.1 still
//! answers through `/etc/resolver/<domain>`, and stays the one that makes a
//! miss fail fast; this mirror adds the same names as host-local Bonjour
//! records so they resolve on a Mac that has no resolver file, as
//! OrbStack's `orb.local` names do. Both derive ownership from the Registry.
//!
//! Every Bonjour operation needs Local Network access (TN3179). A `launchd`
//! agent inherits its app's privilege and the user is asked once; a daemon
//! started from a third-party terminal inherits that terminal's. Without it
//! mDNSResponder answers `kDNSServiceErr_PolicyDenied`, the mirror logs the
//! denial once and retries every [`RETRY_DENIED`], and names keep resolving
//! through the resolver file for clients that are allowed to resolve
//! `.local` names at all.

mod dns_sd;
mod mirror;

use std::io;
use std::net::IpAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use arcbox_net::NetworkManager;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use dns_sd::{Connection, RecordHandle};
use mirror::{Event, Mirror, Registrar};

/// How long after a policy denial the mirror asks again. The user may grant
/// Local Network access in System Settings at any time, and mDNSResponder
/// does not revisit a refused registration on its own.
const RETRY_DENIED: Duration = Duration::from_secs(60);

struct Fd(RawFd);

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Registrar for Connection {
    type Handle = RecordHandle;

    fn register(&mut self, fqdn: &str, ip: IpAddr) -> io::Result<RecordHandle> {
        self.register_record(fqdn, ip)
    }

    fn remove(&mut self, handle: RecordHandle) {
        self.remove_record(handle);
    }
}

/// Runs the resolver owner's mirror until shutdown; the caller retains this future.
pub async fn run(
    network_manager: Arc<NetworkManager>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    // Subscribe before the snapshot so no change falls between the two.
    let mut changes = network_manager.subscribe_dns_registry();
    let connection = Connection::open()?;
    let fd = connection.fd();
    let mut mirror = Mirror::new(connection);
    // SAFETY: Connection keeps the same socket until drop. Declaration order deregisters the socket
    // before mirror drops its Connection, including when this future is cancelled.
    let socket = unsafe { AsyncFd::register_with_interest(Fd(fd), Interest::READABLE) }?;
    mirror.resync(network_manager.local_dns_entries());

    let mut announced = false;
    loop {
        // Captured by value: a borrow of the mirror held across the select
        // would make the future require `Connection: Sync`.
        let denied = mirror.is_denied();
        let retry = async move {
            if denied {
                tokio::time::sleep(RETRY_DENIED).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            change = changes.changed() => match change {
                Ok(()) => mirror.resync(network_manager.local_dns_entries()),
                Err(_) => break,
            },
            guard = socket.readable() => {
                let mut guard = guard?;
                // Edge-triggered: clear first, then drain, so a reply that
                // lands after the drain raises a new event.
                guard.clear_ready();
                let mut replies = Vec::new();
                while mirror.registrar_mut().has_pending_reply() {
                    replies.extend(mirror.registrar_mut().process()?);
                }
                for reply in replies {
                    match mirror.handle_reply(reply.handle, reply.error) {
                        Some(Event::Confirmed { fqdn }) => {
                            if !announced {
                                announced = true;
                                info!("mDNSResponder serves this instance's granted .local names");
                            }
                            debug!(fqdn, "mDNS record live");
                        }
                        Some(Event::Denied) => warn!(
                            retry_secs = RETRY_DENIED.as_secs(),
                            "mDNSResponder refused the granted .local records: this process has no \
                             Local Network access (System Settings › Privacy & Security › Local \
                             Network); names resolve through the leased resolver files meanwhile"
                        ),
                        Some(Event::Recovered) => {
                            info!("mDNSResponder accepts the granted .local records again");
                        }
                        Some(Event::Refused { fqdn, error }) => {
                            warn!(fqdn, error, "mDNSResponder refused a record");
                        }
                        None => {}
                    }
                }
            }
            () = retry => mirror.retry_denied(),
        }
    }
    Ok(())
}
