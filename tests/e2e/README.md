# ArcBox E2E tests

This crate contains end-to-end tests that orchestrate ArcBox binaries, VM boot
assets, Docker, and external test fixtures. Keep long-running test process
management here rather than in `xtask`.

E2E tests use Rust's standard test harness instead of a custom runner. Tests are
ignored by default because they build release binaries, boot a VM, and require
macOS virtualization support (plus a local Docker CLI for the daemon-level test).

## Fixtures

- `signing::ensure_signed` — idempotently signs a test binary with the
  virtualization entitlements: Developer ID when the ArcBox identity is in the
  keychain, otherwise ad-hoc with `bundle/arcbox.dev.entitlements` (sufficient
  for HV/VZ; only bridged vmnet needs Developer ID).
- `daemon::DaemonHandle` — spawns a signed `arcbox-daemon` under an isolated
  `--data-dir` and waits for readiness on the daemon's `WatchSetupStatus` gRPC
  stream, failing fast on a FAILED phase or daemon exit (with the log tail).
  Drop terminates the daemon gracefully (SIGTERM, then SIGKILL). The isolated
  data dir means tests never touch `~/.arcbox` and can run alongside a
  developer's daemon.
- `DaemonHandle::dump_virtio_debug` — fetches the daemon's `GetVirtioDebug`
  snapshot (per-queue kicks, interrupts, live avail/used ring indices;
  HV backend only) into `virtio-debug.json` under the data dir. The
  boot-assets scenario and the `hv_e2e` probe capture it automatically on
  failure, while the VM is still alive.

## Parallel daemon control planes: one data dir per daemon

Persistent state — flock, gRPC/Docker sockets, logs, and machines — derives
from `--data-dir` (`HostLayout`). The harness also requests kernel-assigned
DNS and Kubernetes ports, so daemon control planes with distinct data dirs run
side by side. That is the isolation contract for parallel fix work:
**one worktree/agent = one data dir**, and the harness enforces it —
`DaemonHandle::spawn` refuses a data dir inside `~/.arcbox` or `~/.arcbox-dev`.

The container CIDR and host route are not isolated by the harness. Never run
multiple VM-booting harness daemons, or one beside a live ArcBox daemon.

Three host-global resources are *not* covered by the data dir. Tests must not
touch them:

- **Privileged helper state** (`/var/run/arcbox*`, `/etc/resolver`, host
  routes) — never run the helper from a test.
- **Docker CLI context** (`~/.docker`) — never pass `--docker-integration`;
  point the Docker CLI at the handle's socket with `DOCKER_HOST` instead.
- **Published host ports** — don't publish fixed host ports from containers;
  concurrent runs would collide.

## Commands

Daemon-level test — boots the VM through a real daemon and runs Docker
lifecycle checks over the Docker API socket:

```bash
cargo test -p arcbox-e2e --test boot_assets -- --ignored --nocapture
```

VMM-level HV probe — drives the HV backend directly (no daemon): boot, vsock
agent RPC, DAX, agent supervision, pause/resume, guest PSCI poweroff, worker
teardown. The `guest_poweroff` phase requires completion after the shutdown
request; `stop_vm` records the subsequent host teardown. Protocol v7 admission
finishes before the `guest_poweroff` timer starts. The shutdown RPC must return
`accepted = true` before the probe waits for poweroff:

```bash
cargo test -p arcbox-e2e --test hv_vmm -- --ignored --nocapture
```

Idle-balloon regression (2026-07-15 incident) — boots a VZ daemon with a
short idle timeout, holds a memory workload, lets the VM idle-shrink, then
grows the workload inside the guest and requires the guest-driven pressure
restore (no reclaim storm, Docker responsive):

```bash
cargo test -p arcbox-e2e --test idle_balloon -- --ignored --nocapture
```

Machine stats stream — boots a VZ daemon, subscribes to
`StatsService.Watch`, and asserts sane progressing samples plus the
StatsHub lifecycle (concurrent subscribers share one guest stream; the
pump stops when the last one leaves):

```bash
cargo test -p arcbox-e2e --test stats_watch -- --ignored --nocapture
```

Kubernetes LoadBalancer forwarding — boots a VZ daemon, starts k3s, and
follows a LoadBalancer Service's port on the host: it answers and is reported
forwarded, moves with the Service, closes when the Service is deleted, and is
gone when a Kubernetes stop returns. k3s pulls klipper-lb and the workload
image (`ARCBOX_E2E_IMAGE`) itself, so the guest needs registry access:

```bash
cargo test -p arcbox-e2e --test kubernetes_lb -- --ignored --nocapture
```

Dual-backend matrix — the boot-assets scenario once per backend, VZ first.
VZ is the oracle: HV-only red means an HV implementation bug, double red means
the bug is above the hypervisor layer:

```bash
cargo test -p arcbox-e2e --test backend_matrix -- --ignored --nocapture
```

Stress runner — repeated runs with artifact capture (per-run logs under
`target/e2e-artifacts/<unix-time>/`, failing runs' preserved data dirs
recorded in the summary). Race fixes need a cheap red first:

```bash
cargo xtask e2e --backend both --repeat 20
cargo xtask e2e --test hv_vmm --backend hv --repeat 200 --fail-fast
```

When using the repository development shell, prefix with `devenv shell --`.

List available E2E tests without running ignored tests:

```bash
cargo test -p arcbox-e2e -- --list
```

## Configuration

Environment variables read by the tests:

- `SKIP_BUILD=1` — reuse existing `target/release` binaries.
- `KEEP_TEST_DIR=1` — always preserve the temporary test directory (it is
  preserved automatically when the test fails).
- `ARCBOX_BOOT_ASSET_VERSION=<version>` — override `assets.lock`
  `[boot].version`.
- `ARCBOX_VM_BACKEND=vz|hv` — System VM backend for the daemon under test
  (first-boot default; the daemon reads the same variable). The matrix test
  overrides it per run. To pin the guest vCPU count at this level, set
  `ARCBOX_VM_CPUS=N` in the environment (default: host core count) — the
  spawned daemon inherits it via the config env layer. The isolated data
  dir's `config.toml` is NOT read (`Config::load_for_profile` merges only
  the user/system config files plus `ARCBOX_*` env), and editing the user
  config would leak state across parallel runs. The `ARCBOX_HV_E2E_VCPUS`
  knob only drives the bare `hv_e2e` probe, not daemon-level tests.
- `ARCBOX_E2E_IMAGE=<ref>` — container image for the lifecycle tests
  (default `alpine:latest`). Point it at a mirror on networks where the
  guest cannot reach docker.io.
- `ARCBOX_GUEST_DOCKER_VSOCK_PORT=<port>` — pass a custom guest Docker vsock
  port to `arcbox-daemon`.
- `ARCBOX_IDLE_TIMEOUT_SECS=<secs>` — shorten the daemon's VM idle timeout
  (default 300); the idle-balloon test sets it to 20.
- `ARCBOX_HV_E2E_KERNEL` / `ARCBOX_HV_E2E_ROOTFS` / `ARCBOX_HV_E2E_TIMEOUT` /
  `ARCBOX_DATA_DIR` — HV probe overrides; see `src/bin/hv_e2e.rs`.
- `ARCBOX_E2E_METRICS_DIR` / `ARCBOX_E2E_RUN_LABEL` — archive per-run phase
  timings as `<label>.metrics.json` in the given directory (set automatically
  by `cargo xtask e2e`). Runs also write `metrics.json` into their data dir.
  Each record includes the same label, separate argv entries, host OS/kernel/architecture, and Git commit/dirty state. Failed provenance probes return an error before the measured phases.
  The harness records the resolved boot version passed to the daemon; the direct HV probe records its selected kernel/rootfs paths. Checkout state describes the repository at run time, not the build revision of existing binaries. A boot version identifies the selected bundle generation; locally staged binary overrides can differ.
- `ARCBOX_E2E_IPERF_IMAGE=<ref>` — guest iperf3 image for the
  `network_iperf` throughput matrix (default `networkstatic/iperf3:latest`).
- `ARCBOX_E2E_IPERF_MIN_GBPS=<f64>` — `network_iperf` gate floor. Unset (or
  0) gates on liveness only (VZ throughput is too run-to-run variable for an
  automated target); set it on a quiet machine to enforce a real per-host
  throughput floor. A present-but-invalid value fails the test rather than
  silently disabling the gate.

Tracing is controlled with `RUST_LOG`; it defaults to `info` when unset.

## Sandbox benchmark statistics

`sandbox_coldstart` records its first iteration separately. Later iterations report p50, p95, and maximum latency. A single-iteration run has no steady-state percentile. `metrics.json` retains phase durations in seconds and records each sample distribution with an explicit unit and sample count. A sample count or a throughput value is never a phase duration.

Sandbox benchmarks retry only explicit network-cleanup admission rejections, for at most 60 seconds between attempts. Each successful attempt has a 180-second RPC deadline. Metrics report admission waiting separately from the successful attempt’s latency; transport failures and uncertain commits fail the run.

The `create-networked` group measures the production Create path, which may boot or restore a cached snapshot. `cold-no-network` always boots without the warm-create cache. `ARCBOX_COLDSTART_UNIQUE_GEOMETRY=1` changes the networked group to `cold-networked-unique-geometry`: each iteration adds one MiB to `ARCBOX_COLDSTART_MEMORY_MIB`, forcing a distinct cache key. This changes the workload; metrics retain each memory size in MiB, and comparisons must use the same geometry sequence. `restore` measures an explicit snapshot restore. No iteration number proves that a Create used a warm snapshot.

`ARCBOX_E2E_REPO_ROOT=<path>` selects the runtime checkout, assets, and binaries for a prebuilt test executable. Metrics record that checkout, which can differ from the test executable’s build revision.

## Concurrent snapshot restores

Run `cargo test -p arcbox-e2e --test sandbox_clone_storm -- --ignored --nocapture` on a host with nested virtualization. The benchmark warms one template, checkpoints it, and restores clones concurrently with separate network identities. Synchronous Restore completion is the READY point. Each round checks unique nonempty IP addresses, waits for all requests to finish, and removes every clone before the next round. Template and snapshot cleanup failures fail the run. Final listings must contain no sandboxes and exactly the Runtime warm-cache snapshots recorded before the clone rounds; the benchmark checkpoint must be absent.

`ARCBOX_STORM_DEGREES` selects unique positive concurrency degrees (default `1,2,4,8,16`). `ARCBOX_STORM_ROUNDS` defaults to `3`; `ARCBOX_STORM_VCPUS` defaults to `2`; `ARCBOX_STORM_MEMORY_MIB` defaults to `2048`. Invalid values fail before daemon startup. The first round of each degree is reported separately. A one-round smoke has no steady-state samples.

Metrics retain each round's restore and admission-wait samples, wall time from task launch to the last RPC completion, and throughput in clones per second. Wall time includes admission waiting and excludes cleanup. Steady summaries exclude the first round and retain p50, p95, maximum, units, and sample counts. A failed round preserves its successful samples and stops the workload after cleanup.
