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

- [Packet primitives](../common/arcbox-packet/README.md) — IP views, Ethernet framing, and checksums
- [ArcBox Engine](../engine/arcbox-engine/README.md) — machine lifecycle, force-stop completion, and storage maintenance
- [Local domains and HTTPS](local-https.md) — service contract, trust operations, and completion requirements
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

- [Architecture charter — the engine/computer restructure](architecture/charter.md) (active, product scope revised 2026-10-08)
- [VM stack redesign — the P4b execution design](architecture/vm-stack-redesign.md) (locked, 2026-08-16)
- [HV backend architecture](architecture/hv-backend.md) (current, 2026-08-13; interrupt flow amended by ADR 0001)
- [VirtIO queue abstraction convergence](architecture/virtio-queue-convergence.md) (historical, 2026-06)

### Decisions

- [0009 — Host work must drain before machine teardown](adr/0009-host-work-drains-before-machine-teardown.md) (2026-10-09; supersedes ADR 0003 decision 5)
- [0008 — The local CA covers concrete names under `.local`](adr/0008-local-ca-covers-local-domains.md) (2026-10-08)
- [0007 — Local HTTPS authorization is independent of startup](adr/0007-local-https-independent-of-startup.md) (2026-10-08)
- [0006 — Authorize local CA trust in the login user's session](adr/0006-authorize-local-ca-in-user-session.md) (2026-10-08)
- [0005 — The privileged helper manages local CA trust](adr/0005-helper-manages-local-ca-trust.md) (2026-10-08; trust mutation and authorization superseded by ADR 0006)
- [0004 — The macOS Keychain owns the local CA key](adr/0004-local-ca-signs-in-macos-keychain.md) (2026-10-07)
- [0001 — On the HV backend, asserting the SPI is the whole wake](adr/0001-hv-spi-is-the-whole-wake.md) (2026-09-30)
- [0002 — A distro machine's root is served to the host by a userspace NFSv3 server in its agent, mounted over the bridge NIC](adr/0002-machine-root-export-over-userspace-nfsv3.md) (2026-10-03; decisions 4 and 5 superseded by ADR 0003)
- [0003 — One host mount root: `~/ArcBox` is a plain directory, with the docker export at `docker/` and every running machine's root at `machines/<name>`](adr/0003-single-host-mount-root.md) (2026-10-04)
- [0004 — ArcBox is a local macOS product](adr/0004-macos-local-product.md) (2026-10-08; decisions 2 and 3 superseded by ADR 0006)
- [0005 — Storage recovery repairs only listed findings, and only on a copy](adr/0005-storage-recovery-repairs-listed-findings-on-a-copy.md) (2026-10-20)
- [0006 — Self-hosted GitHub Actions runners return as ArcBox Runners](adr/0006-restore-arcbox-runners.md) (2026-10-25)

### Plans

- [VirtIO improvements](plans/virtio-improvements.md) (historical, 2026-04)

### Experiments

- [2026-10-09 — Does the Keychain CA reject other application identities?](experiments/2026-10-09-keychain-signing-identity.md)
- [2026-10-09 — Can a Linux CA bundle preserve macOS SSL trust?](experiments/2026-10-09-macos-trust-portability.md)
- [2026-10-08 — Why can copying into a stopped container fail with OverlayFS EBUSY?](experiments/2026-10-08-overlay-copy-lifetime.md) — [guest lifecycle recorder](../tests/bench/signoz/probe-overlay-lifecycle.sh), [lifecycle analyzer](../tests/bench/signoz/analyze_overlay_lifecycle.py), [experimental exit barrier](../tests/bench/signoz/moby-overlay-exit/README.md)
- [2026-10-08 — Does ClickHouse still stall with a low-cost observer?](experiments/2026-10-08-clickhouse-native-observer.md) — [native observer](../tests/bench/signoz/observe_clickhouse.c)
- [2026-10-08 — Can kernel events retain the ClickHouse stall path?](experiments/2026-10-08-clickhouse-kernel-events.md) — [recorder controller](../tests/bench/signoz/clickhouse_trace.py), [guest helper](../tests/bench/signoz/trace-clickhouse.sh), [futex PC helper](../tests/bench/signoz/trace-clickhouse-pc.sh), [recorder lifecycle regressions](../tests/bench/signoz/test_clickhouse_trace.py), [PC ownership and export regressions](../tests/bench/signoz/test_trace_clickhouse_pc.py), [capture validator](../tests/bench/signoz/validate_clickhouse_trace.py), [spin/runtime analyzer](../tests/bench/signoz/analyze_clickhouse_spin.py), [stack integrity and association checks](../tests/bench/signoz/analyze_clickhouse_stacks.py), [host scheduler analyzer](../tests/bench/signoz/analyze_host_ktrace.py), [host scheduler extraction regression](../tests/bench/signoz/test_host_ktrace.py)
- [2026-10-08 — Can host CPU contention stall real ClickHouse threads?](experiments/2026-10-08-clickhouse-restart-contention.md) — [procfs reader probe](../tests/bench/signoz/probe_status_read.sh), [reader checks](../tests/bench/signoz/analyze_status_read.py)
- [2026-10-08 — Can a delayed waiter block a condition-variable notifier?](experiments/2026-10-08-condvar-notification-latency.md) — [combined probe](../tests/bench/signoz/cond_scheduler_latency.c), [host driver](../tests/bench/signoz/run_cond_scheduler.py), [evidence checks](../tests/bench/signoz/analyze_cond_scheduler.py)
- [2026-10-08 — Which task clock charges host suspension?](experiments/2026-10-08-thread-cpu-accounting.md)
- [2026-10-08 — Can HV expose correct execution time, and does Colima avoid CPU accounting divergence?](experiments/2026-10-08-hv-cpu-accounting-and-colima.md)
- [2026-10-05 — Does HV observe guest poweroff before host teardown?](experiments/2026-10-05-hv-poweroff-completion.md)
- [2026-10-05 — Does a real storage I/O failure reach the protection and recovery checks?](experiments/2026-10-05-storage-recovery-io-fault.md)
- [2026-10-03 — Can a machine's root be served to the Mac by its agent, read-write, fast enough to work in?](experiments/2026-10-03-machine-root-export.md)
- [2026-10-01 — Why does a miss under `arcbox.local` take 10 s on macOS, and which answer ends it?](experiments/2026-10-01-local-domain-negative-answers.md)
- [2026-09-30 — How a guest vCPU actually gets woken on HV](experiments/2026-09-30-hv-wake-path.md)
- [2026-06-17 — Host tunnel proof: `tun_proxy`](experiments/2026-06-17-surge-tun-proxy.md)

### Benchmarks

- [IPv4 UDP benchmark runner](../tests/bench/xnu-udp/README.md) — [driver](../tests/bench/xnu-udp/run.sh), environment capture, and Criterion send/receive modes
- [IPv4 UDP receive](benchmarks/udp-receive.md) — batch receive contract and loopback measurements
- [HV wake path](benchmarks/hv-wake.md) — host→guest vsock bulk, RPC latency, idle CPU
- [VirtioFS datapath](benchmarks/virtiofs.md) — measured performance and known limits
- [Network datapath](benchmarks/network.md) — measured performance and known limits

### Logs

- [2026-10-30 — Container CA injection retries archive requests that hit a busy overlay](logs/2026-10-30-ca-injection-overlay-busy.md)
- [2026-10-30 — The consumerless arcbox-net-virtio crate is removed](logs/2026-10-30-remove-net-virtio.md)
- [2026-10-26 — Native TCP ownership and silent rejection share one bridge](logs/2026-10-26-native-tcp-drop-integration.md)
- [2026-10-24 — TCP egress resolution supports silent rejection](logs/2026-10-24-tcp-egress-drop.md)
- [2026-10-23 — E2E finalization requires successful daemon exit](logs/2026-10-23-e2e-finalization.md)
- [2026-10-22 — Host egress IPv4 header guard](logs/2026-10-22-host-ipv4-guard.md)
- [2026-10-22 — Inbound UDP associations](logs/2026-10-22-inbound-udp-associations.md)
- [2026-10-22 — Abort ownership coverage](logs/2026-10-22-abort-ownership.md)
- [2026-10-22 — Explicit TCP abort](logs/2026-10-22-explicit-tcp-abort.md)
- [2026-10-22 — TCP CLOSED replies](logs/2026-10-22-tcp-closed-replies.md)
- [2026-10-22 — Handshake cancellation](logs/2026-10-22-handshake-cancellation.md)
- [2026-10-22 — Primary TCP cancellation](logs/2026-10-22-primary-tcp-cancellation.md)
- [2026-10-22 — Reader cancellation](logs/2026-10-22-reader-cancellation.md)
- [2026-10-22 — Native TCP ownership](logs/2026-10-22-native-tcp-ownership.md)
- [2026-10-22 — IPv6 classifier activation](logs/2026-10-22-ipv6-classifier.md)
- [2026-10-22 — Shared classifier parsing](logs/2026-10-22-shared-classifier-parsing.md)
- [2026-10-22 — Classifier address types](logs/2026-10-22-classifier-address-types.md)
- [2026-10-22 — IPv6 upload coalescing](logs/2026-10-22-ipv6-upload-coalescing.md)
- [2026-10-22 — IPv6 TCP handshakes](logs/2026-10-22-ipv6-tcp-handshakes.md)
- [2026-10-22 — IPv6 established TCP parser](logs/2026-10-22-ipv6-established-tcp-parser.md)
- [2026-10-22 — IPv6 TCP bridge endpoints](logs/2026-10-22-ipv6-bridge-endpoints.md)
- [2026-10-22 — TCP segmentation extraction](logs/2026-10-22-tcp-segmentation-extraction.md)
- [2026-10-22 — Validated SYN rejection](logs/2026-10-22-validated-syn-rejection.md)
- [2026-10-22 — Egress address families](logs/2026-10-22-egress-address-families.md)
- [2026-10-22 — IPv6 L3 adapter](logs/2026-10-22-ipv6-l3-adapter.md)
- [2026-10-22 — IPv6 system-proxy endpoints](logs/2026-10-22-ipv6-system-proxy-endpoints.md)
- [2026-10-22 — Bounded IPv6 fragment reassembly](logs/2026-10-22-ipv6-fragment-reassembly.md)
- [2026-10-22 — Pure TCP reset frames](logs/2026-10-22-pure-tcp-reset-frames.md)
- [2026-10-22 — IPv4 and IPv6 TCP SYN frames](logs/2026-10-22-ipv4-ipv6-tcp-syn-frames.md)
- [2026-10-22 — IPv6 TCP control and data frames](logs/2026-10-22-ipv6-tcp-control-data-frames.md)
- [2026-10-22 — Shared TCP frame encoding](logs/2026-10-22-shared-tcp-frame-encoding.md)
- [2026-10-22 — Borrowed TCP packet views](logs/2026-10-22-borrowed-tcp-packet-views.md)
- [2026-10-22 — Borrowed IP packet views](logs/2026-10-22-borrowed-ip-packet-views.md)
- [2026-10-22 — Migrate Tokio descriptor registration](logs/2026-10-22-tokio-descriptor-registration.md)
- [2026-10-22 — Format HTTP CONNECT IPv6 authorities](logs/2026-10-22-http-connect-ipv6-authority.md)
- [2026-10-22 — Preserve IPv6 DNS resolution records](logs/2026-10-22-ipv6-dns-resolution-log.md)
- [2026-10-21 — Boot assets 0.8.10 restore bind mounts from macOS temporary directories](logs/2026-10-21-boot-0.8.10-private-mountpoint.md)
- [2026-10-20 — Storage recovery rebinds a moved pair and boots the current recovery init](logs/2026-10-20-storage-recovery-moved-pair.md)
- [2026-10-20 — Storage recovery rebuilds a stale Btrfs free space tree](logs/2026-10-20-storage-recovery-fst-repair.md)
- [2026-10-19 — `host-gateway` reaches the Mac](logs/2026-10-19-host-gateway-reaches-the-mac.md)
- [2026-10-19 — Child processes kept closed test listeners alive on macOS](logs/2026-10-19-test-listeners-held-by-children.md)
- [2026-10-19 — Docker allocates /24 networks from the container pool](logs/2026-10-19-docker-network-pool-size.md)
- [2026-10-19 — Align Clippy configuration with Rust 1.96](logs/2026-10-19-pinned-clippy.md)
- [2026-10-14 — TCP promotion and receive batches preserve protocol state](logs/2026-10-14-network-followups.md)
- [2026-10-12 — Passive-open completion preserves payload and receive window](logs/2026-10-12-passive-open-payload.md)
- [2026-10-11 — Docker archive teardown releases indexed overlay mounts](logs/2026-10-11-docker-archive-mount-lifetime.md)
- [2026-10-10 — Signed HTTPS candidates pass artifact verification](logs/2026-10-10-https-signed-candidates.md)
- [2026-10-09 — HTTPS packaging prerequisites and isolated NAT validation are ready](logs/2026-10-09-https-validation-preparation.md)
- [2026-10-08 — Custom local domains and deterministic ownership](logs/2026-10-08-custom-local-domains.md)
- [2026-10-08 — Local CA rotation and recovery](logs/2026-10-08-local-ca-rotation.md)
- [2026-10-08 — One DNS suffix reaches the host and System VM](logs/2026-10-08-instance-dns-domain.md)
- [2026-10-08 — Storage dependencies follow the workspace release version](logs/2026-10-08-storage-workspace-release.md)
- [2026-10-08 — Sandbox benchmarks distinguish creation paths and preserve measurement units](logs/2026-10-08-sandbox-benchmark-integration.md)
- [2026-10-07 — Caller-owned rootfs capacity and publication passed Linux validation](logs/2026-10-07-rootfs-capacity-integration.md)
- [2026-10-07 — Storage recovery uses published boot assets](logs/2026-10-07-storage-recovery-release-validation.md)
- [2026-10-07 — Side entries distinguish reused inode generations](logs/2026-10-07-sidecar-inode-generation.md)
- [2026-10-05 — Every agent business request requires protocol v7](logs/2026-10-05-agent-v7-admission.md)
- Application restart probes: [isolated ClickHouse driver](../tests/bench/signoz/run_clickhouse_restart.py), [configuration permission regression](../tests/bench/signoz/test_clickhouse_config.py), [retained-network regression](../tests/bench/signoz/test_clickhouse_network.py), [thread observer](../tests/bench/signoz/observe-clickhouse.sh), [strict evidence checks](../tests/bench/signoz/analyze_clickhouse_samples.py), [independent host CPU sampler](../tests/bench/signoz/sample_host_vm.py), [host/guest counter comparison](../tests/bench/signoz/compare_host_guest_cpu.py), [profiler fixture preparation](../tests/bench/signoz/prepare_profiler_fixture.py)
- [2026-10-07 — SigNoz remains idle after ClickHouse stalls](logs/2026-10-07-signoz-profiler-and-cpu-accounting.md) — Collector rollback, profiler truncation, CPU parser correction, and accounting divergence reproduced by host suspension; the original stall remains unresolved. Probes: [SQL](../tests/bench/signoz/diagnose.sql), [CPU parser](../tests/bench/signoz/cpu_ticks_repro.rs), [CPU sampler](../tests/bench/signoz/sample_cpu.py), [workload](../tests/bench/signoz/cpu_workload.sql), [VM suspension](../tests/bench/signoz/pause_vm.py), [accounting overhead](../tests/bench/signoz/accounting_overhead.c), [stall history](../tests/bench/signoz/stall_history.sql), [scheduler delay](../tests/bench/signoz/run_scheduler_debt.py)
- [2026-10-08 — The helper manages local HTTPS trust](logs/2026-10-08-helper-ca-trust.md)
- [2026-10-08 — The host signs local HTTPS certificates with a Keychain CA](logs/2026-10-08-keychain-ca-migration.md)
- [2026-09-29 — vsock RX: drain a stream per round, aim the kick](logs/2026-09-29-vsock-rx-round-and-targeted-kick.md)
- [2026-10-07 — A restored sandbox can be checkpointed and restored again](logs/2026-10-07-checkpoint-chain-live.md)
