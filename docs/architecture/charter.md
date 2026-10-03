# ArcBox Architecture Charter — the Engine/Computer Restructure

Status: **active** (2026-08-13; moved back from the company repo 2026-09-30). This document is the anchor for the
workspace restructure: every restructure PR cites it, and changes to the
decisions below happen here first.

## Vision

ArcBox becomes the cross-platform **library stack for agent computers** —
the base capabilities (VM lifecycle, images, snapshots, networking, agent
channel, sandbox semantics) packaged as embeddable Rust crates, with thin
product shells assembling them. "Agent computer" supersedes "sandbox" as
the product concept: a Computer is a VM plus identity, storage, network,
snapshot lineage, and an in-guest agent surface.

The unlock is anatomical, not greenfield: the repo already contains a
production Linux microVM engine (`virt/arcbox-vm`, the frozen guest-side
Firecracker sandbox manager) and a macOS engine (welded into
`app/arcbox-core`'s daemon `Runtime`). The restructure extracts one
platform-neutral engine from both hosts and re-assembles it at three
points:

1. `app/arcbox-daemon` — macOS desktop product (today's shape)
2. `guest/` `vm-agent` — inside the System VM (today's sandbox shape)
3. a bare-Linux node daemon profile — the cocoon-equivalent (new)

## Locked decisions

- **D1 — Library-first inversion.** `arcbox_core::Runtime` stops being the
  god object. Engine- and computer-layer crates expose daemon-free APIs;
  the daemon becomes one composer. No engine/computer crate may reference
  the daemon lock, socket paths, or startup pipeline.
- **D2 — One hypervisor trait, plural backends.** `virt/arcbox-hypervisor`
  is the only trait layer. Backends: VZ (macOS, in-process), HV (macOS,
  in-process, custom), **FC-driver** (external Firecracker process,
  extracted from `arcbox-vm` — already production-proven in-guest),
  CH-driver (later; buys Windows guests + vhost-user ecosystem), KVM
  in-process (existing skeleton, demoted to R&D). The "performance paths
  are custom-built" doctrine stays scoped to macOS, where it is the moat;
  on Linux differentiation lives above the VMM.
- **D3 — Repo boundary mirrors cocoonstack's split.** In this repo: the
  Rust library stack, node daemon, CLI, guest agent, protocol, SDKs.
  Sibling repos: K8s operators / virtual-kubelet provider (Go ecosystem),
  snapshot registry service, guest image pipelines (boot-assets already
  is one), desktop app (already is one).
- **D4 — Layer dependency rules** (enforced the way `common/`'s "no VM
  dep" rule is): `engine/` and `computer/` crates depend on macOS-only
  crates (VZ/vmnet/HV) only through the hypervisor trait, and never on
  daemon/CLI/docker-compat crates. `computer/` must compile and pass unit
  tests on Linux; CI grows a job that proves it. (Revised 2026-08-16 with
  D2: the seam is the VM port `virt/arcbox-vm-driver`, and no `engine/`
  or `computer/` crate depends on any adapter — `cargo xtask check-layers`
  enforces it from `cargo metadata`.)
- **D5 — Protocol continuity.** `arcbox.sandbox.v1` stays additive-only
  (published SDKs depend on it). "Computer" naming enters at the domain
  layer (crate/module/type names, docs). A wire-level v2 rename is a
  separate decision, taken only after the domain layer stabilizes.
- **D6 — Strangler-fig migration.** Each extraction leaves compatibility
  re-exports in `arcbox-core` so consumers keep compiling; consumers then
  migrate in follow-up commits and the re-exports die. Commits stay
  atomic and reviewable per repo CLAUDE.md discipline; every move updates
  the AGENTS.md files that describe the moved code.

## Target workspace tree

```
arcbox/
├── common/            # L0 pure utilities + pure net stack (unchanged)
├── virt/              # L1 hypervisor backends + virtio devices
│   │                  #   arcbox-vm-driver = the VM port (VmDriver/VmHandle/
│   │                  #     VmSpec/GuestNetwork + testkit) both orchestrators use
│   │                  #   arcbox-hypervisor = the in-process manual-execution port
│   │                  #   adapters: arcbox-fc-driver (from arcbox-vm),
│   │                  #     arcbox-vz-driver (from arcbox-vmm/darwin.rs),
│   │                  #     arcbox-vmm::HvDriver, arcbox-tap-net (from
│   │                  #     arcbox-vm/network), arcbox-ch-driver (later)
├── engine/            # L2 embeddable, daemon-free engine library
│   ├── arcbox-image       # boot_assets + machine_image + remote_image
│   ├── arcbox-engine      # vm_lifecycle + machine + vm
│   └── arcbox-snapshot    # arcbox-vm snapshot/snapshot_cow/template_catalog
│                          #   + future registry client
├── computer/          # L3 agent-computer domain (platform-neutral)
│   ├── arcbox-computer    # sandbox_capability + business logic distilled
│   │                      #   from arcbox-api sandbox services
│   └── arcbox-computer-runtime  # the VMM-agnostic computer lifecycle
│                          #   (successor of arcbox-vm's SandboxManager):
│                          #   pool/claim, checkpoint, exec, files, timers
├── rpc/               # protocol + transport (unchanged); arcbox-api
│                      #   Connect handlers become thin adapters
├── app/               # L4 product shells: daemon (desktop|node profiles),
│                      #   cli, docker compat, helper, migration
├── guest/             # arcbox-agent; vm-agent recomposed over engine/
├── sdk/  fleet/  runtime/  tests/  xtask/
```

`runtime/` (arcbox-container, arcbox-oci — currently zero consumers) is
either activated inside `engine/arcbox-image`'s OCI story or deleted; it
does not stay as dead weight.

## Crate migration map

| Today | Target | Phase |
|---|---|---|
| `app/arcbox-core/src/boot_assets/` | `engine/arcbox-image` | P1 |
| `app/arcbox-core/src/machine_image.rs`, `remote_image.rs` | `engine/arcbox-image` | P1 |
| `app/arcbox-core/src/vm_lifecycle/`, `machine*`, `vm*`, `agent_client*` | `engine/arcbox-engine` | P2 |
| `app/arcbox-core/src/sandbox_capability.rs` | `computer/arcbox-computer` | P3 |
| `app/arcbox-api/src/connect/{process,filesystem,snapshot,template,sandbox_*}` business logic | `computer/arcbox-computer` (handlers stay as adapters) | P3 |
| `virt/arcbox-vm` Firecracker process driving | `virt/arcbox-fc-driver` behind `virt/arcbox-vm-driver` | P4b (R1) |
| `virt/arcbox-vm/src/{snapshot,snapshot_cow,template_catalog}` | `engine/arcbox-snapshot` | P4a (done) |
| `virt/arcbox-vm/src/network` | `virt/arcbox-tap-net` (Linux `GuestNetwork` adapter) | P4b (R2) |
| `virt/arcbox-vm` remainder (`SandboxManager` cluster) | `computer/arcbox-computer-runtime` over the VM port; `arcbox-vm` deleted | P4b (R3) |
| `virt/arcbox-vmm` VZ path, engine `VmManager` on `Vmm` | `virt/arcbox-vz-driver`, `arcbox-vmm::HvDriver`, engine `VmRegistry` over `dyn VmHandle` | P4b (R4) |
| `app/arcbox-daemon` | gains `node` profile (no docker-compat/vmnet/magic-mount deps) | P5 |

## Phases and acceptance criteria

- **P0 — charter + doc split.** This document; internal-docs moved to
  this repo (arcbox PR #619). *Done when both land.*
- **P1 — `engine/arcbox-image`.** Mechanical extraction, no behavior
  change; `assets.lock` embedding path fixed; arcbox-core re-exports keep
  consumers untouched. *Accept: workspace `cargo check` + unit tests +
  clippy/fmt clean; `boot_assets` e2e green.*
- **P2 — `engine/arcbox-engine`.** vm_lifecycle/machine/vm/agent_client
  move; `Runtime` shrinks to daemon glue. *Accept: daemon-level e2e
  (`boot_assets`, `virtio_debug`) green on both backends; no engine crate
  imports daemon types.*

  Execution design (locked 2026-08-13, from the seam map):
  - `agent_client` is a pure leaf (needs only error + trace); `machine`,
    `vm`, `vm_lifecycle` are one strongly-connected component and move
    together. Cut order: (1) crate scaffold + `EngineError` +
    `agent_client` + `trace`; (2) `vm` + `machine` + `event` +
    `persistence` (persistence is mutually entangled with machine and is
    machine-state domain — it moves too; daemon reaches it via
    re-export); (3) `vm_lifecycle` (module keeps its literal name: the
    balloon controller uses `pub(in crate::vm_lifecycle)`).
  - Errors repeat the P1 mirror pattern: `EngineError` carries the
    engine-flavored variants (Common, Vmm, Snapshot, Vm, Machine, Agent,
    Transport, LockPoisoned, Persistence); `CoreError` keeps its full
    variant set as the app-layer contract and gains a variant-for-variant
    `From<EngineError>`, so api/daemon constructions and matches keep
    exact semantics.
  - The two macOS-only remainers the targets call (`route_reconciler`,
    `bridge_discovery`) are cut by dependency inversion, not by moving
    them: the boot-time route hook becomes an injected closure on
    `VmLifecycleConfig` wired by `runtime.rs`, and the vmnet bridge-name
    lookup moves up to the runtime composition layer (engine keeps only
    `bridge_nic_mac_for_vm_id`).
- **P3 — `computer/arcbox-computer`.** Sandbox business logic distilled
  out of arcbox-api; handlers become adapters. *Accept: control-plane
  registration test + sandbox e2e green; the crate compiles and passes
  unit tests on `x86_64-unknown-linux-gnu` in CI.*

  Execution design (locked 2026-08-13, from the seam map):
  - The hexagonal cut is a `SandboxHost` trait defined in the computer
    crate and implemented by `arcbox_core::Runtime` (L4 depends on L3):
    host-state generation lock/read/clear, port remove, sandbox DNS
    register/deregister, and `agent()` (engine-vocabulary agent connect,
    retiring arcbox-api's `engine_agent` bridge). Port expose/mappings
    join the trait in the third cut. Protocol code is generic over the
    trait; `arcbox-computer` never imports `connectrpc`.
  - Cut order: (1) crate scaffold + `sandbox_locks` + `sandbox_cleanup`
    (both already transport-free); (2) the resume protocol out of
    `sandbox_resume` (retry loop, paused-detection, ensure-resumed) with
    the Connect tail (RequestContext header, ConnectError mapping)
    staying in arcbox-api; (3) `control.rs`'s expose/list/unexpose
    port protocols (generation fence, compensating rollbacks) and the
    create/restore DNS-live-match protocol deduped into one function
    (today duplicated verbatim in `control::create` and
    `snapshot::restore`).
  - Wire types (`arcbox_connect::sandbox_v1`) are allowed in the
    computer crate — they are the message vocabulary, not the transport.
  - `template.rs` stays an adapter in arcbox-api: all template-catalog
    semantics live guest-side (`virt/arcbox-vm/template_catalog.rs`),
    reachable only over the agent channel; nothing to distill until P4
    thaws arcbox-vm.
  - Deviation (2026-08-13): `nested_virt_for_backend` /
    `sandbox_capability` stays in arcbox-core — moving it into the
    computer crate requires rehoming the VZ hardware probe behind the
    vmm/hypervisor layer first (computer must not depend on arcbox-vz).
    Follow-up owned by P4 alongside the arcbox-vm thaw.
  - The control-plane tests pin `arcbox_api::{SharedRuntime,
    SystemServiceImpl, SetupState, connect::router_with_system}` — the
    router and service impls stay in arcbox-api.
- **P4 — thaw `arcbox-vm`.** FC-driver into `virt/`, snapshot/template
  into `engine/arcbox-snapshot`, vm-agent recomposed. *Accept: sandbox
  e2e green and cold-start baselines hold (restore ~22ms / warm ~180ms /
  cold ~640ms).*

  Seam map findings (2026-08-13) — three corrections to the framing this
  phase was written under:

  - **"Frozen" meant feature-frozen, not dormant.** `arcbox-vm` is
    26,171 lines under active development (the CORE-107 template catalog
    landed in it days ago), builds clean on both host and musl targets,
    and has its own 4-job Linux CI pipeline
    (`.github/workflows/test-vm-linux.yml`, real KVM + real Firecracker).
    P4 decomposes a living crate, so cuts stay small and reversible; it
    is not the salvage job the word "thaw" suggests.
  - **`SandboxManager` is one struct with its impl split across 15
    files** (`sandbox.rs` + `sandbox/*`, 13,238 lines — half the crate),
    holding `fc_sdk::FirecrackerProcess` / `Arc<fc_sdk::Vm>` as raw
    fields of `SandboxInstance`. There is no narrow FC interface to lift
    out today.
  - **`virt/arcbox-vm/README.md` is materially wrong** — it documents
    `SandboxServiceImpl`/`SandboxSnapshotServiceImpl` gRPC services and
    two examples, none of which exist (the service wiring lives in
    `guest/arcbox-agent`). Correct it during the phase.

  P4 therefore splits. **P4a** is the part whose design is settled:

  1. Rehome the nested-virt probe behind the vmm seam and move
     `sandbox_capability` into `computer/arcbox-computer`, closing P3's
     recorded deviation. `PlatformCapabilities::nested_virt` already
     carries the same VZ call one layer down — the gap is only that
     `arcbox-vmm` never exposes it.
  2. Prep cuts inside `arcbox-vm`: delete the dead `VmmManager` trio
     (`manager.rs`/`instance.rs`/`store.rs` + their `config.rs` types,
     ~1,300 lines, zero consumers anywhere in the workspace); relocate
     `atomic_write` out of `sandbox/persistence.rs` (`template_catalog`
     depends backward on it, so this must land BEFORE the extraction or
     it will not compile); split `vsock.rs`'s protocol vocabulary from
     its host-client half.
  3. Extract `engine/arcbox-snapshot` (`snapshot`, `snapshot_cow{,/persistence}`,
     `template_catalog` — 3,290 lines) with its own `SnapshotError` on the
     established mirror pattern.

  P4a shipped as arcbox #626 (steps 1–2, squash `939a4f52`) and #630
  (step 3). Two decisions came out of it that outlive the phase:

  - **`common/arcbox-atomic-file` is a new L0 crate.** The durable-write
    primitive is needed by `arcbox-snapshot`'s template catalog and by
    the sandbox record store that stays in `arcbox-vm` — two crates in
    different layers, neither the natural owner. Reusing
    `arcbox-engine`'s same-named `atomic_write` was rejected on
    inspection: it deliberately skips `fsync` ("durability across power
    loss is not guaranteed"), where this one fsyncs the file *and* the
    parent directory and distinguishes `NotCommitted` from
    `DurabilityUncertain`. They are different primitives that share a
    name; merging them would weaken one contract or slow the other.
    Anything in `engine/` or `computer/` that persists state uses the
    crate rather than hand-rolling temp-then-rename.
  - **A `#[cfg(test)]` seam does not survive a crate extraction.**
    `CowManager`'s test probe was `cfg(test)`-gated and its only consumer
    was `arcbox-vm`'s cleanup tests; once those lived in another crate
    the hook simply ceased to exist. The pattern for a cross-crate test
    seam is an opt-in feature (`test-probe`), enabled by the consumer's
    dev-dependencies so it still never reaches a production build. Expect
    to hit this again in P4b — the `SandboxManager` cluster is full of
    `cfg(test)` seams.

  The `vsock.rs` protocol/host-client split from step 2 was deferred: it
  serves P4b's fc-driver work, not the snapshot cut, and belongs with the
  design that consumes it.

  **P4b** — `virt/arcbox-fc-driver` and the `SandboxManager`
  redistribution — does NOT start until it has its own locked execution
  design, the way P2 and P3 got one. It is the largest unscoped item in
  this charter, plausibly larger than P2 and P3 combined.

  Execution design (locked 2026-08-16): `architecture/vm-stack-redesign.md`
  is that design, and it covers the `arcbox-vmm`/engine side as well so
  both orchestrators end on one seam. Its ten decisions (D-VM1..10) and
  five phases (R0..R5) are normative; the short form:

  - One VM port, `virt/arcbox-vm-driver` (`VmDriver`/`VmHandle`/`VmSpec`/
    `GuestNetwork`, object-safe, capabilities as `Option<&dyn Cap>`
    accessors, `testkit` with `FakeDriver` + `driver_contract!`). Adapters:
    `arcbox-fc-driver`, `arcbox-vz-driver`, `arcbox-vmm::HvDriver`
    (`Vmm` split into `HvMachine`; the completion contract becomes one
    `DeviceManager::complete_io`), `arcbox-ch-driver` when Windows starts.
  - `arcbox-hypervisor` narrows to the manual-execution port consumed only
    by `arcbox-vmm`; its VZ implementation goes once `arcbox-vz-driver`
    serves the engine.
  - `arcbox-vm` becomes `computer/arcbox-computer-runtime`: statig HSM +
    per-computer actor + effects, pure policy modules, `NodeEnvironment`
    (behavior) vs `RuntimeConfig` (data), unit tests over `FakeDriver` on
    any host, `Sandbox*` aliases until `guest/arcbox-agent` migrates.
    `arcbox-vm/src/network` becomes `virt/arcbox-tap-net`.
  - Engine: `VmRegistry` over `dyn VmHandle`; `VmBackend` becomes an
    engine-owned config enum resolved to a driver only at composition
    roots; agent transport mode comes from `VsockConn::mode`, never from
    a backend match. One snapshot store (`arcbox_snapshot::SnapshotCatalog`).
  - `cargo xtask check-layers` enforces D4 and the new adapter rule from
    `cargo metadata` in CI.
  - Order: R0 port crate → R1 fc-driver (accept: test-vm-linux green,
    sandbox cold-start baselines ~22/~180/~640 ms held, `rg fc_sdk
    virt/arcbox-vm` empty) → R2 tap-net → R3 runtime → R4 engine/vmm
    (accept: both-backend e2e green, per-boot counters ~2301/~71
    unchanged) → R5 node composition (platform agent + daemon node
    profile). Datapath (SplitQueue, HV workers, `arcbox-net`), vsock
    framing and `arcbox.sandbox.v1` are out of scope by decision.

- **D2 clarified (2026-08-13): the FC driver is not a `Hypervisor` impl.**
  `virt/arcbox-hypervisor`'s trait is shaped for *in-process* hypervisors
  — `memory()`, `create_vcpu()`, `add_virtio_device()`, per-device
  `snapshot_devices()`. Firecracker is an external process driven over an
  HTTP API, with no guest-memory access, no per-vCPU control, and
  whole-VM snapshots. Implementing the trait for it would stub most of
  the surface, which is a false abstraction that makes every consumer
  guess which methods are real. So "backend" in D2 is a
  provisioning-policy concept at the *selection* layer; `arcbox-fc-driver`
  gets its own narrow surface (boot spec in, process + VM handle out,
  plus pause/resume/snapshot verbs). Revisit only if a second
  external-process VMM (Cloud Hypervisor's API mode) makes a shared
  external-VMM trait pay for itself.

- **D2 revised (2026-08-16): the shared trait now pays, as a new coarse
  port.** Cloud Hypervisor is on the roadmap for Windows guests, and VZ
  and the custom HV engine expose the same coarse verbs (boot, shutdown,
  events; vsock, checkpoint/restore and adopt/detach are capabilities —
  a guest may have no vsock, checkpoint owns its own quiescing, and an
  in-process VM cannot outlive the daemon, so none of `dial_vsock`,
  VMM pause/resume or `detach` is ever a mandatory handle verb).
  `virt/arcbox-vm-driver`
  is that port; FC, CH, VZ and HV are its adapters. `arcbox-hypervisor`
  is *not* widened for this — it narrows to the in-process
  manual-execution port and nothing external ever implements it. "Backend"
  in D2 stays a selection-policy concept: `VmBackend` becomes an
  engine-owned config enum resolved to a driver only at composition
  roots. Full design: `architecture/vm-stack-redesign.md`.

- **Three agent binaries, three names — do not merge them.** The Vision
  section's "`guest/` `vm-agent`" was loose wording and caused a real
  ambiguity in P4 planning. The settled naming:
  `arcbox-daemon` (host, macOS), `guest/arcbox-agent` (inside the System
  VM, wraps `SandboxManager`, speaks `sandbox.v1` over vsock), and
  `vm-agent` (`virt/arcbox-vm-agent`, PID 1 inside each nested
  Firecracker sandbox, staged into every sandbox rootfs by the
  `RootfsBuilder` the guest agent composes — CORE-127 split the binary
  and its wire vocabulary, `virt/arcbox-vm-proto`, out of `arcbox-vm`). The migration map's
  "`arcbox-vm` remainder (`vm-agent` bin)" row means the crate's
  *library* body is redistributed over engine+computer; no binary is
  renamed, and `vm-agent` itself stays small and separate — it depends
  only on `arcbox-vm-proto`, never on `arcbox-vm` (or its successor
  `arcbox-computer-runtime`).
- **P5 — Linux node.** `node` daemon profile boots a computer on bare
  Linux/KVM via FC-driver; ubuntu KVM CI job. *Accept: a sandbox claim +
  exec round-trips on a Linux host in CI.* Sibling repos (registry, K8s
  provider) start only after P5.

## Risks / standing rules

- **Desktop-dep leakage** into the library tree is the failure mode D4
  exists for; the Linux CI compile gate is the tripwire.
- **macOS P0 performance targets are unaffected** by this charter; the
  restructure must not regress the CLAUDE.md perf table, and macOS work
  keeps priority until those targets converge.
- **Docs move with code**: every extraction updates the AGENTS.md set in
  the same PR, or the handbook rots.
