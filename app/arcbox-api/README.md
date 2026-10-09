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

`SystemService.ListWebServices` and `WatchWebServices` become available when Runtime is constructed. Core checks each guest independently, so a running machine's web services do not wait for System VM startup. `MachineService.List`, `Inspect`, and `Events` also use this early handle. List and Inspect expose loaded machine metadata; saved addresses do not prove current reachability. Subscribe to Events before listing, and list again after a `resync` event. Events has no initial snapshot. Other runtime RPCs retain their existing readiness and storage-recovery gates.

## Usage

Connect callers can pass a `tokio_util::sync::CancellationToken` in `RequestContext` extensions. Icon lookup and macOS image resolution return `Unavailable` when that token is cancelled. Direct library calls without a token retain their existing behavior. Cancellation must not discard native workers or pending file writes.

```rust
use arcbox_api::{MachineServiceImpl, SharedRuntime};

let operational: SharedRuntime = Default::default();
let early: SharedRuntime = Default::default();
let _machine_service = MachineServiceImpl::new(operational.clone(), early.clone());
// Publish early after Runtime construction and operational after initialization.
```

## License

MIT OR Apache-2.0
