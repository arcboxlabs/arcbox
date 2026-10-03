# 2026-09-30 — How a guest vCPU actually gets woken on HV

- Type: experiment
- Area: HV backend (`arcbox-vmm`, `arcbox-hv`), vsock/blk/net workers
- Outcome: ADR 0001; commits c3004580, 7acb93a0, 646bd279
- Probes: `tests/bench/hv-wake/` (checked in from this session)
- Host: Apple Silicon, 18 cores, macOS 26.4, load 7–20 during runs unless
  noted; guest: 18-vCPU System VM, kernel HZ=1000, NO_HZ_IDLE, no cpuidle
  driver (`CONFIG_CPU_IDLE` unset)

## Question

After 2026-09-29 the vsock worker kicked one vCPU instead of 18. Should the
targeting extend to blk/net/console and to the GIC callback's unpark-all,
or is there a design that does not need to know the routing at all?

## Knobs (experiment build, branch `perf/hv-irq-delivery`, not merged)

`ARCBOX_HV_VSOCK_KICK=none|target|all`, `ARCBOX_HV_WORKER_KICK=none` (blk,
net-rx, console), `ARCBOX_HV_UNPARK=target`, `ARCBOX_HV_VSOCK_EVENT_IDX=1`,
`ARCBOX_HV_HALT_POLL_US=N`, `ARCBOX_HV_SIBLING_SCAN=1`,
`ARCBOX_HV_IROUTER_CHECK=1`. Each configuration: fresh isolated HV daemon,
30 s `cargo xtask idle`, 1 GiB pipe ×3 and 300 `MachineService/Ping` RPCs
with guest CPU 0 idle, then the same with a busy loop pinned to CPU 0
(`--cpuset-cpus=0`), then guest FIFO ping-pong.

## Results

Pipe = 1 GiB stdin pipe, seconds (three runs). RPC = p50 ms over 300
persistent-connection pings. "busy" = guest CPU 0 pinned busy.

| Config | Pipe idle | Pipe busy | RPC idle | RPC busy | vsock irq / GiB | idle CPU |
|---|---|---|---|---|---|---|
| A  target kick, unpark all (master) | 6.07 / 7.23 / 6.08 | 6.59 / 6.21 / 6.21 | 0.632 | 0.536 | ~21 k | — |
| A2 same, later run | 6.39 / 6.29 / 6.19 | 6.18 / 10.2 / 6.98 | 0.687 | 0.582 | 21–31 k | 9.4% |
| B  no vsock kick | 5.87 / 5.82 / 5.78 | 5.93 / 5.77 / 5.65 | 0.380 | 0.324 | ~20 k | — |
| C  no kick + unpark target | 7.42 / 7.64 / 7.09 | 7.36 / 11.1 / 6.07 | 0.892 | 1.048 | 37–64 k | 9.0% |
| D  no kick + EVENT_IDX (first cut) | 5.99 / 6.11 / 6.11 | 6.43 / 6.07 / 7.71 | 0.710 | 1.159 | ~19 k | 8.3% |
| E  no kick + unpark target + EVENT_IDX (first cut) | 7.06 / 6.42 / 6.15 | 5.95 / 5.96 / 5.85 | 0.705 | 0.757 | ~19 k | 9.1% |
| B2 no kick + EVENT_IDX (fixed) | 7.19 / 6.38 / 6.12 | 6.19 / 6.69 / 7.66 | 0.606 | 0.625 | 19–28 k | 9.4% |
| G0 no kick + unpark target + EVENT_IDX (fixed) | 6.58 / 6.29 / 6.06 | 6.37 / 6.12 / 6.17 | 0.712 | 0.807 | 19–24 k | 10.4% |
| G100 G0 + halt-poll 100 µs | 6.17 / 6.15 / 6.48 | 6.44 / 6.48 / 6.24 | 0.669 | 0.685 | 20–25 k | 9.7% |
| G500 G0 + halt-poll 500 µs | 6.28 / 6.28 / 6.45 | 6.23 / 6.43 / 6.40 | 1.222 | 0.850 (p99 11.7) | ~20 k | 5.9% |
| **H no kicks anywhere + EVENT_IDX** | **5.82 / 5.93 / 5.59** | **5.40 / 5.59 / 5.43** | **0.302** | **0.323** | 19–21 k | **6.7%** |
| REF kicks on (back to back with H) | 5.95 / 5.91 / 5.91 | 5.93 / 6.13 / 6.39 | 0.813 | 0.961 | 19–22 k | 9.3% |

H vs REF also: `docker load` 300 MB 3.21 / 3.30 / 2.69 s vs 3.46 / 4.35 /
3.18 s; `docker build` (64 MB layer) 3.75 vs 4.79 s; `docker pull
ubuntu:24.04` 10.9 vs 12.8 s; guest cross-CPU ping-pong 35 vs 55 µs;
`kicks_received` 0 vs ~8 k per GiB; daemon log warn/error lines identical.

## Findings

1. **WFI never exits to userspace.** Over a full boot every vCPU's `wfi`
   and `vtimer` counters are 0. `sample` of an idle daemon: each
   `hv-vcpu-N` thread sits in `Hv::Vcpu::run → HvCore::Hypervisor::
   VcpuStateManager::wait_for_interrupt → _pthread_cond_wait`. With the
   in-kernel GIC the framework parks the vCPU itself and `hv_gic_set_spi`
   signals it. The VMM's WFI-park branch and the unpark-all loop were dead
   code, and the `pthread_cond_signal` seen in every worker profile is that
   wake, not a redundant hop.
2. **`hv_vcpus_exit` after the SPI is pure overhead**, for idle and busy
   targets alike (B, H). Each kick is a forced `Canceled` exit and re-entry
   on every kicked vCPU; at idle (~24 irq/s) it still cost ~2.6 points of a
   core.
3. **Guest IPIs are not delayed by the host**: 30–55 µs cross-CPU. The
   "IPI to a parked vCPU waits up to 1 ms" theory that made C look like a
   real regression was wrong; C's inflation to 37–64 k interrupts was load
   plus the missing EVENT_IDX suppression (G0 with it is normal).
4. **EVENT_IDX honoring on vsock RX is neutral** at pipe pace (~19–21 k
   interrupts per GiB either way — the guest drains faster than the daemon
   writes and wants one interrupt per round). It is only correct when the
   round reports `wrote` and `raise` separately: the first cut folded them
   and made the worker take its 1 ms no-progress backoff on every suppressed
   round (D/E).
5. **`GICD_IROUTER` is readable** through `hv_gic_get_distributor_reg`
   (macOS 15+, offset 0x6000 + 8·intid) at ~14 ns per read; Aff0 equals the
   vCPU index. It agreed with the ACK inference on 50 000 of 50 000 rounds.
   Unused in the end: nothing needs the routing once nothing is kicked or
   unparked.
6. **Halt polling and sibling scanning are moot** (nothing is parked);
   500 µs polling on 18 threads contended with the daemon on a loaded host
   (RPC p99 11.7 ms).
7. **Where HV idle CPU goes** (`sample` + `ps -M` over 10 s, no kicks):
   7.6% of a core total; ~5.6 points are the `rx-inject` thread in
   `cthread_yield`/`swtch_pri` plus `semaphore_timedwait`; all 18 vCPU
   threads together ~0.1 point; `vsock-io` ~0.6 (`__recvfrom`). Event-
   driving `rx-inject` is the next idle-CPU task; "tickless WFI" is not.
8. `abctl machine ping default` is too coarse for wake latency (process
   start dominates, p50 9–15 ms); the persistent-connection Connect probe
   resolves 0.3 ms medians.

## Decisions taken

See ADR 0001. Implemented as c3004580 (no kicks, no unpark, inference
removed), 7acb93a0 (vsock EVENT_IDX with `RxRound`), 646bd279 (docs).

## Open

- Re-measure the multi-flow Host→VM ceiling (`network_iperf`) now that the
  net-rx worker no longer kicks; `docs/benchmarks/network.md` quotes numbers
  that included `hv_vcpus_exit`.
- `rx-inject` idle loop → event-driven wait (the ABX-517 shape).
