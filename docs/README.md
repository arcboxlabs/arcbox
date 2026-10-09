# ArcBox engineering docs

`AGENTS.md` files hold the rules an agent or engineer must follow while
editing a directory. This tree holds everything else worth keeping: how a
subsystem works, what was decided and why, what was tried, and what was
measured.

## Layout

| Directory | Holds | Naming | Lifecycle |
|---|---|---|---|
| `docs/*.md` | **Reference**: how a subsystem works today (`daemon-lifecycle.md`, `data-directories.md`, `helper.md`, …). | `<topic>.md` | Rewritten in the change that alters the behavior. |
| `docs/architecture/` | **Designs** spanning many crates that outlive any one change (charter, stack designs, backend architecture). | `<topic>.md`, `Status:` line first | Status moves active → locked → historical; never deleted. |
| `docs/adr/` | **Decisions**: one per file, the alternatives and the evidence it rests on. | `NNNN-<slug>.md` | Immutable once accepted; a change of mind is a new ADR that supersedes it. |
| `docs/plans/` | **Execution plans** for a piece of work: scope, steps, acceptance. Internal plans stay in the company repo (`engineering/arcbox/plans/`). | `<topic>.md`, `Status:` line first | Closed (not deleted) when done, pointing at the log entry or ADR with the outcome. |
| `docs/experiments/` | **Experiments**: a question, hypotheses, a method, results, a verdict. One-off by nature; if it gets rerun it becomes a benchmark. | `YYYY-MM-DD-<slug>.md` | Append-only; a correction is a new entry linking back. |
| `docs/benchmarks/` | **Benchmarks**: one file per subject with a stable method and dated result rows that stay comparable over time, plus the analysis of what they mean. | `<subject>.md` | Method stable; results append; analysis rewritten. |
| `docs/logs/` | **Logs**: chronological record of changes, investigations and incidents that are neither an experiment nor a benchmark — what happened, with the numbers that motivated and proved it. | `YYYY-MM-DD-<slug>.md` | Append-only. |

Templates: `adr/TEMPLATE.md`, `experiments/TEMPLATE.md`,
`benchmarks/TEMPLATE.md`, `logs/TEMPLATE.md`. Probes and drivers behind any
number in this tree are checked in under `tests/bench/<name>/` or as an e2e
target; a number that cannot be rerun is not recorded.

## Which one to write

- Changed behavior or a contract → the reference doc, and the directory's
  `AGENTS.md` if a rule changed.
- Asked a question and answered it with data ("does X need Y?") → an
  experiment. Even a "nothing to do" verdict is recorded.
- Measured something that will be measured again (throughput, latency, idle
  CPU, boot time) → a row in the subject's benchmark file, creating the file
  with its method if it is the first.
- Chose between designs, retired a mechanism, or set a rule a later change
  must not undo silently → an ADR. It cites experiments and benchmarks; it
  does not repeat their tables.
- Did a piece of work worth remembering that is none of the above (a
  root-cause hunt, an incident, a change whose reasoning is not obvious from
  the commit) → a log entry.
- About to start multi-step work → a plan, with its decisions already
  locked (root `AGENTS.md` "Planning").

Entries are written in the same commit series as the work, not afterwards
from memory. Every new file is added to the index below.

## Index

### Reference

- [Daemon lifecycle](daemon-lifecycle.md) — startup pipeline, lock/handoff, residual state
- [Data directories](data-directories.md) — every path the daemon writes
- [Boot assets](boot-assets.md) — the kernel/rootfs bundle and its pin
- [Disk reclaim](disk-reclaim.md) — how freed guest space returns to the host
- [arcbox-helper](helper.md) — the privileged helper
- [Code signing](code-signing-troubleshooting.md)
- [macOS guest VMs](macos-guest.md)
- [Sandbox gRPC API](sandbox-api.md)
- [Coding agents in a sandbox](agent-sandbox.md)

### Architecture

- [Architecture charter — the engine/computer restructure](architecture/charter.md) (active, 2026-08-13)
- [VM stack redesign — the P4b execution design](architecture/vm-stack-redesign.md) (locked, 2026-08-16)
- [HV backend architecture](architecture/hv-backend.md) (current, 2026-08-13; interrupt flow amended by ADR 0001)
- [VirtIO queue abstraction convergence](architecture/virtio-queue-convergence.md) (historical, 2026-06)

### Decisions

- [0001 — On the HV backend, asserting the SPI is the whole wake](adr/0001-hv-spi-is-the-whole-wake.md) (2026-09-30)
- [0002 — A distro machine's root is served to the host by a userspace NFSv3 server in its agent, mounted over the bridge NIC](adr/0002-machine-root-export-over-userspace-nfsv3.md) (2026-10-03; decisions 4 and 5 superseded by ADR 0003)
- [0003 — One host mount root: `~/ArcBox` is a plain directory, with the docker export at `docker/` and every running machine's root at `machines/<name>`](adr/0003-single-host-mount-root.md) (2026-10-04)

### Plans

- [VirtIO improvements](plans/virtio-improvements.md) (historical, 2026-04)

### Experiments

- [2026-10-08 — Why can copying into a stopped container fail with OverlayFS EBUSY?](experiments/2026-10-08-overlay-copy-lifetime.md) — [guest lifecycle recorder](../tests/bench/signoz/probe-overlay-lifecycle.sh), [lifecycle analyzer](../tests/bench/signoz/analyze_overlay_lifecycle.py), [experimental exit barrier](../tests/bench/signoz/moby-overlay-exit/README.md)
- [2026-10-08 — Does ClickHouse still stall with a low-cost observer?](experiments/2026-10-08-clickhouse-native-observer.md) — [native observer](../tests/bench/signoz/observe_clickhouse.c)
- [2026-10-08 — Can kernel events retain the ClickHouse stall path?](experiments/2026-10-08-clickhouse-kernel-events.md) — [recorder controller](../tests/bench/signoz/clickhouse_trace.py), [guest helper](../tests/bench/signoz/trace-clickhouse.sh), [futex PC helper](../tests/bench/signoz/trace-clickhouse-pc.sh), [PC ownership and export regressions](../tests/bench/signoz/test_trace_clickhouse_pc.py), [capture validator](../tests/bench/signoz/validate_clickhouse_trace.py), [spin/runtime analyzer](../tests/bench/signoz/analyze_clickhouse_spin.py), [stack integrity and association checks](../tests/bench/signoz/analyze_clickhouse_stacks.py), [host scheduler analyzer](../tests/bench/signoz/analyze_host_ktrace.py), [host scheduler extraction regression](../tests/bench/signoz/test_host_ktrace.py)
- [2026-10-08 — Can host CPU contention stall real ClickHouse threads?](experiments/2026-10-08-clickhouse-restart-contention.md) — [procfs reader probe](../tests/bench/signoz/probe_status_read.sh), [reader checks](../tests/bench/signoz/analyze_status_read.py)
- [2026-10-08 — Can a delayed waiter block a condition-variable notifier?](experiments/2026-10-08-condvar-notification-latency.md) — [combined probe](../tests/bench/signoz/cond_scheduler_latency.c), [host driver](../tests/bench/signoz/run_cond_scheduler.py), [evidence checks](../tests/bench/signoz/analyze_cond_scheduler.py)
- [2026-10-08 — Which task clock charges host suspension?](experiments/2026-10-08-thread-cpu-accounting.md)
- [2026-10-08 — Can HV expose correct execution time, and does Colima avoid CPU accounting divergence?](experiments/2026-10-08-hv-cpu-accounting-and-colima.md)
- [2026-10-03 — Can a machine's root be served to the Mac by its agent, read-write, fast enough to work in?](experiments/2026-10-03-machine-root-export.md)
- [2026-10-01 — Why does a miss under `arcbox.local` take 10 s on macOS, and which answer ends it?](experiments/2026-10-01-local-domain-negative-answers.md)
- [2026-09-30 — How a guest vCPU actually gets woken on HV](experiments/2026-09-30-hv-wake-path.md)
- [2026-06-17 — Host tunnel proof: `tun_proxy`](experiments/2026-06-17-surge-tun-proxy.md)

### Benchmarks

- [HV wake path](benchmarks/hv-wake.md) — host→guest vsock bulk, RPC latency, idle CPU
- [VirtioFS datapath](benchmarks/virtiofs.md) — measured performance and known limits
- [Network datapath](benchmarks/network.md) — measured performance and known limits

### Logs

- Application restart probes: [isolated ClickHouse driver](../tests/bench/signoz/run_clickhouse_restart.py), [configuration permission regression](../tests/bench/signoz/test_clickhouse_config.py), [retained-network regression](../tests/bench/signoz/test_clickhouse_network.py), [thread observer](../tests/bench/signoz/observe-clickhouse.sh), [strict evidence checks](../tests/bench/signoz/analyze_clickhouse_samples.py), [independent host CPU sampler](../tests/bench/signoz/sample_host_vm.py), [host/guest counter comparison](../tests/bench/signoz/compare_host_guest_cpu.py), [profiler fixture preparation](../tests/bench/signoz/prepare_profiler_fixture.py)
- [2026-10-07 — SigNoz remains idle after ClickHouse stalls](logs/2026-10-07-signoz-profiler-and-cpu-accounting.md) — Collector rollback, profiler truncation, CPU parser correction, and accounting divergence reproduced by host suspension; the original stall remains unresolved. Probes: [SQL](../tests/bench/signoz/diagnose.sql), [CPU parser](../tests/bench/signoz/cpu_ticks_repro.rs), [CPU sampler](../tests/bench/signoz/sample_cpu.py), [workload](../tests/bench/signoz/cpu_workload.sql), [VM suspension](../tests/bench/signoz/pause_vm.py), [accounting overhead](../tests/bench/signoz/accounting_overhead.c), [stall history](../tests/bench/signoz/stall_history.sql), [scheduler delay](../tests/bench/signoz/run_scheduler_debt.py)
- [2026-09-29 — vsock RX: drain a stream per round, aim the kick](logs/2026-09-29-vsock-rx-round-and-targeted-kick.md)
