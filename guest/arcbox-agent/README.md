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

When running as PID 1, the agent also performs basic system initialisation
(mount filesystems, set hostname, spawn a child reaper).

## Runtime Bootstrap Role

At startup, the agent detects and launches the bundled runtime stack
(`containerd` / `dockerd` / `runc`) so the host-side Docker API proxy can
target a healthy guest `dockerd` endpoint.

The proxy itself listens on vsock port 2375 and relays each connection to
`/var/run/docker.sock`. The vsock leg is framed with
`arcbox_transport::vsock::HalfCloseStream` (agent protocol v4): a
zero-length frame is one side's EOF, which is how a `docker run -i`'s stdin
EOF reaches the container when the vsock fd itself cannot half-close.

## Published Ports

Host-side, a published port is a userspace listener on the Mac that relays
into the guest at its uplink address. dockerd's own DNAT rule for a binding
pinned to a specific host address (`-p 127.0.0.1:8080:80`) carries
`-d 127.0.0.1` and would never match that relayed traffic, so the agent
watches Docker container events and mirrors every such binding with a
PREROUTING rule matching the uplink interface instead (`publish_mirror.rs`).
The rules are tagged `arcbox-publish:<container id>`, removed when the
container dies, and swept at agent startup.

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

## Cross-Compilation

```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add aarch64-unknown-linux-musl
cargo build -p arcbox-agent --target aarch64-unknown-linux-musl --release
```

## License

MIT OR Apache-2.0
