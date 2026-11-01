# runtime/ — Container Model Agent Guidance

One host-side, `std`-only model crate. No hypervisor, no guest memory, no
virtqueue surface — none of the `virt/AGENTS.md` HV/e2e invariants apply here.

- `arcbox-container` — domain types (`Container`, `ContainerId`,
  `ContainerState`) + exec orchestration primitives (`ExecManager`,
  `ExecAgentConnection`).

`arcbox-oci` lived here until 2026-11-01, when the host sandboxes workload
contract became its consumer; it is now `computer/arcbox-oci` with its own
`AGENTS.md`. It models the OCI **runtime** spec and was never image, pull or
layer code — look for image bugs in the pull path elsewhere
(`engine/arcbox-image`, `computer/arcbox-computer-runtime/src/workload/`),
not in either crate.

## Load-bearing wiring reality (read before "fixing container lifecycle")

- **`arcbox-container` is not on the live container/exec path.** It has
  **zero** workspace consumers (`rg arcbox_container` outside its own crate
  returns nothing) and stays parked until the activation condition in
  `docs/architecture/charter.md` ("`runtime/` (arcbox-container) is parked,
  not deleted") is met. The running daemon's container + exec flow lives in
  `app/` (Docker Engine API compat) and the guest agent; a sandbox's exec
  sessions belong to `arcbox-computer-runtime`. Editing this crate changes
  **no** daemon behavior until you also wire it in. WHY: prevents hours of
  "my change did nothing" debugging.
- `ExecManager` with **no** agent returns a fake `exit_code: Some(0)`
  (`arcbox-container/src/exec.rs:294-302`) — a test stub, not real exec.
  Real exec requires an `ExecAgentConnection` impl (`exec.rs:157`).

## `ContainerState` is not OCI state — do not conflate

`arcbox-container::ContainerState` (`state.rs:47`) is a Docker-facing enum
with 8 variants (Created/Starting/Running/Paused/Restarting/Exited/Removing/
Dead) and **no** transition guard: any assignment is allowed. The OCI
`Status` in `computer/arcbox-oci` has 4 (Creating/Created/Running/Stopped),
validated transitions, and no Paused. The two do not map 1:1; when wiring
one to the other, state explicitly which side owns which surface. WHY: wrong
import / lost state distinctions.

## Conventions & pitfalls

- `ContainerId::new` strips dashes and TRUNCATES to 12 chars
  (`container/src/state.rs:16`); `ExecId::new` strips dashes, keeps full 32
  (`exec.rs:18`). Not interchangeable with each other or with OCI's dashed
  36-char state ids.
- Guest-controlled numeric fields must use checked arithmetic and cover
  near-`u64::MAX` in tests — debug panics on overflow, release wraps past
  bounds checks.

## Validation

Pure host-side unit tests, cheapest first:

```
cargo test -p arcbox-container
cargo clippy -p arcbox-container
cargo fmt -p arcbox-container
```

The HV/e2e ladders (`hv_vmm`, `virtio_debug`, `ARCBOX_VM_BACKEND`,
`cargo xtask e2e`) do NOT apply to changes scoped to `runtime/` — running
them here is wasted effort.
