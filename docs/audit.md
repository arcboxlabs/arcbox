# Codebase Audit Report

> Date: 2026-03-29 (updated)  
> Scope: full workspace (327 Rust files, ~110K LOC)  
> Branch: `fix/daemon-shutdown-feedback`  
> Version: `v0.3.13`  
> Method: merged findings from three independent full-repo audits plus direct code verification

## Executive Summary

The repo still has strong macro-level layering: no obvious circular dependencies,
clear crate families, good use of `CancellationToken`, and a mostly coherent
`CommonError -> crate error -> transport` stack.

The main problem is no longer architecture shape. It is structural drift inside
the control plane and low-level virtualization layers:

- startup correctness still depends on call order and nullable state
- lifecycle truth is duplicated across multiple state models
- daemon, CLI, and core repeat host layout and daemon contract logic
- oversized files are no longer isolated exceptions
- two VM stacks are still alive at the same time
- CI signal is not strong enough to protect this amount of structural debt

This revision intentionally re-ranks findings by compound-interest value rather
than by local code smell.

### Scale signals

- `44` Rust files still contain `// ==== ...` style section dividers
- `46` Rust files exceed `700` lines
- `20` Rust files exceed `1000` lines
- `9` Rust files exceed `1500` lines
- `1,469` `.unwrap()` calls in non-test production code
- `649` `map_err` call sites, `197` of which use `.to_string()` erasure
- `50` literal `"lock poisoned"` error strings
- `42` `#[allow(dead_code)]` annotations
- `22` `Arc<Mutex<T>>` usages (guidelines require `RwLock` on hot paths)
- `196` of `327` source files (60%) have zero test coverage

That is not a "few large files" problem. It is a repo-wide maintenance pattern.

### Priority order

1. Tighten startup and daemon contracts
2. Establish a single lifecycle truth and split the main control-plane god objects
3. Converge the VM stack and fix `Vmm` / `VirtIO` abstraction leaks
4. Replace stringly typed error flow and weak persistence semantics
5. Upgrade CI from best-effort coverage to a reliable regression net
6. Eliminate `unwrap()` / `Arc<Mutex>` debt in hot paths and daemon long-run code
7. Remove `async-trait` crate where native async fn in traits (edition 2024) suffices

### Scorecard

| Dimension | Score | Strength | Main gap |
|-----------|-------|----------|----------|
| Top-level layering | 8 / 10 | Crate boundaries are mostly clean | Internal module boundaries are eroding |
| Async / shutdown | 7 / 10 | Consistent `CancellationToken`, `spawn_blocking` discipline | Startup/shutdown ownership spread across too many files |
| Error model | 5.5 / 10 | `CommonError` pattern is solid | 649 `map_err`, 197 `.to_string()` erasures, `VmmError` doesn't use `CommonError`, `guest` layer uses `anyhow` |
| Module structure | 4.5 / 10 | Some crates are still tidy | Large-file debt is now systemic |
| VM architecture | 5 / 10 | New native stack exists and is promising | Firecracker stack still coexists; abstractions leak |
| Runtime safety | 4 / 10 | Tests generally use proper assertions | 1,469 `unwrap()` in non-test code; daemon is long-running |
| Testing / CI | 5 / 10 | `virt/arcbox-net` (34/40), `virt/arcbox-virtio` (7/8) are strong | `arcbox-api` 0/8, `arcbox-daemon` 1/14, `arcbox-docker` 4/27, `arcbox-grpc` 0/1 |
| Concurrency model | 5.5 / 10 | `RwLock` used in some managers | 22 `Arc<Mutex>` in VirtIO backends, VmInstance, SandboxInstance |
| Developer experience | 5.5 / 10 | Makefile and docs are decent | Host layout, prerequisites, and contracts are duplicated and implicit |

---

## T1 · Startup Contract and Control-Plane Ownership <!-- ABX-263 -->

**Compound value**: this is the highest-leverage fix in the repo. Every daemon,
CLI, desktop, and runtime change passes through this boundary.

### Evidence

- `app/arcbox-daemon/src/context.rs:13-35`
- `app/arcbox-daemon/src/services.rs:25-81`
- `app/arcbox-daemon/src/startup/mod.rs:125-181`
- `app/arcbox-api/src/grpc/mod.rs:24-40`

### Problem

The daemon startup contract is still modeled as "one mutable context bag that is
partially initialized over time":

- `DaemonContext` contains `Option<DaemonLock>`
- runtime is an `Arc<OnceLock<Arc<Runtime>>>`
- `DaemonContext::runtime()` panics if called too early
- gRPC services are registered before runtime exists and return `UNAVAILABLE`
  until the `OnceLock` is filled

This works, but the correctness model is "everyone must remember the startup
sequence" instead of "the type system makes illegal states unrepresentable".

### Why this compounds

- adding one new phase or service means touching `main.rs`, `startup/mod.rs`,
  `services.rs`, and often `shutdown.rs`
- behavior is hard to unit test because phase validity is implicit
- the same startup contract must be re-learned by every caller and future maintainer

### Recommendation

Replace the current bag-of-state model with typed phases or an explicit
supervisor:

```text
EarlyContext -> LockedContext -> RuntimeReadyContext -> ServingContext
```

or

```text
DaemonSupervisor
  - owns startup phases
  - owns background task group
  - owns readiness state
  - exposes stable operations to CLI / desktop / tests
```

### First cut

1. Introduce a dedicated `daemon_contract` or `daemon_supervisor` module.
2. Remove `DaemonContext::runtime()` panic path.
3. Replace `Arc<OnceLock<Arc<Runtime>>>` with a phase-aware state holder.
4. Make gRPC startup consume a "runtime-ready" type instead of probing readiness
   from inside handlers.

---

## T2 · Lifecycle Has No Single Source of Truth <!-- ABX-264 -->

**Compound value**: lifecycle drift silently infects runtime, persistence,
recovery, and API semantics.

### Evidence

- `app/arcbox-core/src/vm_lifecycle.rs:82-120`
- `app/arcbox-core/src/vm_lifecycle.rs:430-453`
- `app/arcbox-core/src/machine.rs:18-31`
- `app/arcbox-core/src/vm.rs:64-83`
- `app/arcbox-core/src/persistence.rs:71-88`
- `app/arcbox-core/src/vm_lifecycle.rs:1067-1072`

### Problem

The repo currently maintains multiple overlapping state models:

- `VmLifecycleState`
- `MachineState`
- `VmState`
- `PersistedState`
- setup-facing readiness flags such as `SetupStatus.vm_running`

The mappings are not lossless. Example:

- `PersistedState::Running` becomes `MachineState::Stopped` on reload
- idle transition publishes `Event::MachineStopped` even though the VM is not stopped

That means state is already used for both "real machine lifecycle" and "external
UI signal" and those meanings are diverging.

### Why this compounds

- every new lifecycle transition must be threaded through 4 to 5 representations
- bug fixes in recovery or shutdown are prone to semantic drift
- transport/API layers cannot rely on state names being precise

### Recommendation

Create one authoritative lifecycle state model and make all other layers derive
views from it:

- persistence stores a projection of the authoritative model
- gRPC/CLI status expose read-only projections
- event bus emits domain-specific events instead of overloading `MachineStopped`

### Related issue: non-transactional transitions

`MachineManager::start()` updates in-memory state and persistence separately, and
persistence errors are dropped:

- `app/arcbox-core/src/machine.rs:373-418`
- `app/arcbox-core/src/persistence.rs:219-245`

This should become an explicit transition object or repository transaction, not
ad hoc dual writes.

---

## T3 · Structural Debt Program: Large Files Are Now Systemic <!-- ABX-265 -->

**Compound value**: lower review cost, fewer merge conflicts, better ownership,
safer refactors.

The previous audit understated this problem by treating it as a short list of
outliers. It is broader than that.

### Repo-wide signal

- `44` files already violate the repo's own "section divider means split" rule
- `46` files exceed `700` lines
- `20` files exceed `1000` lines

### Highest-priority split targets

| File | Lines | Why it matters |
|------|-------|----------------|
| `guest/arcbox-agent/src/agent.rs` | 2,169 | guest runtime ensure, Linux server, RPC, Docker proxy, tests all mixed |
| `virt/arcbox-net/src/darwin/tcp_bridge.rs` | 2,036 | TCP state machine + conntrack + forwarding + options |
| `virt/arcbox-fs/src/passthrough.rs` | 2,000 | file ops + handles + attrs + permissions |
| `virt/arcbox-virtio/src/net.rs` | 1,628 | backend, protocol, queue logic, Linux TAP details mixed |
| `app/arcbox-core/src/vm_lifecycle.rs` | 1,484 | lifecycle, config drift, startup, route reconcile, idle policy, shutdown |
| `virt/arcbox-hypervisor/src/linux/ffi.rs` | 1,443 | ioctl numbers, FFI structs, wrappers, safety boundary mixed |
| `virt/arcbox-fs/src/dispatcher.rs` | 1,410 | request dispatch and many operation families mixed |
| `app/arcbox-core/src/vm.rs` | 1,098 | VM registry, VMM config translation, stop/snapshot/read helpers |
| `app/arcbox-core/src/machine.rs` | 968 | registry, hydration, vsock/agent access, readiness, persistence, shutdown |

### Recommended split order

1. `app/arcbox-core/src/vm_lifecycle.rs`
2. `app/arcbox-core/src/machine.rs`
3. `guest/arcbox-agent/src/agent.rs`
4. CLI command files: `setup.rs`, `boot.rs`, `docker.rs`, `daemon.rs`
5. `virt/arcbox-hypervisor/src/linux/ffi.rs`
6. `virt/arcbox-virtio/src/net.rs`

### Suggested layouts

**`app/arcbox-core/src/vm_lifecycle/`**

```text
mod.rs
state.rs
config.rs
startup.rs
default_machine.rs
readiness.rs
idle_policy.rs
recovery.rs
shutdown.rs
serial.rs
```

**`guest/arcbox-agent/src/agent/`**

```text
mod.rs
runtime_ensure.rs
server.rs
rpc_handlers.rs
docker_proxy.rs
system_info.rs
linux_runtime.rs
tests.rs
```

**`virt/arcbox-hypervisor/src/linux/ffi/`**

```text
mod.rs
ioctl.rs
bindings.rs
system.rs
vm_fd.rs
vcpu_fd.rs
dirty_log.rs
```

---

## T4 · Host Layout and Daemon Contract Are Duplicated <!-- ABX-266 -->

**Compound value**: every installation, startup, status, and path-related change
gets cheaper and safer.

### Evidence

- `app/arcbox-cli/src/commands/daemon.rs:108-149`
- `app/arcbox-cli/src/commands/daemon.rs:221-335`
- `app/arcbox-daemon/src/startup/mod.rs:184-190`
- `app/arcbox-cli/src/commands/setup.rs:114-130`
- `app/arcbox-core/src/boot_assets.rs`

### Problem

The daemon contract is documented as load-bearing, but code still duplicates the
same rules in multiple places:

- `resolve_data_dir()` exists in both CLI and daemon
- CLI writes `daemon.lock` itself when spawning the daemon
- daemon later acquires the real flock and owns the lock for its lifetime
- socket paths, log paths, boot asset paths, and install paths are assembled in
  several command files

### Why this compounds

- any daemon lock or socket change risks silently drifting CLI behavior
- new entrypoints will copy one of the existing implementations instead of
  consuming a shared contract
- path logic is difficult to test because it is not centralized

### Recommendation

Introduce a shared host-side contract layer:

```text
host_layout/
  paths.rs
  daemon_contract.rs
  install_layout.rs
```

This layer should own:

- canonical data/run/log/bin/boot paths
- lock file and liveness probe semantics
- canonical gRPC and Docker socket paths
- daemon stop/status protocol

CLI and daemon should stop recalculating these independently.

---

## T5 · VM Strategy Must Converge <!-- ABX-267 -->

**Compound value**: this determines whether future virtualization work compounds
or fragments.

### Evidence

- `virt/arcbox-vm/Cargo.toml:1-15`
- `app/arcbox-core/Cargo.toml:12-19`
- `virt/arcbox-vmm/src/vmm/mod.rs:27-33`
- `virt/arcbox-vmm/src/vmm/mod.rs:176-220`
- `virt/arcbox-hypervisor/src/traits.rs:36-69`

### Problem A: two VM stacks still coexist

The repo is still carrying:

- `virt/arcbox-vm`: Firecracker/fc-sdk based VM stack
- `arcbox-hypervisor + arcbox-vmm + arcbox-virtio`: new native stack used by `arcbox-core`

This is the biggest architectural fork in the workspace.

### Problem B: `Vmm` is not a true abstraction boundary

`Vmm` currently uses:

- `ManagedVm = Box<dyn Any + Send + Sync>`
- `managed_execution: bool`
- Darwin-specific resource fields on the top-level struct
- `VirtualMachine::is_managed_execution()`

That means platform differences are expressed by `cfg + Any + bool`, not by
backend-specific types and capabilities.

### Recommendation

1. Pick one primary VM line and freeze the other from further expansion.
2. Treat `virt/arcbox-vm` as migration/legacy/experimental scope, not as an equal
   long-term platform.
3. Refactor `Vmm` into:

```text
Vmm
  - common lifecycle state machine
  - backend strategy object
  - explicit backend capabilities
```

Possible backend traits:

- `ExecutionBackend`
- `SnapshotBackend`
- `NetworkBackendHooks`
- `ConsoleBackend`

---

## T6 · VirtIO Boundary Is Not Closed Yet <!-- ABX-268 -->

**Compound value**: once transport/device boundaries are correct, device work,
snapshot work, and performance work all get easier.

### Evidence

- `virt/arcbox-vmm/src/device.rs:139-168`
- `virt/arcbox-vmm/src/device.rs:629-660`
- `virt/arcbox-virtio/src/queue.rs:120-166`
- `virt/arcbox-virtio/src/net.rs:161-201`
- `virt/arcbox-virtio/src/net.rs:522-543`
- `virt/arcbox-virtio/src/fs.rs:444-460`

### Problem

There are effectively two queue worlds:

- MMIO layer tracks real guest queue addresses in `VirtioMmioState`
- device layer owns in-memory `VirtQueue` objects with their own descriptor and
  ring state

That means transport and device are not yet operating on one shared queue model.

At the same time, `arcbox-virtio` still mixes in host resource management:

- Linux TAP creation and host-side network setup logic appear inside `virtio-net`

### Why this compounds

- tests can pass against simulated queues while the real MMIO path remains incomplete
- host-specific setup leaks into the device crate, making reuse and testing harder
- snapshot semantics stay fuzzy because queue/device runtime state is split

### Recommendation

1. Define one queue abstraction backed by guest memory view, not duplicate queue state.
2. Keep `arcbox-virtio` focused on protocol/device emulation.
3. Push TAP/vmnet/IP/route lifecycle into `arcbox-net`.
4. Make device crates consume injected backends such as `NetBackend`,
   `BlockBackend`, `ConsoleIo`, not shell commands or host setup directly.

---

## T7 · Error Model Is Too Stringly Typed <!-- ABX-269 -->

**Compound value**: better diagnostics, better transport mapping, safer refactors.

### Evidence

- `app/arcbox-core/src/error.rs:11-31`
- `app/arcbox-api/src/error.rs:40-56`
- `app/arcbox-api/src/grpc/machine.rs:70-118`
- `app/arcbox-api/src/grpc/machine.rs:202-232`

The repo still relies on:

- `CoreError::Vm(String)`
- `CoreError::Machine(String)`
- many `Status::internal(e.to_string())`
- many literal `"lock poisoned"` error strings

Static scan result:

- `"lock poisoned"` appears `53` times across the workspace

### Why this compounds

- transport-level behavior depends on error text, not on typed semantics
- backtraces and original source error types are lost
- error filtering in logs and tests becomes brittle

### Recommendation

1. Replace string-backed domain variants with structured enums.
2. Add `From<DomainError> for tonic::Status` and `From<DomainError> for HTTP error`.
3. Remove `.map_err(|e| Type(e.to_string()))` where a proper `From` chain is possible.
4. Keep `anyhow` only in app shells, not in reusable library layers.

### Related persistence issue

Persistence updates are currently read-modify-write per field update:

- `app/arcbox-core/src/persistence.rs:219-245`

and callers often ignore failures:

- `app/arcbox-core/src/machine.rs:405-407`

This should become a `MachineRepository` with explicit atomic update methods and
visible failures.

---

## T8 · Proto / gRPC API and Codegen Need Tightening <!-- ABX-270 -->

**Compound value**: alpha is the cheapest time to remove API drift.

### Evidence

- `rpc/arcbox-protocol/build.rs:12-54`
- `rpc/arcbox-grpc/build.rs:9-40`
- `rpc/arcbox-protocol/src/lib.rs:35-110`
- `app/arcbox-api/src/grpc/mod.rs:44+`

### Problem A: build scripts write to source tree

`arcbox-protocol/build.rs` writes generated files into `src/generated/` and even
runs `rustfmt` during build.

That creates:

- dirty working tree risk
- extra build-time tool assumptions (`protoc`, `rustfmt`)
- avoidable incremental build noise

### Problem B: duplicated API surface

`arcbox-protocol` exports:

- canonical `v1`
- `sandbox_v1`
- compatibility submodules like `common`, `machine`, `container`, `api`
- many root-level re-exports again

This is convenient short term but expands the supported API surface and makes
refactoring more expensive.

### Problem C: still-present semantic inconsistencies

Preserve and act on the original audit's valid findings:

- filter parameter mismatch across proto files
- multiple timestamp representations
- `x-machine` header is an implicit transport contract, not declared at schema level
- `AgentPingResponse` lacks explicit protocol compatibility signaling

### Recommendation

Pick one of these models and be consistent:

1. generated files are explicit artifacts checked in via `scripts/gen-proto.sh`
2. generated files live only in `OUT_DIR`

Also:

- shrink compatibility exports to one clearly documented layer
- define a single canonical public path
- document transport metadata requirements or move them into request messages

---

## T9 · CI, Testing, and DX Signal Are Not Strong Enough <!-- ABX-271 -->

**Compound value**: structural improvements only stick if CI can police them.

### Evidence

- `.github/workflows/ci.yml:47-54`
- `.github/workflows/test-vm-linux.yml:3-23`
- `.github/workflows/docker-api-e2e.yml:3-6`
- `.pre-commit-config.yaml:11-16`

### Current gaps

- main CI excludes `arcbox-agent` from clippy, build, and test
- Docker API E2E is `workflow_dispatch` only
- Linux VM workflow is gated by narrow manual path filters
- pre-commit excludes `arcbox-agent` from clippy as well

### Original audit findings still valid

- critical test coverage gaps remain in runtime port forwarding, Docker handlers,
  Docker proxy, VirtIO device manager, daemon DNS service
- shared test support is still duplicated across files
- there is still no property-based testing or fuzzing program

### Recommendation

Restructure CI into three layers:

1. **fast**
   - fmt
   - clippy
   - unit tests
   - run on every PR

2. **medium**
   - crate/domain integration tests
   - Docker API compatibility smoke
   - guest agent tests

3. **slow**
   - Linux VM / Firecracker / KVM jobs
   - heavy hypervisor and snapshot tests

Also:

- stop excluding `arcbox-agent`
- replace narrow path filters with package-aware triggers or broader dependency-closure paths
- add a shared `tests/support/` or `arcbox-test-utils` crate

---

## T10 · Runtime Safety: 1,469 `unwrap()` Calls in Production Code <!-- ABX-272 -->

**Compound value**: daemon and guest agent are long-running processes. Each
`unwrap()` is a potential panic → crash → restart cycle. Fixing these
systematically improves uptime and debuggability.

### Evidence (static scan)

| Category | Count |
|----------|-------|
| `.unwrap()` in non-test code | 1,469 |
| `.expect()` in non-test code | 115 |
| `"lock poisoned"` literal strings | 50 |

### Hottest areas

The highest concentrations are in the performance-critical virtualization layer:

| File | `.unwrap()` count | Risk |
|------|-------------------|------|
| `virt/arcbox-virtio/src/net.rs` | high | VirtIO net hot path — panic kills VM |
| `virt/arcbox-fs/src/passthrough.rs` | high | VirtioFS hot path — panic kills shared FS |
| `virt/arcbox-net/src/darwin/tcp_bridge.rs` | high | Network bridge — panic drops all connections |
| `guest/arcbox-agent/src/agent.rs` | high | Guest agent — panic requires VM reboot |
| `app/arcbox-core/src/vm_lifecycle.rs` | high | VM lifecycle — panic orphans VM |

### Why this compounds

- a single `unwrap()` in a VirtIO handler panics a tokio worker thread, which
  can cascade into the entire daemon or guest agent crashing
- `"lock poisoned"` panics are especially insidious: they indicate a prior panic
  has already occurred, and the second panic masks the root cause
- replacing them with `?` or structured errors produces better diagnostics and
  keeps the process alive

### Recommendation

1. **Immediate**: audit all `unwrap()` in `virt/` and `guest/` — replace with `?`
   where the function returns `Result`, or `expect("reason")` for truly invariant cases.
2. **Short-term**: add a clippy config to deny `unwrap_used` in non-test code for
   `virt/` and `guest/` crates.
3. **Medium-term**: extend the deny policy workspace-wide, allowing exceptions
   only with `#[allow]` + comment.

---

## T11 · Concurrency Primitives: `Arc<Mutex<T>>` on Hot Paths <!-- ABX-273 -->

**Compound value**: the AGENTS.md guideline to prefer `RwLock` on hot paths
exists for a reason — the VirtIO and network data paths are throughput-critical.

### Evidence

22 instances of `Arc<Mutex<T>>` in non-test code, including:

| Location | Type | Access pattern |
|----------|------|----------------|
| `virt/arcbox-virtio/src/net.rs:534` | `Arc<Mutex<dyn NetBackend>>` | read-heavy (packet send/recv) |
| `virt/arcbox-virtio/src/console.rs:496` | `Arc<Mutex<dyn ConsoleIo>>` | read-heavy (console output) |
| `virt/arcbox-virtio/src/vsock.rs:41` | `Arc<Mutex<dyn VsockBackend>>` | read-heavy |
| `virt/arcbox-vmm/src/device.rs:341` | `Arc<Mutex<dyn VirtioDevice>>` | mixed, but reads dominate |
| `virt/arcbox-vm/src/manager.rs:25` | `Arc<RwLock<HashMap<..., Arc<Mutex<VmInstance>>>>>` | double-lock nesting |
| `virt/arcbox-vm/src/sandbox.rs:312` | `Arc<RwLock<HashMap<..., Arc<Mutex<SandboxInstance>>>>>` | double-lock nesting |

### Why this compounds

- `Mutex` serializes all readers, creating a throughput ceiling on packet
  forwarding and FS operations
- double-lock nesting (`RwLock<HashMap<_, Mutex<_>>>`) creates deadlock risk
  and makes lock ordering harder to reason about
- performance regressions from lock contention are silent — they don't fail
  tests, they just slow down

### Recommendation

1. Replace `Arc<Mutex<dyn NetBackend>>` and similar with `Arc<RwLock<..>>` where
   the backend is read during packet processing and only written during
   config changes.
2. For `VmInstance`/`SandboxInstance`, consider whether the inner `Mutex` can
   become a `RwLock` or whether the double-layer nesting can be flattened.
3. For truly contended hot paths (packet forwarding), evaluate lock-free
   alternatives (`crossbeam` channels, atomic state machines).

---

## T12 · Error Model Fragmentation Across Crate Boundaries <!-- ABX-274 -->

**Compound value**: consistent error types reduce `map_err` boilerplate across
every call site and enable reliable programmatic error handling in transport layers.

### Evidence (beyond T7's stringly-typed findings)

#### `VmmError` does not participate in the `CommonError` hierarchy

`virt/arcbox-vm/src/error.rs` defines `VmmError` with its own `NotFound`,
`AlreadyExists`, `Io`, `Config` variants that duplicate `CommonError` semantics
but are not convertible:

```rust
// VmmError has its own NotFound, AlreadyExists, Io — duplicating CommonError
pub enum VmmError {
    NotFound(String),       // same as CommonError::NotFound
    AlreadyExists(String),  // same as CommonError::AlreadyExists
    Io(#[from] std::io::Error), // same as CommonError::Io
    Config(String),         // same as CommonError::Config
    // ... plus domain-specific variants
}
```

Every other virt crate (`VirtioError`, `FsError`, `NetError`) uses
`#[from] CommonError`. `VmmError` is the outlier.

#### `guest/arcbox-agent` uses `anyhow` in library code

The agent is a long-running in-VM process, not a CLI tool. Its use of `anyhow`
means the host side cannot programmatically match on agent error types. Files:

- `guest/arcbox-agent/src/agent.rs`
- `guest/arcbox-agent/src/rpc.rs`
- `guest/arcbox-agent/src/mount.rs`
- `guest/arcbox-agent/src/main.rs`

#### `CoreError` manually delegates convenience constructors

`app/arcbox-core/src/error.rs` redefines `config()`, `not_found()`,
`already_exists()` as passthrough methods to `CommonError`. Every new
`CommonError` variant requires updating `CoreError` too.

### Recommendation

1. Add `#[from] CommonError` to `VmmError` and remove its duplicate variants.
2. Replace `anyhow` in `guest/arcbox-agent/src/` with a structured
   `AgentError` using `thiserror`.
3. Consider a `derive` macro or blanket trait for the `CoreError`-style
   delegation pattern, or just rely on `From<CommonError>` and use
   `CommonError::not_found(...)` at call sites directly.

---

## T13 · Test Coverage Is Polarized: Strong Virt, Weak App <!-- ABX-275 -->

**Compound value**: the app layer is where bugs compound fastest — lifecycle,
persistence, API contracts — but it has the least test coverage.

### Per-crate coverage (files with at least one `#[test]`)

| Crate | Tested / Total | Coverage |
|-------|---------------|----------|
| `virt/arcbox-net` | 34 / 40 | **85%** ✓ |
| `virt/arcbox-virtio` | 7 / 8 | **88%** ✓ |
| `runtime/arcbox-oci` | 4 / 6 | 67% |
| `app/arcbox-core` | 10 / 16 | 63% |
| `virt/arcbox-fs` | 4 / 7 | 57% |
| `virt/arcbox-vm` | 7 / 13 | 54% |
| `virt/arcbox-hypervisor` | 9 / 19 | 47% |
| `virt/arcbox-vmm` | 9 / 14 | 64% |
| `guest/arcbox-agent` | 4 / 13 | 31% |
| `app/arcbox-cli` | 3 / 17 | **18%** ✗ |
| `app/arcbox-docker` | 4 / 27 | **15%** ✗ |
| `app/arcbox-daemon` | 1 / 14 | **7%** ✗ |
| `runtime/arcbox-container` | 1 / 5 | **20%** ✗ |
| `rpc/arcbox-transport` | 1 / 10 | **10%** ✗ |
| `app/arcbox-api` | 0 / 8 | **0%** ✗ |
| `rpc/arcbox-grpc` | 0 / 1 | **0%** ✗ |

### Why this compounds

- `arcbox-api` (0%) contains all gRPC service implementations — no unit tests
  for request validation, error mapping, or state transitions
- `arcbox-daemon` (7%) owns the startup contract (T1) — the most
  high-leverage code path has almost no automated verification
- `arcbox-docker` (15%) is the Docker compatibility layer — regressions here
  break every `docker` command users run
- `arcbox-transport` (10%) handles vsock/unix transport — failures here are
  silent and hard to debug in production

### Recommendation

1. Prioritize test coverage for `arcbox-api` and `arcbox-daemon` — these are
   the entry points for every user interaction.
2. Extract `arcbox-docker` handler logic into testable pure functions separated
   from axum framework code.
3. Create a shared `arcbox-test-utils` crate (or `tests/support/`) for mock
   runtimes, test VMs, and fixture management currently duplicated across test files.

---

## T14 · Workspace Hygiene: Dead Code, Stale Dependencies, Missed Edition Features <!-- ABX-276 -->

**Compound value**: small per-item, but in aggregate these create noise that
slows down every code review, every `cargo clippy` run, and every new
contributor's onboarding.

### Dead code suppression

`42` instances of `#[allow(dead_code)]` plus `11` `#[allow(unused_*)]` across
the workspace. These are either:

- genuinely dead code that should be deleted
- code that is only used on one platform and needs `#[cfg]` gating instead

Each suppressed warning is a signal that the code's intent is unclear.

### `async-trait` crate is no longer needed

The workspace uses edition 2024 with `rust-version = "1.85"`, which supports
native `async fn` in traits. There are still `9` uses of the `async-trait`
proc macro. Each one adds:

- a `Box<dyn Future>` allocation on every call
- a hidden `Send` bound that may not be needed
- macro expansion overhead in compile time

### Dependency version inconsistency

Several crates specify their own `tokio` version instead of using
`workspace = true`:

- `virt/arcbox-vz/Cargo.toml` — `tokio = { version = "1", ... }` (twice!)
- `common/arcbox-asset/Cargo.toml` — `tokio = { version = "1", ... }`

This can lead to feature flag mismatches and confusing build behavior.

### Recommendation

1. Audit all `#[allow(dead_code)]` — delete dead code or add proper `#[cfg]` gates.
2. Replace `#[async_trait]` with native async fn in traits across all 9 call sites.
3. Move all `tokio`/`tracing` dependencies to `workspace = true`.

---

## T15 · Focused Tactical Fixes Worth Keeping From the Original Audit <!-- ABX-277 -->

These are not the highest-order structural issues, but they are still correct and
worth preserving in the backlog.

### Port forwarding atomicity

Location:

- `app/arcbox-core/src/runtime.rs` in `start_port_forwarding_macos`

Issue:

- partial success can leave listener state and tracked rule state diverged

Fix:

- use commit-or-rollback semantics for rule installation

### `ExecInstance` state machine

Location:

- `runtime/arcbox-container/src/exec.rs`

Issue:

- boolean + optional fields allow invalid combinations

Fix:

- replace with a typed `ExecState` enum

### Public API surface and dead feature flags

The previous audit was directionally correct here:

- some crates expose more modules than they should
- several feature flags appear underused or undocumented

These are useful cleanup tasks, but they should follow the bigger contract and
boundary work above.

---

## Recommended Execution Order

If engineering time is limited, this is the highest-leverage sequence:

1. **Startup contract** (T1, T4)
   - typed startup phases
   - shared host layout
   - shared daemon contract

2. **Lifecycle truth** (T2)
   - one authoritative lifecycle model
   - repository-style persistence updates
   - remove dual-write / swallowed state errors

3. **Structural split program** (T3)
   - `vm_lifecycle.rs`
   - `machine.rs`
   - `guest/agent.rs`
   - CLI command monoliths

4. **VM convergence** (T5, T6)
   - freeze `arcbox-vm` growth
   - move `Vmm` to backend strategy
   - close the `VirtIO transport <-> device` boundary

5. **Error model unification** (T7, T12)
   - `VmmError` → `#[from] CommonError`
   - `guest/arcbox-agent` → structured `AgentError`
   - replace `.to_string()` erasure with `From` chains

6. **Runtime safety** (T10, T11)
   - eliminate `unwrap()` in `virt/` and `guest/` hot paths
   - replace `Arc<Mutex>` with `RwLock` on read-heavy paths
   - add clippy `unwrap_used` deny in CI

7. **CI uplift** (T9, T13)
   - stop excluding `arcbox-agent`
   - move from best-effort workflows to layered enforcement
   - prioritize test coverage for `arcbox-api` (0%), `arcbox-daemon` (7%),
     `arcbox-docker` (15%)

8. **Workspace hygiene** (T14)
   - remove `async-trait` (9 uses → native async fn in traits)
   - audit `#[allow(dead_code)]` (42 instances)
   - consolidate non-workspace dependency declarations

---

## Positive Patterns Worth Preserving

The repo still has several strong patterns that should survive the refactors:

- crate-level layering is still understandable — no circular dependencies detected
- extension traits such as `request.machine_id()` and `runtime.ready()` reduce handler noise
- `CommonError` remains the right base pattern; most crates (except `arcbox-vm`) follow it
- `CancellationToken` usage is consistent
- `spawn_blocking` discipline is generally good
- RAII ownership patterns like `DaemonLock` are sound
- `virt/arcbox-net` (85% coverage) and `virt/arcbox-virtio` (88% coverage) are
  reference models for test discipline — other crates should aspire to these levels
- workspace-level clippy config (pedantic + nursery) with sensible allows shows
  good lint hygiene intent
- `profile.dev.package."*".opt-level = 2` is a smart default for dev builds
  with many dependencies

The codebase does not need a philosophical redesign. It needs boundary tightening,
ownership clarification, and a deliberate debt paydown program in the control
plane and virtualization layers.
