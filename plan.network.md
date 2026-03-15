# Plan: L3 Direct Routing via utun

> Status: IMPLEMENTED
> Date: 2026-03-14
> Revision: 3 (fixed return path: bidirectional utun with tunnel conntrack)

## Problem

Current host→container connectivity is L4 TCP proxy only (`-p 8080:80` required).
The host `DnsService` registers container IPs (`172.17.0.x`) that are not routable
from the host, making DNS registration meaningless. Guest `add_container_dns` is
dead code. OrbStack solves this with a `utun` interface + `pfctl` rules.

## Architecture Reality (pre-conditions)

Before reading this plan, the following must be understood about the current stack:

**Darwin network datapath is L2, not L3.**
`create_network_device()` (`vmm/darwin.rs:153`) creates a `socketpair` carrying raw
L2 Ethernet frames. `NetworkDatapath` owns the host-side FD and handles all traffic
itself via `SocketProxy` (UDP/ICMP), `TcpBridge` (TCP), `DhcpServer`, and a local
`DnsForwarder`. The comment in `socket_proxy.rs` is explicit: *"bypasses kernel
routing, VPN interference, and pf issues."* There is no `vmnet` handle at runtime
or daemon level — the datapath is fully self-contained inside the VMM task.

**Two separate DnsForwarder instances exist.**
1. `NetworkDatapath.dns_forwarder` — inside the VMM task, handles UDP:53 queries
   from the guest to `192.168.64.1`. Created with `DnsConfig::new(gateway_ip)` and
   has its own (empty) local hosts table, independent of `NetworkManager`.
2. `NetworkManager.forwarder` — in `arcbox-net`, used by `DnsService` on the host
   (`127.0.0.1:5553`). Fed by `runtime.register_dns()` and `arcbox-docker`.

`runtime.register_dns()` only updates forwarder #2. Queries from inside the guest
never reach forwarder #2 — they hit forwarder #1, which knows nothing about
containers. Phase 3 "zero code change" was wrong.

## Approach

**Not Network Extension.** OrbStack uses `utun` + `pfctl` + `route`, installed via
a root LaunchDaemon (one-time `arcbox daemon install --privileged`). No Apple
entitlement required. The Darwin `DarwinTun` in `arcbox-net` already creates `utun`
devices without root. Route management via `/sbin/route` and `pfctl` requires root,
held by the privileged helper.

**Host→guest packet injection path.**
`InboundListenerManager` already sends `InboundCommand` messages over `cmd_tx` into
`NetworkDatapath`. We extend `InboundCommand` with a `RoutePacket(Vec<u8>)` variant.
The datapath loop wraps the IP packet in an L2 Ethernet frame (using the gateway MAC
and the container's IP as dest) and writes it to the guest FD. This is the only
viable injection path given the current socketpair architecture.

**Shared DNS registration table.**
`NetworkDatapath` is constructed in `vmm/darwin.rs`. We pass an
`Arc<RwLock<HashMap<String, IpAddr>>>` shared with `NetworkManager` so that
`runtime.register_dns()` updates both forwarder instances simultaneously. The VMM
crate gets a minimal dependency: `std::sync::Arc` and `std::sync::RwLock` only.

## Target Data Flow

```
Host: curl http://my-nginx.arcbox.local/
  │
  ├─ /etc/resolver/arcbox.local → 127.0.0.1:5553
  │   └─ DnsService (NetworkManager forwarder #2) → 172.17.0.2
  │       (now routable because Phase 2 adds the route)
  │
  ├─ macOS route table: 172.17.0.0/16 → utun{N}   (added by L3TunnelService)
  │                     172.20.0.0/16  → utun{N}
  │
  ├─ L3TunnelService reads IP packet [dst=172.17.0.2] from utun{N}
  │   └─ sends RoutePacket(bytes) → InboundListenerManager.cmd_tx
  │       └─ NetworkDatapath receives InboundCommand::RoutePacket
  │           └─ wraps in L2 frame → writes to guest socketpair FD
  │               └─ Guest kernel → docker0 bridge → container
  │
  └─ Return path: container → docker0 → guest virtio-net → socketpair
      → SmoltcpDevice classifies frame → check TunnelConnTrack
        → HIT (reverse 5-tuple matches an injected RoutePacket flow):
            strip L2 header → write raw IP to TunWriter → utun{N}
            → macOS kernel delivers to host TCP stack → curl receives reply
        → MISS: existing proxy path (SocketProxy / TcpBridge)

Guest: container DNS query for other-container.arcbox.local
  │
  ├─ /etc/resolv.conf → 127.0.0.1:53 (GuestDnsServer in agent)
  │   ├─ registered containers → 172.17.0.x (direct answer)
  │   └─ unknown → forward to 192.168.64.1:53
  │       └─ NetworkDatapath DnsForwarder (forwarder #1)
  │           ├─ *.arcbox.local registered (shared table) → answer
  │           └─ other → upstream Internet DNS
  │
  └─ /etc/docker/daemon.json: {"dns": ["192.168.64.1"]}
      (Docker containers query gateway directly, not 127.0.0.1)
```

## Phases

### Phase 1 — Guest DNS Server + Initial Reconcile

**Goal**: Container-to-container name resolution inside the guest works.
`add_container_dns` is dead code today; this activates it properly.

**Changes**:

**New crate `common/arcbox-dns`**
Extract from `virt/arcbox-net/src/dns.rs`:
- `DnsPacket::parse(data: &[u8]) -> Result<DnsPacket, DnsError>`
- `response_a(pkt, ip: Ipv4Addr, ttl: u32) -> Vec<u8>`
- `response_nxdomain(pkt) -> Vec<u8>`
- `response_servfail(pkt) -> Vec<u8>`

No platform deps. Compiles under `aarch64-unknown-linux-musl`.
`arcbox-net` re-exports from `arcbox-dns`; no behaviour change.

**New `guest/arcbox-agent/src/dns_server.rs`**
UDP server on `0.0.0.0:53` inside the guest. Maintains:
- `containers: Arc<RwLock<HashMap<String, Ipv4Addr>>>` — name → IP
- `sandboxes: Arc<RwLock<HashMap<String, Ipv4Addr>>>` — sandbox_id → TAP IP

Unknown queries forwarded to `192.168.64.1:53` (forwarder #1, which forwards
to upstream for non-local names).

**New `guest/arcbox-agent/src/docker_events.rs`**
Connects to `/var/run/docker.sock` after `ensure_runtime` succeeds.

On startup — **initial reconcile** (mirrors `recover_container_networking`):
1. `GET /containers/json` to list all running containers
2. For each: `GET /containers/{id}/json` → extract name + IP → register

Then subscribes to `GET /events?filters={"type":["container"]}`:
- `start` → inspect → register (handles containers started after reconcile)
- `die` / `destroy` → deregister
- `rename` → deregister old name, register new name

Retries connection with 1s backoff until dockerd socket is ready.

**Modify `guest/arcbox-agent/src/init.rs`**:
- `write_etc_resolv_conf()`: write `nameserver 127.0.0.1` (was `192.168.64.1`)
- New `write_etc_docker_daemon_dns()`: write `/etc/docker/daemon.json`
  with `{"dns": ["192.168.64.1"]}` so containers query the gateway

**Modify `guest/arcbox-agent/src/sandbox.rs`**:
- Replace `add_sandbox_dns(id, ip)` (writes `/etc/hosts`) with
  `dns_server.register_sandbox(id, ip)`
- Replace `remove_dns(id)` with `dns_server.deregister_sandbox(id)`

**Modify `guest/arcbox-agent/src/main.rs`**: spawn `GuestDnsServer::run()`.

**Deliverable**: `docker run --rm alpine nslookup other-container` resolves inside guest.
`add_container_dns`/`remove_container_dns` dead code removed.

---

### Phase 2 — Shared DNS Table (Gateway Resolves Container Names)

**Goal**: Containers inside the guest can resolve `name.arcbox.local` by querying
the gateway (`192.168.64.1:53`), which now knows about container registrations.
This also unblocks Phase 3.

**Problem**: `NetworkDatapath.dns_forwarder` (forwarder #1) is constructed locally
in `vmm/darwin.rs` with an empty hosts table. `runtime.register_dns()` only updates
`NetworkManager` (forwarder #2). The two are unconnected.

**Fix**: Introduce `Arc<LocalHostsTable>` — a shared type alias for
`Arc<RwLock<HashMap<String, IpAddr>>>` in `common/arcbox-dns` (or `arcbox-net`).

**Modify `virt/arcbox-net/src/dns.rs`**:
- `DnsForwarder` gains a `local_hosts: Arc<LocalHostsTable>` field (replacing
  the current owned `HashMap`)
- `add_local_host` / `remove_local_host` write through the `Arc`

**Modify `virt/arcbox-net/src/lib.rs`** (`NetworkManager`):
- Exposes `fn local_hosts_table(&self) -> Arc<LocalHostsTable>`
- The `DnsForwarder` inside `NetworkManager` uses this shared table

**Modify `virt/arcbox-vmm/src/vmm/darwin.rs`** (`create_network_device`):
- Accept `local_hosts: Arc<LocalHostsTable>` parameter
- Pass it to `DnsForwarder::new(dns_config, local_hosts)`

**Modify `app/arcbox-core/src/runtime.rs`**:
- When constructing the VMM, pass `network_manager.local_hosts_table()`
- `register_dns()` / `deregister_dns_by_id()` now update the shared table,
  which is visible to both `DnsService` and `NetworkDatapath` simultaneously

**Deliverable**: `docker run --rm alpine nslookup my-nginx.arcbox.local` returns
`172.17.0.x` from inside the guest.

---

### Phase 3 — Host L3 Routing

**Goal**: `curl http://172.17.0.2/` works from macOS host without `-p`.

**New `virt/arcbox-net/src/darwin/l3_tunnel.rs`**:

```rust
pub struct L3TunnelService {
    tun: DarwinTun,
    tun_name: String,
    subnets: Vec<Ipv4Network>,
    inbound_cmd_tx: mpsc::Sender<InboundCommand>,
    cancel: CancellationToken,
}

/// Cloneable write handle for return packets (datapath → utun).
/// Wraps the utun FD; read side is owned by L3TunnelService's read loop.
pub struct TunWriter { fd: Arc<OwnedFd> }

impl L3TunnelService {
    /// Creates utun, adds routes, starts read loop.
    /// Returns (self, TunWriter) — caller passes TunWriter to the datapath.
    pub async fn start(
        subnets: Vec<Ipv4Network>,
        inbound_cmd_tx: mpsc::Sender<InboundCommand>,
    ) -> Result<(Self, TunWriter)>;

    pub async fn stop(self) -> Result<()>;
}
```

Read loop: reads IP packets from `utun{N}`, sends
`InboundCommand::RoutePacket(packet)` to the datapath.

**New `virt/arcbox-net/src/darwin/tunnel_conntrack.rs`**:

Lightweight connection tracker scoped to tunneled flows only.
Does NOT replace the NAT engine's conntrack — this is a separate, small table
used solely to distinguish "return packets for host-initiated connections"
from "guest-initiated outbound traffic" in the datapath.

```rust
pub struct TunnelConnTrack {
    /// Reverse 5-tuples of packets injected via RoutePacket.
    /// Key = (src_ip, dst_ip, src_port, dst_port, proto) of the EXPECTED REPLY.
    entries: HashMap<FiveTuple, Instant>,
}

impl TunnelConnTrack {
    /// Called when a RoutePacket is injected into the guest.
    /// Parses the injected packet's 5-tuple and registers the reverse flow.
    pub fn register_injected(&mut self, ip_packet: &[u8]);

    /// Called for every frame received from the guest (before proxy dispatch).
    /// Returns true if this packet is a reply to a tunneled flow.
    pub fn is_tunnel_return(&mut self, ip_packet: &[u8]) -> bool;

    /// Periodic sweep to expire stale entries (TCP FIN/RST, timeout).
    pub fn gc(&mut self);
}
```

Timeout defaults: TCP established 300s, TCP half-open 30s, UDP 60s, ICMP 10s.
GC runs every 10s via the datapath's existing timer tick.

**Extend `virt/arcbox-net/src/darwin/inbound_relay.rs`**:

```rust
pub enum InboundCommand {
    // existing variants...
    AddRule(...),
    RemoveRule(...),
    // new:
    RoutePacket(Vec<u8>),  // raw IPv4 packet from utun to inject to guest
}
```

**Modify `virt/arcbox-net/src/darwin/datapath_loop.rs`**:
- Add fields: `tunnel_conntrack: TunnelConnTrack`, `tun_writer: Option<TunWriter>`
- Handle `InboundCommand::RoutePacket(pkt)`:
  1. `tunnel_conntrack.register_injected(&pkt)` — register reverse flow
  2. Wrap in L2 Ethernet frame (dst MAC = guest MAC, src MAC = gateway MAC)
  3. Push to `write_queue`
- In the frame-receive path (guest → host), **before** SmoltcpDevice dispatch:
  1. Parse IP header from the L2 frame
  2. `if tunnel_conntrack.is_tunnel_return(ip_packet)` → strip L2, write raw IP
     to `tun_writer` → done (do NOT pass to SmoltcpDevice/proxy)
  3. Otherwise → existing SmoltcpDevice classification and proxy dispatch
- Add GC call in the existing timer tick (every 10s)

**New `virt/arcbox-net/src/darwin/route_manager.rs`**:

```rust
pub fn add_route(subnet: Ipv4Network, iface: &str) -> Result<()>;
// /sbin/route -n add -net <subnet> -interface <iface>

pub fn remove_route(subnet: Ipv4Network, iface: &str) -> Result<()>;
pub fn enable_ip_forwarding() -> Result<()>;
// sysctl -w net.inet.ip.forwarding=1
```

These call system binaries. Root is provided by the privileged helper (see below).

**Privileged helper (new `app/arcbox-daemon/src/route_helper.rs`)**:
A minimal root process installed as a LaunchDaemon by `arcbox daemon install`.
The main daemon communicates over a Unix socket (`/var/run/arcbox/route.sock`).
Protocol: newline-delimited JSON commands `{"op":"add","subnet":"...","iface":"..."}`.
The helper validates the interface name against `utun` prefix before executing.

**Modify `app/arcbox-core/src/runtime.rs`**:
- Add `#[cfg(target_os = "macos")] tunnel: Option<L3TunnelService>`
- In `init()`: after VM is ready, call `L3TunnelService::start(subnets, cmd_tx)`
  where `cmd_tx` is obtained from the `InboundListenerManager`
- In `shutdown()`: call `tunnel.stop()`

`InboundListenerManager::cmd_tx()` needs to be exposed from the VMM to the runtime.
Currently it is stored in `DarwinVmm.inbound_listener_manager`. Expose via a new
method on the `Machine` / `Vmm` trait.

**Modify `virt/arcbox-vmm`**:
- `DarwinVmm`: expose `inbound_cmd_tx() -> mpsc::Sender<InboundCommand>`
- This is the one required change to this crate

**Modify `app/arcbox-cli/src/commands/daemon.rs`**:
- `arcbox daemon install` also installs the privileged route helper plist

**Deliverable**: `curl http://172.17.0.2/` from macOS host reaches the container.

---

### Phase 4 — Host DNS Returns Routable IPs + Custom Network Routes

**Goal**: `curl http://my-nginx.arcbox.local/` works from host.
Fix custom Docker networks (`172.18.x.x` etc.) being registered but not routed.

**Modify `app/arcbox-docker/src/handlers/container.rs`**:
- `setup_container_networking()`: after extracting the container IP, check if
  its subnet (`/24` or the Docker network CIDR) already has a route via
  `L3TunnelService`. If not, call `L3TunnelService::add_subnet(subnet)` to add
  a route dynamically.
- Add a method to `Runtime`: `add_container_subnet(subnet: Ipv4Network)` which
  delegates to `L3TunnelService`.

This is the only change to `arcbox-docker`.

After Phase 2 (shared DNS table) and Phase 3 (L3 routing), host DNS registrations
(`172.17.0.x`, `172.18.0.x`, etc.) are now routable and DNS resolution works
end-to-end. No separate "Phase 3 zero-change" — it is a consequence of phases 2+3+4.

**Deliverable**: `nslookup my-nginx.arcbox.local` → `172.17.0.2`, `curl` succeeds.

---

### Phase 5 — Sandbox Subnet Routing + Host DNS

**Goal**: `ssh user@sandbox-abc.arcbox.local` works from host.

`CreateSandboxResponse.ip_address` (proto field, already in generated code) contains
the TAP IP. `RestoreResponse.ip_address` also exists.

**Modify `app/arcbox-api/src/grpc.rs`** (`SandboxServiceImpl`):
- `create()`: after forwarding RPC and getting `CreateSandboxResponse`, call
  `runtime.register_dns(resp.id, resp.ip_address.parse()?)` and
  `runtime.add_container_subnet(tap_subnet)` (e.g. `172.20.0.0/16`)
- `stop()` / `remove()`: call `runtime.deregister_dns_by_id(id)`
- `restore()`: same as `create()` using `RestoreResponse.ip_address`

The `172.20.0.0/16` route is already added at VM startup by `L3TunnelService`
(Phase 3), so `add_container_subnet` is a no-op here.

**Deliverable**: `ssh user@10.88.0.2` from host, and `sandbox-abc.arcbox.local`
resolves to the TAP IP.

---

### Phase 6 — Dead Code Cleanup

- Delete `/etc/hosts` write logic from `guest/arcbox-agent/src/dns.rs`
- Remove `add_container_dns`, `remove_container_dns`, `add_sandbox_dns`,
  `remove_dns` functions
- `arcbox-net/src/dns.rs`: remove protocol parsing/building functions now
  provided by `arcbox-dns`

---

## Affected Crates (corrected)

### New

| Crate | Purpose |
|-------|---------|
| `common/arcbox-dns` | DNS packet parse + response build; no platform deps; used by host and guest |

### Modified

| Crate | What changes | Phase |
|-------|-------------|-------|
| `virt/arcbox-net` | `dns.rs`: `DnsForwarder` uses shared `Arc<LocalHostsTable>`; new `darwin/l3_tunnel.rs`, `darwin/tunnel_conntrack.rs`, `darwin/route_manager.rs`; extend `darwin/inbound_relay.rs` with `RoutePacket`; `darwin/datapath_loop.rs` handles `RoutePacket` + tunnel return path via `TunnelConnTrack` + `TunWriter` | 2, 3 |
| `virt/arcbox-vmm` | `vmm/darwin.rs`: accept `Arc<LocalHostsTable>` in `create_network_device()`; expose `inbound_cmd_tx()` | 2, 3 |
| `app/arcbox-core` | `runtime.rs`: pass shared table to VMM; integrate `L3TunnelService`; add `add_container_subnet()` | 2, 3 |
| `app/arcbox-daemon` | New `route_helper.rs` (privileged helper client); `main.rs` wires up tunnel start | 3 |
| `app/arcbox-cli` | `daemon install` installs route helper LaunchDaemon plist | 3 |
| `app/arcbox-docker` | `handlers/container.rs`: call `runtime.add_container_subnet()` on container start | 4 |
| `app/arcbox-api` | `grpc.rs`: `SandboxServiceImpl::create/restore/stop/remove` call `runtime.register_dns` / `deregister_dns_by_id` | 5 |
| `guest/arcbox-agent` | New `dns_server.rs`, `docker_events.rs`; modify `init.rs`, `sandbox.rs`, `main.rs`; remove dead code in `dns.rs` | 1 |

### Unchanged

| Crate | Reason |
|-------|--------|
| `app/arcbox-docker` (DNS side) | `register_dns` call chain already correct; becomes effective after Phase 2+3 |
| `rpc/arcbox-protocol` | No new message types needed (sandbox IP already in `CreateSandboxResponse`) |
| `virt/arcbox-hypervisor` | No change |
| `virt/arcbox-virtio` | No change |
| `virt/arcbox-fs` | No change |
| `runtime/*` | No change |
| `common/arcbox-constants` | May add subnet constants, otherwise no change |
| `common/arcbox-error` | No change |

---

## Key Interface Changes

```rust
// common/arcbox-dns/src/lib.rs
pub type LocalHostsTable = RwLock<HashMap<String, IpAddr>>;

pub struct DnsPacket<'a> { pub id: u16, pub qname: Cow<'a, str>, pub qtype: u16 }
impl<'a> DnsPacket<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, DnsError>;
}
pub fn response_a(pkt: &DnsPacket<'_>, ip: Ipv4Addr, ttl: u32) -> Vec<u8>;
pub fn response_nxdomain(pkt: &DnsPacket<'_>) -> Vec<u8>;
pub fn response_servfail(pkt: &DnsPacket<'_>) -> Vec<u8>;

// virt/arcbox-net/src/darwin/inbound_relay.rs
pub enum InboundCommand {
    AddRule { ... },
    RemoveRule { id: String },
    RoutePacket(Vec<u8>),  // raw IPv4 packet; datapath wraps in L2 and sends to guest
}

// virt/arcbox-net/src/darwin/l3_tunnel.rs
pub struct L3TunnelService { ... }
pub struct TunWriter { fd: Arc<OwnedFd> }
impl TunWriter {
    pub fn write_packet(&self, ip_packet: &[u8]) -> io::Result<usize>;
}
impl L3TunnelService {
    pub async fn start(
        subnets: Vec<Ipv4Network>,
        inbound_cmd_tx: mpsc::Sender<InboundCommand>,
    ) -> Result<(Self, TunWriter)>;
    pub async fn add_subnet(&self, subnet: Ipv4Network) -> Result<()>;
    pub async fn stop(self) -> Result<()>;
    pub fn tun_name(&self) -> &str;
}

// virt/arcbox-net/src/darwin/tunnel_conntrack.rs
pub struct TunnelConnTrack { ... }
impl TunnelConnTrack {
    pub fn register_injected(&mut self, ip_packet: &[u8]);
    pub fn is_tunnel_return(&mut self, ip_packet: &[u8]) -> bool;
    pub fn gc(&mut self);
}

// virt/arcbox-vmm darwin interface addition
impl DarwinVmm {
    pub fn inbound_cmd_tx(&self) -> Option<mpsc::Sender<InboundCommand>>;
}

// app/arcbox-core/src/runtime.rs additions
impl Runtime {
    pub async fn add_container_subnet(&self, subnet: Ipv4Network);
}
```

---

## Constraints & Risks

| Item | Note |
|------|------|
| Guest MAC for `RoutePacket` injection | `NetworkDatapath` must track guest MAC (learned from first ARP or DHCP frame). Add `guest_mac: Option<[u8; 6]>` field; drop `RoutePacket` if MAC not yet known (safe: no container is reachable before DHCP completes) |
| Port 53 in guest | Agent runs as PID 1 (root). No systemd-resolved on minimal Linux guest |
| `pfctl` / `route` require root | Privileged helper installed once at `arcbox daemon install` |
| Docker custom networks | Phase 4 dynamically adds routes per-container-start. User-defined networks created without running containers are not routed until first container starts |
| `arcbox-dns` musl compatibility | No platform deps; verified compilable under `aarch64-unknown-linux-musl` |
| Bidirectional utun return path | Reply packets from containers must go back through utun, NOT through SocketProxy/TcpBridge (smoltcp never created these connections). `TunnelConnTrack` in the datapath distinguishes tunnel-return packets from guest-initiated outbound traffic. False negatives (conntrack miss) cause the reply to hit the proxy path and get dropped — acceptable since it only happens if the conntrack entry expired (5min TCP). False positives are impossible by construction (only registered by RoutePacket injection). |
| `InboundListenerManager` lifetime | `cmd_tx` must outlive the `L3TunnelService`. Both are owned by `Runtime`; shutdown order: stop tunnel first, then stop VMM |
