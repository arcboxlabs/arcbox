# Plan: Privileged Network Operations

> Status: PLANNED
> Date: 2026-03-15
> Revision: 5 — helper downgraded, NE is sole production path

## Context

macOS utun `write()` does not enter the IP input path. Packets written to
a utun fd are processed as **output** (send to peer), not **input** (deliver
to local stack). This was definitively proven:

- 100 UDP packets with correct checksums written to utun fd
- IP input counter delta = 0 (matches background noise exactly)
- tcpdump visibility was due to BPF capturing on the output path

This means the root helper can create a utun and read outbound packets
(host → container), but **cannot inject return packets into the host TCP/UDP
stack**. The helper path provides half-duplex L3 only.

`NEPacketTunnelProvider.packetFlow.writePackets()` is the only supported
mechanism on macOS to inject packets into the local IP input path.

## Decision: NetworkExtension is the sole production path

| Path | Role | Bidirectional L3 |
|------|------|------------------|
| **NetworkExtension** | Production | ✅ Full duplex via packetFlow |
| **Root helper** | Dev/diagnostics only | ❌ Read-only (outbound capture), no return path |

Without the desktop app / NetworkExtension installed, macOS does **not**
support transparent container IP direct access. CLI-only users use the
existing L4 proxy (`-p` port forwarding).

### Crates

| Need | Crate | Rationale |
|------|-------|-----------|
| fd passing (helper, dev) | **`sendfd`** 0.4 | DGRAM fd handoff for utun fd |
| utun creation (helper) | **Existing `DarwinTun`** | Audited with SAFETY comments |

## Architecture

```
                    ┌─────────────────────────┐
                    │     arcbox-daemon        │
                    │     (Rust, user)         │
                    │                          │
                    │  L3TunnelService         │
                    │   TunnelBackend trait     │
                    │   ├─ read_packet()       │
                    │   │  → RoutePacket       │
                    │   └─ write_packet()      │
                    │      ← return pkts       │
                    └──────────┬───────────────┘
                               │
               ┌───────────────┴───────────────┐
               ▼                               ▼
  ┌──────────────────────┐       ┌──────────────────────┐
  │  NetworkExtension    │       │  Root Helper          │
  │  (PRODUCTION)        │       │  (DEV/DIAG ONLY)      │
  │                      │       │                        │
  │  System Extension    │       │  arcbox-helper         │
  │  in arcbox-desktop   │       │  LaunchDaemon          │
  │                      │       │                        │
  │  utun: kernel        │       │  utun: helper (root)   │
  │  routes: NE API      │       │  routes: /sbin/route   │
  │  DNS: NE API         │       │  inbound: ✅ works     │
  │  inbound: ✅         │       │  return:  ❌ utun      │
  │  return:  ✅         │       │    write is output-only│
  │  IPC: DGRAM pair     │       │                        │
  │                      │       │  Use: tunnel doctor    │
  │  Full L3 duplex      │       │    --probe, dev testing│
  └──────────────────────┘       └──────────────────────┘
```

### Backend selection

```rust
pub enum TunnelStrategy {
    Auto,           // NE only in production
    ForceNe,        // Fail if NE unavailable
    HelperReadOnly, // Dev: inbound-only via helper (no return path)
    Direct,         // Dev: running as root
}
```

`Auto` = try NE → if unavailable, log warning, no transparent L3.
`HelperReadOnly` = for `abctl tunnel doctor --probe` and development.

---

## NetworkExtension (production path)

### Components

**System Extension** in arcbox-desktop:

```
ArcBox Desktop.app/Contents/Library/SystemExtensions/
  com.arcboxlabs.desktop.network-extension.systemextension/
```

**Bundle ID**: `com.arcboxlabs.desktop.network-extension`

**Entitlements** (extension):
- `com.apple.developer.networking.networkextension` = `["packet-tunnel-provider-systemextension"]`
- `com.apple.security.application-groups` = `["group.com.arcboxlabs.desktop"]`

**Entitlements** (main app):
- `com.apple.developer.system-extension.install`
- `com.apple.security.application-groups` = `["group.com.arcboxlabs.desktop"]`

### IPC: STREAM rendezvous + DGRAM pair

```
Extension startup:
  1. Bind STREAM at /var/run/arcbox/tunnel.sock
  2. Accept daemon connection
  3. socketpair(AF_UNIX, SOCK_DGRAM) → [ext_end, daemon_end]
  4. Send daemon_end fd via sendfd on STREAM
  5. Close STREAM
  6. Packet I/O on DGRAM pair (atomic, message-oriented)
```

### Data flow

```
Host: curl 172.17.0.2
  → kernel routes to utun (NE owns it)
  → extension: packetFlow.readPackets()
  → send(ext_end, ip_packet)                    [DGRAM]
  → daemon: recv(daemon_end) → RoutePacket → L2 inject → guest

Return:
  guest → socketpair → datapath → TunnelConnTrack match
  → daemon: send(daemon_end, ip_packet)          [DGRAM]
  → extension: recv(ext_end)
  → packetFlow.writePackets()                    [INPUT path ✅]
  → kernel → host TCP stack → curl receives reply
```

### Route and DNS

Handled by `NEPacketTunnelNetworkSettings`:
- `NEIPv4Settings.includedRoutes`: 172.16.0.0/12, 10.88.0.0/16
- `NEDNSSettings`: matchDomains=["arcbox.local"], servers=["127.0.0.1"]
- No `/sbin/route`, no `/etc/resolver/` writes

### Hello handshake

First frame on STREAM before fd exchange:

```json
→ {"hello":{"version":1,"session_id":"...","mtu":1500,"features":[]}}
← {"hello":{"version":1,"backend":"network-extension","session_id":"...","features":[]}}
```

### Lifecycle

1. App startup → `StartupOrchestrator` → `OSSystemExtensionManager.submit()`
2. User approval (first time)
3. `NETunnelProviderManager.connection.startVPNTunnel()`
4. Extension binds `/var/run/arcbox/tunnel.sock`
5. Daemon connects → handshake → DGRAM pair → packets flow

### Affected files (arcbox-desktop)

| File | Change |
|------|--------|
| `ArcBoxNetworkExtension/PacketTunnelProvider.swift` | New: ~150 lines |
| `ArcBoxNetworkExtension/Info.plist` | New |
| `ArcBoxNetworkExtension/*.entitlements` | New |
| `ArcBox/ArcBoxApp.swift` | System extension activation |
| `Packages/.../StartupOrchestrator.swift` | NE setup step |
| `ArcBox.xcodeproj` | New target |
| `ArcBox/*.entitlements` | system-extension.install + App Group |

### Affected files (arcbox repo)

| File | Change |
|------|--------|
| `virt/arcbox-net/src/darwin/l3_tunnel.rs` | `NetworkExtensionBackend` impl |
| `virt/arcbox-net/src/darwin/datapath_loop.rs` | Already done: RoutePacket + TunnelConnTrack + TunWriter |

---

## Root helper (dev/diagnostics only)

Retained for:
- `abctl tunnel doctor --probe`: verifies utun creation, fd passing, route install
- Development: test inbound path without NE

**Not** a production backend. Does not provide return path.

Existing code: `app/arcbox-helper/` with create_utun, add_route, remove_route.

---

## TunnelBackend trait (shared)

```rust
trait TunnelBackend: Send + 'static {
    fn read_packet(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_packet(&self, packet: &[u8]) -> io::Result<usize>;
    fn as_raw_fd(&self) -> RawFd;
    fn session_id(&self) -> &str;
}
```

- `NetworkExtensionBackend`: read/write on DGRAM pair fd. Raw IP, no AF header.
- `UtunBackend` (dev only): read works, write is no-op with warning.

---

## Diagnostics

### `abctl tunnel doctor`

```
Strategy:        auto

Network Extension
  Extension:     installed, running
  Tunnel:        active (utun13)
  IPC:           /var/run/arcbox/tunnel.sock → connected
  Routes:        172.16.0.0/12 ✓, 10.88.0.0/16 ✓

Active Session
  Backend:       network-extension
  Session ID:    a1b2c3d4
  Bidirectional: ✅
```

### `abctl tunnel doctor --probe` (with helper)

```
Probing backends...

  network-extension:
    Result: OK (full duplex)

  helper (dev-only):
    create_utun: utun14 created ✓
    add_route: OK ✓
    ⚠ Return path: utun write() is output-only on macOS.
      This backend cannot deliver return packets to host TCP stack.
      Use NetworkExtension for transparent L3 connectivity.
    Result: INBOUND-ONLY
```

---

## Implementation steps

### Step 1: NetworkExtension System Extension (arcbox-desktop)

Create Xcode target, implement `PacketTunnelProvider` with:
- `startTunnel`: set routes/DNS via NE API, bind rendezvous socket
- DGRAM pair fd exchange
- `packetFlow.readPackets()` → send to daemon
- recv from daemon → `packetFlow.writePackets()`

**Acceptance**: Extension installed, daemon connects, packets flow bidirectionally.

### Step 2: Daemon NetworkExtensionBackend

Implement `NetworkExtensionBackend` in `l3_tunnel.rs`:
- Connect to `/var/run/arcbox/tunnel.sock`
- Hello handshake
- Receive DGRAM fd
- read_packet/write_packet on DGRAM fd

**Acceptance**: `curl http://172.17.0.2/` returns nginx welcome page
without `-p` port mapping.

### Step 3: Doctor + strategy

`abctl tunnel doctor` with status and `--probe` modes.
`TunnelStrategy` enum in daemon config.

**Acceptance**: `abctl tunnel doctor` reports full status.

### Step 4: Helper downgrade

Update helper docs and daemon to not use helper as production backend.
Helper retained for `doctor --probe` only.
