# computer/arcbox-oci — OCI Runtime-Spec Model Agent Guidance

OCI **runtime-spec** v1.2.0 modeling: `config.json` (`config/`), bundle dir
(`bundle.rs`), OCI state (`state.rs`), hooks (`hooks.rs`). Host-side and
`std`-only; it follows `computer/AGENTS.md`'s layer rules (platform-neutral,
no `app/` or adapter edge). This is NOT image/pull/layer code — there is no
manifest, digest, layer or tar unpack here. Image work is in `oci2rootfs`
and `engine/arcbox-image`.

## Its one consumer, and what it is for

- `arcbox-computer-runtime`'s workload contract (`src/workload/`) builds a
  computer's `config.json` from an image config and a create request
  (`bundle_spec`) and translates a spec into the guest agent's
  `StartCommand` (`Workload::from_spec`). Activated 2026-11-01 under
  ADR 0011; the history is the `runtime/` paragraph of
  `docs/architecture/charter.md`.
- The VM is the isolation boundary. The consumer **rejects** hooks,
  namespaces, seccomp, capabilities, rlimits and devices, and every other
  field the guest agent cannot carry out, instead of running without them.
  A field added to `Spec` here is accepted silently by nothing: add it to
  the consumer's check (`workload/start.rs`) and its rejection test in the
  same change, or the new field is ignored.
- OCI `Status` has no Paused state. `ComputerManager` owns lifecycle and
  pause; any OCI state shown for a computer is a view of it, never the
  source.

## Advertised-but-inert features (do not trust as working boundaries)

- **OCI hooks are parsed and validated but NEVER executed.** `hooks.rs` has
  no `Command`/spawn; `HookContext` and `HookResult` are inert data with no
  non-test consumer. `Hooks::validate` only checks path-absolute +
  timeout>0. The workload contract rejects hooks for this reason.
- **`Spec::validate` is shallow** (`config/spec.rs`): it checks only
  `ociVersion` non-empty with ≥2 dotted segments, `root.path` non-empty,
  `process.cwd` non-empty and absolute, and each `mounts[].destination`
  non-empty and absolute. It does NOT validate namespaces, resources,
  seccomp, capabilities, or hook paths. Do not treat it as a security
  boundary; the consumer's check is the boundary.
- `Process::default()` and `Spec::default_linux()` fill in rlimits,
  capabilities and proc/dev/sys mounts, all of which the workload contract
  rejects. Build a computer's spec field by field, as `bundle_spec` does.

## Conventions & pitfalls

- `arcbox_oci::ContainerState` is a metadata **struct** that flattens a
  `State`; the 4-variant enum is `Status` (Creating/Created/Running/Stopped),
  transitions validated by `State::transition_to` → `Status::can_transition_to`,
  `Stopped` terminal. Do not confuse it with `runtime/arcbox-container`'s
  8-variant Docker enum.
- `State::with_generated_id` keeps the full dashed 36-char UUID; container
  and exec ids elsewhere strip dashes.
- `config.rs` was split into `config/` (mod + one file per unit, `03e81c0d`).
  Keep new spec types one per file.
- Numeric fields from an untrusted spec (limits, sizes, console size) use
  checked arithmetic or `try_from`, with tests near the type's maximum.

## Validation

```
cargo test -p arcbox-oci
cargo test -p arcbox-computer-runtime --lib workload
cargo clippy -p arcbox-oci --all-targets -- -D warnings
```
