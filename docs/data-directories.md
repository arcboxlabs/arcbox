# Data Directories

This document describes every filesystem path that ArcBox reads, writes, or
manages at runtime. Paths are grouped by location and annotated with the
component responsible for creating them.

**Components**: daemon (`arcbox-daemon`), cli (`abctl`), helper (`arcbox-helper`),
desktop (`ArcBox.app`), agent (`arcbox-agent`, runs inside the guest VM),
launchd (macOS system).

---

## 1. `~/.arcbox/` — User Data Root

Default data directory. Resolved by `dirs::home_dir().join(".arcbox")`;
falls back to `/var/lib/arcbox` when `HOME` is unset. Overridable via
`--data-dir` on the daemon and CLI.

**Defined in**: `app/arcbox-daemon/src/startup.rs` (`resolve_data_dir`),
`app/arcbox-core/src/config.rs`.

### 1.1 `run/` — Runtime State

Created by daemon on startup. Cleaned on next launch if stale.

| Path | Purpose | Creator |
|------|---------|---------|
| `run/daemon.pid` | Daemon PID file (stale-process detection) | daemon |
| `run/docker.sock` | Docker-compatible API Unix socket | daemon |
| `run/arcbox.sock` | gRPC API Unix socket | daemon |

### 1.2 `log/` — Logs

Daemon and helper use `arcbox-logging` with size-based rotation (10 MB per
file, 5 rotated files max) and JSON format. Guest logs (agent, containerd,
dockerd) are plain text without rotation.

| Path | Format | Rotation | Creator |
|------|--------|----------|---------|
| `log/daemon.log` | JSON | size-based (10 MB × 5) | daemon (tracing-appender) |
| `log/agent.log` | text | none | agent (via VirtioFS from guest) |
| `log/containerd.log` | text | none | agent (via VirtioFS from guest) |
| `log/dockerd.log` | text | none | agent (via VirtioFS from guest) |

Rotated daemon files: `daemon.log.1`, `daemon.log.2`, etc.

Use `abctl logs` to view logs (supports `--follow` and `--component`).

### 1.3 `data/` — Persistent Data

Defined in `app/arcbox-core/src/config.rs`.

| Path | Purpose | Creator |
|------|---------|---------|
| `data/images/` | Image storage | daemon |
| `data/containers/` | Container metadata | daemon |
| `data/machines/` | Linux machines: `<name>/config.toml` and `<name>/data.img` (the btrfs data disk; a clone's is a copy-on-write clone of its source's). `.export-*` / `.import-*` are staging directories of an export or import in progress, swept at daemon start | daemon |
| `data/volumes/` | Named volumes | daemon |
| `data/docker.img` | Docker persistent disk image (Btrfs) | daemon |
| `data/docker-meta.img` | Docker metadata disk image (ext4, fsync-hot boltdb state); paired with `docker.img` — back up or move the two together | daemon |
| `data/docker.storage.json` | Paired image identities, filesystem UUIDs, and durable initialization/migration state | daemon and guest agent |

Runtime storage is a set of two images and one manifest. The Rosetta VM uses `docker-rosetta.img`, `docker-rosetta-meta.img`, and `docker-rosetta.storage.json`. Stop the VM before preserving the complete set. A copy has new host file identities; recovery must verify its filesystem UUIDs and explicitly rebind those identities before normal boot. Do not delete a manifest to bypass a failed identity check.

Normal startup never reformats an existing image or recreates a missing member of a recorded pair. New images receive an exclusive provisioning identity; the guest consumes that authority durably before formatting. An unreadable signature, damaged filesystem, or interrupted format enters recovery instead of starting an empty Docker state. Ordinary boot does not run `e2fsck -y`.

Every System VM start and reboot requires a staged `bin/arcbox-agent` with the storage recovery capability. The host checks the binary before starting the VM because an older agent can mount storage before the protocol handshake. All agent sessions require protocol version 7 or later; version 6 is incompatible, including for observations.

An upgrade from an older Btrfs-only installation requires readable original metadata databases and no retired `.pre-ext4` sources before the new metadata volume can be formatted. Empty mountpoint stubs are ambiguous and require recovery. The migration retains its existing copy, sync, and retire sequence; a recorded migration resumes without treating retired data as a new installation. See [the storage contract](../common/arcbox-storage/README.md).

`storage-recovery/` belongs to the daemon's data directory and is removed with that directory on uninstall. Each operation retains both copy-on-write disk snapshots, the original manifest bytes, and check reports in `storage-recovery/<operation-id>/`. Preservation requires APFS `clonefile` on macOS or a filesystem that supports `FICLONE` on Linux. The daemon does not fall back to a dense copy of a sparse disk.

`abctl disk check` stops workloads, preserves the pair, and checks the unmounted copies with `btrfs check --readonly` and `e2fsck -fn`. The System VM remains stopped. `abctl disk recover` performs the same checks and restarts only after they pass. Recovery then verifies file creation, file and directory `fsync`, read-back, and removal on both original volumes. A local Docker image import and container create/start/write/sync/read/wait/remove cycle must also pass before recovery reports success. Recovery never runs `btrfs check --repair`, reformats a disk, or deletes a preserved pair.

`storage-recovery/hold` prevents automatic and direct System VM starts while an offline check or an unverified recovery needs attention. Do not delete this file to bypass recovery. `storage-recovery/status.json` records the latest operation and outcome. A client disconnect does not cancel the daemon-owned operation; clients recover its outcome through `WatchSetupStatus.storage_recovery`. If the daemon restarts with a nonterminal record, the daemon restores the boot hold and reports an interrupted, unverified result. The control plane remains available for another explicit recovery attempt.

### 1.4 `boot/` — Boot Asset Cache

See also [boot-assets.md](boot-assets.md).

| Path | Purpose | Creator |
|------|---------|---------|
| `boot/{version}/manifest.json` | Asset manifest with SHA256 checksums | daemon (download / bundle seed) |
| `boot/{version}/kernel` | Linux kernel binary | daemon (download / bundle seed) |
| `boot/{version}/rootfs.erofs` | Read-only EROFS root filesystem | daemon (download / bundle seed) |

Assets are version-keyed. The daemon downloads them on first launch if not
already cached. When running inside the Desktop app bundle, they are seeded
from `Contents/Resources/assets/{version}/` to avoid a network round-trip.

### 1.5 `runtime/` — Runtime Binaries

| Path | Purpose | Creator |
|------|---------|---------|
| `runtime/bin/` | Host Docker CLI tools | daemon (bundle seed) |
| `runtime/{version}/bin/` | Guest dockerd, containerd, runc, etc. | daemon (bundle seed) |
| `runtime/{version}/kernel/` | Sandbox guest kernel | daemon (bundle seed) |

Seeded from `Contents/Resources/runtime/` when running inside the app bundle.
Each guest generation is exposed via VirtioFS at
`/arcbox/runtime/{version}/` as a transport source. The agent verifies each
manifest entry and materializes it onto the guest Btrfs data disk before
execution.

### 1.6 `bin/` — User Executables

| Path | Purpose | Creator |
|------|---------|---------|
| `bin/abctl` | CLI symlink | cli (`abctl setup install`) |
| `bin/arcbox-daemon` | Fallback daemon binary path | cli |
| `bin/arcbox-agent` | Guest agent binary | daemon (bundle seed / boot cache) |
| `bin/vm-agent` | Sandbox microVM init binary (guest sees `/arcbox/bin/vm-agent`) | daemon (bundle seed / boot cache) |

### 1.7 `shell/` — Shell Integration

Generated by `abctl setup install`.

| Path | Purpose |
|------|---------|
| `shell/init.zsh` | Zsh init script (sourced from `~/.zprofile`) |
| `shell/init.bash` | Bash init script (sourced from `~/.bash_profile`) |
| `shell/init.fish` | Fish init script (sourced from `~/.config/fish/config.fish`) |

### 1.8 `completions/` — Shell Completions

| Path | Purpose |
|------|---------|
| `completions/zsh/_abctl` | Zsh completion for abctl |
| `completions/zsh/_docker` | Zsh completion for docker |
| `completions/bash/abctl` | Bash completion for abctl |
| `completions/bash/docker` | Bash completion for docker |
| `completions/fish/abctl.fish` | Fish completion for abctl |
| `completions/fish/docker.fish` | Fish completion for docker |

---

### 1.9 `config/` — Host-Rendered Guest Configuration

Written by the daemon before every System VM boot, read by the guest agent at
init through the `arcbox` VirtioFS share (`/arcbox/config/`).

| Path | Purpose | Creator |
|------|---------|---------|
| `config/docker-engine.json` | The operator's `dockerd` `daemon.json` overrides from `[docker]` in `config.toml` (`registry_mirrors`, `insecure_registries`, `[docker.engine]`). Absent when the section is empty. The guest merges it over the keys ArcBox manages. | daemon |

### 1.10 `tls/` — Local CA for Container Domains

Generated once by the daemon before the System VM boots, kept across
restarts; the guest agent reads it through the share (`/arcbox/tls/`) to sign
a certificate per `https://<name>.arcbox.local` name. Trust model:
`common/arcbox-local-ca/src/lib.rs`.

| Path | Purpose | Creator |
|------|---------|---------|
| `tls/ca.pem` | CA certificate, name-constrained to `arcbox.local`; what `abctl tls trust` adds to the login keychain | daemon |
| `tls/ca-key.pem` | CA private key (mode 0600) | daemon |

Deleting `tls/` rotates the CA on the next daemon start; trust the new one again.

### 1.11 `ssh/` — SSH Server

Created by the daemon (0700) the first time its SSH server starts
(`ssh <machine>@arcbox`, `app/arcbox-ssh`). Keys are generated once and kept.

| Path | Purpose | Creator |
|------|---------|---------|
| `ssh/ssh_host_ed25519_key` | Server host key (0600) | daemon |
| `ssh/id_ed25519` | The one client key the server accepts (0600) | daemon |
| `ssh/config` | OpenSSH client config: `Host arcbox` (`arcbox-dev` for the development profile) with the bound port and that key; rewritten on every start, removed when the server cannot bind | daemon |
| `ssh/known_hosts` | Pins the host key for the `config` above (`HostKeyAlias`) | daemon |

---

## 2. Configuration Files

| Path | Purpose | Creator |
|------|---------|---------|
| `~/.config/arcbox/config.toml` | User configuration (`$XDG_CONFIG_HOME/arcbox/config.toml` when set). Read last, so it wins. | user (manual) |
| `~/Library/Application Support/arcbox/config.toml` | macOS-only compatibility location (`dirs::config_dir()`); read before the XDG path. | user (manual) |
| `/etc/arcbox/config.toml` | System-wide configuration | admin (manual) |

Defined in `app/arcbox-core/src/config.rs` (`user_config_paths`,
`system_config_path`). Precedence, lowest first: built-in defaults, the
system file, the user files above, then `ARCBOX_*` environment variables.

---

## 3. Privileged Paths (Require Root)

Defined in `common/arcbox-constants/src/paths.rs`, `privileged` module.
See also [helper.md](helper.md) for versioning, threat model, and ownership rules.

| Path | Purpose | Creator |
|------|---------|---------|
| `/usr/local/libexec/arcbox-helper` | Privileged helper binary | cli (`abctl _install`) |
| `/Library/LaunchDaemons/com.arcboxlabs.desktop.helper.plist` | Helper LaunchDaemon (socket-activation) | cli (`abctl _install`) |
| `/var/run/arcbox-helper.sock` | Helper socket (created by launchd) | launchd |
| `/var/run/docker.sock` | Symlink → `~/.arcbox/run/docker.sock` | helper |
| `/etc/resolver/arcbox.local` | macOS DNS resolver file | helper |
| `/var/log/arcbox/` | Privileged helper log directory | helper |
| `/var/log/arcbox/helper.log` | Privileged helper log file (JSON, size-rotated) | helper |

---

## 4. `/usr/local/bin/` — System Symlinks

Created by the privileged helper. Removed by `abctl uninstall`.

| Path | Target | Creator |
|------|--------|---------|
| `/usr/local/bin/docker` | App bundle `Contents/MacOS/xbin/docker` | helper |
| `/usr/local/bin/docker-buildx` | App bundle xbin | helper |
| `/usr/local/bin/docker-compose` | App bundle xbin | helper |
| `/usr/local/bin/docker-credential-osxkeychain` | App bundle xbin | helper |
| `/usr/local/bin/abctl` | App bundle or `~/.arcbox/bin/abctl` | desktop / cli |
| `/usr/local/bin/arcbox-daemon` | Daemon binary from the `curl \| bash` installer | cli (`scripts/install.sh`) |

The list of Docker CLI tools is defined in
`common/arcbox-constants/src/paths.rs` (`DOCKER_CLI_TOOLS`). A link is
ArcBox's only when it points into an ArcBox bundle's `xbin/`
(`is_arcbox_owned`); OrbStack links its own CLI tools from the same layout
(`/Applications/OrbStack.app/Contents/MacOS/xbin/docker`), and those are
never replaced or removed.

The Homebrew cask links `abctl` from Homebrew's bin directory
(`/opt/homebrew/bin/abctl` on Apple Silicon) instead; that link is Homebrew's.

---

## 4.1 Other Host State

Not files under a fixed directory, but state ArcBox leaves on the Mac.

| State | Created by | Removed by |
|-------|-----------|------------|
| `/etc/hosts` line `127.0.0.1 ArcBox # managed by arcbox-helper` | helper (`hosts_alias_install`), so the `~/ArcBox/docker` mount shows `ArcBox` as its source | `abctl uninstall` |
| `~/ArcBox/`: the host mount root, a plain directory holding every guest filesystem the daemon shows the user (ADR 0003). `ARCBOX_HOST_MOUNT_DIR` moves it (test daemons keep it in their data dir). At startup the daemon force-unmounts whatever a previous daemon left under it — their servers died with it — and the pre-ADR-0003 layout (section 10) | daemon (`startup::host_mounts`) | `abctl uninstall`, when nothing but the daemon's directories is left in it |
| `~/ArcBox/docker`: read-only NFSv4 mount of the guest's Docker data; the containerd data root is the child export at `docker/containerd`, mounted by the NFS client on its own | daemon (`nfs_mount`) | daemon on shutdown; `abctl uninstall` when a daemon left it |
| `~/ArcBox/machines/<name>`: read-write NFSv3 mount of a running machine's root filesystem, served by the machine's agent on its bridge NIC; the mount point exists only while the machine runs | daemon (`machine_mount`) | daemon when the machine stops or is removed and on shutdown; `abctl uninstall` when a daemon left one |
| Login keychain: the `ArcBox Local CA` certificate and its TLS trust | user (`abctl tls trust`) | `abctl tls untrust`, `abctl uninstall` |
| `~/.kube/config`: the `arcbox` context, cluster and user; `~/.arcbox/kube/` | user (`abctl k8s enable`) | `abctl k8s disable`, `abctl uninstall` |
| Login Items entry for the daemon (BTM database) | desktop (SMAppService) | the Desktop app when it quits |

---

## 5. Docker CLI Configuration

Defined in `app/arcbox-docker/src/context.rs`.

| Path | Purpose | Creator |
|------|---------|---------|
| `~/.docker/config.json` | Docker current-context setting | cli (`abctl docker enable`) |
| `~/.docker/contexts/meta/{sha256}/meta.json` | Docker context metadata | cli |
| `~/.docker/.arcbox-previous-context` | Previous context name (for restore on disable) | cli |

### 5.1 OpenSSH Client Configuration

Opt-in: nothing touches it unless the user runs the command.

| Path | Purpose | Creator |
|------|---------|---------|
| `~/.ssh/config` | One `Include ~/.arcbox/ssh/config` line (with a marker comment) at the top, so `ssh <machine>@arcbox` works in every OpenSSH client; the rest of the file, a symlink to it, and its mode are kept | cli (`abctl ssh install`, removed by `abctl ssh uninstall`) |

---

## 6. LaunchAgent / LaunchDaemon Plists

| Path | Purpose | Creator |
|------|---------|---------|
| `/Library/LaunchDaemons/com.arcboxlabs.desktop.helper.plist` | Helper (system-level, socket-activation) | cli |
| `~/Library/LaunchAgents/com.arcboxlabs.desktop.daemon.plist` | Daemon (user-level, production) | cli (`abctl _install`) / desktop (SMAppService) |
| `~/Library/LaunchAgents/dev.arcbox.daemon.plist` | Daemon registered by the `curl \| bash` installer | cli (`scripts/install.sh`) |

Labels defined in `common/arcbox-constants/src/paths.rs`, `labels` module:
- `com.arcboxlabs.desktop.daemon`
- `com.arcboxlabs.desktop.helper`
- `dev.arcbox.daemon` (`curl | bash` installer)

The Desktop app registers its daemon through `SMAppService`, which keeps the
plist inside the bundle (`ArcBox.app/Contents/Library/LaunchAgents/`) and
records the job in the Background Task Management database (the Login Items
entry) rather than in `~/Library/LaunchAgents`. Quitting the app unregisters it.

---

## 6.1 Desktop App Per-User Files

Written by macOS on the Desktop app's behalf, keyed by its bundle identifier
(`com.arcboxlabs.desktop`; `com.arcboxlabs.desktop.dev` for the development
app).

| Path | Purpose |
|------|---------|
| `~/Library/Application Support/com.arcboxlabs.desktop/` | App state |
| `~/Library/Preferences/com.arcboxlabs.desktop.plist` | Preferences (`defaults` domain, cached by `cfprefsd`) |
| `~/Library/Caches/com.arcboxlabs.desktop/` | Caches |
| `~/Library/HTTPStorages/com.arcboxlabs.desktop/` | URL session storage |
| `~/Library/Saved Application State/com.arcboxlabs.desktop.savedState/` | Window state |
| `~/Library/Logs/arcbox/` | Daemon stdout/stderr from the `curl \| bash` installer's plist |

---

## 7. Shell Profile Injection Points

`abctl setup install` appends a source line to the user's shell profile.

| Shell | Profile | Injected Content |
|-------|---------|-----------------|
| Zsh | `~/.zprofile` | `source ~/.arcbox/shell/init.zsh` |
| Bash | `~/.bash_profile` | `source ~/.arcbox/shell/init.bash` |
| Fish | `~/.config/fish/config.fish` | `source ~/.arcbox/shell/init.fish` |

---

## 8. App Bundle Layout (Desktop)

When the daemon runs inside the Desktop app, it detects the bundle and seeds
assets to `~/.arcbox/`. Detection logic: `app/arcbox-daemon/src/startup.rs`
(`find_bundle_contents`).

```
ArcBox.app/Contents/
├── Helpers/
│   └── com.arcboxlabs.desktop.daemon     # Daemon binary
├── MacOS/
│   ├── bin/
│   │   ├── abctl                         # CLI binary
│   │   └── arcbox-helper                 # Helper binary
│   └── xbin/                             # Docker CLI tools
├── Resources/
│   ├── assets/{version}/                 # Boot assets (kernel + rootfs)
│   ├── runtime/                          # Runtime binaries (dockerd, etc.)
│   └── bin/
│       └── arcbox-agent                  # Guest agent binary
└── Info.plist
```

---

## 9. Guest VM Paths

These paths exist inside the Linux VM, not on the macOS host. Defined in
`common/arcbox-constants/src/paths.rs` and
`guest/arcbox-agent/src/config.rs`.

### 9.1 VirtioFS Mounts

| Guest Path | Host Equivalent | VirtioFS Tag | Purpose |
|------------|----------------|--------------|---------|
| `/arcbox` | `~/.arcbox/` | `arcbox` | ArcBox data sharing |
| `/arcbox/log/` | `~/.arcbox/log/` | — | Guest logs visible from host |
| `/arcbox/runtime/{version}/` | `~/.arcbox/runtime/{version}/` | — | Runtime transport source (never executed directly) |
| `/Users` | `/Users` | `users` | macOS home directory passthrough |

Tags defined in `common/arcbox-constants/src/virtiofs.rs`.

### 9.2 Guest-Internal Paths

| Path | Purpose | Creator |
|------|---------|---------|
| `/var/lib/docker` | dockerd data (Btrfs `@docker` subvolume) | agent |
| `/var/lib/containerd` | containerd data (Btrfs `@containerd` subvolume) | agent |
| `/var/run/docker.sock` | dockerd API socket | agent (dockerd) |
| `/run/containerd/containerd.sock` | containerd gRPC socket | agent (containerd) |
| `/run/arcbox/data` | Btrfs temporary mount point | agent |
| `/run/arcbox/data/runtime/{generation}` | Verified runtime generation persisted on Btrfs | agent |
| `/run/arcbox/runtime` | Stable symlink to the active Btrfs runtime generation | agent |
| `/run/arcbox/vmm.sock` | Guest VMM gRPC socket | agent |
| `/var/lib/arcbox/sandbox` | Persistent Btrfs `@sandboxes` subvolume | agent |
| `/var/lib/arcbox/sandbox/sandboxes` | Firecracker runtime files and crash-cleanup journals | agent |
| `/var/lib/arcbox/sandbox/sandbox-records` | Durable Sandbox lifecycle records | agent |
| `/var/lib/arcbox/sandbox/snapshots` | Sandbox checkpoint catalog and data | agent |
| `/var/lib/arcbox/sandbox/cow` | Sandbox dm-snapshot CoW files | agent |
| `/var/lib/arcbox/sandbox/template-catalog` | Template catalog metadata, one JSON per template name (artifacts live in the rootfs cache / snapshot catalog) | agent |
| `/var/lib/arcbox/sandbox/rootfs.ext4` | Default sandbox rootfs (busybox + vm-agent, auto-built) | agent |
| `/var/lib/arcbox/sandbox/rootfs-<layer>-<agent>.ext4` | Converted image rootfs cache, keyed on the source layer and the injected `vm-agent`; superseded entries are swept on the next conversion unless a snapshot still needs them as its dm-snapshot origin | agent |
| `/var/jail` | Firecracker jailer chroots, on a dev-allowing tmpfs. Short on purpose: the jail's sockets carry the sandbox id in their absolute path, and every byte of the base is a byte AF_UNIX leaves the id | agent |
| `/run/arcbox/runtime/bin/{firecracker,jailer}` | Firecracker binaries in the active Btrfs runtime generation | agent |
| `/run/arcbox/runtime/kernel/vmlinux` | Sandbox guest kernel in the active Btrfs runtime generation | agent |
| `/arcbox/bin/vm-agent` | Sandbox init binary, staged next to `arcbox-agent` (via VirtioFS) | host daemon |
| `/etc/arcbox/vmm.toml` | Optional guest VMM config override (not shipped; built-in defaults apply) | admin (manual) |

Sandbox lifecycle metadata survives an `arcbox-agent` restart. Live
Firecracker processes are not re-adopted yet: startup first tears down their
runtime resources, then exposes the affected Sandbox as `Failed`. Create and
Restore share the same durable lifecycle and request-replay model; TTL timers
remain process-local in this first persistence phase. Restore requires jailer
isolation; direct mode is rejected because Firecracker snapshots embed shared
origin paths that cannot safely support concurrent clones.

Create and Restore retries are durable only when the caller supplies `id`.
An empty ID asks the agent to generate a fresh UUID and is intentionally not
retry-idempotent; clients that may retry must generate and retain the ID before
the first call. Remove is intentionally different in this phase: a request
deletes whichever generation owns the ID when it executes, because the public
Remove API does not yet carry an expected generation or idempotency key.

An in-place agent upgrade that still has legacy
`/var/lib/arcbox/sandboxes/*` runtime directories is rejected until the guest restarts,
so the new persistent namespace cannot collide with an old live runtime.

---

## 10. Legacy Paths

These paths are no longer created by current versions but may exist from
older installations. They can be safely deleted.

| Path | Purpose | Status |
|------|---------|--------|
| `/tmp/arcbox-daemon.stdout.log` | Daemon stdout (old Desktop plist) | Legacy — daemon now writes `~/.arcbox/log/daemon.log` |
| `/tmp/arcbox-daemon.stderr.log` | Daemon stderr (old Desktop plist) | Legacy |
| `~/.arcbox/log/daemon.stdout.log` | Old CLI `daemon start` stdout | Legacy |
| `~/.arcbox/log/daemon.stderr.log` | Old CLI `daemon start` stderr | Legacy |
| `~/.arcbox/log/daemon.err` | Old `arcbox install` plist stderr | Legacy |
| `~/ArcBox` as an NFS mount | The guest's Docker data, mounted at the root itself before ADR 0003 (2026-10-04) | Legacy — the daemon force-unmounts it at startup and mounts at `~/ArcBox/docker` |
| `~/ArcBoxMachines/<name>` | Machine root mounts before ADR 0003 | Legacy — the daemon unmounts them and removes the empty directory at startup; `abctl uninstall` does the same |

---

## 11. Uninstall

`abctl uninstall` removes everything above that is ArcBox's. Run it as
yourself, not under `sudo`: the Docker context, shell profile, kubeconfig and
keychain trust are the user's, and the command asks for `sudo` itself, once,
for the privileged paths. It prints what it found before asking to continue,
then reports each step as `done`, `skipped (why)` or `FAILED: why`, and exits
non-zero when any step failed. `--keep-data` keeps `~/.arcbox/data`
(containers, images, volumes, machines); `--yes` skips the prompt.

```bash
abctl uninstall                 # everything
abctl uninstall --keep-data     # keep ~/.arcbox/data for a reinstall
brew uninstall --cask arcbox    # when the app came from Homebrew
```

With Homebrew, run `abctl uninstall` first and `brew uninstall --cask arcbox`
second: the cask's `abctl` link points into the app, and the command leaves
the app bundle to Homebrew (removing it first would make the cask uninstall
fail on the missing app). `brew uninstall --zap` is then redundant.

What the command does, in order:

1. Quits the Desktop app (its termination handler unregisters the daemon's
   Login Items entry), then stops the daemon: through `launchctl bootout` when
   launchd manages it, otherwise through the PID in `~/.arcbox/run/daemon.lock`.
   The daemon stops its own System VM; no other process is killed by name.
2. Unregisters the helper LaunchDaemon, then unmounts whatever a daemon
   left under `~/ArcBox` — the Docker export at `docker/`, machine roots
   under `machines/`, or the export an older daemon mounted at `~/ArcBox`
   itself — and under `~/ArcBoxMachines`, removing the directories that
   are then empty. The daemon is already stopped, so the unmounts are
   forced; a mount of another shape or a directory with your files stays.
3. Removes the Docker context (restoring the previous current context), the
   shell integration (section 1.6, 1.7, 1.8, 7, and the Docker CLI plugin
   registration in section 5), the kubectl integration, the `~/.ssh/config`
   Include, and trust in the local CA.
4. Removes every path in sections 3, 4, 4.1, 6 and 6.1 that is ArcBox's, then
   `~/.arcbox` (or all of it but `data/`), then `/Applications/ArcBox.app`
   unless Homebrew installed it.

Ownership is checked before anything privileged is removed, with the rules
the helper applies when it creates them: a `/usr/local/bin` link must point
into an ArcBox bundle, `/var/run/docker.sock` into `~/.arcbox`, and
`/etc/resolver/arcbox.local` must carry the ArcBox marker. Another tool's
files are left alone and never listed.

Not removed: `~/.config/arcbox/config.toml` and `/etc/arcbox/config.toml`
(section 2), which the user wrote.

The list in this document is the contract behind the command
(`app/arcbox-cli/src/commands/uninstall/inventory.rs`). A new path ArcBox
writes goes into both.

---

## Component Responsibility Matrix

| Component | Paths Managed |
|-----------|--------------|
| **daemon** | `~/.arcbox/{run,log,data,boot,runtime,ssh}/`, sockets, PID, boot asset download, bundle seeding, `~/ArcBox/` and the mounts under it |
| **cli** (`abctl`) | `~/.arcbox/{bin,shell,completions}/`, Docker context, LaunchAgent registration, shell profile injection, the opt-in `~/.ssh/config` Include |
| **helper** (root) | `/etc/resolver/`, `/usr/local/bin/` symlinks, `/var/run/docker.sock` symlink |
| **desktop** | SMAppService LaunchAgent registration, `~/.arcbox/run/` directory creation, helper install trigger |
| **agent** (guest) | `/arcbox/` transport mount, `/run/arcbox/data/runtime/`, `/var/lib/{docker,containerd}/`, guest sockets, Btrfs subvolumes |
| **launchd** | `/var/run/arcbox-helper.sock` (socket-activation), helper service lifecycle |
