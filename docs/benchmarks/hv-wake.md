# HV wake path — host→guest vsock bulk, RPC latency, idle CPU

Benchmark record for the custom Hypervisor.framework backend's interrupt
delivery: how fast host→guest vsock data moves, how long a vsock RPC takes,
and what the daemon costs at idle. Probes: `tests/bench/hv-wake/` (see its
README). Established by `docs/experiments/2026-09-30-hv-wake-path.md`;
decision in `docs/adr/0001-hv-spi-is-the-whole-wake.md`.

## Method

- Isolated HV dev daemon (never the user's `~/.arcbox` one), 18-vCPU System
  VM, `alpine` image pulled first.
- **Pipe**: `tests/bench/hv-wake/pipe1g.sh` — `head -c 1073741824 /dev/zero
  | docker run -i --rm alpine wc -c`, three runs, with `GetVirtioDebug`
  counter deltas around each. Report the three times.
- **RPC**: `tests/bench/hv-wake/rpcping.py <sock> <label> 300` —
  `MachineService/Ping` over one Connect/JSON connection; report p50/p90/p99
  ms. Run once with guest CPU 0 idle and once with `busy0.sh start`.
- **Idle CPU**: `cargo xtask idle --pid <daemon> --seconds 30`, ≥20 s after
  the image pull, nothing else running against the daemon.
- **Guest wake**: `tests/bench/hv-wake/ipipong.sh`, cross-CPU (3↔4) and
  same-CPU (3) µs per round trip.
- Same context: compare configurations back to back on the same host; note
  the 1-minute load average next to every row. The pipe varies ±15% and RPC
  p50 ±0.1 ms run to run on a loaded host.

## Results

| date | build | config | pipe idle (s) | pipe busy0 (s) | RPC p50 idle/busy (ms) | idle CPU | kicks / GiB | load |
|---|---|---|---|---|---|---|---|---|
| 2026-09-29 | master 0a93f43a | one packet per round, 18-vCPU kick per packet | 20.49 / 19.60 / 19.14 | — | — | — | 290 k broadcasts | 29–35 |
| 2026-09-29 | 5de23af4 + e2b6d3ad | round drains the stream, kick aimed at CPU 0 | 5.96 / 5.74 / 5.55 | — | — | — | ~160–250 broadcasts | 25–40 |
| 2026-09-30 | e2b6d3ad (REF) | same, measured with the RPC probe | 5.95 / 5.91 / 5.91 | 5.93 / 6.13 / 6.39 | 0.813 / 0.961 | 9.3% | ~8 k received | 10–15 |
| 2026-09-30 | experiment H | no kicks anywhere, vsock EVENT_IDX | 5.82 / 5.93 / 5.59 | 5.40 / 5.59 / 5.43 | 0.302 / 0.323 | 6.7% | 0 | 7–10 |
| 2026-09-30 | 3bf22fa6 (merged) | c3004580 + 7acb93a0 | 5.56 / 5.67 / 5.57 | 5.53 / 5.69 / 5.49 | 0.305 / 0.315 | 6.8% | 0 | 7–9 |

Guest cross-CPU wake: 55 µs with kicks, 35 µs without; same-CPU 15 µs.
`docker load` 300 MB: 3.46 / 4.35 / 3.18 s with kicks vs 3.21 / 3.30 /
2.69 s without (2026-09-30, back to back).

VZ reference (same build, 2026-09-29, load 12–14): pipe 4.27 / 4.79 /
4.66 s. VZ runs none of this code; it is the oracle for "is the scenario
itself fine".

## Analysis

The 2026-09-29 step removed a per-packet interrupt and kick; the 2026-09-30
step removed the kick itself, which was a forced `Canceled` exit on every
kicked vCPU that bought nothing because the framework already wakes the
target (experiment entry, findings 1–2). What remains per GiB is ~19–21 k
interrupts, one per injection round, paced by the daemon's ~50 KiB writes;
EVENT_IDX does not reduce that at this pace (finding 4). RPC p50 is now the
vsock round trip itself.

Idle CPU is not on this path any more: ~5.6 of the ~6.8 points are the
`rx-inject` thread's yield/poll loop (finding 7).

## Known limits

- A `kicks_received` delta on any vCPU during steady state means a worker
  is calling `hv_vcpus_exit` again — a regression, not a tuning knob.
- The pipe is bounded by the daemon→socketpair write pacing and the guest's
  256 KiB vsock credit window, not by interrupt cost; do not chase the
  remaining ~20 k interrupts per GiB without changing how the daemon writes.
