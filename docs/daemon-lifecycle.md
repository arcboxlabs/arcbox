# Daemon Lifecycle

## Startup Sequence

Startup is a typed pipeline of eight ordered steps
(`app/arcbox-daemon/src/main.rs::start` → `startup/pipeline.rs`). Each step
consumes the previous step's context type, so skipping or reordering a step
is a compile error.

```
prepare_host             Create directories, resolve config      ~instant
    │
acquire_daemon_lease     flock(daemon.lock), terminate stale     ~instant or ≤30 s
    │                    daemon
    │
start_control_plane      Bind arcbox.sock, SystemService up;     ~instant
    │                    bind the DNS socket. Desktop can
    │                    connect from this point on, and a taken
    │                    explicit --dns-port fails here, before
    │                    any VM boots.
    │
release_stale_resources  Wait for disk-image holders to release  0–10 s
    │                    Reported as CLEANING_UP via gRPC.
    │                    Then release the mounts a previous daemon
    │                    left under ~/ArcBox (forced; the servers
    │                    died with it) and create its directories.
    │
prepare_assets           Seed/download boot assets               variable
    │                    Reported as DOWNLOADING_ASSETS →
    │                    ASSETS_READY.
    │
boot_runtime             Construct Runtime, boot the System VM   variable
    │                    Reported as VM_STARTING → VM_READY,
    │                    published from inside Runtime::init so
    │                    the span covers the guest boot alone.
    │
start_runtime_services   Serve DNS, Docker API, critical         ~instant
    │                    recovery. Then optional production
    │                    Docker context; reported as
    │                    NETWORK_READY.
    │
mark_ready               SetupPhase::Ready
```

If any step fails, the daemon publishes `SetupPhase::Failed` with the
error text in `SetupStatus.error`, waits ~200 ms so connected
`WatchSetupStatus` streams flush the final event, and exits non-zero.
Clients should treat FAILED (or stream EOF plus daemon exit) as
startup failure.

A paired storage validation failure enters protected startup after the boot assets and guest binaries are staged. The storage boundary writes `storage-recovery/hold`, and the runtime records `SetupStatus.storage_recovery` with phase `FAILED`, the original cause, and `storage_protected = true`. The daemon continues binding its host services and publishes `READY` with `vm_running = false`, so Desktop and `abctl disk check` / `abctl disk recover` can reach the recovery API. Protection blocks ordinary System VM starts, Docker mutations and interactive attach, mutating Connect RPCs, and new SSH execution or TCP channels to the System VM. Existing interactive sessions reject further input when storage protection starts. Diagnostic RPCs and other machines remain available. Missing image members remain missing; recovery must report the failure without recreating them. A VM, network, or asset failure without a storage hold still follows the fatal startup path above.

If Docker runtime startup fails after the agent becomes ready, the host queries a fresh `RuntimeStatus` from that agent. An explicit `READ_ONLY` or `UNAVAILABLE` state for a configured data or metadata volume also enters protected startup. The host writes the hold, preserves the original error and the complete status response in `storage-recovery/<operation>/startup-runtime-status.txt`, and stops the VM before continuing host services. These observations establish that required storage was unavailable for writes; they do not identify filesystem corruption or authorize repair. A failed probe, unknown observation, or read-write observation preserves the ordinary fatal startup behavior.

When protected startup has no System VM record, recovery rebuilds that record from verified cached boot assets and the existing validated disk pair. Recovery does not download assets, update the asset cache, provision images, or clear the hold during this step. Missing assets or an invalid pair keep recovery blocked. An existing record remains available for offline checks even when its pair is damaged.

### Phase enum caveats

- The observable progression is `INITIALIZING → [CLEANING_UP →
  INITIALIZING] → [DOWNLOADING_ASSETS] → ASSETS_READY → [VM_STARTING →
  VM_READY] → NETWORK_READY → READY` (or `FAILED`). Bracketed phases are
  conditional; the VM pair is skipped entirely by a `--no-linux-vm`
  daemon, which boots no guest. `DEGRADED` is reserved and never emitted.
- A phase the daemon had already left before a client subscribed is never
  observed. Treat a missing phase as unknown, not as zero elapsed and not
  as a reason to keep waiting — the stream never replays it. Once
  subscribed, though, no transition is dropped: `WatchSetupStatus` streams
  every update rather than the newest snapshot, because `NETWORK_READY` and
  `READY` are published ~300 µs apart with no await point between them and
  a snapshot channel would collapse them (see `SetupState` in
  `arcbox-api/src/system.rs`).
- `NETWORK_READY` covers whichever host services this daemon promises. A
  `--no-linux-vm` daemon reaches it with DNS alone. DNS, Docker, and an
  explicitly requested Kubernetes proxy or SSH server (`--kubernetes-port`,
  `--ssh-port`) are *bound* by then; a bind failure becomes `FAILED`. The
  canonical best-effort ports 16443 and 16022 remain the exception: a
  conflict leaves Kubernetes RPCs, or `ssh <machine>@arcbox`, unavailable
  without failing startup. The SSH server runs with or without the Linux VM:
  machines are VMs of their own.
- A non-default container CIDR is reconciled before runtime services start;
  an existing route owned by another interface fails startup. The canonical
  production CIDR keeps its existing background recovery behavior.
- `--docker-integration` is production-only. Its global Docker context switch
  runs after startup-critical recovery, so a resolver failure cannot leave a
  dead ArcBox context selected.
- Enum ordinal ≠ progression order (`DOWNLOADING_ASSETS = 8` occurs before
  `READY = 6`). Clients must match on the value, never compare ordinals.
  New phases are appended with the next free number regardless of where
  they sit in the progression — values are additive-only.
- Phases mark the transitions; the boolean `SetupStatus` fields
  (`vm_running`, `route_installed`, `dns_resolver_installed`, …) carry the
  state that outlives startup, including work `recovery::run` spawns in
  the background and anything `route_status_loop` reconciles afterwards.
  A client wanting "is the route up *now*" reads the flag, not a phase.
  `vm_running` and `route_installed` both track the VM across restarts —
  `services::vm_running_loop` mirrors `VmLifecycleState::is_ready`, and
  `route_status_loop` mirrors the route events — so both fall on a
  lifecycle-managed stop and rise again on the next boot rather than
  reporting one cold-start observation forever. Neither is a liveness probe:
  a guest that dies without the lifecycle noticing leaves `vm_running` true,
  because crash detection is unimplemented (ABX-414).

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
  ├─ cancel CancellationToken         → all services begin draining
  ├─ drain(DNS, Docker, gRPC)         → 5 s timeout, then abort
  │   Docker and the control plane close idle keep-alive connections at
  │   once and finish in-flight requests with `Connection: close`; only
  │   a request still running can use the budget. A client that parks
  │   a pooled connection does not hold the daemon.
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
  ├─ SetupState.set_failed("startup interrupted…")  → WatchSetupStatus clients see the cause
  ├─ cancel CancellationToken                       → gRPC (if started) drains
  ├─ early_runtime empty?  → exit (nothing to tear down)
  ├─ runtime.shutdown()    → bounded to 10 s: an in-flight boot parks
  │                          graceful Stop behind itself, so an unbounded
  │                          wait could last the whole boot timeout
  └─ on timeout / second signal → runtime.shutdown_force()  → VM killed,
                                  no orphaned XPC helpers holding disk images
```

`early_runtime` is filled right after `Runtime` construction, before the
VM boots, so it covers every window in which a VM can exist.

The daemon lease survives the abort: the lock is shared into the
pre-pipeline handles when acquired, so cancelling the startup future
does not release the flock — a concurrent daemon cannot take the lease
while this process is still tearing down its VM. The flock releases at
process exit, as in every other path.

Startup interruption follows the same shutdown error contract: route cleanup completes before the daemon reports a stop error or task panic. A forced stop retains direct process exit and uses status 1 when shutdown failed.

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
| `arcbox.sock` | `services::start_grpc` | `remove_file` before `UnixListener::bind` |
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
