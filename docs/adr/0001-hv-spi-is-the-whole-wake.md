# ADR 0001: On the HV backend, asserting the SPI is the whole wake

- Status: accepted (2026-09-30)
- Deciders: Xuan
- Commits: c3004580 (remove the kicks), 7acb93a0 (vsock EVENT_IDX), 646bd279 (docs)
- Evidence: `docs/experiments/2026-09-30-hv-wake-path.md`,
  `docs/logs/2026-09-29-vsock-rx-round-and-targeted-kick.md`

## Context

Every io worker on the custom Hypervisor.framework backend (blk, net-rx,
console, vsock) completed guest I/O in three steps: set `interrupt_status`,
assert the GIC SPI through `hv_gic_set_spi`, then call `hv_vcpus_exit` to
force every vCPU out of `hv_vcpu_run`. The GIC callback additionally
unparked every vCPU thread. Both came from the ABX-367 campaign, which
had established that `hv_vcpus_exit(NULL, 0)` is a no-op on arm64 and that
a guest idle in WFI was not being woken; the model behind the fix was that
an idle vCPU's thread sits parked on the host and must be pushed back into
`hv_vcpu_run` to notice a pending interrupt.

A 1 GiB `docker run -i` stdin pipe cost ~290 k `hv_vcpus_exit` broadcasts
(one per ≤3776-byte vsock packet) and 19–20 s on a loaded host. The first
fix (5de23af4, e2b6d3ad) drained a stream per injection round and aimed the
kick at the vCPU that last acknowledged the vsock interrupt; that brought
the pipe to 5.5–6 s and the broadcasts to ~200 per GiB, and raised the
question whether to extend the targeting to the other workers and to the
unpark loop.

## What was measured

On macOS 26.4 with an 18-vCPU System VM (details and tables in the
2026-09-30 log):

- Every vCPU's `wfi` and `vtimer` exit counters stay 0 over a full boot.
  `sample` shows idle vCPU threads blocked inside `Hv::Vcpu::run` in
  `HvCore::Hypervisor::VcpuStateManager::wait_for_interrupt` on a
  `pthread_cond_wait`. With the in-kernel GIC the framework handles WFI
  itself; nothing is ever parked by this process outside `pause`.
- With the vsock worker's kick removed and the target CPU pinned busy in
  the guest, vsock RPC p50 fell from 0.54 to 0.32 ms. A running vCPU takes
  the SPI asynchronously; HZ=1000 would have shown a +0.5 ms median if it
  waited for its next exit.
- Guest cross-CPU wakeups (FIFO ping-pong between cpuset-pinned
  containers) take 30–55 µs, same-CPU 15–20 µs: IPIs to idle vCPUs are not
  delayed by the host either.
- With every worker's kick removed, back to back against the kicking
  build: 1 GiB pipe 5.4–5.9 s vs 5.9–6.4 s; RPC p50 0.30/0.32 ms vs
  0.81/0.96 ms (target CPU idle/busy); `docker load` of 300 MB 2.7–3.3 s vs
  3.2–4.4 s; idle daemon CPU 6.7% vs 9.3%; `kicks_received` 0 vs ~8 k per
  GiB. No warning or error in the daemon log differs.
- Targeting the unpark, halt-polling the GIC before parking, scanning
  siblings' pending state, and reading `GICD_IROUTER` directly (14 ns per
  read, agreed with the ACK inference on 50 000/50 000 rounds) were all
  built and measured; none has anything to act on once nothing is parked.

## Decision

1. An io worker's completion path is `trigger_interrupt(INT_VRING)` then
   `irq_callback(irq, true)`. No `hv_vcpus_exit`, from any worker, ever.
2. The GIC callback asserts the SPI and nothing else. The vCPU thread
   registry stays for `pause`/`resume`, which park and unpark explicitly.
3. The vCPU loop's WFI branch stays, commented as unreachable in this
   configuration; its 1 ms `park_timeout` bounds interrupt latency should a
   framework ever trap WFI again, because no SPI unparks it.
4. `hv_vcpus_exit` remains the tool for `stop` and `pause`, where leaving
   `hv_vcpu_run` is the point.
5. The `kick_broadcasts` and `unpark_broadcasts` snapshot fields stay on
   the wire reading 0; the ACK-based routing inference (`irq_ack_vcpu`) is
   removed with the kicks it aimed.
6. The vsock RX round honors `VIRTIO_F_EVENT_IDX` like blk and net-rx do,
   returning `RxRound { wrote, raise }` so a round whose data landed
   silently does not trigger the worker's no-progress backoff. Measured
   neutral on interrupt count at pipe pace; kept for protocol consistency.

## Consequences

- The R2/R3 acceptance numbers read from the broadcast counters (~2301
  unpark / ~71 kick per boot) describe a mechanism that no longer exists.
  A wake-path change is judged by per-vCPU `kicks_received` (flat in steady
  state), the vsock RPC probe (`tests/bench/hv-wake/rpcping.py`) and
  `cargo xtask idle`.
- "Tickless WFI" (R3) is not a lever: the vCPU threads already sleep inside
  the framework. HV idle CPU (~6.7% of a core) is ~5.6 points the
  `rx-inject` thread's `cthread_yield`/`semaphore_timedwait` loop; that is
  the next idle-CPU task.
- `docs/benchmarks/network.md`'s `hv_vcpus_exit` rows are history and its
  multi-flow ceiling needs re-measuring; the `pthread_cond_signal` it saw is
  the framework waking the target vCPU, i.e. the wake itself.
- Anyone reintroducing a kick or an unpark on the interrupt path must first
  show a non-zero `wfi` exit count on the target macOS version.

## Alternatives considered

- Targeted kick to the SPI's routed vCPU (e2b6d3ad, shipped for one day):
  strictly better than the broadcast, strictly worse than no kick.
- Targeted unpark from the GIC callback: no effect, nothing is parked; the
  one run that looked like a regression was load noise plus interrupt
  inflation the EVENT_IDX change removes.
- Halt polling 100/500 µs before the park: moot, and 500 µs on 18 threads
  drove RPC p99 to 11 ms on a loaded host.
- A PV-IPI hypercall in the guest kernel to wake parked vCPUs: solves a
  problem that does not exist here.
