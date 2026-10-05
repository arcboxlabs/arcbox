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

## Runtime Bootstrap Role

At startup, the agent detects and launches the bundled runtime stack
(`containerd` / `dockerd` / `runc`) so the host-side Docker API proxy can
target a healthy guest `dockerd` endpoint.

In a normal System VM, `WatchReadiness` with `start_runtime_if_needed=true` starts or joins one runtime-start attempt. The watch reports a failed attempt immediately and does not retry within that request. A new `WatchReadiness` or explicit `EnsureRuntime` request may retry. Readiness requires a successful start result and a live Docker API probe.

Runtime startup stops if Btrfs capacity cannot be read or metadata entries cannot be inspected. If an ext4 metadata entry is missing while a retired `.pre-ext4` backup exists, the agent returns an error without recreating that entry. Ordinary boot does not run `e2fsck -y` after an ext4 mount failure. Preserve both `docker.img` and `docker-meta.img` before offline repair.

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

## Container Domains

`http://<container>.arcbox.local` (and `<service>.<project>.arcbox.local`)
resolves to the container's IP, and the agent makes port 80 there reach the
port the container actually serves (`domains/`). The HTTP port is, in order:
the `dev.arcbox.http-port` label (a port number, or `off`); 80 when the
container listens on it; the lowest listening port the container exposes;
the lowest listening port. 443 never counts. Listeners are read from
`/proc/<pid>/net/tcp{,6}` of the container's init process, repeatedly for two
minutes after `start` because servers bind late. The agent then DNATs port 80
of each of the container's IPv4 addresses to that port in nat PREROUTING,
tagged `arcbox-domain:<container id>`, removes the rules on `die`/`destroy`,
and sweeps a previous agent's at startup. A rule matches the destination
only, so it serves both the Mac (routed in over the bridge NIC) and sibling
containers (switched on a Docker bridge, which reaches iptables through the
kernel's built-in `br_netfilter`).

`https://` works the same way once the daemon has written its local CA to
`/arcbox/tls/` (`arcbox-local-ca`): port 443 of each container with an HTTP
port is REDIRECTed to a proxy on port 61443 of the VM's namespace, unless the
container listens on 443 itself. The proxy finds the container the client
dialled through conntrack, presents a certificate minted for the SNI name, and
relays plain HTTP/1.1 to the container's HTTP port. Port 61443 is therefore
not available to container publishes.

## Cross-Compilation

```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add aarch64-unknown-linux-musl
cargo build -p arcbox-agent --target aarch64-unknown-linux-musl --release
```

## License

MIT OR Apache-2.0
