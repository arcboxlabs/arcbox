# arcbox-protocol

Protocol Buffer message and service definitions for ArcBox.

## Overview

This crate provides generated Rust types for `arcbox.v1` protobuf schemas,
re-exported through:

- `arcbox_protocol::v1::*` (canonical)
- compatibility modules (`arcbox_protocol::machine`, `::container`, `::agent`, etc.)
- selected crate-root re-exports for convenience

## Modules

| Module | Source proto | Description |
|--------|--------------|-------------|
| `common` | `common.proto` | Shared types (`Empty`, `Mount`, `PortBinding`, ...) |
| `machine` | `machine.proto` | Machine lifecycle + agent passthrough requests |
| `container` | `container.proto` | Container lifecycle and exec messages |
| `image` | `image.proto` | Image pull/list/inspect/remove messages |
| `agent` | `agent.proto` | Guest agent health/runtime messages |
| `api` | `api.proto` | Network/system/volume/migration API messages |

## Generated code

`build.rs` writes prost output into `src/generated/` (not `OUT_DIR`) and
runs rustfmt on it; the result is committed. After any `.proto` edit,
rebuild (`cargo build -p arcbox-protocol`) and commit the regenerated
files in the same change. A new `.proto` file must be registered in both
`arcbox-protocol/build.rs` and `arcbox-grpc/build.rs`.

## Protocol evolution

The daemon and the in-VM `arcbox-agent` are distributed separately, so a
running guest may speak an older schema than the host. Two mechanisms keep
skew safe:

- **Additive-only schemas.** CI runs `buf breaking` against `master`
  (`.github/workflows/ci.yml`): never remove or renumber fields; use
  `reserved` when retiring them.
- **Connection handshake.** The agent reports
  `AgentPingResponse.protocol_version`
  (`arcbox_constants::wire::AGENT_PROTOCOL_VERSION`). Before sending any business request, the host rejects agents below `MIN_AGENT_PROTOCOL_VERSION`. Unary RPCs, observation watches, sandbox streams, and machine exec/debug/TCP sessions share this admission rule. A compatible Ping admits only its current connection; disconnecting or reconnecting clears admission. Ping remains available for protocol negotiation and reports incompatible versions to boot probes.
  Bump the protocol version when a change alters the *meaning* of
  existing messages; purely additive, ignorable fields don't need one.

## Usage

`SetupStatus.storage_health` reports persistent storage independently of startup
phase and daemon liveness. Each observation names the Btrfs data volume and the
ext4 metadata volume. `MOUNTED_READ_WRITE` reports mount mode; it does not prove
that a write and fsync succeeded. `READ_ONLY` reports protection without
claiming the cause. Failed observations remain unknown. An absent optional
metadata volume is `NOT_CONFIGURED`.

A missing mount remains unknown while runtime initialization is pending. Once
the initialization attempt settles, a missing mount is `UNAVAILABLE`. An
observed read-only mount is reported immediately, including during startup.

An absent `storage_health` message means unknown: the guest may be stopped,
disconnected, or older than this protocol addition. Clients must not interpret
absence as writable. The daemon clears the observation when the guest changes
or the watch disconnects. Clients may retain an earlier fault as explicitly
stale diagnostic information.

The host opens `WatchStorageHealth` on the framed agent channel after the
shared handshake accepts agent protocol version 7 or newer. When observation
fails or the watch disconnects, the daemon clears the observation and retries
after five seconds while the VM remains ready. A missing storage observation
stays unknown. The guest samples mount flags every five seconds, sends changes
immediately, and sends a heartbeat every thirty seconds. The watch performs no
writes, never starts a VM, and holds no VM activity lease. Storage changes do
not change `SetupStatus.phase`.

Agent protocol version 7 adds storage observations and controlled storage checks. Every interface requires version 7 or newer; version 6 is rejected by the shared handshake. Every System VM start also checks the staged binary's storage capability before boot. Recovery uses a dedicated init entry and read-only disk attachments; an older rootfs without that entry cannot start the normal runtime in its place.

`StorageCheck` uses a dedicated framed agent connection. `OFFLINE_CHECK` is accepted only by an isolated recovery guest and performs read-only checks on unmounted volumes. `VERIFY_WRITES` is accepted only by the System VM and requires successful volume write probes plus a local Docker container lifecycle. Each `StorageCheckResult` identifies a volume; `ROLE_UNSPECIFIED` identifies the Docker lifecycle result. `passed` requires every check and cleanup to succeed. Neither action grants permission to format or repair an existing filesystem.

`StorageRecoveryProgress.storage_protected` reports whether the host still blocks storage writes. A `COMPLETE` check-only operation retains protection and leaves the System VM stopped. A completed recovery clears protection only after write verification succeeds. Clients must use `storage_protected` to gate writes after an operation completes or fails, including progress replay after reconnect.

`PrepareMigrationResponse.replacements` always contains the target replacement summary, including when empty. For a runnable prepare, the summary describes the plan saved under `plan_id`. Before `RunMigration`, clients must compare the target names with the user's confirmation. If the summary is absent, the daemon cannot provide this check. The full `plan`, including container environments, remains available only for an explicit dry run.

```rust
use arcbox_protocol::{CreateContainerRequest, PullImageRequest};

let create = CreateContainerRequest {
    name: "demo".to_string(),
    image: "alpine:latest".to_string(),
    cmd: vec!["echo".to_string(), "hello".to_string()],
    tty: false,
    stdin_open: false,
    ..Default::default()
};

let pull = PullImageRequest {
    reference: "nginx:latest".to_string(),
    ..Default::default()
};

assert_eq!(create.image, "alpine:latest");
assert_eq!(pull.reference, "nginx:latest");
```

## License

MIT OR Apache-2.0
