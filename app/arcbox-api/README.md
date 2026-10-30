# arcbox-api

API server layer for ArcBox.

## Overview

This crate provides ArcBox gRPC service implementations over `arcbox-core`
runtime state. It currently exposes machine, sandbox, and host-side migration
service implementations.

## Features

- gRPC `MachineService` with machine lifecycle + guest-agent pass-through calls
- gRPC `MigrationService` with host-side migration planning/execution entrypoints

During storage recovery or a retained recovery hold, public System VM write requests fail with `FailedPrecondition`. Protection covers machine execution, disk compaction, sandbox mutations, template builds, snapshots, and file or stdin writes. Existing streams check each new input, signal, terminal resize, and file commit. Machine execution input continues while the client pauses output consumption and stops when the response stream closes. Read requests and stop operations remain available, but reads cannot automatically resume a paused sandbox while storage is protected. Other machines remain available.

`SystemService.ListWebServices` and `WatchWebServices` become available when Runtime is constructed. Core checks each guest independently, so a running machine's web services do not wait for System VM startup. `MachineService.List`, `Inspect`, and `Events` also use this early handle. List and Inspect expose loaded machine metadata; saved addresses do not prove current reachability. Subscribe to Events before listing, and list again after a `resync` event. Events has no initial snapshot.

Machine and macOS operations become available after host preparation. System VM configuration and storage recovery use the same host admission. Docker, Kubernetes, sandbox, migration, and System filesystem requests wait for System runtime readiness. Existing storage-recovery protection still applies to writes. `SetupStatus.phase` reports host startup; `SetupStatus.system_runtime` independently reports System state and failure reason. System failure does not close host admission or change local HTTPS status.

The machine name `default` is reserved for the daemon-managed System VM. Generic machine create, start, stop, remove, clone targets, and import targets reject that name. System VM resources, backend changes, and storage recovery use `SystemService`.

## Usage

Connect callers can pass a `tokio_util::sync::CancellationToken` in `RequestContext` extensions. Icon lookup and macOS image resolution return `Unavailable` when that token is cancelled. Direct library calls without a token retain their existing behavior. Cancellation must not discard native workers or pending file writes.

```rust
use arcbox_api::{MachineServiceImpl, SharedRuntime};

let host: SharedRuntime = Default::default();
let early: SharedRuntime = Default::default();
let _machine_service = MachineServiceImpl::new(host.clone(), early.clone());
// Publish early after Runtime construction and host after host preparation.
```

## License

MIT OR Apache-2.0
