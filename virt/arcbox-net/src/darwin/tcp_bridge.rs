//! TCP bridge: smoltcp socket pool ↔ host `TcpStream` bidirectional relay.
//!
//! Manages outbound TCP connections through smoltcp's TCP socket
//! implementation, bridging each to a host-side `TcpStream` for actual network
//! I/O.
//!
//! # Architecture
//!
//! ```text
//! Guest VM  (sends SYN to 1.1.1.1:443)
//!     ↕ smoltcp tcp::Socket (listen on port 443, any_ip)
//! TcpBridge
//!     ↕ tokio channels (smoltcp is !Send, host I/O is async)
//! HostConn task (tokio::spawn)
//!     ↕ TcpStream::connect(1.1.1.1:443)
//! Remote server
//! ```
//!
//! # Design
//!
//! Since smoltcp can only listen on a specific port (not port 0), we
//! dynamically create listen sockets when we see new TCP SYN destination ports
//! from the guest. Each listen socket accepts one connection at a time and is
//! recycled back to listening state after the connection closes.
//!
//! Host connections use tokio async I/O through channels:
//! - `host_to_guest_rx`: host TcpStream read data → smoltcp socket.send()
//! - `guest_to_host_tx`: smoltcp socket.recv() → host TcpStream write

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Instant as StdInstant;

use smoltcp::iface::SocketHandle;
use smoltcp::iface::{Interface, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::IpEndpoint;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::darwin::smoltcp_device::{SmoltcpDevice, TcpSynInfo};
use crate::ethernet::ETH_HEADER_LEN;
use crate::nat_engine::checksum;

/// Size of each smoltcp socket's rx/tx buffer. 512 KiB enables larger TCP
/// windows and reduces stalls from smoltcp backpressure on high-bandwidth
/// transfers (doubled from 256 KiB).
const SOCKET_BUF_SIZE: usize = 512 * 1024;

/// Number of pre-allocated listen sockets per port is 1 (created on demand).
/// Additional connections to the same port reuse the socket after it returns
/// to Closed state.
///
/// Maximum segments the host→guest channel can buffer. Provides backpressure
/// when smoltcp's tx buffer is full. Worst-case memory per connection is
/// `512 × avg_read_size` — in practice reads average 4-16 KiB (not the full
/// 256 KiB buffer), so real usage is ~2-8 MiB per flow under sustained load.
const HOST_TO_GUEST_CHANNEL: usize = 512;

/// Maximum payload chunks the guest→host channel can buffer. Backpressure
/// propagates through smoltcp's flow control when the host socket is slow.
const GUEST_TO_HOST_CHANNEL: usize = 512;

/// Start of the inbound ephemeral port range.
const INBOUND_EPHEMERAL_START: u16 = 61000;
/// End of the inbound ephemeral port range (inclusive).
const INBOUND_EPHEMERAL_END: u16 = 65535;

/// Maximum number of concurrent pending SYN gate entries. Prevents SYN flood
/// from exhausting resources.
const MAX_PENDING_SYNS: usize = 256;

/// Timeout for host-side `TcpStream::connect` during SYN gate (seconds).
const SYN_GATE_CONNECT_TIMEOUT_SECS: u64 = 5;

/// TTL for pre-connected streams waiting to be consumed by
/// `detect_new_connections`. If the guest doesn't retransmit SYN within this
/// window, the stream is dropped.
const PRE_CONNECTED_TTL_SECS: u64 = 10;

/// Full four-tuple key for deduplicating SYN gate entries and fast-path lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SynFlowKey {
    src_ip: Ipv4Addr,
    src_port: u16,
    dst_ip: Ipv4Addr,
    dst_port: u16,
}

/// A TCP SYN waiting for host connect to complete before being injected into
/// smoltcp.
struct PendingSyn {
    /// The original SYN frame to inject on success.
    frame: Vec<u8>,
    /// Initial sequence number from the SYN — used to detect new connection
    /// attempts that reuse the same four-tuple but with a different ISN.
    syn_seq: u32,
    /// Receives the connected `TcpStream` on success, or `None` on failure.
    result_rx: oneshot::Receiver<Option<tokio::net::TcpStream>>,
    /// When this pending entry was created.
    created: StdInstant,
}

/// A successfully connected host stream waiting for smoltcp to establish the
/// guest-side connection so detect_new_connections can pick it up.
struct PreConnected {
    stream: tokio::net::TcpStream,
    /// ISN from the SYN that triggered this connect, for strict matching.
    syn_seq: u32,
    created: StdInstant,
}

/// Tracks a single TCP connection bridged between smoltcp and a host TcpStream.
///
/// Used for both outbound (guest-initiated) and inbound (host-initiated)
/// connections. The relay logic in `relay_all` is identical in both directions.
struct BridgedConn {
    /// smoltcp socket handle.
    handle: SocketHandle,
    /// Remote address label (actual destination for outbound, guest endpoint
    /// for inbound) used in log messages.
    remote: SocketAddr,
    /// Receives data from the host TcpStream read task.
    host_to_guest_rx: mpsc::Receiver<Vec<u8>>,
    /// Sends data consumed from smoltcp socket to the host TcpStream write task.
    /// `None` after guest EOF has been signalled (sender dropped to close channel).
    guest_to_host_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Set to true when the host read task has sent all data (EOF).
    host_eof: bool,
    /// Set to true when the host channel has disconnected (connect failed or
    /// normal task exit after EOF).
    host_disconnected: bool,
    /// Leftover bytes from a partial `send_slice` that need to be retried.
    pending_send: Option<Vec<u8>>,
}

/// Manages TCP connections bridged between the smoltcp socket pool and host
/// TcpStreams, for both outbound (guest→host) and inbound (host→guest) flows.
pub struct TcpBridge {
    /// Active connections keyed by smoltcp socket handle.
    connections: HashMap<SocketHandle, BridgedConn>,
    /// Ports that have at least one listen socket in the socket set.
    listening_ports: HashSet<u16>,
    /// Maps a port to the socket handles listening on it, so we can track
    /// which ports are covered.
    port_handles: HashMap<u16, Vec<SocketHandle>>,
    /// Next inbound ephemeral port to allocate (wraps within 61000-65535).
    next_ephemeral: u16,
    /// SYN gate: pending host connects keyed by four-tuple.
    pending_syns: HashMap<SynFlowKey, PendingSyn>,
    /// Pre-connected host streams ready for detect_new_connections to consume.
    /// Keyed by four-tuple so the correct stream is matched to its connection.
    pre_connected: HashMap<SynFlowKey, PreConnected>,
    /// DNS resolution log for mapping IPs back to domain names.
    dns_log: Option<super::dns_log::DnsResolutionLog>,
    /// Detected proxy environment on the host.
    proxy_env: Option<super::proxy_detect::ProxyEnvironment>,
    /// Gateway IP used by the guest. Connections targeting this IP are
    /// translated to `127.0.0.1` so they reach the host's loopback
    /// (enables `host.docker.internal` support).
    gateway_ip: Ipv4Addr,
    /// Fast-path connections that bypass smoltcp for data transfer.
    /// Keyed by (guest_src_ip, guest_src_port, dest_ip, dest_port).
    fast_path_conns: HashMap<SynFlowKey, FastPathConn>,
    /// Gateway MAC for constructing fast-path frames to the guest.
    fast_path_gateway_mac: [u8; 6],
    /// Guest MAC for constructing fast-path frames to the guest.
    fast_path_guest_mac: Option<[u8; 6]>,
}

/// A TCP connection promoted to the fast path — bypasses smoltcp entirely
/// for data transfer. smoltcp handled the initial 3-way handshake; data
/// frames are now intercepted at `classify_ipv4` and relayed directly.
struct FastPathConn {
    /// Host-side TCP stream (std blocking — used from the sync datapath loop).
    stream: std::net::TcpStream,
    /// Our SEQ number for frames sent TO guest.
    our_seq: u32,
    /// Last ACK we sent to guest (= next SEQ we expect FROM guest).
    last_ack: u32,
    /// Remote IP as seen by the guest.
    remote_ip: Ipv4Addr,
    /// Guest IP.
    guest_ip: Ipv4Addr,
    /// Remote port as seen by the guest.
    remote_port: u16,
    /// Guest port.
    guest_port: u16,
    /// Read buffer for host → guest data (reused across polls).
    read_buf: Vec<u8>,
    /// True if host stream has reached EOF.
    host_eof: bool,
}

impl TcpBridge {
    pub fn new(gateway_ip: Ipv4Addr) -> Self {
        Self {
            connections: HashMap::new(),
            listening_ports: HashSet::new(),
            port_handles: HashMap::new(),
            next_ephemeral: INBOUND_EPHEMERAL_START,
            pending_syns: HashMap::new(),
            pre_connected: HashMap::new(),
            dns_log: None,
            proxy_env: None,
            gateway_ip,
            fast_path_conns: HashMap::new(),
            fast_path_gateway_mac: [0; 6],
            fast_path_guest_mac: None,
        }
    }

    /// Updates the MAC addresses used for fast-path frame construction.
    pub fn set_fast_path_macs(&mut self, gateway_mac: [u8; 6], guest_mac: [u8; 6]) {
        self.fast_path_gateway_mac = gateway_mac;
        self.fast_path_guest_mac = Some(guest_mac);
    }

    /// Checks if a TCP frame matches a fast-path connection.
    ///
    /// Called from `classify_ipv4` before the frame reaches smoltcp.
    /// Returns `Some(ack_frame)` if the frame was handled (payload written
    /// to host stream, ACK generated), or `None` if not a fast-path match.
    pub fn try_fast_path_intercept(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < ETH_HEADER_LEN + 40 {
            return None; // Too short for ETH + IP + TCP minimum
        }

        let ip_start = ETH_HEADER_LEN;
        let protocol = frame[ip_start + 9];
        if protocol != 6 {
            return None; // Not TCP
        }

        let ihl = ((frame[ip_start] & 0x0F) as usize) * 4;
        let l4_start = ip_start + ihl;
        if frame.len() < l4_start + 20 {
            return None;
        }

        let src_ip = Ipv4Addr::new(
            frame[ip_start + 12],
            frame[ip_start + 13],
            frame[ip_start + 14],
            frame[ip_start + 15],
        );
        let dst_ip = Ipv4Addr::new(
            frame[ip_start + 16],
            frame[ip_start + 17],
            frame[ip_start + 18],
            frame[ip_start + 19],
        );
        let src_port = u16::from_be_bytes([frame[l4_start], frame[l4_start + 1]]);
        let dst_port = u16::from_be_bytes([frame[l4_start + 2], frame[l4_start + 3]]);
        let flags = frame[l4_start + 13];

        let key = SynFlowKey {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
        };

        let conn = self.fast_path_conns.get_mut(&key)?;

        // FIN or RST → teardown, let smoltcp handle it.
        if flags & 0x01 != 0 || flags & 0x04 != 0 {
            tracing::debug!(
                "Fast path: FIN/RST on {src_ip}:{src_port} → {dst_ip}:{dst_port}, removing"
            );
            self.fast_path_conns.remove(&key);
            return None; // Fall through to smoltcp for teardown.
        }

        // Extract payload.
        let tcp_data_offset = ((frame[l4_start + 12] >> 4) as usize) * 4;
        let payload_start = l4_start + tcp_data_offset;
        let payload_len = if payload_start < frame.len() {
            frame.len() - payload_start
        } else {
            0
        };

        // Write payload to host stream (if any).
        if payload_len > 0 {
            use std::io::Write;
            let payload = &frame[payload_start..];
            match conn.stream.write(payload) {
                Ok(n) => {
                    conn.last_ack = conn.last_ack.wrapping_add(n as u32);
                    tracing::trace!(
                        "Fast path TX: {src_ip}:{src_port}→{dst_ip}:{dst_port} {n} bytes"
                    );
                }
                Err(e) => {
                    tracing::warn!("Fast path TX write error: {e}");
                    self.fast_path_conns.remove(&key);
                    return None;
                }
            }
        }

        // Generate ACK frame back to guest.
        let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
        let ack = crate::ethernet::build_tcp_ack_frame(&crate::ethernet::TcpFrameParams {
            src_ip: conn.remote_ip,
            dst_ip: conn.guest_ip,
            src_port: conn.remote_port,
            dst_port: conn.guest_port,
            seq: conn.our_seq,
            ack: conn.last_ack,
            window: 65535,
            src_mac: self.fast_path_gateway_mac,
            dst_mac: guest_mac,
        });

        Some(ack)
    }

    /// Polls fast-path host streams for readable data and generates frames
    /// to inject into the guest.
    ///
    /// Returns frames to be written to the guest FD via `enqueue_or_write`.
    pub fn poll_fast_path(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut to_remove = Vec::new();
        let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
        let gw_mac = self.fast_path_gateway_mac;

        for (key, conn) in &mut self.fast_path_conns {
            if conn.host_eof {
                continue;
            }

            use std::io::Read;
            match conn.stream.read(&mut conn.read_buf) {
                Ok(0) => {
                    // Host EOF — send FIN to guest.
                    conn.host_eof = true;
                    let fin =
                        crate::ethernet::build_tcp_fin_frame(&crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: conn.our_seq,
                            ack: conn.last_ack,
                            window: 65535,
                            src_mac: gw_mac,
                            dst_mac: guest_mac,
                        });
                    conn.our_seq = conn.our_seq.wrapping_add(1); // FIN consumes 1 SEQ
                    frames.push(fin);
                    to_remove.push(*key);
                }
                Ok(n) => {
                    // Build data frame.
                    let data_frame = crate::ethernet::build_tcp_data_frame(
                        &crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: conn.our_seq,
                            ack: conn.last_ack,
                            window: 65535,
                            src_mac: gw_mac,
                            dst_mac: guest_mac,
                        },
                        &conn.read_buf[..n],
                    );
                    conn.our_seq = conn.our_seq.wrapping_add(n as u32);
                    frames.push(data_frame);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available — expected for non-blocking.
                }
                Err(e) => {
                    tracing::warn!(
                        "Fast path RX error {}:{} → {}:{}: {e}",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port
                    );
                    to_remove.push(*key);
                }
            }
        }

        for key in to_remove {
            self.fast_path_conns.remove(&key);
        }

        frames
    }

    /// Promotes a connection to the fast path.
    ///
    /// Called when a smoltcp connection reaches ESTABLISHED and has a
    /// pre-connected host stream. The connection is removed from smoltcp
    /// and data transfer bypasses the TCP state machine entirely.
    pub fn promote_to_fast_path(
        &mut self,
        key: SynFlowKey,
        stream: std::net::TcpStream,
        our_seq: u32,
        last_ack: u32,
    ) {
        // Set non-blocking for polling in the event loop.
        stream.set_nonblocking(true).ok();
        stream.set_nodelay(true).ok();

        tracing::info!(
            "Fast path: promoted {}:{} → {}:{} (seq={our_seq}, ack={last_ack})",
            key.src_ip,
            key.src_port,
            key.dst_ip,
            key.dst_port,
        );

        self.fast_path_conns.insert(
            key,
            FastPathConn {
                stream,
                our_seq,
                last_ack,
                remote_ip: key.dst_ip,
                guest_ip: key.src_ip,
                remote_port: key.dst_port,
                guest_port: key.src_port,
                read_buf: vec![0u8; 32768],
                host_eof: false,
            },
        );
    }

    /// Returns the number of active fast-path connections.
    #[must_use]
    pub fn fast_path_count(&self) -> usize {
        self.fast_path_conns.len()
    }

    /// Determines whether to connect via a proxy tunnel for the given destination.
    ///
    /// Returns `Some((proxy_authority, target_host, target_port, protocol))` if a
    /// proxy should be used, or `None` for direct connection. The proxy authority
    /// is a `"host:port"` string that `TcpStream::connect` can resolve (supports
    /// both IP addresses and hostnames like `proxy.corp.com`).
    fn resolve_proxy_target(
        &self,
        dst_ip: Ipv4Addr,
        dst_port: u16,
        domain: Option<&str>,
    ) -> Option<(String, String, u16, &'static str)> {
        let env = self.proxy_env.as_ref()?;

        // No proxy configured → always direct.
        if !env.has_usable_proxy() {
            return None;
        }

        // Need a domain name to use proxy tunnel (can't send raw IP to CONNECT).
        let host = domain?;

        // Check bypass list.
        if env.should_bypass(host) {
            return None;
        }

        // Proxy fake-ip destinations (VPN virtual IPs that only the proxy can
        // resolve) and traffic when an explicit system proxy is configured
        // (corporate proxy environments).
        let need_proxy = env.is_fake_ip(dst_ip)
            || env.http_proxy.is_some()
            || env.https_proxy.is_some()
            || env.socks_proxy.is_some();
        if !need_proxy {
            return None;
        }

        // Prefer SOCKS5 (supports all protocols and avoids TLS issues),
        // then HTTPS proxy (HTTP CONNECT works on any port, not just 443),
        // then HTTP proxy as last resort.
        if let Some(ref socks) = env.socks_proxy {
            let authority = format!("{}:{}", socks.host, socks.port);
            return Some((authority, host.to_string(), dst_port, "socks5"));
        }

        if let Some(ref https) = env.https_proxy {
            let authority = format!("{}:{}", https.host, https.port);
            return Some((authority, host.to_string(), dst_port, "http-connect"));
        }

        if let Some(ref http) = env.http_proxy {
            let authority = format!("{}:{}", http.host, http.port);
            return Some((authority, host.to_string(), dst_port, "http-connect"));
        }

        None
    }

    /// Configures proxy-aware connection support.
    ///
    /// When set, `gate_syns()` will use the DNS log to resolve destination IPs
    /// to domain names and connect via system proxy (HTTP CONNECT / SOCKS5)
    /// when available. Without this, all connections use direct `TcpStream::connect`.
    pub fn set_proxy_awareness(
        &mut self,
        dns_log: super::dns_log::DnsResolutionLog,
        proxy_env: super::proxy_detect::ProxyEnvironment,
    ) {
        self.dns_log = Some(dns_log);
        self.proxy_env = Some(proxy_env);
    }

    /// Ensures listen sockets exist for the given TCP SYN destination ports.
    ///
    /// Called before `iface.poll()` so that when smoltcp processes the SYN,
    /// a matching listen socket is ready to accept it.
    pub fn ensure_listen_sockets(
        &mut self,
        syn_ports: &[crate::darwin::smoltcp_device::TcpSynInfo],
        sockets: &mut SocketSet<'_>,
    ) {
        for syn in syn_ports {
            let port = syn.dst_port;
            if self.listening_ports.contains(&port) {
                continue;
            }

            // Create a new listen socket for this port.
            let rx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
            let tx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
            let mut sock = tcp::Socket::new(rx_buf, tx_buf);
            if let Err(e) = sock.listen(port) {
                tracing::warn!("TCP bridge: failed to listen on port {port}: {e:?}");
                continue;
            }
            sock.set_nagle_enabled(false);
            sock.set_ack_delay(None);
            let handle = sockets.add(sock);

            self.listening_ports.insert(port);
            self.port_handles.entry(port).or_default().push(handle);

            tracing::debug!("TCP bridge: listen socket created for port {port}");
        }
    }

    /// Processes gated TCP SYN frames: spawns a host connect task for each new
    /// SYN, deduplicates retransmissions, and detects ISN changes.
    ///
    /// Called from the datapath loop with newly gated SYNs from SmoltcpDevice.
    /// Returns RST frames for SYNs rejected due to capacity limits.
    pub fn gate_syns(&mut self, syns: &[TcpSynInfo], gateway_mac: [u8; 6]) -> Vec<Vec<u8>> {
        let mut rst_frames = Vec::new();

        for syn in syns {
            let key = SynFlowKey {
                src_ip: syn.src_ip,
                src_port: syn.src_port,
                dst_ip: syn.dst_ip,
                dst_port: syn.dst_port,
            };

            // Check for existing pending entry.
            if let Some(existing) = self.pending_syns.get(&key) {
                if existing.syn_seq == syn.syn_seq {
                    tracing::debug!("TCP SYN gate: retransmit dropped for {key:?}");
                    continue;
                }
                // Different ISN = new connection attempt, remove stale entry.
                tracing::debug!("TCP SYN gate: ISN changed for {key:?}, replacing pending");
                self.pending_syns.remove(&key);
            }

            // Check pre_connected: same ISN = retransmit (SYN will be injected
            // on next poll), different ISN = guest retried with new connection.
            if let Some(pre) = self.pre_connected.get(&key) {
                if pre.syn_seq == syn.syn_seq {
                    tracing::debug!(
                        "TCP SYN gate: retransmit dropped (pre-connected exists) for {key:?}"
                    );
                    continue;
                }
                tracing::debug!(
                    "TCP SYN gate: ISN changed for {key:?}, evicting stale pre-connected stream"
                );
                self.pre_connected.remove(&key);
            }

            // Capacity check — send RST instead of silent drop.
            if self.pending_syns.len() >= MAX_PENDING_SYNS {
                tracing::warn!("TCP SYN gate: capacity limit reached, sending RST for {key:?}");
                if let Some(rst) = build_rst_from_syn(&syn.frame, gateway_mac) {
                    rst_frames.push(rst);
                }
                continue;
            }

            // Connections to the gateway IP are destined for the host itself
            // (e.g. host.docker.internal). Translate to loopback so the
            // host-side connect reaches services on 127.0.0.1 / 0.0.0.0.
            let connect_ip = if syn.dst_ip == self.gateway_ip {
                Ipv4Addr::LOCALHOST
            } else {
                syn.dst_ip
            };
            let dst_addr = SocketAddr::V4(SocketAddrV4::new(connect_ip, syn.dst_port));

            let (result_tx, result_rx) = oneshot::channel();

            // Resolve domain name from DNS log (if available) for proxy-aware connects.
            let domain = self.dns_log.as_ref().and_then(|log| log.lookup(syn.dst_ip));

            // Gateway-destined flows (host.docker.internal) always connect
            // directly to loopback — never route through a proxy.
            let proxy_target = if syn.dst_ip == self.gateway_ip {
                None
            } else {
                self.resolve_proxy_target(syn.dst_ip, syn.dst_port, domain.as_deref())
            };

            // Spawn host connect task.
            tokio::spawn(async move {
                let connect_fut = async {
                    match proxy_target {
                        Some((proxy_authority, host, port, proto)) => {
                            tracing::debug!(
                                proxy = %proxy_authority,
                                target = %format!("{host}:{port}"),
                                protocol = proto,
                                "TCP SYN gate: connecting via proxy"
                            );
                            if proto == "socks5" {
                                super::proxy_tunnel::connect_via_socks5(
                                    &proxy_authority,
                                    &host,
                                    port,
                                )
                                .await
                            } else {
                                super::proxy_tunnel::connect_via_http_proxy(
                                    &proxy_authority,
                                    &host,
                                    port,
                                )
                                .await
                            }
                        }
                        None => tokio::net::TcpStream::connect(dst_addr).await,
                    }
                };

                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(SYN_GATE_CONNECT_TIMEOUT_SECS),
                    connect_fut,
                )
                .await;

                let stream = match result {
                    Ok(Ok(s)) => {
                        tracing::debug!("TCP SYN gate: connected to {dst_addr}");
                        Some(s)
                    }
                    Ok(Err(e)) => {
                        tracing::debug!("TCP SYN gate: connect to {dst_addr} failed: {e}");
                        None
                    }
                    Err(_) => {
                        tracing::debug!("TCP SYN gate: connect to {dst_addr} timed out");
                        None
                    }
                };
                let _ = result_tx.send(stream);
            });

            self.pending_syns.insert(
                key,
                PendingSyn {
                    frame: syn.frame.clone(),
                    syn_seq: syn.syn_seq,
                    result_rx,
                    created: StdInstant::now(),
                },
            );

            tracing::debug!(
                "TCP SYN gate: host connect started for {}:{} → {}:{}",
                syn.src_ip,
                syn.src_port,
                syn.dst_ip,
                syn.dst_port,
            );
        }

        rst_frames
    }

    /// Polls pending SYN gate entries and processes results.
    ///
    /// - Success: injects the SYN frame into SmoltcpDevice, creates a listen
    ///   socket, and stores the pre-connected stream.
    /// - Failure: constructs an RST|ACK frame and queues it for the guest.
    /// - Timeout: cleans up expired entries.
    ///
    /// Returns RST frames that should be written to the guest.
    pub fn poll_pending_syns(
        &mut self,
        device: &mut SmoltcpDevice,
        sockets: &mut SocketSet<'_>,
        gateway_mac: [u8; 6],
    ) -> Vec<Vec<u8>> {
        let mut rst_frames = Vec::new();
        let mut completed = Vec::new();

        for (key, pending) in &mut self.pending_syns {
            // Check if the oneshot has a result (non-blocking).
            match pending.result_rx.try_recv() {
                Ok(Some(stream)) => {
                    // Host connect succeeded. Inject the SYN frame and create
                    // a listen socket so smoltcp can process the handshake.
                    completed.push((*key, Some(stream)));
                }
                Ok(None) => {
                    // Host connect failed. Send RST|ACK to guest.
                    completed.push((*key, None));
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Still waiting. Check deadline.
                    if pending.created.elapsed()
                        > std::time::Duration::from_secs(SYN_GATE_CONNECT_TIMEOUT_SECS + 1)
                    {
                        // Task probably leaked, clean up.
                        completed.push((*key, None));
                    }
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending (task panicked).
                    completed.push((*key, None));
                }
            }
        }

        for (key, result) in completed {
            let pending = self.pending_syns.remove(&key).unwrap();
            match result {
                Some(stream) => {
                    // 1. Create a dedicated listen socket for this SYN.
                    //    Each injected SYN needs its own listen socket because
                    //    smoltcp transitions a listen socket to SYN-RECEIVED on
                    //    accept. If multiple SYNs to the same port are injected
                    //    in one batch, iface.poll() would RST the extras.
                    if !self.create_listen_socket(key.dst_port, sockets) {
                        // Listen failed — send RST instead of injecting SYN.
                        if let Some(rst) = build_rst_from_syn(&pending.frame, gateway_mac) {
                            rst_frames.push(rst);
                            tracing::debug!("TCP SYN gate: listen failed, sending RST for {key:?}");
                        }
                        continue;
                    }

                    // 2. Inject the original SYN frame.
                    device.inject_rx(pending.frame);

                    // 3. Store the pre-connected stream.
                    self.pre_connected.insert(
                        key,
                        PreConnected {
                            stream,
                            syn_seq: pending.syn_seq,
                            created: StdInstant::now(),
                        },
                    );

                    tracing::debug!(
                        "TCP SYN gate: injected SYN + stored pre-connected stream for {key:?}"
                    );
                }
                None => {
                    // Build RST|ACK from the original SYN frame.
                    if let Some(rst) = build_rst_from_syn(&pending.frame, gateway_mac) {
                        rst_frames.push(rst);
                        tracing::debug!("TCP SYN gate: sending RST for failed connect {key:?}");
                    }
                }
            }
        }

        // Expire stale pre-connected streams.
        self.pre_connected.retain(|key, pre| {
            if pre.created.elapsed() > std::time::Duration::from_secs(PRE_CONNECTED_TTL_SECS) {
                tracing::debug!("TCP SYN gate: pre-connected stream expired for {key:?}");
                false
            } else {
                true
            }
        });

        rst_frames
    }

    /// Unconditionally creates a new listen socket for `port`.
    ///
    /// Used by `poll_pending_syns` where each injected SYN needs its own
    /// listen socket — smoltcp consumes a listen socket when it transitions
    /// to SYN-RECEIVED, so concurrent SYNs to the same port each need a
    /// dedicated listener.
    fn create_listen_socket(&mut self, port: u16, sockets: &mut SocketSet<'_>) -> bool {
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let mut sock = tcp::Socket::new(rx_buf, tx_buf);
        if let Err(e) = sock.listen(port) {
            tracing::warn!("TCP bridge: failed to listen on port {port}: {e:?}");
            return false;
        }
        sock.set_nagle_enabled(false);
        sock.set_ack_delay(None);
        let handle = sockets.add(sock);

        self.listening_ports.insert(port);
        self.port_handles.entry(port).or_default().push(handle);
        tracing::debug!("TCP bridge: listen socket created for port {port}");
        true
    }

    /// Initiates an inbound TCP connection: creates a smoltcp socket that
    /// actively connects to the guest, and spawns a relay task for the
    /// already-accepted host `TcpStream`.
    ///
    /// Called when `InboundListenerManager` accepts a new host connection.
    pub fn initiate_inbound(
        &mut self,
        container_port: u16,
        stream: tokio::net::TcpStream,
        guest_ip: Ipv4Addr,
        gateway_ip: Ipv4Addr,
        iface: &mut Interface,
        sockets: &mut SocketSet<'_>,
    ) {
        let eph_port = self.allocate_ephemeral();

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let mut sock = tcp::Socket::new(rx_buf, tx_buf);
        sock.set_nagle_enabled(false);
        sock.set_ack_delay(None);

        let local_ep = IpEndpoint::new(gateway_ip.into(), eph_port);
        let remote_ep = IpEndpoint::new(guest_ip.into(), container_port);

        if let Err(e) = sock.connect(iface.context(), remote_ep, local_ep) {
            tracing::warn!("TCP bridge: inbound connect to guest:{container_port} failed: {e:?}");
            return;
        }

        let handle = sockets.add(sock);

        let (h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(HOST_TO_GUEST_CHANNEL);
        let (g2h_tx, g2h_rx) = mpsc::channel::<Vec<u8>>(GUEST_TO_HOST_CHANNEL);

        // Spawn a task that relays between the already-connected host TcpStream
        // and the channels. Same pattern as host_conn_task but the stream is
        // already connected.
        tokio::spawn(inbound_host_relay(stream, h2g_tx, g2h_rx));

        let guest_addr = SocketAddr::V4(SocketAddrV4::new(guest_ip, container_port));

        self.connections.insert(
            handle,
            BridgedConn {
                handle,
                remote: guest_addr,
                host_to_guest_rx: h2g_rx,
                guest_to_host_tx: Some(g2h_tx),
                host_eof: false,
                host_disconnected: false,
                pending_send: None,
            },
        );

        tracing::debug!(
            "TCP bridge: inbound connect initiated  gw:{eph_port} → guest:{container_port}"
        );
    }

    /// Allocates the next inbound ephemeral port, wrapping at the end of the
    /// range.
    fn allocate_ephemeral(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral = if self.next_ephemeral == INBOUND_EPHEMERAL_END {
            INBOUND_EPHEMERAL_START
        } else {
            self.next_ephemeral + 1
        };
        port
    }

    /// Polls all connections, performing bidirectional data relay and detecting
    /// new/closed connections.
    ///
    /// Must be called after `iface.poll()`.
    pub fn poll(&mut self, sockets: &mut SocketSet<'_>) {
        self.detect_new_connections(sockets);
        self.relay_all(sockets);
        self.cleanup_closed(sockets);
    }

    /// Detects smoltcp sockets that have transitioned from Listen to an active
    /// state (SYN received), and sets up host relay for them.
    ///
    /// For outbound connections: uses a pre-connected stream from the SYN gate
    /// (matched by four-tuple), or falls back to spawning a new host connect.
    fn detect_new_connections(&mut self, sockets: &mut SocketSet<'_>) {
        // Collect ports to scan: only ports we know have listen sockets.
        let ports: Vec<u16> = self.listening_ports.iter().copied().collect();
        let mut ports_to_replenish = Vec::new();

        for port in ports {
            let Some(handles) = self.port_handles.get(&port) else {
                continue;
            };

            for &handle in handles {
                // Skip handles already tracked as active connections.
                if self.connections.contains_key(&handle) {
                    continue;
                }

                let sock = sockets.get_mut::<tcp::Socket>(handle);
                if !sock.is_active() {
                    continue;
                }

                // Socket accepted a SYN — extract the remote endpoint.
                let Some(remote_ep) = sock.remote_endpoint() else {
                    continue;
                };
                let Some(local_ep) = sock.local_endpoint() else {
                    continue;
                };

                let remote_addr = endpoint_to_sockaddr(remote_ep);
                let dest_addr = endpoint_to_sockaddr(local_ep);
                tracing::debug!(
                    "TCP bridge: new connection detected  guest:{remote_addr} → {dest_addr}"
                );

                // Build four-tuple key to look up pre-connected stream.
                let flow_key = SynFlowKey {
                    src_ip: match remote_ep.addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => v4,
                    },
                    src_port: remote_ep.port,
                    dst_ip: match local_ep.addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => v4,
                    },
                    dst_port: local_ep.port,
                };

                // Create channels for host↔guest data bridging.
                let (h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(HOST_TO_GUEST_CHANNEL);
                let (g2h_tx, g2h_rx) = mpsc::channel::<Vec<u8>>(GUEST_TO_HOST_CHANNEL);

                // Try to use a pre-connected stream from the SYN gate.
                if let Some(pre) = self.pre_connected.remove(&flow_key) {
                    tracing::debug!(
                        "TCP bridge: using pre-connected stream for guest:{remote_addr} → {dest_addr}"
                    );
                    tokio::spawn(inbound_host_relay(pre.stream, h2g_tx, g2h_rx));
                } else {
                    tracing::debug!(
                        "TCP bridge: no pre-connected stream, spawning connect for guest:{remote_addr} → {dest_addr}"
                    );
                    tokio::spawn(host_conn_task(dest_addr, h2g_tx, g2h_rx));
                }

                self.connections.insert(
                    handle,
                    BridgedConn {
                        handle,
                        remote: dest_addr,
                        host_to_guest_rx: h2g_rx,
                        guest_to_host_tx: Some(g2h_tx),
                        host_eof: false,
                        host_disconnected: false,
                        pending_send: None,
                    },
                );

                // This port's listen socket is now occupied. Remove from
                // listening_ports and schedule a replacement.
                self.listening_ports.remove(&port);
                ports_to_replenish.push(port);
            }
        }

        // Replenish listen sockets for consumed ports so the next SYN
        // (retransmitted or concurrent) finds a ready listener.
        for port in ports_to_replenish {
            self.replenish_listen_socket(port, sockets);
        }
    }

    /// Creates a fresh listen socket for the given port, so smoltcp can
    /// accept the next SYN without waiting for a new frame batch.
    fn replenish_listen_socket(&mut self, port: u16, sockets: &mut SocketSet<'_>) {
        if self.listening_ports.contains(&port) {
            return;
        }

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF_SIZE]);
        let mut sock = tcp::Socket::new(rx_buf, tx_buf);
        if let Err(e) = sock.listen(port) {
            tracing::warn!("TCP bridge: failed to listen on port {port}: {e:?}");
            return;
        }
        sock.set_nagle_enabled(false);
        sock.set_ack_delay(None);
        let handle = sockets.add(sock);

        self.listening_ports.insert(port);
        self.port_handles.entry(port).or_default().push(handle);
        tracing::debug!("TCP bridge: replenished listen socket for port {port}");
    }

    /// Relays data between smoltcp sockets and host TcpStreams for all active
    /// connections.
    fn relay_all(&mut self, sockets: &mut SocketSet<'_>) {
        for conn in self.connections.values_mut() {
            let sock = sockets.get_mut::<tcp::Socket>(conn.handle);

            // Host → Guest: flush pending partial send first, then drain channel.
            while sock.can_send() {
                // Retry leftover bytes from a previous partial send.
                if let Some(pending) = conn.pending_send.take() {
                    match sock.send_slice(&pending) {
                        Ok(sent) if sent < pending.len() => {
                            conn.pending_send = Some(pending[sent..].to_vec());
                            break;
                        }
                        Ok(_) => {} // fully sent, continue draining channel
                        Err(e) => {
                            tracing::debug!("TCP bridge: send error to {}: {e:?}", conn.remote);
                            conn.pending_send = Some(pending);
                            break;
                        }
                    }
                }

                match conn.host_to_guest_rx.try_recv() {
                    Ok(data) => {
                        if data.is_empty() {
                            conn.host_eof = true;
                            break;
                        }
                        tracing::debug!(
                            "TCP bridge: h2g relay {} bytes to {}",
                            data.len(),
                            conn.remote
                        );
                        match sock.send_slice(&data) {
                            Ok(sent) if sent < data.len() => {
                                conn.pending_send = Some(data[sent..].to_vec());
                                break;
                            }
                            Err(e) => {
                                tracing::debug!("TCP bridge: send error to {}: {e:?}", conn.remote);
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        conn.host_disconnected = true;
                        break;
                    }
                }
            }

            // Probe for host channel disconnect while the socket can't send.
            // Only probe when there is no queued partial payload; otherwise a
            // queued host payload could overwrite `pending_send` and corrupt
            // the host→guest byte stream ordering.
            if !conn.host_disconnected
                && !conn.host_eof
                && conn.pending_send.is_none()
                && !sock.can_send()
            {
                match conn.host_to_guest_rx.try_recv() {
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        conn.host_disconnected = true;
                    }
                    Ok(data) if data.is_empty() => {
                        conn.host_eof = true;
                    }
                    Ok(data) => {
                        conn.pending_send = Some(data);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
            }

            // If host read EOF'd and all pending data flushed to smoltcp,
            // close the smoltcp socket (sends FIN to guest).
            if conn.host_eof && conn.pending_send.is_none() && sock.may_send() {
                sock.close();
                conn.host_eof = false;
            }

            // Host channel disconnected: abort during handshake (connect
            // failure), gracefully close if already established.
            if conn.host_disconnected && !conn.host_eof {
                match sock.state() {
                    tcp::State::SynSent | tcp::State::SynReceived => {
                        sock.abort();
                        continue;
                    }
                    _ => {
                        sock.close();
                    }
                }
            }

            // Guest → Host: drain smoltcp rx buffer into channel.
            if sock.may_recv() {
                let _ = sock.recv(|buf| {
                    if buf.is_empty() {
                        return (0, ());
                    }
                    tracing::debug!(
                        "TCP bridge: g2h relay {} bytes from {}",
                        buf.len(),
                        conn.remote
                    );
                    match conn.guest_to_host_tx.as_ref() {
                        Some(tx) => match tx.try_send(buf.to_vec()) {
                            Ok(()) => (buf.len(), ()),
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Backpressure: don't dequeue from smoltcp.
                                // smoltcp will shrink the window automatically.
                                (0, ())
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                // Host write task gone, consume and drop.
                                (buf.len(), ())
                            }
                        },
                        None => {
                            // EOF already sent, consume and drop.
                            (buf.len(), ())
                        }
                    }
                });
            }

            // Signal guest EOF to the host write task only when the guest has
            // actually closed the receive half (FIN received). Check for
            // specific states where the remote FIN has been processed, NOT
            // just `!may_recv()` which is also false during handshake states.
            if conn.guest_to_host_tx.is_some() {
                let guest_fin_received = matches!(
                    sock.state(),
                    tcp::State::CloseWait
                        | tcp::State::LastAck
                        | tcp::State::Closing
                        | tcp::State::TimeWait
                        | tcp::State::Closed
                );
                if guest_fin_received {
                    conn.guest_to_host_tx.take();
                }
            }
        }
    }

    /// Removes closed connections and their socket handles.
    fn cleanup_closed(&mut self, sockets: &mut SocketSet<'_>) {
        let mut to_remove = Vec::new();

        for (&handle, conn) in &self.connections {
            let sock = sockets.get_mut::<tcp::Socket>(handle);
            if !sock.is_open() || sock.state() == tcp::State::Closed {
                to_remove.push(handle);
                tracing::debug!("TCP bridge: connection to {} closed", conn.remote);
            }
        }

        for handle in to_remove {
            self.connections.remove(&handle);
            self.port_handles.retain(|_, handles| {
                handles.retain(|h| *h != handle);
                !handles.is_empty()
            });
            sockets.remove(handle);
        }

        // Prune excess listen sockets created by `create_listen_socket`.
        // Each port needs at most one idle listener; extras waste 512 KiB each.
        self.prune_excess_listen_sockets(sockets);
    }

    /// Removes surplus listen sockets so at most one remains per port.
    ///
    /// `create_listen_socket` creates a fresh listener for every injected SYN.
    /// After `iface.poll()` consumes some and `detect_new_connections` promotes
    /// them to active connections, leftover LISTEN or CLOSED sockets accumulate
    /// in `port_handles`. This method keeps one LISTEN socket per port and
    /// removes the rest.
    fn prune_excess_listen_sockets(&mut self, sockets: &mut SocketSet<'_>) {
        for (port, handles) in &mut self.port_handles {
            // Partition handles: active connections stay, non-connection sockets
            // are candidates for pruning.
            let mut idle_listen: Vec<SocketHandle> = Vec::new();
            let mut to_remove: Vec<SocketHandle> = Vec::new();

            handles.retain(|&handle| {
                // Active connections are managed by cleanup_closed above.
                if self.connections.contains_key(&handle) {
                    return true;
                }

                let sock = sockets.get_mut::<tcp::Socket>(handle);
                match sock.state() {
                    tcp::State::Listen => {
                        idle_listen.push(handle);
                        true // keep for now, prune extras below
                    }
                    tcp::State::Closed => {
                        to_remove.push(handle);
                        false
                    }
                    // SYN-RECEIVED, ESTABLISHED, etc. — leave alone; these will
                    // be picked up by detect_new_connections or close naturally.
                    _ => true,
                }
            });

            // Keep one idle listener, remove the rest.
            if idle_listen.len() > 1 {
                for &extra in &idle_listen[1..] {
                    to_remove.push(extra);
                    handles.retain(|h| *h != extra);
                }
            }

            // Update listening_ports: true if at least one idle listener remains.
            if idle_listen.is_empty() {
                self.listening_ports.remove(port);
            } else {
                self.listening_ports.insert(*port);
            }

            for handle in to_remove {
                sockets.remove(handle);
            }
        }

        // Remove ports with no handles left.
        self.port_handles.retain(|_, handles| !handles.is_empty());
    }

    /// Returns the number of active connections.
    pub fn active_count(&self) -> usize {
        self.connections.len()
    }
}

/// Constructs an RST|ACK Ethernet frame in response to a SYN frame.
///
/// The RST has: seq=0, ack=syn_seq+1, flags=RST|ACK.
/// MAC addresses are swapped (gateway MAC as source, original source as dest).
/// IP addresses are swapped. Ports are swapped.
fn build_rst_from_syn(syn_frame: &[u8], gateway_mac: [u8; 6]) -> Option<Vec<u8>> {
    let ip_start = ETH_HEADER_LEN;
    if syn_frame.len() < ip_start + 40 {
        return None;
    }

    let ihl = ((syn_frame[ip_start] & 0x0F) as usize) * 4;
    let l4_start = ip_start + ihl;
    if l4_start + 20 > syn_frame.len() {
        return None;
    }

    // Extract from original SYN.
    let src_mac = &syn_frame[6..12];
    let syn_src_ip = [
        syn_frame[ip_start + 12],
        syn_frame[ip_start + 13],
        syn_frame[ip_start + 14],
        syn_frame[ip_start + 15],
    ];
    let syn_dst_ip = [
        syn_frame[ip_start + 16],
        syn_frame[ip_start + 17],
        syn_frame[ip_start + 18],
        syn_frame[ip_start + 19],
    ];
    let syn_src_port = u16::from_be_bytes([syn_frame[l4_start], syn_frame[l4_start + 1]]);
    let syn_dst_port = u16::from_be_bytes([syn_frame[l4_start + 2], syn_frame[l4_start + 3]]);
    let syn_seq = u32::from_be_bytes([
        syn_frame[l4_start + 4],
        syn_frame[l4_start + 5],
        syn_frame[l4_start + 6],
        syn_frame[l4_start + 7],
    ]);

    // Build RST|ACK: ETH(14) + IP(20) + TCP(20) = 54 bytes.
    let mut frame = vec![0u8; ETH_HEADER_LEN + 40];

    // Ethernet header: dst=original src MAC, src=gateway MAC.
    frame[0..6].copy_from_slice(src_mac);
    frame[6..12].copy_from_slice(&gateway_mac);
    frame[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4

    // IPv4 header (swapped IPs).
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x45; // version=4, IHL=5
    frame[ip + 2..ip + 4].copy_from_slice(&40u16.to_be_bytes()); // total length
    frame[ip + 6..ip + 8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF flag
    frame[ip + 8] = 64; // TTL
    frame[ip + 9] = 6; // TCP
    // src = original dst, dst = original src (we're the "server" responding).
    frame[ip + 12..ip + 16].copy_from_slice(&syn_dst_ip);
    frame[ip + 16..ip + 20].copy_from_slice(&syn_src_ip);
    // IP checksum.
    let ip_cksum = checksum::ipv4_header_checksum(&frame[ip..ip + 20]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // TCP header (swapped ports).
    let tcp_start = ip + 20;
    frame[tcp_start..tcp_start + 2].copy_from_slice(&syn_dst_port.to_be_bytes()); // src port
    frame[tcp_start + 2..tcp_start + 4].copy_from_slice(&syn_src_port.to_be_bytes()); // dst port
    // seq = 0
    frame[tcp_start + 4..tcp_start + 8].copy_from_slice(&0u32.to_be_bytes());
    // ack = syn_seq + 1
    frame[tcp_start + 8..tcp_start + 12].copy_from_slice(&(syn_seq.wrapping_add(1)).to_be_bytes());
    frame[tcp_start + 12] = 0x50; // data offset = 5 (20 bytes)
    frame[tcp_start + 13] = 0x14; // RST|ACK
    frame[tcp_start + 14..tcp_start + 16].copy_from_slice(&0u16.to_be_bytes()); // window = 0

    // TCP checksum.
    let tcp_cksum =
        checksum::tcp_checksum(syn_dst_ip, syn_src_ip, &frame[tcp_start..tcp_start + 20]);
    frame[tcp_start + 16..tcp_start + 18].copy_from_slice(&tcp_cksum.to_be_bytes());

    Some(frame)
}

/// Host connection task: connects to the remote server and bridges data
/// through channels back to the smoltcp poll loop.
async fn host_conn_task(
    remote: SocketAddr,
    h2g_tx: mpsc::Sender<Vec<u8>>,
    mut g2h_rx: mpsc::Receiver<Vec<u8>>,
) {
    let connect_started = StdInstant::now();
    tracing::debug!("TCP bridge: host_conn_task started for {remote}");
    // Connect to the remote host.
    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(remote),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!(
                "TCP bridge: connect to {remote} failed after {:?}: {e}",
                connect_started.elapsed()
            );
            // Drop h2g_tx — bridge will detect Disconnected and abort.
            return;
        }
        Err(_) => {
            tracing::debug!(
                "TCP bridge: connect to {remote} timed out after {:?}",
                connect_started.elapsed()
            );
            return;
        }
    };

    tracing::debug!(
        "TCP bridge: connected to {remote} in {:?}",
        connect_started.elapsed()
    );

    let (mut reader, mut writer) = stream.into_split();

    // Host → Guest: read from TcpStream, send via channel.
    let read_task = {
        let h2g_tx = h2g_tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 262_144];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        // EOF — send empty vec as signal.
                        let _ = h2g_tx.send(Vec::new()).await;
                        break;
                    }
                    Ok(n) => {
                        tracing::debug!("TCP bridge: host read {n} bytes from {remote}");
                        if h2g_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("TCP bridge: host read error for {remote}: {e}");
                        break;
                    }
                }
            }
        })
    };

    // Guest → Host: receive from channel, write to TcpStream.
    let write_task = tokio::spawn(async move {
        while let Some(data) = g2h_rx.recv().await {
            if data.is_empty() {
                // Guest closed connection.
                let _ = writer.shutdown().await;
                tracing::debug!("TCP bridge: host writer got guest EOF for {remote}");
                break;
            }
            tracing::debug!("TCP bridge: host write {} bytes to {remote}", data.len());
            if let Err(e) = writer.write_all(&data).await {
                tracing::debug!("TCP bridge: host write error for {remote}: {e}");
                break;
            }
        }
    });

    let _ = tokio::join!(read_task, write_task);
}

/// Relays data between an already-connected host TcpStream and channels,
/// for inbound (host→guest) connections where the stream is already accepted.
async fn inbound_host_relay(
    stream: tokio::net::TcpStream,
    h2g_tx: mpsc::Sender<Vec<u8>>,
    mut g2h_rx: mpsc::Receiver<Vec<u8>>,
) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |a| a.to_string());
    tracing::debug!("TCP bridge: inbound relay started for {peer}");

    let (mut reader, mut writer) = stream.into_split();

    let read_task = {
        let h2g_tx = h2g_tx.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 262_144];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        tracing::debug!("TCP bridge: inbound host EOF for {peer}");
                        let _ = h2g_tx.send(Vec::new()).await;
                        break;
                    }
                    Ok(n) => {
                        tracing::debug!("TCP bridge: inbound host read {n} bytes from {peer}");
                        if h2g_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("TCP bridge: inbound host read error for {peer}: {e}");
                        break;
                    }
                }
            }
        })
    };

    let write_task = tokio::spawn(async move {
        while let Some(data) = g2h_rx.recv().await {
            if data.is_empty() {
                tracing::debug!("TCP bridge: inbound guest EOF for {peer}");
                let _ = writer.shutdown().await;
                break;
            }
            tracing::debug!(
                "TCP bridge: inbound host write {} bytes to {peer}",
                data.len()
            );
            if let Err(e) = writer.write_all(&data).await {
                tracing::debug!("TCP bridge: inbound host write error for {peer}: {e}");
                break;
            }
        }
    });

    let _ = tokio::join!(read_task, write_task);
}

/// Converts a smoltcp `IpEndpoint` to a `SocketAddr`.
fn endpoint_to_sockaddr(ep: IpEndpoint) -> SocketAddr {
    let smoltcp::wire::IpAddress::Ipv4(v4) = ep.addr;
    SocketAddr::V4(SocketAddrV4::new(v4, ep.port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::iface::{Config, Interface};
    use smoltcp::wire::{EthernetAddress, IpCidr};

    use crate::darwin::smoltcp_device::{SmoltcpDevice, TcpSynInfo};
    use crate::ethernet::ETH_HEADER_LEN;

    const GW_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 64, 1);
    const GW_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    const GUEST_MAC: [u8; 6] = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
    const GUEST_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 64, 2);

    fn make_iface_and_sockets(device: &mut SmoltcpDevice) -> (Interface, SocketSet<'static>) {
        let hw_addr = EthernetAddress(GW_MAC);
        let config = Config::new(hw_addr.into());
        let mut iface = Interface::new(config, device, smoltcp::time::Instant::now());
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(GW_IP.into(), 24)).unwrap();
        });
        iface.set_any_ip(true);
        // Add a default route via the gateway so smoltcp can send replies.
        iface.routes_mut().add_default_ipv4_route(GW_IP).unwrap();
        let sockets = SocketSet::new(vec![]);
        (iface, sockets)
    }

    /// Builds a minimal TCP SYN frame from guest to remote.
    fn make_syn_frame(dst_ip: Ipv4Addr, dst_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; ETH_HEADER_LEN + 40]; // 20 IP + 20 TCP
        // Ethernet
        frame[0..6].copy_from_slice(&GW_MAC); // dst
        frame[6..12].copy_from_slice(&GUEST_MAC); // src
        frame[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4
        // IPv4
        let ip = ETH_HEADER_LEN;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&40u16.to_be_bytes()); // total len
        frame[ip + 8] = 64; // TTL
        frame[ip + 9] = 6; // TCP
        frame[ip + 12..ip + 16].copy_from_slice(&GUEST_IP.octets());
        frame[ip + 16..ip + 20].copy_from_slice(&dst_ip.octets());
        // IP checksum
        let cksum = ip_checksum(&frame[ip..ip + 20]);
        frame[ip + 10..ip + 12].copy_from_slice(&cksum.to_be_bytes());
        // TCP
        let tcp = ip + 20;
        frame[tcp..tcp + 2].copy_from_slice(&12345u16.to_be_bytes()); // src port
        frame[tcp + 2..tcp + 4].copy_from_slice(&dst_port.to_be_bytes()); // dst port
        frame[tcp + 4..tcp + 8].copy_from_slice(&1000u32.to_be_bytes()); // seq
        frame[tcp + 12] = 0x50; // data offset = 5 (20 bytes)
        frame[tcp + 13] = 0x02; // SYN
        frame[tcp + 14..tcp + 16].copy_from_slice(&65535u16.to_be_bytes()); // window
        // TCP checksum (pseudo-header + TCP segment)
        let tcp_cksum = tcp_checksum(&GUEST_IP.octets(), &dst_ip.octets(), &frame[tcp..ip + 40]);
        frame[tcp + 16..tcp + 18].copy_from_slice(&tcp_cksum.to_be_bytes());
        frame
    }

    fn ip_checksum(header: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        let mut i = 0;
        while i + 1 < header.len() {
            if i != 10 {
                sum += u32::from(u16::from_be_bytes([header[i], header[i + 1]]));
            }
            i += 2;
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !sum as u16
    }

    fn tcp_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], tcp_segment: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        // Pseudo-header
        sum += u32::from(u16::from_be_bytes([src_ip[0], src_ip[1]]));
        sum += u32::from(u16::from_be_bytes([src_ip[2], src_ip[3]]));
        sum += u32::from(u16::from_be_bytes([dst_ip[0], dst_ip[1]]));
        sum += u32::from(u16::from_be_bytes([dst_ip[2], dst_ip[3]]));
        sum += 6u32; // protocol TCP
        sum += tcp_segment.len() as u32;
        // TCP segment (skip checksum field at offset 16-17)
        let mut i = 0;
        while i + 1 < tcp_segment.len() {
            if i != 16 {
                sum += u32::from(u16::from_be_bytes([tcp_segment[i], tcp_segment[i + 1]]));
            }
            i += 2;
        }
        if i < tcp_segment.len() {
            sum += u32::from(tcp_segment[i]) << 8;
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !sum as u16
    }

    #[test]
    fn ensure_listen_sockets_creates_on_demand() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        let syns = vec![
            TcpSynInfo {
                dst_port: 443,
                src_ip: GUEST_IP,
                src_port: 1000,
                dst_ip: Ipv4Addr::new(1, 1, 1, 1),
                syn_seq: 0,
                frame: vec![],
            },
            TcpSynInfo {
                dst_port: 80,
                src_ip: GUEST_IP,
                src_port: 1001,
                dst_ip: Ipv4Addr::new(1, 1, 1, 1),
                syn_seq: 0,
                frame: vec![],
            },
        ];

        bridge.ensure_listen_sockets(&syns, &mut sockets);

        assert!(bridge.listening_ports.contains(&443));
        assert!(bridge.listening_ports.contains(&80));
        assert!(bridge.port_handles.contains_key(&443));
        assert!(bridge.port_handles.contains_key(&80));
    }

    #[test]
    fn ensure_listen_sockets_deduplicates() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        let syns = vec![TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 1000,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 0,
            frame: vec![],
        }];
        bridge.ensure_listen_sockets(&syns, &mut sockets);
        bridge.ensure_listen_sockets(&syns, &mut sockets);

        assert_eq!(bridge.port_handles[&443].len(), 1);
    }

    #[test]
    fn smoltcp_accepts_syn_with_listen_socket() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (mut iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Ensure a listen socket for port 443.
        let syns = vec![TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 1000,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 0,
            frame: vec![],
        }];
        bridge.ensure_listen_sockets(&syns, &mut sockets);

        // First, inject an ARP request from the guest to populate smoltcp's
        // neighbor cache with the guest MAC. Without this, smoltcp can't
        // send SYN-ACK because it doesn't know the destination MAC.
        let mut arp = vec![0u8; 42];
        arp[0..6].copy_from_slice(&[0xFF; 6]); // broadcast
        arp[6..12].copy_from_slice(&GUEST_MAC);
        arp[12..14].copy_from_slice(&[0x08, 0x06]); // ARP
        arp[14..16].copy_from_slice(&[0x00, 0x01]); // HW: Ethernet
        arp[16..18].copy_from_slice(&[0x08, 0x00]); // Proto: IPv4
        arp[18] = 6; // HLEN
        arp[19] = 4; // PLEN
        arp[20..22].copy_from_slice(&[0x00, 0x01]); // Op: Request
        arp[22..28].copy_from_slice(&GUEST_MAC);
        arp[28..32].copy_from_slice(&GUEST_IP.octets());
        arp[32..38].copy_from_slice(&[0x00; 6]);
        arp[38..42].copy_from_slice(&GW_IP.octets());
        device.inject_rx(arp);

        let ts = smoltcp::time::Instant::now();
        iface.poll(ts, &mut device, &mut sockets);
        // Clear the ARP reply.
        let _ = device.take_tx_pending();

        // Inject a SYN frame into the device.
        let syn = make_syn_frame(Ipv4Addr::new(1, 1, 1, 1), 443);
        device.inject_rx(syn);

        // Poll smoltcp.
        let ts = smoltcp::time::Instant::now();
        iface.poll(ts, &mut device, &mut sockets);

        // Check that the socket accepted the SYN (should be SynReceived).
        let handle = bridge.port_handles[&443][0];
        let sock = sockets.get_mut::<tcp::Socket>(handle);
        assert!(
            sock.is_active(),
            "Socket should be active after SYN; state={:?}",
            sock.state()
        );
    }

    #[test]
    fn bridge_active_count_starts_at_zero() {
        let bridge = TcpBridge::new(GW_IP);
        assert_eq!(bridge.active_count(), 0);
    }

    #[test]
    fn partial_send_preserves_remainder() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);

        // Create a socket with a tiny tx buffer to force partial sends.
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 64]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 16]);
        let mut sock = tcp::Socket::new(rx_buf, tx_buf);
        sock.set_nagle_enabled(false);
        // Put socket in a state where can_send() returns true. We'll test
        // BridgedConn's pending_send logic directly via relay_all.
        let handle = sockets.add(sock);

        let (_h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(4);
        let (g2h_tx, _g2h_rx) = mpsc::channel::<Vec<u8>>(4);

        let mut bridge = TcpBridge::new(GW_IP);
        let remote: SocketAddr = "1.1.1.1:443".parse().unwrap();

        // Manually construct a connection with pending data larger than
        // the tx buffer.
        bridge.connections.insert(
            handle,
            BridgedConn {
                handle,
                remote,
                host_to_guest_rx: h2g_rx,
                guest_to_host_tx: Some(g2h_tx),
                host_eof: false,
                host_disconnected: false,
                pending_send: Some(vec![0xAA; 32]),
            },
        );

        // The socket is in Closed state (not connected), so can_send() is
        // false. The pending data should be preserved across the relay call.
        bridge.relay_all(&mut sockets);

        let conn = bridge.connections.get(&handle).unwrap();
        assert!(
            conn.pending_send.is_some(),
            "Pending data should be preserved when socket can't send"
        );
        assert_eq!(conn.pending_send.as_ref().unwrap().len(), 32);
    }

    #[test]
    fn host_eof_waits_for_pending_send() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 64]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 64]);
        let sock = tcp::Socket::new(rx_buf, tx_buf);
        let handle = sockets.add(sock);

        let (_h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(4);
        let (g2h_tx, _g2h_rx) = mpsc::channel::<Vec<u8>>(4);

        let mut bridge = TcpBridge::new(GW_IP);
        let remote: SocketAddr = "1.1.1.1:443".parse().unwrap();

        // Simulate: host EOF received but there's still pending data.
        bridge.connections.insert(
            handle,
            BridgedConn {
                handle,
                remote,
                host_to_guest_rx: h2g_rx,
                guest_to_host_tx: Some(g2h_tx),
                host_eof: true,
                host_disconnected: false,
                pending_send: Some(vec![0xBB; 10]),
            },
        );

        // Socket is closed (can't send), so pending stays. The key check is
        // that host_eof is NOT consumed when pending_send is non-empty.
        bridge.relay_all(&mut sockets);

        let conn = bridge.connections.get(&handle).unwrap();
        assert!(
            conn.host_eof,
            "host_eof should remain set while pending_send is non-empty"
        );
    }

    #[test]
    fn cleanup_removes_stale_port_handles() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Create a listen socket for port 443.
        let syns = vec![TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 1000,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 0,
            frame: vec![],
        }];
        bridge.ensure_listen_sockets(&syns, &mut sockets);

        assert!(bridge.port_handles.contains_key(&443));
        let handle = bridge.port_handles[&443][0];

        // Close the socket so it transitions from Listen to Closed.
        let sock = sockets.get_mut::<tcp::Socket>(handle);
        sock.abort();

        // Simulate: the socket was an active connection that has now closed.
        let (_h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(1);
        let (g2h_tx, _g2h_rx) = mpsc::channel::<Vec<u8>>(1);
        bridge.connections.insert(
            handle,
            BridgedConn {
                handle,
                remote: "1.1.1.1:443".parse().unwrap(),
                host_to_guest_rx: h2g_rx,
                guest_to_host_tx: Some(g2h_tx),
                host_eof: false,
                host_disconnected: false,
                pending_send: None,
            },
        );

        bridge.cleanup_closed(&mut sockets);

        assert!(
            !bridge.port_handles.contains_key(&443),
            "port_handles should be cleaned up after socket removal"
        );
        assert!(bridge.connections.is_empty());
    }

    #[test]
    fn guest_eof_drops_sender() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);

        let rx_buf = tcp::SocketBuffer::new(vec![0u8; 64]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; 64]);
        let sock = tcp::Socket::new(rx_buf, tx_buf);
        let handle = sockets.add(sock);

        let (_h2g_tx, h2g_rx) = mpsc::channel::<Vec<u8>>(4);
        let (g2h_tx, mut g2h_rx) = mpsc::channel::<Vec<u8>>(4);

        let mut bridge = TcpBridge::new(GW_IP);
        let remote: SocketAddr = "1.1.1.1:443".parse().unwrap();

        bridge.connections.insert(
            handle,
            BridgedConn {
                handle,
                remote,
                host_to_guest_rx: h2g_rx,
                guest_to_host_tx: Some(g2h_tx),
                host_eof: false,
                host_disconnected: false,
                pending_send: None,
            },
        );

        // Socket is Closed — may_recv() returns false, state != Listen.
        // This should trigger the guest EOF signal (drop the sender).
        bridge.relay_all(&mut sockets);

        let conn = bridge.connections.get(&handle).unwrap();
        assert!(
            conn.guest_to_host_tx.is_none(),
            "guest_to_host_tx should be taken after EOF"
        );

        // The original sender was replaced, so the receiver should detect
        // disconnection once the replacement is dropped.
        drop(bridge);
        assert!(
            g2h_rx.try_recv().is_err(),
            "Receiver should see disconnect after sender replaced and dropped"
        );
    }

    #[tokio::test]
    async fn initiate_inbound_creates_connecting_socket() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (mut iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Create a pair of connected TcpStreams for the host-side stream.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let (stream, _accepted) = tokio::join!(connect, listener.accept());
        let stream = stream.unwrap();

        bridge.initiate_inbound(80, stream, GUEST_IP, GW_IP, &mut iface, &mut sockets);

        assert_eq!(
            bridge.active_count(),
            1,
            "Should have one inbound connection"
        );

        // The socket should be in SynSent state (attempting connect to guest).
        let (handle, conn) = bridge.connections.iter().next().unwrap();
        let sock = sockets.get_mut::<tcp::Socket>(*handle);
        assert!(
            sock.is_open(),
            "Socket should be open after connect; state={:?}",
            sock.state()
        );
        assert_eq!(conn.remote, SocketAddr::V4(SocketAddrV4::new(GUEST_IP, 80)));
    }

    #[tokio::test]
    async fn syn_gate_connect_success_injects_syn() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Start a local TCP listener so the connect succeeds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Build a SYN frame targeting the local listener.
        let syn = make_syn_frame(addr.ip().to_string().parse().unwrap(), addr.port());
        let syn_info = TcpSynInfo {
            dst_port: addr.port(),
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: addr.ip().to_string().parse().unwrap(),
            syn_seq: 1000,
            frame: syn.clone(),
        };

        // Gate the SYN — this spawns a connect task.
        bridge.gate_syns(&[syn_info], GW_MAC);
        assert_eq!(bridge.pending_syns.len(), 1);

        // Accept the connection on the listener side.
        let _accepted = listener.accept().await.unwrap();

        // Allow the connect task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Poll pending SYNs — should inject frame and store pre-connected stream.
        let rst_frames = bridge.poll_pending_syns(&mut device, &mut sockets, GW_MAC);
        assert!(
            rst_frames.is_empty(),
            "No RST should be generated on success"
        );
        assert!(bridge.pending_syns.is_empty(), "Pending should be consumed");
        assert_eq!(
            bridge.pre_connected.len(),
            1,
            "Should have pre-connected stream"
        );

        // The SYN frame should have been injected into device rx_queue.
        assert_eq!(device.take_tx_pending().len(), 0); // TX is separate
        // A listen socket should have been created for the port.
        assert!(bridge.listening_ports.contains(&addr.port()));
    }

    /// Verifies that a SYN destined for the gateway IP is translated to
    /// loopback and successfully connects to a local listener.
    #[tokio::test]
    async fn syn_gate_gateway_ip_translates_to_loopback() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Start a local TCP listener on loopback.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Build a SYN targeting the gateway IP (not loopback).
        let syn = make_syn_frame(GW_IP, port);
        let syn_info = TcpSynInfo {
            dst_port: port,
            src_ip: GUEST_IP,
            src_port: 54321,
            dst_ip: GW_IP,
            syn_seq: 2000,
            frame: syn,
        };

        bridge.gate_syns(&[syn_info], GW_MAC);
        assert_eq!(bridge.pending_syns.len(), 1);

        // The bridge should translate GW_IP → 127.0.0.1 and connect.
        let _accepted = listener.accept().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let rst_frames = bridge.poll_pending_syns(&mut device, &mut sockets, GW_MAC);
        assert!(
            rst_frames.is_empty(),
            "Gateway IP connect should succeed via loopback translation"
        );
        assert!(bridge.pending_syns.is_empty());
        assert_eq!(bridge.pre_connected.len(), 1);
    }

    #[tokio::test]
    async fn syn_gate_connect_failure_sends_rst() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Use a port that's definitely not listening (connection refused).
        let syn = make_syn_frame(Ipv4Addr::LOCALHOST, 1);
        let syn_info = TcpSynInfo {
            dst_port: 1,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::LOCALHOST,
            syn_seq: 1000,
            frame: syn,
        };

        bridge.gate_syns(&[syn_info], GW_MAC);

        // Wait for connect failure.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let rst_frames = bridge.poll_pending_syns(&mut device, &mut sockets, GW_MAC);
        assert_eq!(rst_frames.len(), 1, "Should generate exactly one RST");
        assert!(bridge.pending_syns.is_empty());
        assert!(bridge.pre_connected.is_empty());

        // Verify the RST frame structure.
        let rst = &rst_frames[0];
        assert!(rst.len() >= ETH_HEADER_LEN + 40);
        let ip = ETH_HEADER_LEN;
        let tcp_start = ip + 20;
        // Flags should be RST|ACK (0x14).
        assert_eq!(rst[tcp_start + 13], 0x14, "Flags should be RST|ACK");
        // ack = syn_seq + 1 = 1001 (make_syn_frame uses seq=1000).
        let ack = u32::from_be_bytes([
            rst[tcp_start + 8],
            rst[tcp_start + 9],
            rst[tcp_start + 10],
            rst[tcp_start + 11],
        ]);
        assert_eq!(ack, 1001, "ACK should be syn_seq + 1");
        // Dst MAC should be guest MAC.
        assert_eq!(&rst[0..6], &GUEST_MAC);
        // Src MAC should be gateway MAC.
        assert_eq!(&rst[6..12], &GW_MAC);
    }

    #[tokio::test]
    async fn syn_gate_retransmit_dedup() {
        let mut bridge = TcpBridge::new(GW_IP);

        let syn = make_syn_frame(Ipv4Addr::new(1, 1, 1, 1), 443);
        let syn_info = TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 1000,
            frame: syn.clone(),
        };

        // Gate the same SYN twice with identical ISN.
        bridge.gate_syns(std::slice::from_ref(&syn_info), GW_MAC);
        assert_eq!(bridge.pending_syns.len(), 1);

        bridge.gate_syns(&[syn_info], GW_MAC);
        // Should still be 1 — retransmit was deduplicated.
        assert_eq!(bridge.pending_syns.len(), 1);

        // Now gate with different ISN — should replace.
        let syn_info_new_isn = TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 5000,
            frame: syn,
        };
        bridge.gate_syns(&[syn_info_new_isn], GW_MAC);
        assert_eq!(bridge.pending_syns.len(), 1);
        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            dst_port: 443,
        };
        assert_eq!(bridge.pending_syns[&key].syn_seq, 5000);
    }

    #[tokio::test]
    async fn pre_connected_expires_after_ttl() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            dst_port: 443,
        };

        // Create a dummy TCP connection pair for the pre-connected stream.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let (stream, _accepted) = tokio::join!(connect, listener.accept());
        let stream = stream.unwrap();

        // Insert with an already-expired timestamp.
        bridge.pre_connected.insert(
            key,
            PreConnected {
                stream,
                syn_seq: 1000,
                created: StdInstant::now()
                    - std::time::Duration::from_secs(PRE_CONNECTED_TTL_SECS + 1),
            },
        );

        assert_eq!(bridge.pre_connected.len(), 1);

        // poll_pending_syns should expire the stale entry.
        let rst_frames = bridge.poll_pending_syns(&mut device, &mut sockets, GW_MAC);
        assert!(rst_frames.is_empty());
        assert!(
            bridge.pre_connected.is_empty(),
            "Expired entry should be removed"
        );
    }

    #[tokio::test]
    async fn pre_connected_same_isn_retransmit_dedup() {
        let mut bridge = TcpBridge::new(GW_IP);

        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            dst_port: 443,
        };

        // Create a dummy pre-connected stream.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let (stream, _accepted) = tokio::join!(connect, listener.accept());
        let stream = stream.unwrap();

        bridge.pre_connected.insert(
            key,
            PreConnected {
                stream,
                syn_seq: 1000,
                created: StdInstant::now(),
            },
        );

        // Retransmit SYN with same ISN — should be dropped, no new pending entry.
        let syn = make_syn_frame(Ipv4Addr::new(1, 1, 1, 1), 443);
        let syn_info = TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 1000,
            frame: syn,
        };

        bridge.gate_syns(&[syn_info], GW_MAC);
        assert!(
            bridge.pending_syns.is_empty(),
            "Same ISN retransmit should not create a new pending entry"
        );
        assert_eq!(
            bridge.pre_connected.len(),
            1,
            "Pre-connected stream should be preserved"
        );
    }

    #[tokio::test]
    async fn pre_connected_different_isn_evicts_stale_stream() {
        let mut bridge = TcpBridge::new(GW_IP);

        let key = SynFlowKey {
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            dst_port: 443,
        };

        // Create a dummy pre-connected stream.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let (stream, _accepted) = tokio::join!(connect, listener.accept());
        let stream = stream.unwrap();

        bridge.pre_connected.insert(
            key,
            PreConnected {
                stream,
                syn_seq: 1000,
                created: StdInstant::now(),
            },
        );

        // New SYN with different ISN — should evict the stale stream and gate.
        let syn = make_syn_frame(Ipv4Addr::new(1, 1, 1, 1), 443);
        let syn_info = TcpSynInfo {
            dst_port: 443,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            syn_seq: 5000,
            frame: syn,
        };

        bridge.gate_syns(&[syn_info], GW_MAC);
        assert!(
            bridge.pre_connected.is_empty(),
            "Stale pre-connected stream should be evicted"
        );
        assert_eq!(
            bridge.pending_syns.len(),
            1,
            "New ISN should create a new pending entry"
        );
        assert_eq!(bridge.pending_syns[&key].syn_seq, 5000);
    }

    #[test]
    fn prune_excess_listen_sockets_keeps_one() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Simulate what happens when multiple SYNs to port 443 complete:
        // create_listen_socket is called multiple times for the same port.
        assert!(bridge.create_listen_socket(443, &mut sockets));
        assert!(bridge.create_listen_socket(443, &mut sockets));
        assert!(bridge.create_listen_socket(443, &mut sockets));

        assert_eq!(bridge.port_handles[&443].len(), 3);

        // Pruning should keep exactly one LISTEN socket per port.
        bridge.prune_excess_listen_sockets(&mut sockets);

        assert_eq!(
            bridge.port_handles[&443].len(),
            1,
            "Should keep exactly one idle listen socket per port"
        );
        assert!(bridge.listening_ports.contains(&443));

        // The remaining socket should still be in LISTEN state.
        let handle = bridge.port_handles[&443][0];
        let sock = sockets.get_mut::<tcp::Socket>(handle);
        assert_eq!(sock.state(), tcp::State::Listen);
    }

    /// Regression test: multiple SYNs to the same port completing in one
    /// poll_pending_syns batch must each get their own listen socket so
    /// smoltcp doesn't RST the extras.
    #[tokio::test]
    async fn concurrent_syns_same_port_no_rst() {
        let mut device = SmoltcpDevice::new(0, GW_IP, 1500);
        let (_iface, mut sockets) = make_iface_and_sockets(&mut device);
        let mut bridge = TcpBridge::new(GW_IP);

        // Single listener — both SYNs connect to the same (ip, port).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dst_ip: Ipv4Addr = addr.ip().to_string().parse().unwrap();
        let port = addr.port();

        // Two SYN frames: same dst but different source ports (distinct four-tuples).
        let syn_a = make_syn_frame(dst_ip, port);
        let mut syn_b_frame = syn_a.clone();
        let tcp_start = ETH_HEADER_LEN + 20;
        syn_b_frame[tcp_start..tcp_start + 2].copy_from_slice(&54321u16.to_be_bytes());

        let syn_info_a = TcpSynInfo {
            dst_port: port,
            src_ip: GUEST_IP,
            src_port: 12345,
            dst_ip,
            syn_seq: 1000,
            frame: syn_a,
        };
        let syn_info_b = TcpSynInfo {
            dst_port: port,
            src_ip: GUEST_IP,
            src_port: 54321,
            dst_ip,
            syn_seq: 2000,
            frame: syn_b_frame,
        };

        // Gate both SYNs — spawns two concurrent connect tasks.
        bridge.gate_syns(&[syn_info_a, syn_info_b], GW_MAC);
        assert_eq!(bridge.pending_syns.len(), 2);

        // Accept both connections on the single listener.
        let _a = listener.accept().await.unwrap();
        let _b = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Both should complete without RST.
        let rst_frames = bridge.poll_pending_syns(&mut device, &mut sockets, GW_MAC);
        assert!(
            rst_frames.is_empty(),
            "No RSTs should be generated for concurrent SYNs to the same port"
        );
        assert_eq!(bridge.pre_connected.len(), 2);
        assert!(bridge.pending_syns.is_empty());
    }
}
