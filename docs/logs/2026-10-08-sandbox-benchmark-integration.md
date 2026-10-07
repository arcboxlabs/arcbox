# 2026-10-08 — Sandbox benchmarks distinguish creation paths and preserve measurement units

- Type: change
- Area: `tests/e2e`
- Sources: `397b44a8`, `7ee97241`, `e40c2225`
- Integration branch: `test/metrics-benchmark-integration`, based on merged `master` at `164c57fd44efd7c62a0621877089354a707b6362`
- Status: validated; benchmark-owned resources are removed while Runtime-owned cache snapshots are preserved.

## Problem or trigger

The remaining benchmark branches contained useful p95 reporting, concurrent snapshot restores, and admission timing. The original implementation treated all `Unavailable` responses as retryable. The original metrics stored sample counts and throughput as durations. Networked `Create` can restore a cached snapshot, so an iteration number cannot establish a cold or warm path.

## What was done

The integrated serial benchmark labels the production networked path as `create-networked`, the guaranteed cold path as `cold-no-network`, and explicit snapshot restores as `restore`. Optional unique geometry forces networked cache misses by increasing memory by one MiB per iteration. Each memory size is retained in the metrics. A comparison must use the same geometry sequence because this option changes the workload.

The metrics retain ordered samples, units, sample counts, p50, p95, and maximum values. First iterations and first rounds remain separate. A single-iteration or single-round run has no steady-state percentile. Throughput uses `per_second`; sample counts remain integer metadata.

The admission helper retries only explicit startup-cleanup or requested-sandbox network-cleanup rejections. The helper propagates transport failures and uncertain commits without replaying the mutation. Admission waiting is reported separately from the successful attempt.

The concurrent benchmark uses synchronous `Restore` completion as READY. Each round checks unique nonempty IP addresses, drains every launched request, and removes every requested clone before another round. Final cleanup removes the template and its user checkpoint. The benchmark records Runtime-owned warm-cache snapshot IDs before clone rounds. Final listings must contain no sandboxes and exactly those cached snapshot IDs. Cleanup errors and artifact-write errors fail the run and retain the test directory.

## Original baseline validation

The original baseline was `9ff2c76e4d7314af5809ad876f21a78a169cff7a`. The runs in this section preceded the rebase onto `164c57fd`. Final baseline validation appears separately below.

The macOS runner reported `Darwin 25.4.0 arm64`. The original daemon and both musl agents were built in this worktree. Runtime source for these runs matched the original baseline; the following hashes do not identify the final baseline binaries. The daemon was signed with `Developer ID Application: ArcBox, Inc. (422ACSY6Y5)` and the development entitlements.

All live runs used VZ with `ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1`. These runs validate the benchmark workflow and internal sandbox networking. They do not validate bridge networking or establish comparative performance. The production daemon was not restarted.

The selected boot bundle was `0.8.9`. The static bundle came from the storage-recovery worktree. Its manifest SHA256 matched `assets.lock`: `f2003a5919551afd3f34ebeea6b20acdf0ba2bd55fd855068cc33fd8b7aa037b`. Staged agent hashes were checked against the original baseline builds after every run; no installed agent was selected implicitly.

| Binary | SHA256 |
|---|---|
| Signed `arcbox-daemon` | `e1c4d1346ebc22a5e819df8ff73631d2f05555d2a5c84594a2f70265d479a0f0` |
| `arcbox-agent` | `5822160932da3862a547007a126ecd3a8bb0623e1f796c861cc216764bea0231` |
| `vm-agent` | `9fbd50e15441e6af52493a970b840b6fe8211c79e5f693d4a054076ae3bbb98e` |

| Run label | Workload | Result |
|---|---|---|
| `serial-default` | Two iterations of each Create/Restore group | Passed; one steady sample per group |
| `serial-unique-geometry` | Two iterations per group | Passed; networked memory `[512, 513]` MiB, no-network memory `[512, 512]` MiB |
| `clone-storm-minimal` | Degrees `1,2`, two rounds each, one vCPU, 512 MiB | Passed; six restored clones, unique IP checks, clone/template/checkpoint cleanup RPCs |
| `clone-storm-cleanup` | The same clone workload with additional final listing assertions | Failed only on the new all-snapshots-empty assertion; the sandbox listing was empty |
| `clone-storm-ownership-cleanup` | The same clone workload with corrected ownership assertions | Passed; no sandboxes, no benchmark checkpoint, and the recorded Runtime cache snapshot remained |

The failed listing contained one automatic warm-create snapshot: `warm-5cd71c9db45d`, labeled `arcbox.warm_key`, with origin `storm-template`. `publish_warm_snapshot` creates this runtime-owned cache entry independently of the user checkpoint. Therefore an empty snapshot catalog is not the current Create contract. The corrected assertion compares the final snapshot ID set with the Runtime cache IDs recorded before the clone rounds. A leaked benchmark checkpoint, an unexpected snapshot, or a missing cache snapshot fails the run. The passing rerun preserved cache snapshot `7279c232-ce8b-4861-bdc8-6aa5c70057b8` and completed in 23.42 seconds.

Formatting and strict Clippy passed. The earlier targeted checks passed 24 E2E library tests and two non-ignored benchmark tests. After the ownership correction, workspace formatting, E2E all-target strict Clippy, the clone configuration test, and the live clone smoke passed. The tests cover metric units and empty sample sets, explicit admission classification, propagation of both scenario and cleanup failures, geometry overflow, and malformed concurrency input.

The evidence directory is `/private/tmp/arcbox-redwhisk-integration.MHIwSg3Y`. `metrics-remainder-readiness.json` records commits, changed-line counts, preserved run directories, provenance, and agent hashes. `metrics-smoke/*.metrics.json` contains the raw records. Build, lint, test, and smoke logs use the `metrics-remainder-` prefix. The ownership correction is recorded under `benchmark-cleanup-approval-fix/`, including the passing metrics and the unchanged daemon and agent hashes. No benchmark daemon or benchmark mount remained after the runs.

The serial smoke commands use `ARCBOX_COLDSTART_ITERS=2`. The unique-geometry run additionally uses `ARCBOX_COLDSTART_UNIQUE_GEOMETRY=1`. The clone smoke uses these settings:

```sh
SKIP_BUILD=1 KEEP_TEST_DIR=1 ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1 \
ARCBOX_STORM_DEGREES=1,2 ARCBOX_STORM_ROUNDS=2 \
ARCBOX_STORM_VCPUS=1 ARCBOX_STORM_MEMORY_MIB=512 \
cargo test -p arcbox-e2e --test sandbox_clone_storm sandbox_clone_storm -- --ignored --nocapture
```

## Final baseline validation

The seven benchmark commits were rebased onto merged `master` at `164c57fd44efd7c62a0621877089354a707b6362`. The rebase preserved all seven patches, original authors, and author dates. The previous source ref remains at `refs/archive/metrics-benchmark-pre-master-3c6a8ddc`.

Independent review found two failure-reporting gaps. Both benchmark Events subscriptions now have a 180-second deadline. One 60-second deadline covers command startup, attach headers, and output draining through exit. An archive write failure now changes `passed` to false and updates an already-written local metrics record. The caller receives the original archive error and any local update error. A real invalid archive path reproduced the previous false-positive JSON record before the fix and passed after the fix.

The functional source commit is `7d5db32a747d30345057d5225e8092b34f749c5c`. Workspace formatting, E2E all-target strict Clippy, 25 E2E library tests, and both benchmark configuration tests passed. The existing dependency future-incompatibility notice for `proc-macro-error2` remains; Clippy reported no source warnings.

The daemon and both musl agents were rebuilt at `44e3868921924e1c02ceee9a72d0160ff1607fce` after the rebase. The daemon was signed with the same Developer ID and development entitlements. All runtime source directories, the workspace manifest, lockfile, and Cargo configuration matched the functional source commit; only E2E code and its documentation changed afterward.

| Final baseline binary | SHA256 |
|---|---|
| Signed `arcbox-daemon` | `ebb4a67ad588673821450b6e935e656490d5638b655b8b12a89b160a643bc899` |
| `arcbox-agent` | `c2768a431b59e5dc8c802ab8253c77f03fe2f6bed2b6e3b9f68cad78eb1acdd4` |
| `vm-agent` | `9fbd50e15441e6af52493a970b840b6fe8211c79e5f693d4a054076ae3bbb98e` |

The `clone-storm-final-master` run passed on the clean functional source commit in 22.39 seconds. Degrees `1,2` with two rounds each restored six clones at one vCPU and 512 MiB. Final listings contained no sandboxes and no benchmark checkpoint. Runtime cache snapshot `15d9ed4b-e7e9-4660-a3d5-3069e25423c7` remained unchanged. The daemon exited successfully, and no benchmark daemon or mount remained. Staged agent hashes matched the rebuilt binaries.

Final evidence is under `/private/tmp/arcbox-redwhisk-integration.MHIwSg3Y/benchmark-cleanup-approval-fix/rebased-master/`. `validation.json` links the source, binary hashes, tests, cleanup checks, and raw metrics. The run used VZ, boot bundle `0.8.9`, and `ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1`. The result proves the functional workflow; bridge networking and comparative performance remain unverified.

## Follow-ups

Run larger sample sets under the intended network configuration before drawing performance conclusions. The previous failed assertion and its metrics remain in the evidence directory.
