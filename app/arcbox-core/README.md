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

## Storage recovery

`Runtime::recover_storage` owns recovery independently of the client connection. Recovery requires a stable System VM lifecycle and retains exclusive storage maintenance through checks and write verification. A client disconnect does not cancel recovery.

The recovery owner retains storage maintenance if the durable hold cannot be written or the recovery worker fails. A retry reuses that reservation. Protected shutdown retains the reservation until the runtime is dropped, so a missing hold cannot permit another System VM boot.

Daemon shutdown closes recovery admission, cancels dedicated guest RPCs, and joins recovery work before stopping the reserved VM. Interrupted or unverified recovery retains its durable hold. Cancellation during write verification does not confirm guest Docker probe cleanup; daemon or VM shutdown can interrupt that cleanup. Successful recovery removes protection only after write verification and Docker probe cleanup both complete.

## License

MIT OR Apache-2.0
