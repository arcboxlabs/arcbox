# Plan: Privileged Network Operations

> Status: PLANNED
> Date: 2026-03-15
> Revision: 4

## Problem

L3 tunnel routing requires three privileged operations on macOS:

1. **utun creation**: `socket(PF_SYSTEM)` + `connect()` carries `CTL_FLAG_PRIVILEGED`,
   requires root on all macOS versions. (Confirmed: macOS 15.7.4 returns `EPERM`.)
2. **Interface configuration**: `ifconfig utunN inet <ip> <ip> up`
3. **Route installation**: requires root to modify the routing table.

## Decision: Dual-track with shared session abstraction

Two privilege escalation paths, selected by explicit strategy:

| Path | When | Mechanism | Root required |
|------|------|-----------|---------------|
| **NetworkExtension** | Desktop app installed | `NEPacketTunnelProvider` System Extension | No |
| **Root helper** | CLI-only (no desktop app) | `arcbox-helper` LaunchDaemon | Yes (one-time install) |

### Crates

| Need | Crate | Rationale |
|------|-------|-----------|
| fd passing | **`sendfd`** 0.4 | `SendWithFd` / `RecvWithFd`. **Used on DGRAM sockets only** — on STREAM, data and ancillary may arrive independently (see sendfd docs). |
| Route management | **`net-route`** 0.4 | Type-safe `handle.add()` / `handle.delete()`. Replaces shell `/sbin/route`. |
| utun creation | **Existing `DarwinTun`** | Already audited with SAFETY comments. `tun-rs` lacks safety annotations and pulls `nix` with no net benefit. |
| Helper protocol | **`serde_json`** + hand-written framing | 3 commands total. RPC framework is overhead. |

---

## Daemon-side architecture

### BackendFactory → TunnelSession split

The startup/orchestration logic and the data-plane session are separate types.
`L3TunnelService` never contains backend-specific `if` branches.

```rust
/// Creates a TunnelSession by negotiating with the chosen backend.
/// Handles all startup complexity: detection, handshake, utun creation,
/// route installation, cleanup registration.
trait BackendFactory: Send + Sync {
    /// Probe whether this backend is usable right now.
    /// Must do a real connect + handshake, not just path exists.
    fn probe(&self) -> bool;

    /// Start a tunnel session. On success, routes are installed and
    /// the session is ready for packet I/O.
    fn start(&self, config: &TunnelConfig) -> io::Result<Box<dyn TunnelSession>>;
}

/// Active tunnel session. Owns the utun fd (or IPC fd) and routes.
/// When dropped, cleans up routes and releases the fd.
trait TunnelSession: Send + 'static {
    /// Read one IP packet (AF header stripped if utun, raw if IPC).
    fn read_packet(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Write one IP packet (AF header prepended if utun, raw if IPC).
    fn write_packet(&self, packet: &[u8]) -> io::Result<usize>;

    /// Raw fd for tokio AsyncFd registration.
    fn as_raw_fd(&self) -> RawFd;

    /// Add a route for a subnet (e.g. dynamically discovered Docker network).
    fn add_subnet(&self, subnet: &str) -> io::Result<()>;

    /// Remove a route.
    fn remove_subnet(&self, subnet: &str) -> io::Result<()>;

    /// Session identifier (for route reconciliation and diagnostics).
    fn session_id(&self) -> &str;
}
```

### TunnelConfig

```rust
pub struct TunnelConfig {
    pub subnets: Vec<String>,       // ["172.16.0.0/12", "10.88.0.0/16"]
    pub utun_ip: Ipv4Addr,          // 240.0.0.1
    pub mtu: u16,                   // 1500
}
```

### Backend selection: strategy + probe

Not just runtime detection — an explicit strategy enum, configurable via
daemon args or config file:

```rust
pub enum TunnelStrategy {
    /// Try NE → helper → direct. Default.
    Auto,
    /// Only use NetworkExtension. Fail if unavailable.
    ForceNe,
    /// Prefer NE, fall back to helper.
    PreferNe,
    /// Only use root helper. Fail if unavailable.
    ForceHelper,
    /// Prefer helper, fall back to NE.
    PreferHelper,
    /// Direct utun creation (must be root). For development only.
    Direct,
}
```

`L3TunnelService::start()`:

```rust
pub async fn start(
    config: TunnelConfig,
    strategy: TunnelStrategy,
    cmd_tx: mpsc::Sender<InboundCommand>,
) -> io::Result<(Self, TunWriter)> {
    let factories: Vec<Box<dyn BackendFactory>> = match strategy {
        TunnelStrategy::Auto => vec![ne_factory(), helper_factory(), direct_factory()],
        TunnelStrategy::ForceNe => vec![ne_factory()],
        TunnelStrategy::PreferNe => vec![ne_factory(), helper_factory()],
        TunnelStrategy::ForceHelper => vec![helper_factory()],
        TunnelStrategy::PreferHelper => vec![helper_factory(), ne_factory()],
        TunnelStrategy::Direct => vec![direct_factory()],
    };

    for factory in &factories {
        if factory.probe() {
            match factory.start(&config) {
                Ok(session) => {
                    let writer = TunWriter { session: session.clone_arc() };
                    return Ok((Self { session, cmd_tx, ... }, writer));
                }
                Err(e) => tracing::warn!(error = %e, "backend start failed, trying next"),
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "no tunnel backend available"))
}
```

### Hello handshake

Both Path A and Path B begin with a version handshake as the first frame.
The field set is frozen at v1 — new fields go into `features`, not new
top-level keys.

**Client → server:**
```json
{
  "hello": {
    "version": 1,
    "session_id": "a1b2c3d4",
    "mtu": 1500,
    "features": []
  }
}
```

**Server → client:**
```json
{
  "hello": {
    "version": 1,
    "backend": "helper",
    "session_id": "a1b2c3d4",
    "features": []
  }
}
```

| Field | Required | Semantics |
|-------|----------|-----------|
| `version` | yes | Protocol version. **Server rejects if client version > server version** (server cannot understand future protocol). Client may downgrade if server version < client version, or reject. |
| `session_id` | yes | UUID generated by client. Server echoes it back. Used for route reconciliation. |
| `mtu` | yes | Requested MTU. Server may lower it (never raise). |
| `backend` | server only | `"helper"` or `"network-extension"`. Client uses this to confirm it hit the right backend. |
| `features` | yes | Extensibility. Empty array at v1. Future: `["ipv6", "multi-subnet-dynamic"]`. |

**Error handling:**
- Version mismatch → server sends `{"error":"version_mismatch","server_version":1}`, closes.
- Malformed hello → server sends `{"error":"bad_hello"}`, closes.
- In both cases, `probe()` returns false and the next backend is tried.

### Session-owned route reconciliation

Each session owns a `session_id` (UUID). Routes and utun interfaces are
tagged with this ID conceptually — the session tracks what it created:

```rust
struct SessionState {
    session_id: String,
    utun_name: String,
    installed_routes: Vec<String>,  // subnets
}

impl Drop for SessionState {
    fn drop(&mut self) {
        // Remove only routes installed by THIS session.
        for subnet in &self.installed_routes {
            let _ = remove_route(subnet, &self.utun_name);
        }
        // utun fd close happens via OwnedFd Drop — interface disappears.
    }
}
```

**Orphan route cleanup ownership**: The **daemon** is solely responsible for
reconciliation, not the helper or extension. Rationale: the daemon is the
only component that knows the expected state (which subnets should be
routed). The helper/extension only execute individual add/remove commands.

On daemon startup, before creating a new session:
1. `net-route` lists all routes whose interface matches `utun*`
2. For each, check if the utun interface still exists (`if_nametoindex`)
3. If the interface is gone → orphaned route → delete it
4. Then proceed to create a new session normally

This runs exactly once per daemon start. The helper and extension never
do reconciliation — they are stateless command executors.

---

## Path A: NetworkExtension

### Components

**System Extension** (`ArcBoxNetworkExtension`, Swift, in arcbox-desktop):

```
ArcBox Desktop.app/
└── Contents/
    └── Library/
        └── SystemExtensions/
            └── com.arcboxlabs.desktop.network-extension.systemextension/
```

**Bundle ID**: `com.arcboxlabs.desktop.network-extension`

**Entitlements** (extension):
- `com.apple.developer.networking.networkextension` = `["packet-tunnel-provider-systemextension"]`
- `com.apple.security.application-groups` = `["group.com.arcboxlabs.desktop"]`

**Entitlements** (main app):
- `com.apple.developer.system-extension.install`
- `com.apple.security.application-groups` = `["group.com.arcboxlabs.desktop"]`

### IPC design

**Socket location**: The extension and app share an App Group container
(`group.com.arcboxlabs.desktop`). The rendezvous socket lives inside
this container. The extension creates a discovery symlink at a well-known
path so the daemon can find it:

```
<group-container>/tunnel.sock          ← actual STREAM rendezvous socket
/var/run/arcbox/tunnel.sock            ← symlink → above (created by extension)
```

The extension runs outside the app sandbox as a System Extension, so it
can create the `/var/run/arcbox/` symlink.

**Handshake + DGRAM pair**:

```
1. Extension binds STREAM rendezvous at <group-container>/tunnel.sock
   Symlinks /var/run/arcbox/tunnel.sock → <group-container>/tunnel.sock
2. Daemon connects (STREAM) to /var/run/arcbox/tunnel.sock
3. Hello handshake on STREAM:
   → {"hello":{"version":1,"session_id":"a1b2c3","mtu":1500}}
   ← {"hello":{"version":1,"backend":"network-extension"}}
4. Extension creates socketpair(AF_UNIX, SOCK_DGRAM, 0) → [ext_end, daemon_end]
5. Extension sends daemon_end fd to daemon via sendfd on STREAM
6. Daemon receives daemon_end fd via recvmsg on STREAM
   (This is a single fd-only transfer with one marker byte — acceptable
   on STREAM because it's just the fd, not data+fd mixed.)
7. Both close the STREAM connection.
8. Packet I/O on the DGRAM pair:
   Extension: packetFlow.readPackets → send(ext_end)
   Daemon:    recv(daemon_end) → RoutePacket
   Return:    daemon send(daemon_end) → extension recv(ext_end) → writePackets
```

DGRAM socketpair guarantees message boundaries. Each packet is one
datagram — no framing needed, data and fd cannot desynchronize.

### Availability probe

`NetworkExtensionBackend::probe()`:
1. Check if `/var/run/arcbox/tunnel.sock` exists **and** is connectable
2. Send hello handshake
3. Return true only if hello response has `backend: "network-extension"`

Stale sockets from crashed extensions fail at step 1 or 2.

### Data flow

```
Host: curl 172.17.0.2
  → kernel routes to utun (NEPacketTunnelProvider owns it)
  → extension: packetFlow.readPackets()
  → extension: send(ext_end, ip_packet)      [DGRAM — atomic]
  → daemon: recv(daemon_end) in read loop
  → daemon: RoutePacket → datapath → L2 wrap → guest

Return:
  guest → socketpair → datapath → TunnelConnTrack match
  → daemon: TunWriter.write_packet() → send(daemon_end)  [DGRAM — atomic]
  → extension: recv(ext_end)
  → extension: packetFlow.writePackets()
  → kernel → utun → host TCP stack → curl
```

### Route and DNS handling

Handled entirely by `NEPacketTunnelNetworkSettings`:
- Routes installed/removed automatically when tunnel starts/stops
- DNS configured via `matchDomains` — only `*.arcbox.local` queries use our resolver
- No `/sbin/route`, no `/etc/resolver/` file writes

### Affected files (arcbox-desktop)

| File | Change |
|------|--------|
| `ArcBoxNetworkExtension/PacketTunnelProvider.swift` | New: NEPacketTunnelProvider + rendezvous + DGRAM pair |
| `ArcBoxNetworkExtension/Info.plist` | New: extension metadata |
| `ArcBoxNetworkExtension/*.entitlements` | New: NE + App Group entitlements |
| `ArcBox/ArcBoxApp.swift` | Add system extension activation |
| `Packages/.../StartupOrchestrator.swift` | Add network extension setup step |
| `ArcBox.xcodeproj` | Add System Extension target |
| `ArcBox/*.entitlements` | Add `system-extension.install` + App Group |

---

## Path B: Root Helper

Fallback for CLI-only users without the desktop app.

### Components

**Binary**: `arcbox-helper` (Rust)
**Control socket**: `/var/run/arcbox/helper.sock` (STREAM, for JSON control)
**Config**: `/etc/arcbox/helper.json` (persistent, survives reboot)
**LaunchDaemon**: `/Library/LaunchDaemons/com.arcboxlabs.helper.plist`

### Dependencies

```toml
[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
libc = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
sendfd = "0.4"
net-route = "0.4"
```

### Protocol

STREAM socket. One JSON request per connection, one JSON response.
For `create_utun`, the fd is transferred on a **separate DGRAM socketpair**,
not mixed with JSON on the STREAM.

**Standard ops (JSON only on STREAM):**

```
→ {"hello":{"version":1,"session_id":"a1b2c3","mtu":1500}}
← {"hello":{"version":1,"backend":"helper"}}

→ {"op":"add_route","subnet":"172.16.0.0/12","iface":"utun13"}
← {"ok":true}
```

**create_utun (STREAM control + DGRAM fd handoff):**

```
→ {"hello":{"version":1,"session_id":"a1b2c3","mtu":1500}}
← {"hello":{"version":1,"backend":"helper"}}

→ {"op":"create_utun","ip":"240.0.0.1"}

Helper creates utun, then:
  1. Creates socketpair(AF_UNIX, SOCK_DGRAM) → [helper_end, client_end]
  2. Sends client_end fd to daemon via sendfd on STREAM (1 marker byte + fd)
  3. Sends utun fd via sendfd on helper_end (DGRAM — atomic delivery)

Daemon:
  1. Receives client_end fd via recvmsg on STREAM
  2. Receives utun fd via recvmsg on client_end (DGRAM — atomic)
  3. Helper sends JSON response on STREAM:
     ← {"ok":true,"name":"utun13"}
  4. Daemon reads JSON, now has utun fd + name.
```

This avoids the STREAM data+fd desync issue. The DGRAM socketpair
is single-use for the fd transfer.

### Data flow

```
1. Daemon connects to /var/run/arcbox/helper.sock (STREAM)
2. Hello handshake
3. Sends: {"op":"create_utun","ip":"240.0.0.1"}
4. Helper (root): DarwinTun::new() → configure() → utun created
5. Helper: creates DGRAM socketpair, sends client_end via STREAM
6. Helper: sends utun fd on helper_end (DGRAM, atomic)
7. Daemon: receives client_end, then utun fd
8. Helper: responds {"ok":true,"name":"utun13"} on STREAM
9. Daemon sends: {"op":"add_route","subnet":"172.16.0.0/12","iface":"utun13"}
10. Helper: net_route::Handle::add(&route)
11. Daemon starts L3TunnelSession read loop on utun fd (no per-packet IPC)
```

### Input validation

- `iface`: must match `^utun\d+$`
- `subnet`: must be valid CIDR (`<ipv4>/<0-32>`)
- `ip`: must be valid IPv4

### Authorization

Socket is accessible only to the authorized user:

1. **Socket ownership**: Mode `0660`, owned by `root:<authorized_gid>`
   where `<authorized_gid>` is the primary group of the installing user.
   Helper calls `fchmod()` + `fchown()` after `bind()`.

2. **UID check**: On each connection, helper calls `getpeereid()` and
   rejects if peer UID ≠ stored authorized UID and peer UID ≠ 0.

3. **Persistent config**: `/etc/arcbox/helper.json` (root-owned 0600):
   ```json
   {"authorized_uid": 501, "authorized_gid": 20}
   ```
   Written during `sudo abctl daemon install` from `SUDO_UID`/`SUDO_GID`.
   Reloaded on `SIGHUP`.

### Single-user semantics

The helper is explicitly single-user:

- **Fast User Switching**: Only the authorized UID can connect. Other
  logged-in users get `EACCES` (socket permissions) or rejection
  (`getpeereid` mismatch).
- **SSH as another user**: Same — rejected.
- **CI runner**: If the CI runner runs as the same UID, it works. If not,
  the admin must `sudo abctl daemon install --for-user <uid>` to
  re-authorize, or `sudo abctl daemon install --for-user $(id -u ci-user)`.
- **Multiple admins**: Not supported. One authorized UID at a time.
  `sudo abctl daemon rebind-user` rewrites the config and sends SIGHUP.

### Availability probe

`HelperBackend::probe()`:
1. Connect to `/var/run/arcbox/helper.sock`
2. Send hello handshake
3. Return true only if hello response has `backend: "helper"` and
   compatible version

### LaunchDaemon plist

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.arcboxlabs.helper</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/arcbox-helper</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/arcbox-helper.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/arcbox-helper.log</string>
</dict>
</plist>
```

### Affected files (arcbox repo)

| File | Change |
|------|--------|
| `app/arcbox-helper/Cargo.toml` | New |
| `app/arcbox-helper/src/main.rs` | Socket listener, dispatch, auth, hello, SIGHUP |
| `app/arcbox-helper/src/network.rs` | `create_utun` (DGRAM fd handoff), route ops via `net-route` |
| `app/arcbox-helper/src/validate.rs` | Input validation |
| `Cargo.toml` | Add workspace member |
| `app/arcbox-cli/src/commands/daemon.rs` | `install` / `uninstall` / `rebind-user` |
| `app/arcbox-cli/src/commands/tunnel.rs` | New: `abctl tunnel doctor` |

---

## Security

| Threat | Path A mitigation | Path B mitigation |
|--------|-------------------|-------------------|
| Unauthorized tunnel creation | System Extension user approval + Apple code signing | Socket 0660 root:user-gid + getpeereid UID check |
| Route hijacking | Routes set via NE API | Input validation + `net-route` typed API |
| Stale/wrong backend | Hello handshake version + backend_kind check | Same |
| Config tampering | N/A | `/etc/arcbox/helper.json` root:wheel 0600 |
| Orphaned routes after crash | System removes routes when tunnel stops | Session-owned cleanup + startup reconcile |
| Binary tampering | Gatekeeper + notarization | `/usr/local/bin` owned by root |
| Privilege persistence | System Extension removed with app | `sudo abctl daemon uninstall` |

---

## Diagnostics

`abctl tunnel doctor` has two modes:

### `abctl tunnel doctor` (default: status)

Reads current state without sending any requests. Safe to run while
the daemon is active.

```
Strategy:        auto (from daemon config)

Helper
  Socket:        /var/run/arcbox/helper.sock (exists)
  Config:        /etc/arcbox/helper.json (present, uid=501)
  LaunchDaemon:  com.arcboxlabs.helper (running, pid 1234)

Network Extension
  Extension:     not installed

Active Session (from daemon)
  Backend:       helper
  Session ID:    a1b2c3d4
  Interface:     utun13 (UP, 240.0.0.1)
  Routes:        172.16.0.0/12 via utun13
                 10.88.0.0/16 via utun13

Orphan Routes:   none
```

### `abctl tunnel doctor --probe`

Actively probes each backend: connect + hello + (optionally) create_utun
+ verify fd. This may create and immediately destroy a utun. Run when
troubleshooting — not during normal operation.

```
Probing backends...

  network-extension:
    Socket:      /var/run/arcbox/tunnel.sock → not found
    Result:      UNAVAILABLE (no socket)

  helper:
    Socket:      /var/run/arcbox/helper.sock → connected
    Hello:       version=1, backend=helper ✓
    Auth:        uid=501 accepted ✓
    create_utun: utun14 created, fd received, ifconfig UP ✓
    add_route:   172.16.0.0/12 via utun14 ✓
    Cleanup:     utun14 destroyed, route removed
    Result:      OK

  direct:
    DarwinTun::new(): EPERM
    Result:      UNAVAILABLE (not root)

Recommendation: backend "helper" is functional.
```

---

## Implementation steps and acceptance criteria

### Step 1: Root helper binary

Create `app/arcbox-helper/` with: socket listener, hello handshake,
`create_utun` (DGRAM fd handoff via `sendfd`), `add_route`/`remove_route`
(via `net-route`), input validation, UID authorization from
`/etc/arcbox/helper.json`.

**Acceptance**:
```bash
# Start helper
sudo target/debug/arcbox-helper &

# Test hello
python3 -c "
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('/var/run/arcbox/helper.sock')
s.send(json.dumps({'hello':{'version':1,'session_id':'test','mtu':1500}}).encode() + b'\n')
print('hello:', s.recv(4096))
s.close()
"
# Must print hello response with backend: "helper"

# Test create_utun with fd verification (Rust integration test)
cargo test -p arcbox-helper -- test_create_utun_e2e --ignored
# Test creates utun via helper, receives fd via DGRAM pair,
# verifies fd is valid by calling getsockopt(UTUN_OPT_IFNAME),
# verifies interface is UP via ifconfig.
```

### Step 2: Daemon integration

Implement `BackendFactory`/`TunnelSession` traits. Wire `HelperBackend`
into `L3TunnelService` with hello handshake and DGRAM fd receive.
Add `TunnelStrategy` to daemon config/args.

**Acceptance**: Daemon started as normal user with `--tunnel-strategy helper`;
`ifconfig` shows utun UP; `netstat -rn` shows routes; daemon logs show
`session_id`.

### Step 3: E2E test

**Acceptance**:
```bash
DOCKER_HOST=unix://$HOME/.arcbox/run/docker.sock \
  docker run -d --name test-nginx nginx
curl http://$(docker inspect test-nginx \
  --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')/
# Returns nginx welcome page WITHOUT -p port mapping
```

### Step 4: CLI install + doctor

`abctl daemon install` / `uninstall` / `rebind-user`.
`abctl tunnel doctor`.

**Acceptance**: `sudo abctl daemon install` → reboot → helper auto-starts →
`abctl tunnel doctor` reports all green → daemon creates tunnel on start.

### Step 5: NetworkExtension (separate PR, arcbox-desktop repo)

Implement `NetworkExtensionBackend` + System Extension + DGRAM
socketpair rendezvous.

**Acceptance**: Uninstall root helper, rely only on desktop app. Same
`curl` test from Step 3 passes. `abctl tunnel doctor` shows
`Active: network-extension`.
