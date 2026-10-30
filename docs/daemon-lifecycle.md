# Daemon Lifecycle

## Startup Sequence

The host starts through eight typed stages in `app/arcbox-daemon/src/main.rs::start`:

```
prepare_host             Resolve profile, directories, and socket paths
acquire_daemon_lease      Hold daemon.lock before binding endpoints
start_control_plane      Serve setup RPCs and bind the DNS socket
release_stale_resources  Release previous image holders and host mounts
prepare_assets           Seed coherent bundle and cached agent files, without downloads
boot_runtime             Construct Runtime and await shared CA private-key removal
start_runtime_services   Serve DNS and SSH; publish the host resolver and start TLS signing
mark_ready               Admit Machine operations and publish host READY
```

`mark_ready` publishes Machine admission and host `READY` before starting the retained optional System VM task. System asset downloads, guest boot, Docker readiness, sandbox replay, and container networking run independently of the Machine host. Docker and Kubernetes endpoints bind before System admission opens. A failed System startup preserves the Runtime and guest for diagnostics, reports its cause, and leaves Machine management and HTTPS available. Restart the daemon after fixing an ordinary System startup failure. A successful storage recovery retries System initialization in the same daemon.

A host-stage failure publishes `SetupStatus.phase = FAILED`, includes the cause in `error`, allows 200 ms for connected status streams, and exits. Host readiness requires removal of the legacy shared `tls/ca-key.pem`; System VM disablement and storage protection do not bypass this security barrier. Local CA preparation and login-session authorization remain independent of both host and System readiness.

### Independent readiness

- `SetupStatus.phase` describes host startup: `INITIALIZING → [CLEANING_UP → INITIALIZING] → ASSETS_READY → NETWORK_READY → READY`, or `FAILED`. `DOWNLOADING_ASSETS`, `VM_STARTING`, `VM_READY`, and `DEGRADED` are not emitted by this host sequence.
- `SetupStatus.system_runtime.state` describes the optional System VM: `DISABLED`, or `STARTING → VM_STARTING → VM_READY → READY`, with `FAILED` and a reason on error. `STARTING` includes asset preparation. The `VM_STARTING → VM_READY` interval measures guest boot through agent readiness; the container runtime may still be starting after `VM_READY`.
- Machine controls require host `READY`. Docker, Kubernetes, and sandbox controls require System `READY`. A host-only daemon reaches host `READY` with System `DISABLED`.
- `WatchSetupStatus` delivers every update in order. A late subscriber receives the current snapshot, not historical transitions. Missing timestamps remain unknown; enum ordinal values do not define progression order.
- `NETWORK_READY` promises the bound DNS listener and an explicitly requested SSH listener. The default SSH port remains best-effort. An explicitly requested Kubernetes port is required only by System startup; a conflict fails System readiness without stopping Machines.
- A non-default container CIDR lease and route belong to System startup. A route ownership conflict fails that System startup. Production retains best-effort route recovery. Host resolver publication is independent of container recovery.
- `vm_running` mirrors `VmLifecycleState::is_ready`, which means the lifecycle actor considers the guest agent ready. It does not establish Docker readiness or provide a fresh liveness probe. `route_installed` and resolver flags describe their own current integrations.
- `early_runtime` exposes diagnostics and read-only directories after Runtime construction. `host_runtime` admits Machine and macOS guest operations only after host preparation and listener startup. `shared_runtime` admits System services only after System initialization and endpoint binding. System settings and storage recovery use host admission so failures remain repairable.
- The System startup task waits for the existing System operation reservation and holds the reservation through initialization and readiness publication. Shutdown cancels the lock wait. A concurrent configuration operation delays startup without publishing a failure. Storage recovery and System configuration changes cannot race readiness publication. Generic Machine mutations reject the reserved System name `default`.

### Storage protection and recovery

A storage validation failure retains a durable hold, reports `storage_recovery` with the original cause, and leaves host readiness available. Ordinary System VM starts and protected writes remain rejected. Diagnostic RPCs and other machines remain available. Missing image members remain missing; recovery must report the failure without recreating them.

If Docker runtime startup fails after the agent becomes ready, the host queries a fresh `RuntimeStatus`. An explicit `READ_ONLY` or `UNAVAILABLE` state for a required volume preserves the status and original error before stopping the System VM. These observations establish unavailable writes, not filesystem corruption or authorization to repair.

When protected startup has no System VM record, recovery rebuilds that record from verified cached assets and the existing validated disk pair. Recovery does not download assets, provision images, or clear protection during this step. A missing cache or invalid pair keeps recovery blocked.

Recovery persists its terminal result before releasing ownership. Live completion is published only after the maintenance reservation and operation guards are released. `COMPLETE` with `storage_protected = false` from a new recovery operation permits retained System startup to retry. Startup records the preceding recovery operation ID while holding the System reservation, so a delayed completion from that operation cannot retry a later startup failure. A check-only completion that preserves protection does not permit retry.

### Shutdown during System startup

All shutdown paths close RPC admission, interrupt operational guest sockets, cancel service work, and join retained System startup before joining the services that startup can create. Runtime ownership survives a canceled join. Normal shutdown reaches the Runtime through `early_runtime`, including when System admission never opened. Initialization retains local file and native work through completion; existing bounded boot or asset waits can delay this join. The shutdown grace and second-signal VM stop begin after retained work drains.

### Why gRPC starts before resource cleanup

The desktop app polls the daemon's gRPC `WatchSetupStatus` stream with a
30 s timeout. If gRPC were started after stale-daemon cleanup (which can
block for up to 40 s), the desktop would time out. Moving gRPC earlier
lets clients observe the full phase progression in real time.

## Daemon Lock (`daemon.lock`)

Exclusive ownership is managed by a POSIX advisory lock (`flock(2)`) on
`~/.arcbox/run/daemon.lock`. The lock file also stores the current PID
for diagnostics.

### Properties

- **Kernel-managed**: released automatically on process exit, crash, or
  SIGKILL. No stale-lock scenarios are possible.
- **Reentrant-safe**: the file is never deleted. New daemons reuse it.
- **Non-blocking probe**: `flock(LOCK_EX | LOCK_NB)` tests whether
  another daemon is alive without polling.

### Acquisition flow

```
open(daemon.lock, O_CREAT | O_RDWR)
    │
flock(LOCK_EX | LOCK_NB)
    ├─ success → no stale daemon, proceed
    └─ EWOULDBLOCK → lock held
        │
        read PID from file
        │
        is_arcbox_daemon(pid)?
        ├─ yes → SIGTERM, wait up to 30 s, SIGKILL fallback
        └─ no  → log warning, wait for lock release
        │
        flock(LOCK_EX)   ← blocks until holder exits
        │
write current PID
```

## Graceful Shutdown (SIGTERM / Ctrl+C)

```
signal received
  ├─ close RPC admission              → client input fails with Unavailable
  ├─ unmount owned NFS exports        → guest servers remain available during unmount
  ├─ close agent connection scope     → pending guest I/O is interrupted
  ├─ cancel CancellationToken         → all services begin draining
  ├─ join resolver publication        → Bonjour connection and records are released
  ├─ join DNS                        → running upstream queries finish; listener closes
  ├─ drain(Docker, gRPC)              → 5 s timeout, then abort
  │   Docker and the control plane close idle keep-alive connections at
  │   once and finish in-flight requests with `Connection: close`; only
  │   a request still running can use the budget. A client that parks
  │   a pooled connection does not hold the daemon.
  ├─ join admitted RPC handlers       → shared barrier survives a cancelled cleanup waiter
  ├─ runtime.shutdown()
  │   ├─ stop port forwarders
  │   ├─ vm_lifecycle.shutdown()       → graceful VM stop; bridge routes expire
  │   ├─ stop remaining machines
  │   └─ network_manager.stop()
  ├─ DockerContextManager.disable()   → remove Docker CLI integration
  ├─ cleanup_files()                  → delete docker.sock, arcbox.sock
  │                                     daemon.lock kept (flock auto-releases)
  └─ process exits
```

The daemon completes host cleanup before reporting a runtime shutdown error. A failed graceful stop returns the error to `main`; a failed forced stop exits with status 1. A shutdown task panic also remains a failure, even if the subsequent forced stop succeeds. Forced stops still use `process::exit` after cleanup because an abandoned graceful task can block Tokio runtime destruction.

The DNS service owns its UDP listener and retains blocking upstream queries in a `JoinSet`. Cancellation stops admission and replies, cancels queued work, then joins running queries. A listener error or query panic follows the same cleanup path before returning an error. A running query can wait up to two seconds for each configured upstream. DNS uses a shared `BackgroundTask` outside the five-second server drain. Normal shutdown, interrupted startup, and failed startup join that task before stopping Runtime. Cancelling a cleanup waiter preserves the service in the shared slot so another waiter can finish the join. DNS task failures are logged before the eventual join.

All three exit paths close RPC admission and the operational agent socket scope before joining services. Normal exit first unmounts owned NFS exports. Each handler context carries a shutdown token. Remote preparation cancels HTTP requests or kills and reaps its Docker child; machine downloads drain pending file writes before removing staging. The shared RPC barrier retains admitted handlers through client disconnects and deadlines, then joins them before Runtime stops. Client and bidirectional input streams receive one `Unavailable` error followed by EOF; shutdown cannot turn a partial upload into successful input completion. Socket closure interrupts both async and blocking guest I/O, including split transports. The final VM shutdown connection remains outside this scope. The handler barrier does not own response-stream producers or detached native workers; their owners must retain and join those tasks separately.

Cleanup continues to await accepted native operations. The force-shutdown prompt appears after service and RPC cleanup, when runtime shutdown starts listening for another signal.

### Residual state after graceful exit

| File | State | Next startup |
|------|-------|-------------|
| `daemon.lock` | exists, old PID, **lock released** | `try_flock` succeeds instantly |
| `docker.sock` | deleted | — |
| `arcbox.sock` | deleted | — |
| disk images (`docker.img` + `docker-meta.img`, Rosetta counterparts) | exist, no holders | — |
| VM | gracefully stopped | — |

No manual intervention needed.

## Signal During Startup

The signal watcher is armed before the startup pipeline runs (`main::run`
selects the pipeline against `wait_for_signal`), so SIGTERM / Ctrl+C
arriving mid-startup triggers an orderly abort instead of the default
kill-and-orphan:

```
signal received during startup
  ├─ close RPC admission                            → reject new handlers
  ├─ SetupState.set_failed("startup interrupted…")  → WatchSetupStatus clients see the cause
  ├─ close agent connection scope                    → interrupt pending guest I/O
  ├─ cancel CancellationToken                       → gRPC (if started) drains
  ├─ join resolver publication                       → retain cleanup across a cancelled join
  ├─ join admitted RPC handlers                       → finish before stopping Runtime
  ├─ early_runtime empty?  → exit (nothing to tear down)
  ├─ runtime.shutdown()    → bounded to 10 s: an in-flight boot parks
  │                          graceful Stop behind itself, so an unbounded
  │                          wait could last the whole boot timeout
  └─ on timeout / second signal → runtime.shutdown_force()  → VM killed,
                                  no orphaned XPC helpers holding disk images
```

`early_runtime` is filled right after `Runtime` construction, before the
VM boots, so it covers every window in which a VM can exist. Machine directory
List, Inspect, and Events use this handle. Operations retain the operational
Runtime gate and existing storage-recovery checks.

The daemon lease survives the abort: the lock is shared into the
pre-pipeline handles when acquired, so cancelling the startup future
does not release the flock — a concurrent daemon cannot take the lease
while this process is still tearing down its VM. The flock releases at
process exit, as in every other path.

Startup interruption follows the same shutdown error contract: route cleanup completes before the daemon reports a stop error or task panic. A forced stop retains direct process exit and uses status 1 when shutdown failed.

The resolver service uses `BackgroundTask`, shared through the pre-pipeline handles and daemon context. The shared slot retains the publication task until its join completes, including when the first waiter is cancelled. Normal shutdown, interrupted startup, and failed startup all join the same task before stopping Runtime. The resolver task owns the helper resolver lease, Bonjour publication, and guest authority delivery.

The guest trust service also retains its `BackgroundTask` through all three shutdown paths and joins any active connection or certificate delivery before Runtime stops.

The TLS signer also uses a shared `BackgroundTask`. All three shutdown paths join that task before stopping Runtime. Cancellation closes socket waits and waits for an active native connection or signature operation to finish. Each signing connection targets one captured physical VM; replacement rejects signing and certificate delivery. System and machine workers run independently of System VM autostart. Lifecycle changes cancel parked sessions, and periodic rescans recover missed events. Each machine retains its host hold through native cleanup; a replacement worker starts only after the previous worker joins.

The HTTPS access observer starts in `init_runtime` beside the machine web producer, before `Runtime::init`, including when System VM autostart is disabled. System and running distro machine observers operate independently. The machine supervisor rescans each second and retains cancelled workers until cleanup finishes. All three shutdown paths join the shared `BackgroundTask` before stopping Runtime. Each poll keeps its 750 ms cancellation budget and drains native connection and RPC work before disabling the observation lease. Enable, publication after the claim check, and disable target the captured physical VM. A stopped or replaced VM rejects the request; its guest lease expires without contacting a replacement.

The web index service retains the same task ownership. Failed, interrupted, and stopped-VM lookups withdraw the index. Cancellation drains the active request while withdrawing, so a request holding the host commit lock cannot deadlock cleanup. Runtime stops only after the task finishes.

The Kubernetes LoadBalancer poller also uses a shared `BackgroundTask`. All three shutdown paths join the poller before stopping Runtime. A VM change, Kubernetes hold change, or shutdown cancels the current poll. The poller drains the connection, listing, and publication while closing listeners. A listing failure discards the connection and withdraws stale listeners before retrying.

## Crash / SIGKILL

When the daemon is killed without graceful shutdown:

- `flock` is released by the kernel (fd closed).
- Socket files are **not** cleaned up.
- VM is **not** gracefully stopped.
- Container subnet route is **not** removed.
- The disk images (`docker.img`, `docker-meta.img`, Rosetta counterparts) may still be held by Virtualization.framework XPC helpers.

### Residual state after crash

| File | State | Next startup |
|------|-------|-------------|
| `daemon.lock` | exists, old PID, **lock released** | `try_flock` succeeds instantly |
| `docker.sock` | **stale** | `DockerApiServer::bind` removes before bind |
| `arcbox.sock` | **stale** | `start_grpc` removes before bind |
| disk images | **possibly held by XPC helpers** | `wait_for_resources` waits up to 10 s |
| `~/ArcBox/docker`, `~/ArcBox/machines/<name>` | **dead NFS mounts** (their servers died with the daemon) | `startup::host_mounts::prepare` force-unmounts them, and the pre-ADR-0003 layout (`~/ArcBox` itself, `~/ArcBoxMachines/<name>`) |
| VM | non-graceful termination | Virtualization.framework cleans up |
| Route | **stale** | `recovery::run()` rebuilds |

All residual state is handled automatically on next startup. No manual
intervention needed.

## Stale Daemon Takeover

When a new daemon starts while an old one is still running:

1. `acquire_lock` detects the held lock.
2. Reads the old PID from `daemon.lock`.
3. Verifies it is an arcbox-daemon process (`libproc::pidpath`).
4. Sends SIGTERM → old daemon begins graceful shutdown.
5. Waits up to 30 s for the old daemon to exit.
6. Falls back to SIGKILL if unresponsive.
7. Acquires the lock once released.
8. `start_grpc` removes any stale sockets before binding.
9. `wait_for_resources` waits for disk-image holders to release, then
   `host_mounts::prepare` releases whatever the old daemon left mounted
   under `~/ArcBox`.

The old daemon's graceful shutdown runs its full sequence (drain, unmount,
VM stop, socket cleanup). The new daemon only needs to handle the
disk-image holdover case and a mount the old one could not release.

## Socket Lifecycle

Each server owns its socket file. Sockets are **not** cleaned up
centrally during startup — each server removes and rebinds independently:

| Socket | Owner | Cleanup |
|--------|-------|---------|
| `arcbox.sock` | `services::control_plane::start_grpc` | `remove_file` before `UnixListener::bind` |
| `docker.sock` | `DockerApiServer::bind` | `remove_file` before `UnixListener::bind` |

This avoids race conditions where a centralized cleanup could delete a
socket that another component has already bound.

## Edge Cases

### Disk images held by orphaned XPC helpers

Virtualization.framework spawns XPC helper processes that may outlive the
daemon. These processes hold the disk images (`docker.img`, `docker-meta.img`, Rosetta counterparts) open. The daemon waits up to
10 s for them to exit (`wait_for_resources`), then proceeds. If they
persist, `init_runtime` may fail because the disk image is locked.

**Manual fix**: `ps aux | grep -i virtualization` and kill the orphaned
helpers, then restart the daemon.

The daemon does **not** SIGKILL these processes automatically because
forceful termination risks corrupting the guest filesystem.

### Lock held by non-arcbox process

If `daemon.lock` is held by a process that is not an arcbox-daemon (e.g.,
a debugger or strace wrapper), `acquire_lock` logs a warning and blocks
until the lock is released. It does not send signals to non-arcbox
processes.

### PID reuse

With `flock`, PID reuse is not a concern. The lock is tied to the file
descriptor, not the PID. Even if the kernel reuses a PID for an unrelated
process, the new daemon detects that the lock is not held (because the
original holder's fd was closed on exit) and proceeds immediately.

The CLI (`abctl daemon status/stop/start`) also uses `flock` probing
(`LOCK_EX | LOCK_NB`) rather than `kill(pid, 0)` for liveness detection.
PID is only read from the lock file for SIGTERM delivery and display.

### Spawn serialization (`daemon-spawn.lock`)

The alive probe releases its flock immediately, so two racing
`abctl daemon start` invocations could both observe "not running" and
both spawn a daemon — the flock loser would then displace the winner
mid-boot via the stale-daemon takeover. To close this TOCTOU, the CLI
holds a separate `daemon-spawn.lock` (in the run directory) from the
alive check until the spawned daemon owns `daemon.lock` (bounded by a
10 s handoff timeout). A concurrent start blocks on the spawn lock and
then re-checks liveness, reporting "already running" instead of
spawning a duplicate. `daemon.lock` itself remains exclusively the
daemon's resource.

launchd (`RunAtLoad`/`KeepAlive`) does not take the spawn lock; a
launchd-vs-CLI race still resolves through the daemon-side takeover,
which is orderly now that signals are handled during startup.
