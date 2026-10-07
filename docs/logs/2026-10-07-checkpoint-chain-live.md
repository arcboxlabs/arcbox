# 2026-10-07 — A restored sandbox can be checkpointed and restored again

- Type: change
- Area: `arcbox-agent`, `arcbox-computer-runtime`, `arcbox-fc-driver`
- Related: PR #706

## Problem or trigger

The manager regression over fake adapters verified checkpoint provenance and geometry. The real Firecracker contracts verified capture and restore at the driver boundary. Neither check exercised a second checkpoint through the production `SandboxManager` composition.

## What was done

The `checkpoint::jailed_checkpoint_chain_preserves_source_and_geometry` case extends `sandbox_manager_e2e`. The test uses `node_environment`, the production guest composition, with jailer isolation and networking disabled.

The test creates a sandbox with 3 vCPUs, 768 MiB of memory, and explicit kernel/rootfs sources that differ from the configured defaults. Each of two generations performs these checks:

1. Capture a checkpoint and read its durable catalog entry.
2. Assert the exact source paths and geometry.
3. Remove the source VM and verify its Firecracker process has stopped.
4. Restore the checkpoint into a new VM.
5. Assert the manager's geometry, the guest's online CPUs (`0-2`), and the previous generation's marker.
6. Write the next marker before another checkpoint.

The marker lives on `/run`, which `vm-agent` mounts as tmpfs. The marker checks memory retention, not writable rootfs persistence. The test removes the final VM and both catalog entries.

The test also catches assertion panics before dropping its temporary directory. It removes every sandbox and checkpoint through the manager, attempts the remaining cleanup after an error, and then resumes the original panic. A cleanup error is reported and retains the backing files for diagnosis.

## Evidence

The final test passed on 2026-10-07: **1 passed, 0 failed, 0 ignored**, in 5.90 seconds. The elapsed time describes one run, not a benchmark.

- Candidate base: `e6a967d6b59aae0a41387d748ce9521d4bee887d`, plus this test, rebased onto master `48c222def2530fe34f1184166dc8aeab3643c99a`; no rootfs API commits are required.
- Runner: Lima `arcbox-m2`, Linux `6.8.0-137-generic`, aarch64, nested KVM.
- Firecracker and jailer: `1.16.1`.
- Rootfs: a 256 MiB ext4 image built by `RootfsBuilder`, with static aarch64 BusyBox and musl `vm-agent` from `81435b0d`. The agent source is unchanged on the candidate base. The existing test configuration explicitly selects `init=/sbin/vm-agent`.
- Both catalog entries retained the explicit source kernel/rootfs paths and `SnapshotGeometry { vcpus: 3, memory_mib: 768 }`.
- Native Firecracker logs recorded two `/snapshot/create` operations and two `/snapshot/load` operations.
- Cleanup left no test VM, mount, loop device, or data directory. The source fixture SHA256 remained `af8e488f6cce23570921bc91858312b4bff9452fcdd89851d13bebb8018eb097`.
- Workspace formatting, strict musl Clippy for all agent targets, strict native Clippy for `sandbox_manager_e2e` and all computer-runtime targets, and the architecture layer gate passed. The computer-runtime suite passed 227 library tests and 42 manager tests.

Build the Linux test executable as the development user:

```sh
cargo +nightly test --locked -p arcbox-agent --test sandbox_manager_e2e --no-run --message-format=json
```

Set `FC_BINARY`, `FC_JAILER`, `FC_KERNEL`, and `FC_ROOTFS` to local assets. Execute the `sandbox_manager_e2e` executable reported by Cargo as root, with those variables preserved:

```sh
<test-executable> checkpoint::jailed_checkpoint_chain_preserves_source_and_geometry --ignored --exact --nocapture --test-threads=1
```

The test asserts its prerequisites. Missing configuration cannot produce a successful early return.

## Failure cleanup verification

The cleanup revision passed the same jailed chain: **1 passed, 0 failed, 0 ignored**, in 6.44 seconds. Strict native Clippy for `sandbox_manager_e2e` and strict musl Clippy for all agent targets passed.

A temporary probe then panicked after the first checkpoint, before the original guest removal. The probe confirmed an active `/dev/dm-0` snapshot and two backing devices, `loop0` and `loop1`. The test preserved the original panic and exited with status 101: **0 passed, 1 failed, 0 ignored**. The cleanup collector found no test process, mount, loop device, dm mapping, or temporary directory. The source fixture hash remained unchanged. The probe was removed after this check; the original test assertions remain unchanged.

The final success and expected-failure reports are `/private/tmp/arcbox-checkpoint-cleanup-final.json` and `/private/tmp/arcbox-checkpoint-cleanup-failure.json`. The retained temporary probe is `/private/tmp/arcbox-checkpoint-cleanup-failure-probe.rs`.

## Follow-ups

This check does not exercise a host daemon, networking, seccomp, cache sweeping, invalid geometry, or RPC 412 classification. Re-run the target when integration changes its runtime dependencies. The earlier driver-contract evidence and the failed auxiliary collector remain separate records.
