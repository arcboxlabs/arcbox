<div align="center">

# ArcBox

**Sandboxed execution engine for AI agents, containers, and virtual machines.**

**Built from scratch in Rust -- from hypervisor to CLI.**

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
[![Desktop](https://img.shields.io/github/v/release/arcboxlabs/arcbox-desktop?label=desktop&color=green)](https://github.com/arcboxlabs/arcbox-desktop/releases)
[![Discord](https://img.shields.io/discord/1234567890?logo=discord&label=discord&color=5865F2)](https://arcbox.link/discord)
[![Telegram](https://img.shields.io/badge/telegram-chat-26A5E4?logo=telegram)](https://arcbox.link/telegram)
[![Docs](https://img.shields.io/badge/docs-arcbox.dev-blueviolet?logo=gitbook)](https://arcbox.link/docs)

</div>

---

## Why ArcBox

Computer Use is the next frontier for AI -- agents that can write files, run code, browse the web, and operate a real machine. But giving an agent a full computer means giving it a full attack surface. Containers share the host kernel; a single exploit and the agent is out.

ArcBox solves this with Firecracker-style microVMs that boot their own Linux kernel in under 200ms. Each sandbox is a real computer -- real filesystem, real network, real process tree -- with VM-level isolation that containers can't provide. And when you just need Docker, ArcBox is a drop-in replacement for Docker Desktop.

## Three-Tier Runtime

| Tier | Isolation | Boot Time | Overhead | Use Case |
|------|-----------|-----------|----------|----------|
| **Container** | Namespace + chroot | Instant | ~1 MB | Standard Docker workloads |
| **Sandbox** | microVM (own kernel) | <200ms | ~10-30 MB | Untrusted code, CI/CD, AI agents |
| **Machine** | Independent VM | ~1.5s | ~200 MB | Full Linux dev environment |

```
Host
├── arcbox daemon (Docker API + gRPC)
│
├── System VM (Container + Sandbox tiers, shared kernel)
│   └── arcbox-agent
│       ├── Container Runtime ── namespace + chroot
│       └── Sandbox Runtime ─── KVM microVM (<200ms boot)
│
├── Machine VM "ubuntu-dev" (independent kernel + rootfs)
└── Machine VM "alpine-test"
```

### Sandbox — Computer Use Runtime

Give an AI agent a real computer it can't break out of.

- **<200ms cold boot** -- KVM microVM with minimal device model (virtio-MMIO only, no PCI/ACPI/BIOS)
- **<50ms warm start** -- snapshot/restore for instant sandbox cloning
- **VM-level isolation** -- each sandbox runs its own kernel; a vulnerability in one cannot escape to others
- **Real computer** -- real filesystem, real networking, real process tree -- not a simulated shell
- **Disposable** -- spin up, let the agent work, tear down; no state leaks between sessions
- **Docker-compatible** -- `docker run --runtime=sandbox untrusted-image`

### Container

Drop-in Docker engine replacement. Point your existing Docker CLI at ArcBox:

```bash
arcbox docker enable
docker run -d -p 8080:80 nginx
```

### Machine

Full Linux VMs with persistent storage, SSH access, and their own init system.

```bash
arcbox machine create dev --distro ubuntu
arcbox machine ssh dev
```

## Quick Start

```bash
# Install
curl -sSL https://install.arcbox.dev | sh

# Start the daemon
arcbox daemon start

# Enable Docker compatibility
arcbox docker enable

# Run a container
docker run -d -p 8080:80 nginx
curl http://localhost:8080
```

## What Works Today

- **Container lifecycle** -- `run`, `stop`, `rm`, `logs`, `exec`, `inspect`
- **Image management** -- pull from Docker Hub and OCI registries (ARM64)
- **Port forwarding** -- `-p 8080:80` maps host ports into containers
- **Volume mounts** -- bind mounts and named volumes
- **Networking** -- internet access, DNS resolution, inter-container DNS
- **Docker Compose** -- `docker-compose up/down` for multi-container stacks
- **Context switching** -- `arcbox docker enable/disable` to toggle with Docker Desktop
- **Machine management** -- `create/start/stop/rm/ls/inspect/exec/ssh`
- **40+ Docker API endpoints** -- Docker Engine API v1.43 compatible

## Performance

Custom VirtIO stack, zero-copy networking, purpose-built VirtioFS.

| Metric | Container | Sandbox | Machine |
|--------|-----------|---------|---------|
| Boot | Instant | <200ms cold / <50ms warm | ~1.5s |
| Memory | ~1 MB | ~10-30 MB | ~200 MB |
| File I/O (vs native) | >90% | >85% | >90% |

|  | ArcBox | E2B (Firecracker) | Docker Desktop |
|--|--------|-------------------|----------------|
| Sandbox boot | <200ms | ~150ms | N/A |
| Container boot | Instant | N/A | Instant |
| Idle memory | <150 MB | Cloud-only | 1-2 GB |

## Known Limitations

| Feature | Status |
|---------|--------|
| `docker build` | Not yet -- use `docker buildx` or pre-built images |
| Sandbox runtime (`--runtime=sandbox`) | Designed, not yet implemented |
| Machine distro management | Designed, not yet implemented |
| x86/amd64 images (Rosetta) | Not yet -- ARM64 only |
| Linux host | macOS first, Linux planned |
| GUI | CLI only -- desktop app planned |

## Requirements

- macOS 13 (Ventura) or later
- Apple Silicon (M1/M2/M3/M4) -- Intel support in progress
- Docker CLI installed (ArcBox replaces the engine, not the CLI)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, code standards,
and development setup.

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)

---

<div align="center">

**[Website](https://arcbox.dev)** · **[Docs](https://arcbox.link/docs)** · **[Discord](https://arcbox.link/discord)**

</div>
