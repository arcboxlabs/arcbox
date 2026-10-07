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

## Usage

```rust
use arcbox_api::MachineServiceImpl;
use arcbox_core::Runtime;
use std::sync::Arc;

let runtime = Arc::new(Runtime::new(Default::default())?);
let _machine_service = MachineServiceImpl::new(runtime);
```

## License

MIT OR Apache-2.0
