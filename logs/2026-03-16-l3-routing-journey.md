# L3 Direct Routing to Containers — Development Journey

> Date: 2026-03-14 ~ 2026-03-16
> Branch: `feat/l3-utun-routing`
> PR: https://github.com/arcboxlabs/arcbox/pull/68

## Goal

Allow the macOS host to directly `curl http://172.17.0.2/` to reach
containers inside the guest VM **without `-p` port mapping**. Also
enable DNS-based access via `curl http://container-name.arcbox.local/`.

## Final Solution (3 lines of explanation)

Add a second NIC (`VZNATNetworkDeviceAttachment`) to the VM. Apple's vmnet
creates `bridge100` on the host with real L2 connectivity. Route container
subnets through the guest's bridge IP.

## The Journey

### Phase 1-2: DNS infrastructure (succeeded, kept)

Built the foundation that works regardless of routing approach:

1. **`arcbox-dns` crate**: Shared DNS packet parsing for host and guest.
2. **Guest DNS server** (`dns_server.rs`): UDP server on 0.0.0.0:53 inside
   the VM. Resolves container names from a registry, forwards unknown
   queries to the gateway.
3. **Docker event listener** (`docker_events.rs`): Watches Docker lifecycle
   events, auto-registers container name → IP mappings.
4. **Shared DNS table**: `Arc<LocalHostsTable>` shared between the host-side
   `DnsService` (127.0.0.1:5553) and the VMM-side `DnsForwarder`, so
   `runtime.register_dns()` is visible to both.

### Phase 3: utun approach (failed, removed)

**Hypothesis**: Create a macOS utun device, route container subnets through
it, read packets in the daemon, inject into guest via L2 frames, and write
return packets back to utun.

**What worked**:
- utun creation via privileged helper (root) with fd passing (SCM_RIGHTS
  over DGRAM socketpair)
- Inbound path: host → utun → daemon read → RoutePacket → L2 inject → guest
- Guest received packets and replied

**What failed**: **Return path**. macOS utun `write()` does not deliver
packets to the local TCP/UDP stack.

**Key discovery** (proven by controlled experiment):
- Wrote 100 UDP packets (correct IP + UDP checksums) to utun fd
- Measured `netstat -s -p ip` "total packets received" before/after
- Delta matched background noise exactly (30 vs expected 31)
- **Zero packets from our writes entered IP input path**
- `tcpdump` shows packets because BPF captures on the output path
- macOS utun `write()` = "send to peer" (output), NOT "received from
  peer" (input)

This is a fundamental macOS kernel limitation. Apple's only supported
way to inject packets into local IP input is `NEPacketTunnelProvider.
packetFlow.writePackets()` (NetworkExtension framework).

### Phase 3.5: Privileged helper evolution

Built `arcbox-helper` (root LaunchDaemon) through several iterations:

1. **v1**: Simple Unix socket, `ifconfig` + `/sbin/route` commands
2. **v2**: Hello handshake protocol, `create_utun` with fd passing via
   `sendfd` crate over DGRAM socketpair (atomic delivery)
3. **v3**: Added `add_route_gateway` for bridge approach

**Findings about macOS utun creation**:
- `socket(PF_SYSTEM) + connect()` requires root (`CTL_FLAG_PRIVILEGED`)
- Confirmed on macOS 15.7.4 with Apple engineer Quinn's documentation
- `DarwinTun::new()` comment "no root required" was wrong for modern macOS
- NetworkExtension is Apple's recommended alternative (no root needed
  but requires entitlements + System Extension)

### Phase 4: vmnet bridge approach (succeeded, final solution)

**Insight**: Instead of fighting utun's limitations, use Apple's vmnet
framework which provides real L2 bidirectional connectivity.

**Implementation** (surprisingly simple):
1. `darwin.rs`: Add `VirtioDeviceConfig::network()` — one line to create
   a second NIC with NAT attachment
2. `init.rs`: `configure_bridge_nic()` — DHCP on eth1 without default
   route (outbound stays on eth0/socketpair)
3. `bridge_discovery.rs`: Read `/var/db/dhcpd_leases` to find guest's
   bridge IP
4. `main.rs`: Auto-discover bridge IP, install routes via helper

**Data flow**:
```
curl 172.17.0.2 → route via 192.168.65.x → bridge100 → vmnet
  → guest eth1 → ip_forward → docker0 → container
  → reply: same path reversed (real L2, kernel handles everything)
```

## Technical Lessons

### 1. macOS utun is NOT a bidirectional tunnel

On Linux, TUN devices are bidirectional — `write()` injects into the
kernel's IP input path. On macOS, utun `write()` goes through the
**output** path (as if the kernel is sending the packet). This is a
fundamental architectural difference that makes utun unsuitable for
"inject a packet and have the local TCP stack receive it."

### 2. tcpdump lies (sort of)

`tcpdump` on a utun interface shows both "outgoing" and "incoming"
packets. But "incoming" packets visible in tcpdump are actually packets
written to the utun via `write()` — they appear on the BPF capture
but never reach the IP input processing. The only reliable way to
verify is `netstat -s -p ip` counter deltas.

### 3. The simplest solution was hiding in plain sight

The vmnet bridge approach (VZNATNetworkDeviceAttachment) was actually
the first thing tried (the old `feat/dual-nic-inbound-routing` branch)
but was abandoned due to NAT IP discovery instability. The fix was
trivial: read `/var/db/dhcpd_leases` instead of parsing interface lists.

### 4. Entitlements matter

The daemon requires `com.apple.security.virtualization` + `hypervisor` +
`network.client` + `network.server` + `allow-unsigned-executable-memory`.
Signing with only `virtualization` (from `tests/resources/entitlements.plist`)
caused silent failures. Always use `bundle/arcbox.entitlements`.

### 5. Port 5553 conflicts

The desktop app's daemon (`com.arcboxlabs.desktop.daemon`) runs in
background via launchd and binds DNS on port 5553. When testing the
development daemon, it silently fails to bind and exits. Always
`pkill -f com.arcboxlabs.desktop.daemon` before testing.

## Abandoned Approaches (with reasons)

| Approach | Why abandoned |
|----------|---------------|
| utun + TunnelConnTrack | macOS utun write() is output-only |
| utun + raw socket injection | SOCK_RAW needs root, macOS limits raw→local |
| utun + BPF injection | BPF on loopback is also output path |
| utun + feth pair | Requires root, more complex than vmnet |
| NetworkExtension (NEPacketTunnelProvider) | Correct but heavy — needs System Extension, Apple signing, user approval. Deferred to future. |
| Unified Rust helper replacing Swift XPC | Over-engineering — separated concerns is better |

## Code Removed (utun approach cleanup)

- `l3_tunnel.rs` (306 lines): L3TunnelService, TunWriter, async read loop
- `tunnel_conntrack.rs` (285 lines): 5-tuple connection tracking
- `RoutePacket` variant in InboundCommand
- All tunnel return filtering in datapath_loop.rs
- `create_utun` with fd passing in helper
- `sendfd` dependency from arcbox-net

## Code Kept (final architecture)

```
New files:
  common/arcbox-dns/              — shared DNS parsing (host + guest)
  guest/.../dns_server.rs         — guest UDP DNS server
  guest/.../docker_events.rs      — Docker event → DNS registration
  virt/.../bridge_discovery.rs    — discover guest bridge IP
  app/arcbox-helper/              — privileged route helper

Modified files:
  virt/.../darwin.rs              — +1 line: second NIC
  guest/.../init.rs               — bridge NIC DHCP
  virt/.../dns.rs                 — Arc<LocalHostsTable> shared table
  app/.../main.rs                 — auto route installation
  app/.../grpc.rs                 — sandbox DNS registration
```

## Metrics

- **41 files changed**, +3,149 / -233 lines (net +2,916)
- **17 commits** on branch
- **~30 hours** of development and debugging
- **3 architectural pivots**: utun → utun+helper → vmnet bridge
- **1 fundamental macOS discovery**: utun write is output-only
