# Network datapath — measured performance and known limits

Benchmark record. Method and probes are described inline; results are dated and must be compared same-context (see `docs/README.md` "Benchmarks").

As of `net-perf-mergeable-rx` tag (commit `eb45a46`, 2026-04-24). Host: Apple Silicon, macOS 26.0.1. Guest: Linux under HV backend. Bench: `iperf3` client on host against `networkstatic/iperf3` container in guest, port-forwarded.

## Headline numbers

| Direction | Single stream | `-P 4` | VZ reference |
|---|---|---|---|
| Host → VM | **22.7 Gbps** | (see below) | ~10 Gbps |
| VM → Host | **15.0 Gbps** | **19.1 Gbps** (0 retrans) | ~10 Gbps |

Single-stream exceeds Apple's native VZ virtio-net by ~2×. See `virt/arcbox-net-inject/src/inject.rs` — the inject thread uses `VIRTIO_NET_F_MRG_RXBUF` with a vectored `readv()` across up to 16 descriptors per syscall, so one GSO_TCPV4 frame fans out into 40+ MSS-sized segments on the guest.

## Known limitation: Host → VM collapses under multi-flow saturation

When multiple parallel Host → VM flows each try to push above ~5 Gbps at once, throughput collapses. One flow usually survives near full speed; the others drop to near-zero at the receiver. Measurement at `-P 2` with rate caps (`iperf3 -b`):

| Rate cap per flow | Receiver total | Per-flow status |
|---|---|---|
| `-b 500M` | 1.0 Gbps | both clean |
| `-b 2G` | 4.0 Gbps | both clean |
| `-b 4G` | 6.6 Gbps | 17% loss on one flow |
| `-b 6G` | 6.0 Gbps | one flow full, **other ≈ 0** |
| `-b 8G+` | ≈ 1 Gbps | one flow survives, rest drop |
| `-b 2G` with `-P 4` | 8.0 Gbps | all four clean |

Unbounded `-P 2/3/4` at line rate reproduces the collapse. The per-flow breaking point is roughly 4–5 Gbps each.

## What we've ruled out

- **Inject scheduler fairness.** Both the serial drain-per-conn and a round-robin across conns produce the same collapse under saturating load. The hot loop isn't starving conns.
- **RX ring capacity.** `QUEUE_NUM_MAX` is already 1024 (`virt/arcbox-vmm/src/device/mmio_state.rs:163`); bumping the inject-side caps didn't change the failure mode.
- **Inject CPU.** The host-side inject thread is nowhere near pegged during these tests.

## What we haven't checked yet

Three cheap guest-side experiments to run before considering structural changes (multi-queue virtio-net, new backend, etc.):

1. **Raise `QUEUE_NUM_MAX` from 1024 to 2048** and rerun `-P 2 -b 6G`. If the picture changes (both flows get data but still lossy), ring capacity/fairness is real. If unchanged, the bottleneck is deeper.
2. **Guest `ethtool -S` counters** (`rx_missed`, `rx_no_buffer`) + **`/proc/interrupts` CPU distribution** during `-P 2 -b 6G`. If all virtio-input IRQs land on one guest CPU, multi-queue is justified. If CPU distribution is fine, look elsewhere (GRO, softirq budget).
3. **Guest softirq %** during `-P 2 -b 4G`. If `%si` is pegged on one CPU, it's a NAPI-budget problem — try tuning `netdev_budget` / `netdev_budget_usecs` inside the guest before any structural work.

Pick the fix based on which of those three actually moves the needle. Multi-queue virtio-net is substantial (frontend, backend N threads, feature negotiation, flow hashing) and should not be speculated into.

## Diagnostic results (2026-04-24)

Ran all three. Short answer: **bottleneck is not guest-side**.

**Experiment 1 — `QUEUE_NUM_MAX` 1024 → 2048**: unchanged. `-P 2 -b 6G` still collapses to ~550 Kbps receiver. Ring size is not the cap.

**Experiment 2 — guest `ethtool` + `/proc/interrupts`**:

| Signal | Value |
|---|---|
| `ethtool -l eth0` combined queues | **1** (current = max, no multi-queue negotiated) |
| `ethtool -g eth0` RX/TX rings | 1024 / 1024 |
| `ethtool -S eth0` rx_drops after `-b 6G -t 20` | **0** (no loss at the NIC) |
| `ethtool -k eth0` GRO/TSO/LRO | all on |
| `/proc/interrupts` virtio1 | **7499 on CPU0, 0 on CPU1-3** (pinned) |

Interrupts pin to CPU0, but see experiment 3 — that CPU is not saturated.

**Experiment 3 — guest `mpstat` during `-P 2 -b 6G -t 20`** (steady state, iperf3 SUM ≈ 10.3 Gbps):

| CPU | %sys | %soft | %idle |
|---|---|---|---|
| 0 | 9.6 | 23.0 | **66.7** |
| 1 | 0 | 1.5 | 98.5 |
| 2 | 0 | 0 | 100 |
| 3 | 0 | 0 | 100 |

CPU0 has **65–70 % idle headroom** — NAPI/softirq is nowhere near saturated. Multi-queue would parallelize work CPU0 already handles comfortably. It is not the right fix.

### What this rules out

- **Ring capacity** (ruled out by exp 1)
- **Guest NIC drops** (ruled out by exp 2 — `rx_drops=0`)
- **Guest softirq CPU saturation** (ruled out by exp 3 — CPU0 at 25 % soft)
- **Multi-CPU parallelism** (ruled out by exp 3 — CPU0 has headroom)

### What this implicates

The host-side pipeline, not the guest. Candidates:

- **ACK-intercept path** in the tokio datapath loop (`try_fast_path_intercept` in `tcp_bridge.rs`) runs on a single tokio task. Each guest ACK frame does a HashMap lookup, a TCP flow update, a `stream.write` to the host socket, and an ACK-frame build — per ACK, sequentially across all flows. `tx_kicks = 660 k` during the 20 s test, so the datapath processed ~33 k ACKs/sec single-threaded.
- **Inject thread** sharing one CPU for N flows — but mpstat on the host wasn't captured; this is the next thing to measure.
- **Host-side flow-control interaction** with loopback TCP.

### Repeatability note

The 5-second `-P 2 -b 6G` run that showed "one flow 6 Gbps, other 275 Kbps" is a transient startup pattern. Over 20 seconds the flows converge to ~6 + 4.3 Gbps = ~10.3 Gbps aggregate. The *steady-state* cap is 10–12 Gbps combined, roughly half of the single-flow 22 Gbps. The pipeline isn't collapsing — it just doesn't scale beyond one flow's worth of throughput.

### Next step

Host-side profile, not guest-side structural work. Specifically, check inject-thread CPU on the host and profile `try_fast_path_intercept` during `-P 2 -b 6G`.

## Host-side profile (2026-04-24, follow-up)

Sampled the daemon with `/usr/bin/sample` during both `-t 30` runs. Results below are per-thread active samples (total − wait-state syscalls) out of ~12,400 samples per thread over 15 s. Single core = 100 %.

**P1 single-flow, receiver 29.5 Gbps:**

| Thread | Active | % core |
|---|---|---|
| hv-vcpu-0 | 10844 | 88 % |
| rx-inject | 1456 | 12 % |
| tokio-rt-worker (busy A) | 952 | 8 % |
| tokio-rt-worker (busy B) | 879 | 7 % |
| tokio-rt-worker (busy C) | 163 | 1 % |
| **Total active** | **14502** | **118 %** |

**P2 dual-flow `-b 6G`, receiver 10.9 Gbps:**

| Thread | Active | % core |
|---|---|---|
| hv-vcpu-0 | 8297 | 67 % |
| **rx-inject** | **4418** | **36 %** |
| hv-vcpu-1/2/3 | ~980 | 8 % combined |
| tokio-rt-worker (3 workers combined) | ~730 | 6 % combined |
| **Total active** | **~15000** | **121 %** |

Both runs burn **≈ 120 % of one core**. P1 delivers 29.5 Gbps with that budget; P2 delivers 10.9 Gbps. **Same CPU, 2.7× less throughput.** That rules out every hypothesis that starts with "we need more CPU cycles" — multi-queue, tokio restructuring, work-stealing — none of those add throughput when the budget isn't the constraint.

### What shifted between P1 and P2

- `hv-vcpu-0` dropped from 88 % to 67 %: guest is doing **less** work per second under P2 (fewer bytes moved, fewer TCP transitions).
- `rx-inject` **tripled** from 12 % to 36 %.
- Tokio workers stayed trivial (<10 % combined in both cases). **ACK intercept is not the bottleneck.**

### Where rx-inject's extra time went

Hot leaves under the rx-inject thread (active samples, leaves only):

| Function | P1 | P2 | factor |
|---|---|---|---|
| `readv` | 931 | 2277 | 2.4× |
| `hv_vcpus_exit` | 91 | 1176 | **13×** |
| `pthread_cond_signal` | 76 | 879 | 11× |
| `hv_gic_set_spi` | — | 133 | — |
| `Hv::Vm::set_spi` | — | 124 | — |
| `Hv::Vm::interrupt_cpus` | — | 105 | — |
| `Hv::Vcpu::interrupt` | — | 104 | — |

IRQ-delivery-related functions — `hv_vcpus_exit`, `hv_gic_set_spi`, `Hv::Vm::set_spi`, `Hv::Vm::interrupt_cpus`, `Hv::Vcpu::interrupt`, `pthread_cond_signal` — account for **~2500 samples** under P2 vs ~170 under P1. The inject thread is spending **~15× more time delivering interrupts into the guest** under dual-flow.

### Root cause

GSO RX-merge coalesces only within a single TCP flow. One flow of 29 Gbps fits into a few dozen giant 60 KB frames per millisecond — each inject cycle moves ~60 KB and fires one IRQ for the whole batch. Two flows at 5–6 Gbps each cannot cross-merge, so the frame count per unit throughput roughly doubles, and every extra frame carries the full IRQ-delivery cost (GIC SPI write, vCPU exit signal, pthread wakeup).

The cap at ≈ 10–12 Gbps combined is where the per-IRQ cost in `hv_vcpus_exit` + `hv_gic_set_spi` eats the remaining headroom.

### What this invalidates

- **Multi-queue virtio-net**: the guest has CPU headroom (exp 3 on the guest side showed 67 % idle on CPU0). Splitting onto more vCPUs doesn't reduce the *per-IRQ* host-side cost, which is where the hot leaves live.
- **Tokio restructuring / Phase D sync datapath**: tokio workers are trivially loaded in both cases. They are not the bottleneck.
- **Ring size**: already ruled out.

### What actually remains

> **2026-09-30 update.** Two of the items below have shipped. EVENT_IDX
> suppression is live in `arcbox-net-inject` (`write_avail_event_current`).
> The `hv_vcpus_exit` rows above are gone entirely: with the in-kernel GIC an
> idle vCPU sleeps inside `hv_vcpu_run` and `hv_gic_set_spi` wakes it, so the
> kick was a forced exit for nothing and every io worker stopped issuing it
> (`c3004580`). The `pthread_cond_signal` samples are that wake — the
> framework signalling `VcpuStateManager::wait_for_interrupt` — not a
> redundant hop of ours. The multi-flow ceiling should be re-measured with
> the `network_iperf` matrix before the ~10–12 Gbps figure is quoted again.

- **`VIRTIO_F_EVENT_IDX` IRQ suppression.** We already write `avail_event` in `flush_interrupt` but then unconditionally fire: `// Unconditionally fire interrupt. EVENT_IDX suppression can be added later once the basic path is validated.` (`virt/arcbox-net-inject/src/inject.rs`). Consulting the guest's `used_event_idx` before firing lets us skip 50–90 % of IRQs when the guest is already polling. This directly attacks the hot leaves.
- **Longer `COALESCE_TIMEOUT`.** Currently 200 µs. Bumping to 500 µs or dynamically scaling under load trades tail latency for fewer IRQs. Easy knob.
- **Avoid the `pthread_cond_signal` hop.** The inject-thread → vCPU wakeup currently goes `set_interrupt_status + pthread_cond_signal`. If the vCPU is already running (`Hv::Vcpu::run`) we only need `hv_vcpus_exit`; the `pthread_cond_signal` is redundant and shows up at 879 samples in P2.

EVENT_IDX is the direct target of the signature and already scaffolded; it should be tried first.

## Reproducer

```bash
# Start a clean daemon.
abctl daemon start

# Run an iperf3 server in a guest container.
docker run -d --rm --name iperf3-srv -p 5201:5201 networkstatic/iperf3 -s

# Headline tests.
iperf3 -c 127.0.0.1 -p 5201 -t 10                  # single stream (expect ≥20 Gbps)
iperf3 -c 127.0.0.1 -p 5201 -R -t 10               # VM→Host (expect ≥15 Gbps)
iperf3 -c 127.0.0.1 -p 5201 -R -P 4 -t 10          # VM→Host parallel (expect ≥15 Gbps sum)

# Collapse reproducer (each flow 6 Gbps — expect one survives, others drop).
iperf3 -c 127.0.0.1 -p 5201 -P 2 -b 6G -t 10

# Rate-limited works clean.
iperf3 -c 127.0.0.1 -p 5201 -P 4 -b 2G -t 10
```

## 2026-10-10 — TCP frame checksum

The Ethernet TCP checksum builder used a scalar loop with a branch at each 16-bit word. The builder now calls the existing `checksum_add` dispatcher and subtracts the stored checksum field before folding. The change preserves checksum-field exclusion, odd-length input, and short-input behavior.

Run `devenv shell -- cargo run --locked -p arcbox-packet --example checksum_bench --release` from the ArcBox checkout. These historical measurements used ArcTap's downstream ArcBox commits `72a59f9` (baseline) and `8f14a07` (candidate), which are not upstream commit references. Both sides used the same benchmark with `black_box` on the input and result; only the checksum implementation changed between the two release builds. To reproduce the implementation comparison from upstream history, compare `f2516a6` with `7075f050`, using `common/arcbox-packet/examples/checksum_bench.rs` from `7075f050` in both checkouts. Their checksum implementations match the historical baseline and candidate. Host: Apple M5 Max, macOS, Rust 1.96.0. These are single-run, warm-cache microbenchmarks.

| TCP segment bytes | Baseline ns/call | Shared dispatcher ns/call |
| --- | ---: | ---: |
| 20 | 3.6 | 2.1 |
| 1,480 | 279.2 | 12.8 |
| 3,980 | 731.6 | 37.5 |
| 65,535 | 12,195.0 | 745.2 |

The scalar-oracle test covers unaligned input, nonzero checksum fields, odd lengths, the SIMD threshold, and the maximum IPv4 segment length. The original downstream validation record reported 44 packet tests and 108 splicetcp tests passing, plus strict Clippy and formatting with Rust 1.98.1. Those counts describe the downstream checkout; upstream validation is recorded below. These measurements cover checksum computation only; they do not establish a network-throughput improvement.

## 2026-10-10 — TCP frame initialization

Both TCP data-frame builders allocated a zeroed frame and then copied the payload over those zeros. They now reserve the full capacity, initialize the 54-byte Ethernet/IPv4/TCP header, and append the payload. Reserved header fields remain zero. Frame contents, IPv4 length limits, and public interfaces remain unchanged.

Run `devenv shell -- cargo run --locked -p arcbox-packet --example tcp_frame_bench --release`. The benchmark includes allocation, header writes, payload copy, checksum, and deallocation. Inputs and returned frames pass through `black_box`. Each binary runs seven rounds over seven payload sizes and both full and partial checksum builders. The historical downstream baseline uses `8f14a07` plus the same benchmark; the candidate is `c31f3706`. To reproduce the implementation comparison from upstream history, compare `7075f050` with `42b55314`, using `common/arcbox-packet/examples/tcp_frame_bench.rs` from `42b55314` in both checkouts. Their data-frame builders match the historical baseline and candidate; the candidate changes only frame initialization in these builders. Host: Apple M5 Max, macOS, Rust 1.96.0, warm allocator and cache.

Two sequential baseline/candidate pairs produced these medians for full TCP checksums, in ns/frame:

| Payload bytes | Baseline 1 | Candidate 1 | Baseline 2 | Candidate 2 |
| --- | ---: | ---: | ---: | ---: |
| 0 | 17.236 | 17.355 | 18.212 | 17.135 |
| 1 | 17.514 | 18.493 | 18.588 | 18.069 |
| 64 | 18.909 | 18.793 | 18.908 | 18.549 |
| 1,460 | 52.384 | 38.041 | 50.316 | 34.827 |
| 3,960 | 121.767 | 90.582 | 121.177 | 81.441 |
| 32,768 | 1,567.621 | 748.156 | 1,547.054 | 767.519 |
| 65,495 | 3,101.965 | 1,606.331 | 3,024.942 | 1,540.798 |

Packet-sized and larger payloads improved in both pairs. Very short payloads have no consistent improvement; the first 1-byte measurement regressed by 0.979 ns. These microbenchmarks do not prove a NE throughput or CPU/GB improvement. Runtime acceptance remains open.

The existing frame test now compares every byte against a fixed wire-header fixture and scalar checksum oracle. Both checksum modes cover payload lengths 0 through 65, odd and even MTU-sized payloads, 32 KiB, and the maximum IPv4 payload. The original downstream validation record reported 44 packet tests and 108 splicetcp tests passing, plus strict Clippy, including the benchmark target, and formatting with Rust 1.98.1.

### Upstream validation — 2026-10-14

At upstream commit `42b55314`, validation on macOS Apple Silicon passed with the following commands. Tests and formatting used Rust 1.96.0; the Clippy command selected Clippy 0.1.99. Test counts depend on platform and feature flags.

- `devenv shell -- cargo test -p arcbox-packet --all-features`: 44 unit tests passed.
- `devenv shell -- cargo test -p splicetcp --all-features`: 101 unit tests passed.
- `devenv shell -- cargo fmt --all -- --check`: passed.
- `devenv shell -- cargo clippy -p arcbox-packet --all-targets --all-features -- -D warnings`: passed.

The benchmark numbers above remain the original downstream measurements. No throughput or microbenchmark measurements were repeated for the upstream submission.

## 2026-10-11 — IPv4 UDP batch sends

`BatchDgram::send_batch_to_v4` sends to an explicit IPv4 destination without connecting the socket. The method uses the existing kernel cap and syscall error handling. Partial success returns the accepted prefix count. Tests cover destination changes, zero-length payloads, packet boundaries, and a valid packet followed by an oversized packet. The original validation passed all 24 crate tests, the documentation test, strict Clippy, and formatting.

The historical command was `devenv shell -- cargo bench --manifest-path .source/arcbox/Cargo.toml -p arcbox-xnu-net --bench throughput -- ipv4 --sample-size 10 --warm-up-time 1 --measurement-time 2` from the ArcTap checkout. The benchmark sends 64 datagrams of 1472 bytes, then receives each datagram individually. Host: Apple M5 Max (Mac17,6), macOS 26.7.1 (25G241), Rust 1.96.0.

To repeat the send measurement from ArcBox, run `devenv shell -- tests/bench/xnu-udp/run.sh send`. The checked-in [driver](../../tests/bench/xnu-udp/README.md) retains these sampling settings and selects only `^ipv4/`; the historical `ipv4` filter also matches the newer receive group. The driver records the command, environment, output, status, and Criterion data.

| Send method | Criterion time estimate per 64 datagrams | Estimate interval | Datagrams/s estimate |
| --- | ---: | ---: | ---: |
| `send_to` | 273.04 µs | 257.39–286.07 µs | 234,400 |
| `send_batch_to_v4` | 263.83 µs | 254.16–275.19 µs | 242,580 |

The point estimate improves by 3.4%, but the intervals overlap. This single sequential run includes receiver cost and does not establish a NE throughput improvement. ArcTap integration requires a separate runtime comparison.

## 2026-10-24 — Ordinary TCP ACK abandonment check

The [ignored probe](../../common/splicetcp/src/tcp_bridge/tests/abandon_bench.rs) compares the existing `intercept_open_at` entry with the new `intercept_at` wrapper in the same release binary. Both paths process the same valid pure ACK for an ordinary inline connection. The control and discard maps are empty. Inputs, the bridge, and results pass through `black_box`.

Run from an ArcTap checkout with this ArcBox submodule:

```sh
devenv shell -- cargo test -p splicetcp --release ordinary_ack_abandon_branch_cost -- --ignored --nocapture
```

Each path receives 10,000 warmup calls. Nine measurement rounds each execute 1,000,000 calls per path, alternating which path runs first. The probe prints every round and the median; the probe has no timing assertion. Host: Apple M5 Max, macOS 26.7.1, Rust 1.96.0.

| Path | Median ns/ACK | Minimum ns/ACK | Maximum ns/ACK |
| --- | ---: | ---: | ---: |
| Existing entry | 21.025 | 20.699 | 21.328 |
| Empty-map wrapper | 21.865 | 21.553 | 23.541 |

The wrapper adds 0.840 ns/ACK, or 3.99%, in this run. This measures only the empty-map branch cost on a repeated ACK with a warm cache. It does not measure controlled connections, discard-table load, host I/O, packet batching, network throughput, or Network Extension CPU cost. Native runtime acceptance remains separate.

## 2026-10-22 — Shared TCP frame encoding

Correction: the original comparison reused one Cargo build directory and did not recompile the candidate. The original values below do not compare the two revisions. Use the independent-build correction later in this document.

Compare the data-frame builder at `5a8c5d66` with the commit containing this entry. Both checkouts use the unchanged `common/arcbox-packet/examples/tcp_frame_bench.rs`. Build with `devenv shell -- cargo build --locked -p arcbox-packet --example tcp_frame_bench --release`, then execute each resulting benchmark binary. Run order: baseline, candidate, candidate, baseline. Each execution contains seven rounds per payload and checksum mode; the table pools 14 samples per implementation.

Host: dedicated `xuan-work-mbp`, Apple M5 Pro (Mac17,9), macOS 26.5 (25F71), Rust 1.96.0. The benchmark includes allocation, header writes, payload copying, checksums, and deallocation. The original App and NE remain running; their binding is checked before and after the measurement.

| Payload bytes | Baseline full checksum ns/frame | Shared encoder ns/frame | Change |
| --- | ---: | ---: | ---: |
| 0 | 14.621 | 15.083 | +3.2% |
| 1 | 14.639 | 15.174 | +3.7% |
| 64 | 14.979 | 15.157 | +1.2% |
| 1460 | 35.687 | 37.692 | +5.6% |
| 3960 | 86.749 | 81.280 | -6.3% |
| 32768 | 715.995 | 715.086 | -0.1% |
| 65495 | 1546.463 | 1570.518 | +1.6% |

The full-checksum medians range from 6.3% faster to 5.6% slower. The 1460-byte case is about 2 ns slower; the maximum payload is about 24 ns slower. The unchanged partial-checksum builder ranges from 3.5% faster to 14.2% slower, and baseline executions also differ. These results show the measured cost and variation, not a consistent speedup or native throughput improvement. The refactor consolidates IPv4 encoding before adding IPv6 support.

## 2026-10-22 — IPv6-capable TCP frame encoder

Correction: the original comparison reused one Cargo build directory and did not recompile the candidate. The original values below do not compare the two revisions. Use the independent-build correction later in this document.

Compare `2001d081` with the commit containing this entry using the unchanged `common/arcbox-packet/examples/tcp_frame_bench.rs`. Build both with `devenv shell -- cargo build --locked -p arcbox-packet --example tcp_frame_bench --release`. Execute the resulting binaries in baseline, candidate, candidate, baseline order. The table pools 14 samples per implementation from two executions of seven rounds. The benchmark uses IPv4 inputs to measure the effect on existing callers.

Host: dedicated `xuan-work-mbp`, Apple M5 Pro (Mac17,9), macOS 26.5 (25F71), Rust 1.96.0. Original App and NE bindings are checked before and after execution.

| IPv4 payload bytes | Baseline full checksum ns/frame | IPv6-capable encoder ns/frame | Change |
| --- | ---: | ---: | ---: |
| 0 | 12.326 | 12.672 | +2.8% |
| 1 | 13.690 | 14.306 | +4.5% |
| 64 | 19.038 | 18.337 | -3.7% |
| 1460 | 37.529 | 37.682 | +0.4% |
| 3960 | 89.038 | 89.489 | +0.5% |
| 32768 | 732.148 | 733.217 | +0.1% |
| 65495 | 1604.738 | 1628.648 | +1.5% |

Full-checksum median changes range from -3.7% to +4.5%. These measurements include allocation, frame initialization, payload copies, and checksums. They do not measure IPv6 throughput or establish a native application performance improvement.

## 2026-10-22 — TCP options in the shared frame encoder

Compare `ecc02b43` with the commit containing this entry using the unchanged `common/arcbox-packet/examples/tcp_frame_bench.rs`. Build both with `devenv shell -- cargo build --locked -p arcbox-packet --example tcp_frame_bench --release`. Use a fresh and separate `CARGO_TARGET_DIR` for each source snapshot. Execute the binaries in baseline, candidate, candidate, baseline order. Each table value pools 14 samples from two executions of seven rounds. IPv4 data inputs measure the effect on existing callers; the benchmark does not measure handshake throughput.

Host: dedicated `xuan-work-mbp`, Apple M5 Pro (Mac17,9), macOS 26.5 (25F71), Rust 1.96.0. Original App and NE bindings are checked before and after execution.

| IPv4 payload bytes | Baseline full checksum ns/frame | Encoder with options ns/frame | Change |
| --- | ---: | ---: | ---: |
| 0 | 11.843 | 13.106 | +10.7% |
| 1 | 12.725 | 12.788 | +0.5% |
| 64 | 13.587 | 13.779 | +1.4% |
| 1460 | 36.974 | 37.874 | +2.4% |
| 3960 | 89.021 | 89.242 | +0.2% |
| 32768 | 2525.008 | 747.298 | -70.4% |
| 65495 | 1617.079 | 1616.706 | -0.0% |

Full-checksum median changes range from -70.4% to +10.7%. The measurements include allocation, frame initialization, payload copies, and checksums. An earlier run reused one build directory and produced identical binaries without recompiling the candidate; that run is excluded from this comparison. They do not establish a native application throughput improvement.

The SYN comparison has run-to-run variation: the baseline median for 32768-byte payloads changed from 731.717 to 4331.929 ns between executions. The pooled decrease at this payload size is not evidence of an encoder speedup. The zero-payload candidate median increased by 1.264 ns; the 1460-byte median increased by 0.900 ns.

## 2026-10-22 — Independent-build corrections for TCP frame comparisons

The two original comparisons above produced identical baseline and candidate binaries, and the candidate build logs contained no compilation. Their performance conclusions are withdrawn. Each corrected build uses a fresh, separate `CARGO_TARGET_DIR`; verbose compiler logs and dependency files identify the source snapshot. The unchanged benchmark runs in baseline, candidate, candidate, baseline order on the same dedicated test Mac. Each value pools 14 samples. Source and original App/NE bindings remained unchanged.

| Change | Payload bytes | Baseline ns/frame | Candidate ns/frame | Change |
| --- | ---: | ---: | ---: | ---: |
| Shared IPv4 encoder | 0 | 12.690 | 12.599 | -0.7% |
| Shared IPv4 encoder | 1 | 12.797 | 13.133 | +2.6% |
| Shared IPv4 encoder | 64 | 13.493 | 13.780 | +2.1% |
| Shared IPv4 encoder | 1460 | 36.243 | 37.197 | +2.6% |
| Shared IPv4 encoder | 3960 | 88.124 | 89.240 | +1.3% |
| Shared IPv4 encoder | 32768 | 746.370 | 730.982 | -2.1% |
| Shared IPv4 encoder | 65495 | 1581.741 | 1615.075 | +2.1% |
| IPv6-capable encoder | 0 | 12.155 | 12.070 | -0.7% |
| IPv6-capable encoder | 1 | 12.995 | 13.036 | +0.3% |
| IPv6-capable encoder | 64 | 13.633 | 13.978 | +2.5% |
| IPv6-capable encoder | 1460 | 37.075 | 37.935 | +2.3% |
| IPv6-capable encoder | 3960 | 87.525 | 89.542 | +2.3% |
| IPv6-capable encoder | 32768 | 731.844 | 734.985 | +0.4% |
| IPv6-capable encoder | 65495 | 1610.253 | 1617.429 | +0.4% |

The corrected measurements include run-to-run variation and establish neither a consistent speedup nor native application throughput. The functional packet regressions are independent of these microbenchmarks.

## 2026-10-22 — L3 adapter validation

Compare `902f73f5` with the commit containing this entry using the same `common/splicetcp/examples/l3_frame_bench.rs` in both snapshots. Build each through `devenv shell -- cargo build --locked -p splicetcp --example l3_frame_bench --release` with separate, initially absent `CARGO_TARGET_DIR` directories. Execute the binaries in baseline, candidate, candidate, baseline order. Run the candidate once more with the `ipv6` argument. Compiler logs and dependency files identify each source snapshot.

Host: Apple M5 Pro, macOS 26.5, Rust 1.96.0, aarch64-apple-darwin. Each process warms up once, then collects seven samples per payload. The IPv4 columns pool 14 samples; IPv6 has seven samples. Preallocated buffers exclude allocation and socket work. Inputs have valid declared lengths. The measurement includes IP validation, Ethernet header writes, and packet copies.

| Payload bytes | IPv4 baseline ns/frame | IPv4 candidate ns/frame | Difference ns/frame | IPv6 candidate ns/frame |
|---:|---:|---:|---:|---:|
| 0 | 2.375 | 3.040 | +0.665 | 3.975 |
| 64 | 3.112 | 3.851 | +0.739 | 3.822 |
| 1460 | 16.534 | 16.400 | -0.134 | 16.403 |
| 32768 | 287.913 | 290.096 | +2.183 | 275.982 |

The candidate replaces a version/minimum-length check with shared structural parsing, declared-length validation, and EtherType selection. Small-packet differences measure that added fixed work; large-packet results also include memory-copy and scheduling variation. These are adapter measurements, not native application throughput or end-to-end IPv6 acceptance.

## 2026-10-22 — TCP segmentation extraction

Compare `ded7cbe5` with the commit containing this entry. Both snapshots use identical `common/splicetcp/examples/tcp_bridge_bench.rs` source. Build with `devenv shell -- cargo build --locked -p splicetcp --example tcp_bridge_bench --release`. Give each snapshot a separate, initially absent `CARGO_TARGET_DIR`. Compiler logs and dependency files identify both snapshots. Run baseline, candidate, candidate, baseline.

Host: Apple M5 Pro, macOS 26.5, Rust 1.96.0, aarch64-apple-darwin. Each process establishes 1 or 64 flows through public handshake APIs over owned loopback sockets. Setup is outside timing. Each mode warms up once, then records seven samples; the table pools 14 samples per snapshot. ACK samples contain 200,000 established pure ACK operations. Download samples contain 10,000 transfers of 1400 bytes. Each transfer includes a socket write, bridge polling, output parsing and payload checks, and injected ACKs. Polling visits all active flows. These measurements are not pure encoder timing or native tunnel throughput.

| Operation | Active flows | Baseline ns/operation | Candidate ns/operation | Difference |
|---|---:|---:|---:|---:|
| ack | 1 | 27.855 | 27.590 | -0.95% |
| ack | 64 | 24.883 | 24.424 | -1.84% |
| download | 1 | 7125.167 | 7174.013 | +0.69% |
| download | 64 | 21397.131 | 20968.273 | -2.00% |

The production change moves the same encoder body to an internal module. Differences include compiler layout and host scheduling variation; download timings also include socket and validation costs. These measurements establish the baseline for the next address-family migration.

## 2026-10-22 — TCP bridge address families

Compare `56fc9bd7` with the commit containing this entry using the [TCP bridge benchmark method](#2026-10-22--tcp-segmentation-extraction). Both snapshots use the same checked-in driver, separate fresh release build directories, and baseline/candidate/candidate/baseline execution order. Compiler logs and dependency files identify each snapshot. Each cell pools 14 samples on the Apple M5 Pro test Mac, macOS 26.5, Rust 1.96.0. The traffic is IPv4; IPv6 correctness is checked separately by the socket-backed library test.

| Operation | Active flows | Baseline ns/operation | Candidate ns/operation | Difference |
|---|---:|---:|---:|---:|
| ack | 1 | 28.498 | 21.820 | -23.43% |
| ack | 64 | 24.814 | 22.928 | -7.60% |
| download | 1 | 5964.004 | 4986.370 | -16.39% |
| download | 64 | 19799.879 | 18773.054 | -5.19% |

The initial derived-hash candidate increased pure ACK time by 16.98% with one flow and 25.84% with 64 flows while download times remained within 0.2%. The final candidate keeps `IpAddr` storage but hashes IPv4 endpoints as the original compact four-tuple. Non-IPv4 keys retain their address-family tags, and the standard randomized hasher is unchanged. The table reports a new paired run after that correction. Address-family checks still select frame encoding. Pure ACK operations expose lookup and dispatch costs. Download operations also include socket I/O, scanning active flows, output parsing, and content validation. These measurements do not establish native tunnel throughput.

The final candidate's two pure ACK run medians were 21.790/21.845 ns with one flow and 23.053/22.925 ns with 64 flows. Baseline single-flow download medians varied from 7118.446 to 4950.142 ns between runs; candidate medians were 4946.425 and 5030.133 ns. That baseline variation prevents attributing the pooled download difference to the code change.

## 2026-10-22 — Established TCP input validation

Compare `27b859ce` with the commit containing this entry using the [TCP bridge benchmark method](#2026-10-22--tcp-segmentation-extraction). Both snapshots use the same driver and separate fresh release build directories. Compiler logs and dependency files identify each snapshot. Execution order is baseline/candidate/candidate/baseline. Each cell pools 14 samples on the Apple M5 Pro test Mac, macOS 26.5, Rust 1.96.0. Benchmarked traffic is IPv4; IPv6 behavior is covered by the socket-backed regression.

| Operation | Active flows | Baseline ns/operation | Candidate ns/operation | Difference |
|---|---:|---:|---:|---:|
| ack | 1 | 21.827 | 30.579 | +40.10% |
| ack | 64 | 22.720 | 31.261 | +37.59% |
| download | 1 | 6972.965 | 6993.820 | +0.30% |
| download | 64 | 19308.479 | 19405.408 | +0.50% |

The parser now validates IP version, declared lengths, fragmentation, TCP header bounds, and Ethernet/IP family agreement before accessing connection state. Pure ACK operations expose the fixed parsing cost. Download operations also include socket I/O, active-flow scans, frame encoding, and payload validation. These measurements do not establish native tunnel throughput.

The final parser adds 8.753 ns per pure ACK with one flow and 8.541 ns with 64 flows. An earlier snapshot without the two parsing inline hints measured 10.6–13.3 ns of added ACK cost. The final paired run retains all structural checks; download differences are 0.30% and 0.50%. The ACK microbenchmark records the remaining cost of the added structural checks.

## 2026-10-22 — IPv6 upload coalescing

Compare `54b699c4` with the commit containing this entry using identical `common/splicetcp/examples/tcp_upload_bench.rs` drivers. Build through `devenv shell -- cargo build --locked -p splicetcp --example tcp_upload_bench --release`, with separate initially absent `CARGO_TARGET_DIR` directories. Compiler logs and dependency files identify both snapshots. Run baseline, candidate, candidate, baseline.

Host: Apple M5 Pro, macOS 26.5, Rust 1.96.0. Each process warms up once and records seven samples per case; the table pools 14 samples. Each sample uploads 2000 batches through one established flow over an owned IPv4 or IPv6 loopback socket. Each packet carries 1400 bytes. Timing includes frame encoding, bridge interception, socket writes, ACK parsing, peer reads, and content validation. The driver consumes every packet and drains retained suffixes before the next batch. Setup is excluded. These results do not measure native tunnel throughput.

| Input | Packets per batch | Baseline ns/batch | Candidate ns/batch | Difference |
|---|---:|---:|---:|---:|
| ipv4-ethernet | 1 | 8081.781 | 8246.729 | +2.04% |
| ipv4-ethernet | 32 | 24834.730 | 25030.531 | +0.79% |
| ipv4-ip | 1 | 7873.198 | 8117.427 | +3.10% |
| ipv4-ip | 32 | 24801.125 | 24924.531 | +0.50% |
| ipv6-ethernet | 1 | 7823.781 | 8068.157 | +3.12% |
| ipv6-ethernet | 32 | 269824.115 | 25510.677 | -90.55% |
| ipv6-ip | 1 | 7885.511 | 7937.844 | +0.66% |
| ipv6-ip | 32 | 270998.750 | 25388.344 | -90.63% |

IPv4 already coalesced uploads; its comparison measures the shared parser and complete endpoint checks plus socket and scheduling variation. The IPv6 baseline processes every packet separately. The candidate can process all 32 packets in one vectored write and one ACK when the socket accepts the batch.

The final run records 90.55% and 90.63% lower IPv6 32-packet times. IPv4 32-packet times increase by 0.50% and 0.79%. Single-packet times increase by 0.66% to 3.12% (52 to 244 ns); socket and scheduling variation prevent attributing those differences solely to parsing. An earlier snapshot reparsed the first packet and recorded an 8.42% IPv4 Ethernet single-packet increase. The final implementation reuses the validated first view. The baseline single-packet times also changed between runs, so the measurements do not isolate the performance benefit of removing that parse.

## 2026-10-22 — Classifier address types

Compare `e0ff997d` with this commit using identical `common/splicetcp/examples/classifier_bench.rs` drivers. Build through `devenv shell -- cargo build --locked -p splicetcp --example classifier_bench --release`. Use separate initially absent `CARGO_TARGET_DIR` directories. Compiler logs and dependency files identify both snapshots. Execute baseline, candidate, candidate, baseline.

Host: Apple M5 Pro, macOS 26.5, Rust 1.96.0. Each process warms up once and records seven samples per case. Each sample classifies and drains 100,000 IPv4 frames, with four pool slots and one queued frame at a time. Construction is outside timing. Timing includes SYN allocation or pooled data copies, queue draining, and retained-byte assertions. The table pools 14 samples. The driver supports an `ipv6` argument for later activation; this batch measures IPv4 only.

| Mode | Entry | Baseline ns/frame | Candidate ns/frame | Difference |
|---|---|---:|---:|---:|
| syn | ethernet | 36.288 | 39.809 | +9.70% |
| syn | ip | 30.506 | 33.521 | +9.88% |
| data64 | ethernet | 11.657 | 11.639 | -0.15% |
| data64 | ip | 12.268 | 12.286 | +0.15% |
| data1400 | ethernet | 46.336 | 47.385 | +2.26% |
| data1400 | ip | 48.078 | 47.721 | -0.74% |

The production change widens SYN metadata to `IpAddr`. Only SYN retention changes its allocation layout. Data cases retain the same behavior and expose compiler-layout and timing variation. These measurements do not establish socket or native tunnel throughput.

## 2026-10-22 — Shared classifier parsing

Compare `1b7726be` with this commit using the [classifier benchmark method](#2026-10-22--classifier-address-types). Both snapshots use the same driver, separate fresh release build directories, and baseline/candidate/candidate/baseline execution order. Compiler logs and dependency files identify each source. Each cell pools 14 IPv4 samples from the Apple M5 Pro test Mac, macOS 26.5, Rust 1.96.0.

| Mode | Entry | Baseline ns/frame | Candidate ns/frame | Difference ns/frame |
|---|---|---:|---:|---:|
| syn | ethernet | 35.733 | 37.460 | +1.727 |
| syn | ip | 34.447 | 37.175 | +2.728 |
| data64 | ethernet | 11.248 | 12.851 | +1.602 |
| data64 | ip | 11.880 | 13.300 | +1.421 |
| data1400 | ethernet | 47.602 | 48.968 | +1.365 |
| data1400 | ip | 48.100 | 49.574 | +1.473 |

The old classifier read a few fixed header fields and retained incomplete TCP segments. The candidate validates declared IP lengths, fragmentation, complete TCP header bounds, and UDP lengths before retaining bytes. The benchmark includes that fixed validation cost, allocation or pool operations, queue draining, and byte assertions. No socket or native tunnel throughput claim follows from this table.

The pooled differences are +1.366 to +2.728 ns/frame. Baseline SYN run medians changed from 33.831 to 37.800 ns for Ethernet and from 32.945 to 35.695 ns for bare IP; candidate medians lie within or near those ranges. Timing variation therefore prevents attributing the full SYN difference to validation. The 64-byte data run medians were 11.162–11.249 ns baseline and 12.748–13.172 ns candidate for Ethernet, and 11.750–12.249 ns baseline and 13.289–13.315 ns candidate for bare IP.

## 2026-10-22 — IPv6 classifier

Compare `6dd0c89c` with this commit using the [classifier benchmark method](#2026-10-22--classifier-address-types). Both snapshots use the same driver and separate fresh release build directories. IPv4 runs use baseline/candidate/candidate/baseline order, followed by two candidate IPv6 runs. Compiler logs and dependency files identify each source. Each cell pools 14 samples from the Apple M5 Pro test Mac, macOS 26.5, Rust 1.96.0.

| Mode | Entry | Baseline IPv4 ns/frame | Candidate IPv4 ns/frame | Difference ns/frame | Candidate IPv6 ns/frame |
|---|---|---:|---:|---:|---:|
| syn | ethernet | 38.545 | 40.670 | +2.125 | 46.516 |
| syn | ip | 43.922 | 46.070 | +2.148 | 45.761 |
| data64 | ethernet | 12.820 | 12.848 | +0.027 | 13.279 |
| data64 | ip | 13.377 | 15.671 | +2.294 | 16.069 |
| data1400 | ethernet | 49.713 | 58.587 | +8.873 | 58.672 |
| data1400 | ip | 50.424 | 59.887 | +9.463 | 60.145 |

The candidate dispatches both families after shared validation and selects the synthetic EtherType for bare input. The measurements include classification, allocation or pool operations, queue draining, and byte assertions. IPv6 has no supported baseline in the parent. The driver excludes socket forwarding, the HostEgress boundary check, and native tunnels; no forwarding throughput claim follows from these results.

The 1400-byte IPv4 path increases by 8.873 ns/frame for Ethernet and 9.463 ns/frame for bare IP, about 18%. Both runs show this difference. SYN timing varies substantially: baseline Ethernet run medians are 37.369 and 51.853 ns, so the pooled SYN comparison cannot isolate the implementation cost. The feature adds family selection; these measurements do not identify which generated instructions account for the full difference.

A separate candidate added only `#[inline]` to the shared dispatcher. Its paired 1400-byte medians were 47.499/53.825 ns for Ethernet baseline/candidate and 47.919/56.539 ns for bare IP. The remaining increases were 13.3% and 18.0%; the hint did not eliminate the regression. Different baseline timings limit cross-run attribution. This commit retains the direct implementation without the hint. Both experiments retain compiler logs, source manifests, and per-run CSV evidence under `/private/tmp/arctap-classifier-ipv6-benchmark-oct22-v2` and `-v3` on the test Mac.

## 2026-10-22 — Primary cancellation

Compare `df036498` with this commit using `common/splicetcp/examples/tcp_bridge_bench.rs`. The driver accepts `--managed` to retain a native lifecycle token. Both snapshots use the same driver, independent fresh release targets, and Rust 1.96.0 on the dedicated test Mac. Each ownership mode uses baseline/candidate/candidate/baseline order, with 14 samples per cell. Downloads transfer 1400 bytes through real loopback sockets, verify payload and sequence numbers, and send guest ACKs.

| Socket mode | Operation | Flows | Baseline ns/op | Candidate ns/op | Difference |
|---|---|---:|---:|---:|---:|
| managed | ack | 1 | 41.887 | 44.037 | +5.13% |
| managed | ack | 64 | 42.495 | 45.685 | +7.51% |
| managed | download | 1 | 6631.629 | 6287.223 | -5.19% |
| managed | download | 64 | 25483.150 | 26247.935 | +3.00% |
| unmanaged | ack | 1 | 42.590 | 46.112 | +8.27% |
| unmanaged | ack | 64 | 42.758 | 44.971 | +5.18% |
| unmanaged | download | 1 | 7036.283 | 10780.013 | +53.21% |
| unmanaged | download | 64 | 26006.933 | 32171.525 | +23.70% |

These initial runs overlapped compilation on the test Mac, including an unrelated Rust 1.98 workspace build. They do not isolate the implementation cost. ACK medians increased by 2.150–3.522 ns/op; unmanaged download medians increased by 23.70–53.21%, while managed download medians changed by -5.19%/+3.00%. Do not interpret the unmanaged download difference as an established code regression or the managed decrease as an improvement. A compiler-idle repeat remains pending.

Source manifests, fresh-build compiler logs, dependency paths, binary hashes, all 224 timing samples, and native binding snapshots are in `/private/tmp/arctap-primary-cancel-benchmark-oct22-v1`. Reproduce with `devenv shell -- cargo run --release -p splicetcp --example tcp_bridge_bench` and add `-- --managed` for managed sockets. This driver does not measure VM, TUN, or native App/NE throughput.

The first compiler-idle repeat stopped after ten minutes because another Cargo task remained active. No repeat samples ran. The waiting record is `/private/tmp/arctap-primary-cancel-benchmark-oct22-v2`; the isolated performance verdict remains pending.
