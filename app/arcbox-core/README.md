# arcbox-core

Core orchestration runtime for ArcBox.

## Overview

`arcbox-core` provides the host-side runtime that coordinates machine lifecycle,
VM readiness, guest-agent connectivity, and networking/port-forward state.

The main entry point is `Runtime`:

- `Runtime::new(config)` creates the runtime synchronously
- `runtime.init().await` prepares runtime state and assets
- `runtime.ensure_vm_ready().await` ensures the default machine is running

## Key Components

- `Runtime`: top-level orchestrator
- `MachineManager`: named machine lifecycle and metadata
- `VmLifecycleManager`: automatic start/health/recovery for default machine
- `AgentClient`: guest RPC client over vsock
- `NetworkManager`: network lifecycle and IP allocation

## Usage

```rust
use arcbox_core::{Config, Runtime};

let runtime = Runtime::new(Config::default())?;
runtime.init().await?;
let cid = runtime.ensure_vm_ready().await?;
println!("default machine CID: {cid}");
```

## Architecture

```text
arcbox-api / arcbox-cli
          |
          v
      arcbox-core::Runtime
          |
          +-- MachineManager
          +-- VmLifecycleManager
          +-- NetworkManager
          +-- AgentClient accessors
```

## Storage recovery journal

`Runtime::new` reconciles the durable recovery journal before the System VM can start. An interrupted operation becomes a protected failure, and a newer hold takes precedence over an older result. A protected result restores a missing hold. `subscribe_storage_recovery` replays the result, and `storage_writes_protected` reports the restored protection. Journal or hold errors fail runtime creation.

## License

MIT OR Apache-2.0
