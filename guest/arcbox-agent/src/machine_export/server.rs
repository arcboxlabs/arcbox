//! The listeners behind a machine export: the NFS server on loopback and
//! the bridge-NIC relay that admits the host and nobody else.
//!
//! `nfs3_server` accepts whoever connects to its listener and performs no
//! authentication, while every VM and container on the vmnet bridge can
//! reach a machine's bridge address. The server therefore binds loopback
//! only, and the relay in front of it — the one socket on the bridge NIC —
//! compares each peer against the addresses the host named in the request
//! before copying bytes. The hop costs one loopback round trip per RPC.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use nfs3_server::tcp::{NFSTcp as _, NFSTcpListener};
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::{TcpListener, TcpStream};

use super::ExportConfig;
use super::vfs::MachineRoot;

/// Relay buffer per direction: a full NFS read or write payload.
const RELAY_BUFFER: usize = 1024 * 1024;

/// Where the host mounts the export from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub address: Ipv4Addr,
    pub port: u16,
}

/// Starts serving `root` to the peers in `config` and returns the endpoint
/// on `bridge`. Both listeners live for the rest of the agent's life, like
/// the machine they export.
pub async fn serve(root: PathBuf, bridge: Ipv4Addr, config: ExportConfig) -> io::Result<Endpoint> {
    let nfs = NFSTcpListener::bind("127.0.0.1:0", MachineRoot::new(root, config.ids)).await?;
    let nfs_port = nfs.get_listen_port();
    tokio::spawn(async move {
        if let Err(e) = nfs.handle_forever().await {
            tracing::error!(error = %e, "machine export: NFS server stopped accepting");
        }
    });

    let relay = TcpListener::bind(SocketAddr::from((bridge, 0))).await?;
    let port = relay.local_addr()?.port();
    tracing::info!(%bridge, port, nfs_port, peers = ?config.client_addresses, "machine export listening");
    tokio::spawn(admit(relay, nfs_port, config.client_addresses.into()));
    Ok(Endpoint {
        address: bridge,
        port,
    })
}

async fn admit(relay: TcpListener, nfs_port: u16, allowed: Arc<[IpAddr]>) {
    loop {
        let (client, peer) = match relay.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "machine export: accept failed");
                continue;
            }
        };
        if !allowed.contains(&peer.ip()) {
            tracing::warn!(%peer, "machine export: refused a peer that is not the host");
            continue;
        }
        tokio::spawn(async move {
            if let Err(e) = relay_connection(client, nfs_port).await {
                tracing::debug!(%peer, error = %e, "machine export: connection ended");
            }
        });
    }
}

async fn relay_connection(mut client: TcpStream, nfs_port: u16) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut server = TcpStream::connect((Ipv4Addr::LOCALHOST, nfs_port)).await?;
    let _ = server.set_nodelay(true);
    copy_bidirectional_with_sizes(&mut client, &mut server, RELAY_BUFFER, RELAY_BUFFER).await?;
    Ok(())
}
