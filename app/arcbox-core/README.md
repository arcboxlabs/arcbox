# arcbox-core

Core orchestration runtime for ArcBox.

## Overview

`arcbox-core` provides the host-side runtime that coordinates machine lifecycle,
VM readiness, guest-agent connectivity, and networking/port-forward state.

The main entry point is `Runtime`:

- `Runtime::new(config)` creates the runtime synchronously
- `runtime.init(|_| {}).await` prepares runtime state and assets
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
runtime.init(|_| {}).await?;
let cid = runtime.ensure_vm_ready().await?;
println!("default machine CID: {cid}");
```

On macOS, initialization removes the legacy shared `tls/ca-key.pem` before the runtime becomes usable, including with System VM autostart disabled. A removal failure stops initialization. Public certificates and unrelated application files remain intact.

Capture `Runtime::capture_web_guest` before collecting HTTPS observation addresses. Read the bridge through `web_guest_bridge_address` and pass the captured guest to `Arc<Runtime>::reconcile_https_access` for enable and disable requests. The call retains the expected VM through native connection and RPC cleanup. Cancel through the token and await completion before disabling the lease. Validate prompt ownership through `with_web_guest_claim`. The daemon starts independent System and distro machine observers before Runtime initialization, including when System VM autostart is disabled.

Capture `Runtime::container_network_incarnation` before querying Kubernetes Services. Pass that identity and a cancellation token to `apply_kubernetes_load_balancers`; the runtime rejects old VM snapshots and withdraws interrupted publications before returning.

`Runtime::set_machine_web_ports` saves typed HTTP and HTTPS backend policies for a user machine. Omitted fields retain their values. The System VM is excluded. This method does not apply routes.

`Arc<Runtime>::list_web_services` concurrently lists containers and the index from the ready System VM, plus services from each running distro machine. A machine hostname selects its exact guest independently of System VM readiness. Each machine response must match the captured raw machine name, canonical hostname, IPv4 address, and current claim revision. Any guest error cancels and drains sibling queries, fails the snapshot, and terminates Watch. Results have stable owner and hostname ordering.

`Arc<Runtime>::list_web_services` owns query cancellation and retains the captured VM through native connection and RPC cleanup. Each guest query has a five-second budget. A disconnected List or Watch caller cancels its query.

## Architecture

`MigrationManager::prepare_migration` accepts a cancellation token. Cancellation closes command output pipes, kills and reaps the active Docker command, and prevents later planning commands. Callers must cancel and await preparation before releasing its owner. Migration execution has a separate lifecycle.

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
