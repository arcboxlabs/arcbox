# 2026-10-30 — The consumerless arcbox-net-virtio crate is removed

- Type: change
- Area: `virt/arcbox-net-virtio`, workspace layout
- Commits / PRs: `8358bedc` (crate created), this change (crate removed)
- Related: `docs/architecture/charter.md`, `docs/architecture/vm-stack-redesign.md`

## Problem or trigger

A dependency audit of the workspace on 2026-10-30 found that no member depends on `arcbox-net-virtio`. The crate was created on 2026-06-13 (`8358bedc`) to move `NatNetBackend` and `TsoNetBackend` out of `arcbox-net`, so that `arcbox-net` no longer depended on `arcbox-virtio`. The commit message already stated that "neither type had a consumer". No consumer appeared afterwards: the HV datapath feeds `arcbox-virtio-net` through the workers in `arcbox-vmm`, and the VZ path uses the framework's own NIC.

## What was done

- Removed `virt/arcbox-net-virtio` (817 lines) and its workspace member entry.
- Removed its mentions from `common/AGENTS.md`.

The `arcbox-net` → `arcbox-virtio` separation that motivated the crate is kept: `arcbox-net` still does not depend on `arcbox-virtio`. The removed code is recoverable from `8358bedc` and its parent history.

## Evidence

- `cargo metadata` listed no workspace package with `arcbox-net-virtio` among its dependencies (normal, dev, or build).
- `git grep net-virtio` outside the crate returned only `CHANGELOG.md`, the generated `NOTICE.md`, and `common/AGENTS.md`.

## Follow-ups

- None. A future VirtIO-net backend over the NAT engine is written against the `arcbox-vm-driver` port and the `HvDriver` adapter, not revived from this crate.
