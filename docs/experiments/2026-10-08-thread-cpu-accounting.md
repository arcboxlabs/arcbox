# 2026-10-08 — Which task clock charges host suspension?

- Status: complete for the clock comparison; the original ClickHouse stall remains unresolved.
- Environment: M5 Max, macOS 26.4, isolated VZ VM, 18 vCPUs, Linux 6.18.38-arcbox baseline.
- Outcome: scheduler runtime charges a host pause to the running task; raw task ticks do not.

## Question and method

The [earlier investigation](../logs/2026-10-07-signoz-profiler-and-cpu-accounting.md) established divergent cgroup and `/proc/stat` counters. This experiment compares both accounting paths inside one thread, without starting ClickHouse or a profiler.

The [C probe](../../tests/bench/signoz/thread_accounting.c) pins itself to one guest CPU. It reads `CLOCK_THREAD_CPUTIME_ID` and Linux's dynamic current-thread `CPUCLOCK_PROF`, encoded as clock ID `-4`. The baseline has `CONFIG_VIRT_CPU_ACCOUNTING_GEN=n`. In this configuration, `CPUCLOCK_PROF` reads raw `utime + stime`; the standard thread clock reads `task_sched_runtime()`. `/proc/PID/stat` cannot substitute for raw PROF because Linux adjusts those exported task counters against scheduler runtime.

The probe performs a busy loop or sleeps in 10 ms intervals for approximately 20 seconds. Each CPU-clock snapshot is bracketed by `CLOCK_MONOTONIC` reads. The [driver](../../tests/bench/signoz/run_thread_accounting.py) captures per-CPU `/proc/stat`, unmasked `arch_timer` interrupt counts, cgroup usage, and host VM process CPU time before and after the probe. These external counters have sequential sampling skew and include auxiliary work.

For pause cases, the existing [pause helper](../../tests/bench/signoz/pause_vm.py) sends SIGSTOP to the verified disposable VZ helper and resumes it after ten seconds. A detached watchdog and a `finally` block both ensure SIGCONT. The helper PID was 51919, with start identity `Thu Oct 8 00:39:58 2026`. Its open kernel and disk files belonged to `/private/tmp/arcbox-cpu-repro-w0v7qz49`. Existing user VMs were not signaled.

The final driver waits for a guest sample at least two seconds into the workload before suspension. The first CPU 1 cases used a three-second host delay; their captured guest samples independently confirm that the workload was active before suspension. The CPU 9 repeat used the explicit sample readiness condition.

## Results

| Case | CPU | Elapsed s | Scheduler s | Raw PROF s | Per-CPU busy s | Timer IRQs |
|---|---:|---:|---:|---:|---:|---:|
| Busy | 1 | 20.002940 | 20.001709 | 19.980 | 19.99 | 20,186 |
| Busy, 10 s host pause | 1 | 20.046466 | 20.046164 | 10.025 | 10.03 | 10,149 |
| Busy, 10 s host pause | 9 | 20.110653 | 20.109702 | 10.087 | 10.10 | 10,245 |
| Sleep | 1 | 20.095821 | 0.082041 | 0.019 | 0.02 | 3,807 |
| Sleep, 10 s host pause | 1 | 20.080573 | 0.042178 | 0.010 | 0.03 | 2,014 |

The busy CPU 1 pause case accumulated 20.069 cgroup CPU seconds but only 10.64 host VM process CPU seconds. The latter includes the whole VM and host overhead, so it is not a pure task execution measurement. Raw PROF resolution was 1 ms in every run. The maximum CPU-clock read span across all cases was 47,583 ns; no recorded snapshot straddled the ten-second pause.

The controlled pause adds approximately ten seconds to scheduler runtime for a running task, but not to raw tick accounting. The sleeping control does not acquire those ten seconds. The CPU 9 repeat excludes a CPU 1-specific effect. Timer IRQ counts support the distinction, but `arch_timer` services other high-resolution timers as well; those counts are not an exact scheduler-tick count.

## Interpretation and limits

This establishes a task-accounting mechanism below Docker and independent of ClickHouse. With no effective stolen-time subtraction, elapsed host suspension advances the running task's scheduler clock. Busy tick accounting does not charge the missed interval. Neither changing Docker's display formula nor merely aligning its two input counters corrects the task runtime.

The deployed ClickHouse CPU profiler uses `CLOCK_THREAD_CPUTIME_ID`, so its expiry clock is affected by this mechanism. This source relationship motivates a separate timer-attribution experiment; these five runs contain no profiler and do not prove signal delivery behavior.

The experiment does not identify the trigger of the original ClickHouse stall, establish ordinary host scheduling latency, or prove an Apple VZ implementation defect. It also does not establish that raw ticks measure exact physical CPU use under arbitrary contention.

## Reproduction and validation

Use an explicitly isolated Docker socket. Compile and copy the static probe into a task-owned container. The privileged container exposes guest `/proc/interrupts`; it uses no network.

```sh
aarch64-linux-musl-gcc -O2 -Wall -Wextra -Werror -static tests/bench/signoz/thread_accounting.c -o /tmp/thread_accounting
docker --host "$SOCKET" run -d --name thread-accounting --privileged --network none --entrypoint /bin/sleep clickhouse/clickhouse-server:25.12.5 infinity
docker --host "$SOCKET" cp /tmp/thread_accounting thread-accounting:/tmp/thread_accounting
python3 tests/bench/signoz/run_thread_accounting.py --socket "$SOCKET_PATH" --container thread-accounting --mode busy --host-vm-pid "$VM_PID" --output /tmp/thread-accounting-busy
```

Run the sleep control with `--mode sleep`. Before a pause case, verify the VZ helper's open experiment disk files with `lsof` and obtain its start time with `ps`. Add `--pause --expected-start "$VM_START"` and use a fresh output directory. Repeat with `--cpu 9` on a guest with at least ten available CPUs. Stop the task-owned container if a driver fails; terminating the Docker client alone does not ensure that its guest process exits.

The static build passed GCC 14.2.0 with warnings treated as errors. Apple clang-format 21.0.0 passed with `{BasedOnStyle: LLVM, IndentWidth: 4, ColumnLimit: 100}`. Python syntax validation, workspace `cargo fmt --check`, and Clippy for the existing `arcbox-hv` CPU accounting example passed. The meaningful runtime checks are the busy/sleep comparison and the two controlled pause cases.

Raw samples, before/after counters, pause records, the analysis script, and the baseline kernel config are preserved under the task artifact directory `thread-accounting-2026-10-08`. No production kernel or accounting implementation changed.

## Follow-up: CPU timer attribution inside a signal handler

The [timer probe](../../tests/bench/signoz/profiler_attribution.c) tests the effect on CPU timer delivery. A single thread on CPU 1 creates two one-shot timers with four-second thresholds: one uses `CLOCK_THREAD_CPUTIME_ID`, and one uses raw `CPUCLOCK_PROF`. It then raises SIGUSR1. That synthetic Real handler announces entry through an async-signal-safe `write` and computes until raw PROF advances one second. Separate signals record whether each timer fires while the Real handler is active. The handler has a thirty-second monotonic deadline and uses no formatting or locks.

The [driver](../../tests/bench/signoz/run_profiler_attribution.py) waits for the entry marker before pausing the disposable VM. This places the controlled host suspension inside the known handler. The baseline omits suspension. The probe does not run ClickHouse or unwind a stack.

| Case | Elapsed s | Scheduler s | Raw PROF s | SCHED timer signals | PROF timer signals | SCHED signal inside Real handler |
|---|---:|---:|---:|---:|---:|---|
| Baseline | 0.999382 | 0.999343 | 1.000 | 0 | 0 | No |
| Ten-second host pause | 11.015258 | 11.015173 | 1.000 | 1 | 0 | Yes |

The same four-second SCHED timer fires inside a handler that accrued only one second of raw tick CPU time. The PROF timer remains below its threshold. This directly validates false CPU-budget expiry and misleading sample attribution under the controlled suspension. The one-shot timer reports zero overruns; this experiment does not test periodic overrun behavior or reproduce a signal storm.

The deployed ClickHouse [CPU profiler](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Common/QueryProfiler.cpp#L324) uses the affected clock. Its Real and CPU handlers can nest. Therefore, the original concentration of CPU samples in the Real handler is not independent evidence that the handler consumed that much physical CPU. The experiment does not establish that this exact suspension sequence occurred during the original incident or caused its prolonged stall.

To repeat, compile `profiler_attribution.c` with the same static compiler flags, then copy the executable to `/tmp/profiler_attribution` in the existing diagnostic container. Run:

```sh
python3 tests/bench/signoz/run_profiler_attribution.py --socket "$SOCKET_PATH" --container thread-accounting --output /tmp/profiler-baseline
python3 tests/bench/signoz/run_profiler_attribution.py --socket "$SOCKET_PATH" --container thread-accounting --pause --host-vm-pid "$VM_PID" --expected-start "$VM_START" --output /tmp/profiler-pause
```

Both cases satisfied the driver's predictions. GCC strict compilation and static analysis, the same explicit clang-format style, and Python syntax validation passed. Raw records and the probes are preserved under the task artifact directory `profiler-attribution-2026-10-08` with a verified SHA256 manifest. The diagnostic container and daemon were stopped after collection; the previously stopped task builder was returned to its stopped state.

## Follow-up: incorrect runtime causes prolonged scheduling exclusion

The [competition driver](../../tests/bench/signoz/run_scheduler_debt.py) starts eight existing busy probes in the same container, all pinned to guest CPU 1 for 120 seconds. The container has unlimited `cpu.max`; recorded throttling counters stay zero. A control runs without suspension. A second run suspends the verified disposable helper for ten seconds after every worker has produced two seconds of samples. The final driver also reads task state and `/proc/PID/schedstat` every three seconds; this state sampling was added for the pause run.

This experiment tests a scheduling consequence, not merely a displayed counter. Linux [update_curr](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/fair.c#L1297) advances the current entity's `vruntime` by its accounted runtime. The [EEVDF eligibility check](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/fair.c#L797) compares that value with the runqueue's weighted average. For eight equal-weight, continuously runnable tasks, a ten-second overcharge to one task predicts approximately `(8 - 1) × 10 = 70` seconds of subsequent exclusion while its peers catch up. The `vlag` clamp does not repair this overcharge during ordinary preemption.

| Observation | Unpaused control | Pause run |
|---|---:|---:|
| Largest sample gap across eight workers | 1.133074 s | 80.338194 s |
| Largest gap among the seven unaffected peers | — | 10.218061 s |
| Affected worker's raw PROF advance across its largest gap | — | 0.021 s |
| Affected worker's scheduler advance across that gap | — | 10.042077 s |
| Unchanged runnable snapshots after resume | — | 24 |

The affected worker, index 6 and guest PID 40, remained in state `R` at all 24 snapshots from 0.026 to 69.795 seconds after the pause helper returned. Its kernel runtime stayed exactly 10.301243054 s and its dispatch count stayed exactly 120. The worker was therefore runnable without further execution across snapshots spanning about 69.8 seconds. These host timestamps follow each Docker read, so the interval is not a strict lower bound. The other seven workers continued producing samples. At the next snapshot, 72.839 seconds after resume, the affected worker had resumed: dispatch count was 235 and recorded runqueue delay had advanced from 1.903 s to 74.469 s. Runqueue delay is accounted on dispatch, so its earlier unchanged value does not mean the task was not waiting.

The maximum clock-read span was 45,167 ns. No recorded CPU-clock snapshot straddled the pause. The affected worker's largest gap starts at elapsed 2.013045 s and ends at 82.351238 s. Its final scheduler runtime is 15.552710 s, but raw PROF is only 5.504 s; the scheduler treated the unexecuted ten seconds as that task's share of CPU time.

This proves an application-visible scheduling defect under controlled host suspension. The chain is host descheduling without effective steal subtraction → excessive guest task runtime → excessive `vruntime` → prolonged exclusion of a runnable task. The result matches the roughly seventy-second prediction and cannot be explained solely by Docker display arithmetic or a blocked logging pipe.

The experiment pins tasks to one CPU to keep the runqueue and equal-weight comparison controlled. It does not prove that the original ClickHouse threads had the same affinity, host pause, or scheduling sequence. The original incident's hundreds of runnable threads and long waits are consistent with this mechanism, but fault-time host/vCPU evidence is still missing. Generic virtual accounting alone does not remove the scheduler-runtime overcharge and therefore does not address this cause.

Reproduce with the same static `thread_accounting` executable copied into a dedicated container:

```sh
python3 tests/bench/signoz/run_scheduler_debt.py --socket "$SOCKET_PATH" --container scheduler-debt --output /tmp/scheduler-control
python3 tests/bench/signoz/run_scheduler_debt.py --socket "$SOCKET_PATH" --container scheduler-debt --pause --host-vm-pid "$VM_PID" --expected-start "$VM_START" --output /tmp/scheduler-pause
```

Verify helper ownership with its open kernel and disk files before suspension. The successful helper was PID 19743, started at `Thu Oct  8 12:20:07 2026`; SIGSTOP/SIGCONT occurred at 04:25:01.084988 and 04:25:11.141336 UTC. An earlier attempt omitted a space in that start-time string and was rejected before sending a signal. The driver stopped its dedicated container on failure; no user VM was suspended. That failed record is retained separately from the successful run.

The runtime analysis checks the control and peer gaps, the affected task's continued runnable state and unchanged dispatch count, unlimited quota, and zero throttling. Python syntax, workspace formatting, and the existing `arcbox-hv` CPU accounting example's Clippy check passed. Raw records, exact source, kernel identity, and the analysis are preserved with a SHA256 manifest under `scheduler-debt-2026-10-08`. No production scheduler or kernel change was made.
