# arcbox-agent

Guest-side agent for ArcBox VMs.

## Overview

`arcbox-agent` runs inside the Linux guest and serves host requests over vsock
(port `1024`). Its active RPC surface focuses on host/guest liveness and runtime
readiness, not full container lifecycle RPCs.

Current request surface includes:

- Ping
- System information
- Ensure guest runtime stack (`containerd`/`dockerd`/`runc`) is ready
- Runtime status
- Read-only storage health observations and explicit storage verification

Every host interface requires agent protocol version 7 or newer. The host completes protocol admission before sending any business request, including observation, sandbox, and machine session requests. A successful Ping admits only its current connection; disconnecting or reconnecting clears admission. Ping remains available to report incompatible versions during negotiation.

When running as PID 1, the agent also performs basic system initialisation
(mount filesystems, set hostname, spawn a child reaper).

## Storage Health

`RuntimeStatus` includes read-only observations of the Btrfs data volume and optional ext4 metadata volume. Each observation identifies the device, mount point, filesystem, and mount state. A read-only mount means writes are unavailable; the observation does not identify the cause or verify successful writes.

`WatchStorageHealth` sends an immediate snapshot and samples mount flags every 5 seconds. The watch sends changed snapshots after sampling and sends a heartbeat every 30 seconds until the host disconnects. Before runtime initialization completes, a missing mount remains unknown; after a completed or failed start, the missing mount is unavailable. Mount inspection errors remain unknown. An unconfigured metadata volume reports `NOT_CONFIGURED`.

## Storage Recovery

The host starts an isolated recovery guest with `init=/sbin/arcbox-storage-recovery arcbox.storage_recovery=1`. The dedicated rootfs launcher verifies the `arcbox-storage-recovery-v1` marker in the trusted agent binary before executing `arcbox-agent storage-recovery`. Older rootfs bundles lack this launcher; older agents lack its marker. Both cases stop before normal initialization. The marker declares compatibility with this recovery contract; asset integrity comes from the boot asset verification.

The `storage-recovery` command requires the recovery kernel flag. A recovery kernel flag also requires that explicit command. Unknown agent commands fail before initialization. The guest keeps both persistent data devices unmounted and starts no runtime services. Its RPC allowlist permits only ping, system information, shutdown, agent readiness, and `StorageCheck`. A readiness request that would start the runtime is rejected.

`StorageCheck.OFFLINE_CHECK` runs `btrfs check --readonly` and `e2fsck -f -n` after verifying that both block devices are unmounted. Each checker has a 30-minute deadline and bounded diagnostic output. Closing the dedicated RPC connection terminates an active offline checker. The agent never requests filesystem repair.

`StorageCheck.VERIFY_WRITES` runs only in the normal System VM after runtime startup. It writes, fsyncs, reads, and removes an owned temporary file on each configured volume. It then imports the guest's static BusyBox into an owned Docker image and runs a container with networking disabled. The container verifies writes and reads in its writable layer. The check passes only after the container exits successfully and both the container and image are removed. Docker cleanup continues within its operation deadlines if the host disconnects.

The storage manifest grants one-time formatting authority only to host-created volumes with matching identities. Missing, replaced, unreadable, or corrupted existing volumes require recovery. See [data directories](../../docs/data-directories.md) for the paired backup and initialization contract.

The privileged [`storage-recovery-linux.sh`](tests/storage-recovery-linux.sh) harness runs the ignored Linux checks on disposable loop devices. It requires the compiled Linux agent test binary and a recovery rootfs image. The [I/O fault experiment](../../docs/experiments/2026-10-05-storage-recovery-io-fault.md) records the exact scope, prerequisites, commands, and kernel evidence.

## Runtime Bootstrap Role

At startup, the agent detects and launches the bundled runtime stack
(`containerd` / `dockerd` / `runc`) so the host-side Docker API proxy can
target a healthy guest `dockerd` endpoint.

In a normal System VM, `WatchReadiness` with `start_runtime_if_needed=true` starts or joins one runtime-start attempt. The watch reports a failed attempt immediately and does not retry within that request. A new `WatchReadiness` or explicit `EnsureRuntime` request may retry. Readiness requires a successful start result and a live Docker API probe.

Runtime startup stops if Btrfs capacity cannot be read or metadata entries cannot be inspected. If an ext4 metadata entry is missing while a retired `.pre-ext4` backup exists, the agent returns an error without recreating that entry. Ordinary boot does not run `e2fsck -y` after an ext4 mount failure. Preserve both `docker.img` and `docker-meta.img` before offline repair.

An older formatted pair without a manifest requires metadata migration verification. Existing ext4 entries remain authoritative. Missing entries require explicit recovery because old empty entries left no retired marker; a populated source can be stale state from a later Btrfs-only boot. The guest preserves both volumes and records `Paired` only after all mappings are prepared.

The proxy itself listens on vsock port 2375 and relays each connection to
`/var/run/docker.sock`. The vsock leg is framed with
`arcbox_transport::vsock::HalfCloseStream` (agent protocol v5): a
zero-length frame is one side's EOF, which is how a `docker run -i`'s stdin
EOF reaches the container when the vsock fd itself cannot half-close, and a
frame with the top header bit set grants the peer window, which is how a
paused `docker attach` backs up into dockerd instead of leaving the vsock
unread and stalling the VM.

## Storage Write Probe

`arcbox_agent::storage_probe::verify_writes(directory)` checks file creation, write, file and directory sync, read-back, and removal in an existing directory. The caller must verify the intended runtime mount before calling the probe. The probe creates one unique file with exclusive creation, removes only that file, and syncs the directory after removal. Write, sync, read-back, and cleanup failures propagate to the caller. The probe does not create a missing directory or establish filesystem consistency after a crash.

## Published Ports

Host-side, a published port is a userspace listener on the Mac that relays
into the guest at its uplink address. dockerd's own DNAT rule for a binding
pinned to a specific host address (`-p 127.0.0.1:8080:80`) carries
`-d 127.0.0.1` and would never match that relayed traffic, so the agent
watches Docker container events and mirrors every such binding with a
PREROUTING rule matching the uplink interface instead (`publish_mirror.rs`).
The rules are tagged `arcbox-publish:<container id>`, removed when the
container dies, and swept at agent startup.

## Distro Machines

A distro machine boots through the machine boot shim, which runs
`arcbox-agent machine-init` before the distro's own init. That one-shot step
gives the machine the identity and network the distro cannot know on its own
(`init.rs`, `machine_identity.rs`, `boot_done.rs`):

- the machine name from `arcbox.machine_name=` on the kernel command line
  becomes the hostname — the kernel nodename, `/etc/hostname`, and a
  `127.0.1.1` line in `/etc/hosts` — so every init re-applies it at boot;
- the uplink (`eth0`, ArcBox's own network stack) gets its address by DHCP
  and a default route tagged `proto 200`; the boot-done hook removes that
  route once the distro's network manager has installed its own next to it,
  so a machine ends up with exactly one default route, via the uplink;
- the bridge NIC (`eth1`, the vmnet interface the Mac reaches directly) gets
  an address and nothing else, and is declared unmanaged to systemd-networkd
  and NetworkManager by MAC so the distro never routes out of it. The agent
  reports that address as `SystemInfo.bridge_ip_address`, and the daemon
  publishes `<name>.arcbox.local` there while the machine runs.

The agent in a machine then serves RPC and nothing else: none of the System
VM services below run there.

The machine root export stores oversized Mac attributes in `.arcbox-xattrs` beside each target. Side-entry version 3 combines Linux file handles with filesystem IDs to distinguish recreated files and replaced mounts. Unsupported side-entry versions return an error and preserve the side entry and inline attributes. See the [identity investigation](../../docs/logs/2026-10-07-sidecar-inode-generation.md) for compatibility and validation.

## Container Domains

`http://<container>.arcbox.local` and `<service>.<project>.arcbox.local`
resolve to a container IPv4 address. The agent discovers HTTP with a bounded
`HEAD /` request that includes the container domain in `Host`. A valid HTTP
response is required; listening TCP ports alone do not identify web services.

DNS checks local names for every query type, including HTTPS and SVCB. Known IPv4-only names return NODATA for other types. Missing names under the configured suffix return NXDOMAIN; external names are forwarded to the gateway.

The guest reserves `host`, `host.<configured suffix>`, `host.docker.internal`, `gateway.docker.internal`, and the configured index apex before containers register. These exact names use the host's current DNS answer through the gateway even without a local address. Container aliases cannot override these reservations; descendant names remain independent.

The configured apex serves a browser index through gateway ports 80 and 443. Managed HTTPS requires a `.local` apex and the trusted ArcBox CA. The agent publishes the index only after the frontend binds and both exact gateway redirects precede Docker's PREROUTING jump. The page distinguishes unavailable Docker discovery from an empty service list. Failed publication retries without containers. Shutdown withdraws the index before joining the frontend and removing its redirects. Installed NAT and browser validation remain pending.

Machines use `<hostname>.machine.<configured suffix>` through the gateway. Sandboxes use exact `<id>.sandbox.<configured suffix>` names in the same guest Registry as container claims and instance authority. Both resource trees remain reserved after addresses disappear. Sandbox IDs must form one DNS label; creation and restore reject invalid IDs before provisioning. Sandbox DNS ownership is scoped to the injected System VM services.

Guest registration uses complete container IDs and canonical name claims. Generated names include descendant wildcards; `dev.arcbox.domains` accepts exact and explicit wildcard `.local` names. Independent owners of an exclusive name produce SERVFAIL. Compose service replicas select the smallest complete ID. Short names require an exact claim, so a container named `com` cannot capture `example.com` through a wildcard. Host registration, helper resolver leases, and host-to-guest authority delivery are implemented; installed macOS resolver and browser validation remain pending.

`ListGuestWebServices` accepts an optional concrete `hostname` to resolve wildcard claims against current ownership; an empty value lists exact aliases. It returns the installed HTTP and HTTPS routes and their
current DNS names. Each shared Compose alias appears only on the container
selected by guest DNS. The host uses this snapshot for service URLs; explicit
port labels describe configured routes and do not guarantee application health.
Container directory entries remain hidden until Docker subscription and complete reconciliation succeed. A disconnected or cancelled watcher hides cached directory entries until a new subscription replays the complete snapshot; installed forwarding routes remain available independently.

The HTTPS proxy accepts a TLS SNI name only when the selected DNS owner has an installed managed route at the container address the client dialled. The proxy pins that owner's registration revision and installed route identity. A change during authorization, certificate configuration, TLS handshake, or upstream connection rejects the request before application bytes are forwarded, including replacement at the same address and port. Identical registration and route publication preserve the handshake. A failed connection cannot remove a newer installed route.

Leaf certificates renew after three quarters of their actual remaining validity.
Concurrent requests for the same domain share one signing operation. The cache
holds up to 256 domains and evicts idle entries without interrupting TLS sessions.
The proxy reads `/arcbox/tls/ca.pem` before cache reuse and verifies new leaves
against the requested name and published root. Replacing the root renews cached
leaves. Missing or invalid roots and signing results from a replaced root fail
the handshake; the next connection can retry after publication recovers.

The host can observe managed HTTPS access through the unary `ReconcileHttpsAccess` RPC on either backend. The observer supplies exact Mac source addresses and renews a three-second lease. The guest reports the first valid ClientHello in that observation, including when the leaf is cached. A matching Mac connection waits at most 30 seconds for authorization before continuing TLS. Disabling observation or losing the lease releases waiting connections. Container clients and clients without an active observer proceed immediately. Native HTTPS bypasses observation. The proxy checks DNS ownership again after waiting.
The agent tries port 80, then exposed listeners, then other listeners in
ascending order. Port 443 is reserved for HTTPS. Set `dev.arcbox.http-port`
to a port number to override discovery, or `off` to disable managed HTTP.

Native HTTPS on 443 or 8443 takes precedence over automatic TLS termination.
Set `dev.arcbox.https-port` to another native TLS port, or `off` to disable
managed HTTPS. Native HTTPS preserves the application's certificate. Otherwise,
a selected HTTP service receives HTTPS through the guest proxy on port 61443.
Native discovery negotiates HTTP/2 or HTTP/1.1 with ALPN and requires an HTTP response. Use an explicit native port for a service that requires a client certificate.
The proxy obtains a certificate for the client's SNI through the host signing
channel, then relays HTTP/1.1. The CA private key stays in the macOS Keychain;
only the public root is shared in `/arcbox/tls/`. Port 61443 is reserved for
the proxy and cannot be published by a container.

Managed HTTP/1.1 forwarding replaces `X-Forwarded-Proto` with `https` on every request. The relay preserves Host, redirects, repeated cookies, streaming bodies, negotiated trailers, and WebSocket upgrades. The relay removes hop-by-hop headers and owns both connection drivers until the session ends. Native TLS remains passthrough; managed HTTP/2 and gRPC translation are not implemented.

Listeners are read from `/proc/<pid>/net/tcp{,6}` with startup backoff, then
on a five-second schedule while the container runs. Socket identity changes
invalidate successful protocol probes, including replacement listeners on
the same port. Failed discovery probes retry. Failed proxy upstream connects
also invalidate discovery. Closed listeners lose their managed routes.
Network connect/disconnect events refresh container addresses and discovery.
DNS and protocol probes use the same default address; disconnecting the last
addressed network removes the container's DNS ownership and routes.

The agent installs destination-only rules in nat PREROUTING for every
container IPv4 address, tagged `arcbox-domain:<container id>`. HTTP and native
HTTPS use DNAT; automatic TLS termination uses REDIRECT. The agent removes
rules on `die`/`destroy` and sweeps stale rules at startup. The rules serve
both routed Mac traffic and switched sibling traffic through the kernel's
built-in `br_netfilter`.

## Cross-Compilation

```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add aarch64-unknown-linux-musl
cargo build -p arcbox-agent --target aarch64-unknown-linux-musl --release
```

## License

MIT OR Apache-2.0
