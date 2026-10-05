# 2026-10-05 — Does HV observe guest poweroff before host teardown?

- Type: experiment
- Area: HV VMM shutdown, storage recovery lifecycle
- Outcome: the final API passed the boot-only probe and daemon boot/shutdown checks; full-probe DAX failure reproduced with the baseline host; the complete recovery fixture remains pending
- Probes: [`hv_e2e`](../../tests/e2e/src/bin/hv_e2e.rs), [`hv_vmm`](../../tests/e2e/tests/hv_vmm.rs), [`virtio_debug`](../../tests/e2e/tests/virtio_debug.rs), [`boot_assets`](../../tests/e2e/tests/boot_assets.rs)
- Host: Apple M5 Max, macOS 26.4 (25E246); concurrent host load was not controlled
- Direct probe: HV, 2 vCPUs, 1024 MiB; no writable data disk, networking, balloon, bridge, or extra shares

## Question and hypotheses

Can `Vmm::wait_for_stopped` distinguish guest poweroff from host teardown, retain the completion result, and reject a vCPU exit that did not execute PSCI `SYSTEM_OFF`?

1. The host must observe PSCI `SYSTEM_OFF` before `Vmm::stop()` joins workers.
2. A zero-timeout wait before shutdown must be false; a repeated wait after completion must remain true.
3. A disconnected completion channel without `SYSTEM_OFF` must report an error.

## Method

Run from the repository root on Apple Silicon with the development kernel, BusyBox-init rootfs, and Linux agent already staged. The wrapper builds and signs the release probe. Use an isolated `ARCBOX_DATA_DIR` containing `bin/arcbox-agent`; never use an existing ArcBox data directory. These asset hashes were identical for the changed-host and baseline-host runs:

| Asset | SHA256 |
|---|---|
| Kernel | `a122ce7a69a77408eeef9423afd9f6915828ba74a767e3e1eace635f913e02c4` |
| Rootfs | `e0fe5227096fcbdfb522c3337bac1cdb72c91f39ba25e9aeb541e649b549d19f` |
| Current agent | `9a954e6989431294b9552af12873c935ebf7daca0ec4e813db2b5f838352f4c8` |

```sh
cargo fmt --check
cargo test -p arcbox-vmm --lib hv_stop_wait -- --nocapture
cargo clippy -p arcbox-vmm -p arcbox-engine -p arcbox-e2e --all-targets -- -D warnings
cargo check --target aarch64-unknown-linux-musl -p arcbox-vmm --all-targets
cargo build --release -p arcbox-e2e --bin hv_e2e -p arcbox-daemon --bin arcbox-daemon
probe_data="$(mktemp -d "${TMPDIR:-/tmp}/arcbox-hv-poweroff.XXXXXX")"
mkdir -p "$probe_data/bin"
cp boot-assets/dev/arcbox-agent "$probe_data/bin/arcbox-agent"
export ARCBOX_DATA_DIR="$probe_data"
export ARCBOX_HV_E2E_KERNEL="$PWD/boot-assets/dev/kernel"
export ARCBOX_HV_E2E_ROOTFS="$PWD/boot-assets/dev/rootfs.erofs"
export ARCBOX_HV_E2E_VCPUS=2 ARCBOX_HV_E2E_MEMORY_MB=1024
export ARCBOX_HV_E2E_BALLOON=0 ARCBOX_HV_E2E_NETWORKING=0
export ARCBOX_HV_E2E_DATA_IMG_MB=0 ARCBOX_HV_E2E_EXTRA_SHARES=0
export ARCBOX_HV_E2E_BRIDGE=0 ARCBOX_HV_E2E_TIMEOUT=30 ARCBOX_HV_E2E_LOGLEVEL=4
SKIP_BUILD=0 ARCBOX_HV_E2E_BOOT_ONLY=1 cargo test -p arcbox-e2e --test hv_vmm -- --ignored --nocapture
SKIP_BUILD=0 ARCBOX_HV_E2E_BOOT_ONLY=0 cargo test -p arcbox-e2e --test hv_vmm -- --ignored --nocapture
ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test virtio_debug -- --ignored --nocapture
ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test boot_assets -- --ignored --nocapture
```

The boot-only mode skips DAX, agent supervision, and pause/resume. Phase 7 still sends `ShutdownRequest`, waits for guest poweroff, checks retained completion, and then stops the VMM. The full mode reaches DAX before shutdown.

To repeat the host comparison, retain the same exported asset paths and knobs. Build an unmodified `1106545e` source snapshot with its own target directory:

```sh
baseline_src="$(mktemp -d "${TMPDIR:-/tmp}/arcbox-hv-baseline.XXXXXX")"
git archive 1106545e | tar -x -C "$baseline_src"
cargo build --manifest-path "$baseline_src/Cargo.toml" --target-dir "$baseline_src/target" --release -p arcbox-e2e --bin hv_e2e
codesign --force --options runtime --entitlements "$baseline_src/bundle/arcbox.dev.entitlements" -s "Developer ID Application: ArcBox, Inc. (422ACSY6Y5)" "$baseline_src/target/release/hv_e2e"
ARCBOX_HV_E2E_BOOT_ONLY=0 "$baseline_src/target/release/hv_e2e"
```

## Results

| Check | Result | Evidence |
|---|---|---|
| Completion unit tests | Passed, including the final `&mut self` API | Both tests passed: retained completion and rejection of a vCPU exit without poweroff. |
| Formatting | Passed for the final API | `cargo fmt --check` completed successfully. |
| Host Clippy | Passed for the final API | VMM, engine, and e2e targets passed with `-D warnings`. |
| Linux VMM compile check | Passed for the final API | The cross-target check completed; unrelated Linux warnings remained. |
| Initial boot-only probe | Passed in 2.92 s total test time | `PSCI SYSTEM_OFF` preceded `Stopping VMM`; the completion assertions passed. |
| Initial full probe | Failed before shutdown | Phase 4.5 produced a guest kernel Oops in `dax_disassociate_entry.isra.0` → `dax_insert_entry`; `MmapReadFile` then timed out. |
| Baseline host full probe | Reproduced the same failure | Unmodified `1106545e` host, identical kernel/rootfs/current agent and probe knobs; the same DAX Oops and timeout. |
| Final API release build | Passed in 3 min 14 s | Both the release probe and daemon compiled; the build command exited with status 0. |
| Final API boot-only probe | Passed in 4.14 s total test time | The real HV run observed `PSCI SYSTEM_OFF` before `Stopping VMM`; all completion assertions passed. |
| HV daemon `virtio_debug` | Passed in 12.41 s | The test completed with status 0. |
| HV daemon `boot_assets` | Passed in 47.00 s | The metrics report `backend: "hv"` and `passed: true`; the harness does not assert daemon shutdown status. |
| Daemon shutdown evidence | Passed in a 46.76 s `RUST_LOG=info` boot rerun | The daemon exited with status 0 after PSCI poweroff, VMM teardown, and runtime shutdown; a 5 s server-drain timeout was still observed. |
| Daemon storage recovery fixture | Pending authorization | The complete recovery fixture has not run; the boot and debug checks do not establish recovery success. |

Initial evidence is in `/tmp/arcbox-storage-hv-poweroff-{tests,clippy,linux,probe,only}.log`. The baseline evidence is in `/tmp/arcbox-storage-hv-baseline-{build,probe}.log`. Final API checks are in `/tmp/arcbox-storage-hv-exclusive-{tests,clippy,linux,build,probe,virtio,boot,shutdown}.log`. Initial boot metrics are in `/private/var/folders/22/mcnmg__54l7br31ms3k_fs1m0000gn/T/arcbox-boot-test-yuYCX3/metrics.json`. The INFO rerun retained `/private/var/folders/22/mcnmg__54l7br31ms3k_fs1m0000gn/T/arcbox-boot-test-JokZKb/log/daemon.log`. These local logs are diagnostics; the checked-in probes above define the reproducible procedure.

The INFO daemon log records `PSCI SYSTEM_OFF` at 07:15:50.303161 UTC (line 1583), before `Stopping VMM` at 07:15:50.303433 (line 1586), then `Custom VMM stopped` (line 1677), runtime shutdown completion (line 1683), and daemon completion (line 1684). The harness confirms exit status 0. The run still logged `Server drain timed out after 5s, aborting remaining tasks` at line 1536. The unmodified baseline daemon was not tested, so this experiment does not attribute that timeout.

## Findings and open checks

1. Both boot-only runs support all three completion hypotheses when combined with the unit tests. The 2.92 s and 4.14 s values measure total test time, not shutdown performance.
2. The public wait API now requires `&mut self`, which prevents concurrent waiters from consuming one completion event. Its unit, compile, and real HV boot-only checks passed.
3. The host comparison isolates the host diff only. No baseline guest agent was run, so this result does not establish that the complete baseline stack has the same DAX failure. The DAX cause remains unresolved.
4. PSCI `SYSTEM_OFF` proves that the guest reached poweroff. It does not prove successful filesystem writes or a clean unmount. The host must still stop and join its workers.
5. The full HV probe remains failed. The daemon boot and shutdown checks passed, but these checks do not verify storage recovery. A complete recovery scenario must separately establish offline filesystem checks and durable writes before protection is released.
