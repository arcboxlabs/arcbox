---
name: macos-network-debugging
description: macOS network stack debugging for ArcBox. Covers utun, vmnet, bridge, routing, pf, entitlements, VPN compatibility. Use when troubleshooting VM networking, container direct access, tunnel devices, fd passing. Trigger on: utun, vmnet, bridge100, route, pf, network debug, tunnel, macOS network.
---

# macOS Network Debugging for ArcBox

## Proven facts (from controlled experiments)

### utun write() is output-only

macOS utun `write()` sends packets through the kernel OUTPUT path (like
sending to a remote peer), NOT the INPUT path (delivering to local TCP/UDP
sockets).

**Evidence**: Wrote 100 UDP packets with correct checksums to utun fd.
Measured `netstat -s -p ip` "total packets received" delta. Result: delta
matched background noise (30 vs expected 31). Zero packets entered IP input.

**Implication**: utun cannot be used for "inject a packet and have the local
TCP stack process it." Only Apple's `NEPacketTunnelProvider.packetFlow.
writePackets()` can do this.

**tcpdump is misleading**: It shows packets on utun in both directions
because BPF captures on the output path too. Presence in tcpdump does NOT
mean the packet was delivered to the local stack.

### utun creation requires root

`socket(PF_SYSTEM) + connect()` for utun carries `CTL_FLAG_PRIVILEGED`.
Returns `EPERM` for non-root on all macOS versions. Apple engineer Quinn
confirmed this. The `tun.rs` comment "no root required" is wrong for
modern macOS.

### vmnet bridge provides true L2 bidirectional connectivity

`VZNATNetworkDeviceAttachment` creates `bridge100` (or similar) on the
host. Packets traverse real L2 Ethernet — kernel routing, ARP, and
TCP stack work normally in both directions. No special tunnel or injection
needed.

## Diagnostic commands

### Interfaces and bridges

```bash
ifconfig bridge100                     # vmnet bridge
ifconfig -a | grep "^utun"             # all utun interfaces
ifconfig utunN                         # specific utun
```

### Routing

```bash
route -n get 172.17.0.2                # where does this IP route?
netstat -rn | grep -E "172.16|10.88"   # container subnet routes
netstat -rn | grep bridge              # bridge routes
```

### ARP and DHCP

```bash
arp -a -i bridge100                    # who's on the bridge?
cat /var/db/dhcpd_leases               # vmnet DHCP assignments
```

### Firewall

```bash
sudo pfctl -s info                     # pf status (Enabled/Disabled)
sudo pfctl -s rules                    # active pf rules
/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate  # app firewall
```

### Kernel network stats

```bash
netstat -s -p ip | head -5             # IP input counters
netstat -s -p tcp | grep -i drop       # TCP drops
netstat -s -p udp                      # UDP stats
sysctl net.inet.ip.forwarding          # forwarding enabled?
sysctl net.inet.ip.check_interface     # interface check (1=strict)
```

### Process and socket inspection

```bash
lsof -i :5553                          # who holds DNS port
lsof -i :8080                          # who holds forwarded port
lsof -p PID | grep -i "utun\|system"   # check if process holds utun fd
```

## Entitlements

### Required for arcbox-daemon

```xml
com.apple.security.virtualization           — VZ framework
com.apple.security.hypervisor              — low-level VM control
com.apple.security.network.client          — outbound network
com.apple.security.network.server          — inbound network (listeners)
com.apple.security.cs.allow-unsigned-executable-memory  — JIT in containers
```

**File**: `bundle/arcbox.entitlements` (NOT `bundle/arcbox.entitlements`)

### Verify entitlements

```bash
codesign -d --entitlements :- target/debug/arcbox-daemon 2>/dev/null | \
  grep -o "com.apple.security\.[a-z.]*" | sort
```

### Sign correctly

```bash
codesign --force --options runtime \
  --entitlements bundle/arcbox.entitlements \
  -s - target/debug/arcbox-daemon
```

## VPN compatibility

### Surge Enhanced Mode

- Uses utun4 with 198.18.0.0/15
- Our routes (172.16.0.0/12 via bridge100 gateway) don't conflict
- Both use different subnets and different routing mechanisms
- Safe to coexist

### Tailscale

- Uses 100.x.x.x (CGNAT range)
- No overlap with container subnets (172.16/12, 10.88/16)

### Corporate VPNs

- May use 172.16.0.0/12 split-tunnel routes
- This WILL conflict with container routing
- Workaround: configure Docker to use non-overlapping subnets

## Privileged helper (arcbox-helper)

### Socket

- Production: `/var/run/arcbox-helper.sock` (launchd socket activation)
- Development: `/tmp/arcbox-helper.sock` (via `make run-helper`)
- Override: `ARCBOX_HELPER_SOCKET` env var (both server and client)

### Protocol

tarpc (bincode serialization over Unix STREAM socket). Not JSON — cannot be
tested with raw socket tools.

### RPC methods (tarpc service)

| Method | Description |
|--------|-------------|
| `route_add(subnet, iface)` | `/sbin/route -n add -net <subnet> -interface <iface>` |
| `route_remove(subnet)` | `/sbin/route -n delete -net <subnet>` |
| `dns_install(domain, port)` | Write `/etc/resolver/<domain>` |
| `dns_uninstall(domain)` | Remove `/etc/resolver/<domain>` |
| `dns_status(domain)` | Check if resolver file exists |
| `socket_link(target)` | Create `/var/run/docker.sock` → target symlink |
| `socket_unlink()` | Remove `/var/run/docker.sock` symlink |
| `cli_link(name, target)` | Create `/usr/local/bin/<name>` → target symlink |
| `cli_unlink(name)` | Remove `/usr/local/bin/<name>` if ArcBox-owned |
| `version()` | Return helper version string |

### Dev workflow

```bash
# Terminal 1: run helper in manual mode (no launchd)
make run-helper

# Terminal 2: daemon auto-connects via /tmp/arcbox-helper.sock
make run-daemon

# Or install into launchd for production-like testing
make install-helper   # build + copy + launchctl bootstrap
make reload-helper    # rebuild + bootout + copy + re-bootstrap
```

### Auth

- Debug builds: peer auth skipped (`cfg!(debug_assertions)`)
- Release builds: verifies peer code signature via Security.framework
  (Team ID 422ACSY6Y5, allowed identifiers: daemon, cli, desktop)

## fd passing on macOS

### sendfd crate (recommended)

```rust
use sendfd::{SendWithFd, RecvWithFd};
stream.send_with_fd(&[0u8], &[fd])?;
stream.recv_with_fd(&mut buf, &mut fds)?;
```

### STREAM vs DGRAM for fd passing

On STREAM sockets, data and ancillary (fds) may arrive independently
(sendfd docs warn about this). Use DGRAM for atomic fd delivery:

```
1. Create socketpair(AF_UNIX, SOCK_DGRAM) → [a, b]
2. Send 'b' via SCM_RIGHTS on STREAM connection
3. Send target fd via SCM_RIGHTS on 'a' (DGRAM — atomic)
4. Receiver gets 'b', then reads target fd from 'b'
```

## IP address ranges in ArcBox

| Range | Owner | Interface |
|-------|-------|-----------|
| 192.168.64.0/24 | socketpair datapath | eth0 (guest) |
| 192.168.65.0/24 | vmnet bridge | eth1 (guest) / bridge100 (host) |
| 172.16.0.0/12 | Docker containers | docker0 (guest) |
| 10.88.0.0/16 | Sandbox TAP | vmtap* (guest) |
| 240.0.0.1 | utun (deprecated) | utunN (host) |
| 198.18.0.0/15 | Surge VPN | utun4 (host) |
