# 2026-10-08 — Storage dependencies follow the workspace release version

- Type: incident
- Area: Cargo workspace, storage, release-please
- Related: [Release PR #731](https://github.com/arcboxlabs/arcbox/pull/731), [failed release-please run](https://github.com/arcboxlabs/arcbox/actions/runs/37653189054)

## Problem

Release-please changed the workspace version to `0.9.0`, then `cargo update --workspace` failed. Storage dependencies in member manifests still required `0.8.2`. Cargo rejected the local packages because `^0.8.2` does not accept `0.9.0`.

## Change

Declare `arcbox-storage` in `[workspace.dependencies]` with the existing `x-release-please-version` annotation. Use workspace inheritance for every storage dependency and the remaining local `arcbox-atomic-file` dependencies. Release-please now updates each shared requirement with the workspace package version.

## Validation

In a temporary source tree, apply the configured release-please version annotations for `0.9.0`, then run `cargo update --workspace` and `cargo metadata --locked --format-version 1`. The original manifests reproduce the failure; the corrected manifests pass both commands. The external package records in `Cargo.lock` remain unchanged. The independently versioned helper and hypervisor packages keep their versions.

No dependency versions change in this fix. The normal master workflow must refresh the release PR before the release is merged.
