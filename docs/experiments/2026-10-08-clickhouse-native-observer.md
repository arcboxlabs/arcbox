# Does ClickHouse still stall with a low-cost observer?

- Date: 2026-10-08
- Previous evidence: [restart contention](2026-10-08-clickhouse-restart-contention.md)
- Probes: [native observer](../../tests/bench/signoz/observe_clickhouse.c), [driver](../../tests/bench/signoz/run_clickhouse_restart.py), [strict analyzer](../../tests/bench/signoz/analyze_clickhouse_samples.py)
- Evidence directory: `/private/tmp/arcbox-stall-native-20261008`

## Question and method

The Bash observer consumed 60–88 guest CPU seconds per 300-second application case and lost tens of seconds during faults. The native observer tests whether lower collection cost preserves the failure. A separate FIFO arm tests whether an observer outside the fair scheduler can retain samples during the failure.

The experiment retains the previous stopped seeds, image, glibc 2.35, Linux 6.18.38 baseline kernel, 18-vCPU VZ VM, and 512-thread background pool. Only the Real profiler is enabled. Its configured period remains 10,000,000,000 ns, with the old binary's truncation to 1.410065408 seconds. Each case lasts 300 seconds; 36 owned host workers run from application start +30 to +90 seconds. Other user VMs remain running, and unrelated host activity is uncontrolled.

The native observer emits the existing format without changing the strict analyzer. Each sweep reads the eight Common and 512 BgSchPool threads, then sleeps for one second. Reads have no seek and status is parsed from a complete buffer. The observer asserts that sampled application threads use SCHED_OTHER. FIFO mode sets only the observer to SCHED_FIFO priority 1 and adds SYS_NICE to its owned container. The recorded guest RT budget is 950,000 microseconds per 1,000,000-microsecond period.

The final probe flushes the start marker before scanning and the remaining output at sweep end. The final driver freezes the observer bytes and executable mode before preparing containers. The probe rejects NDEBUG because the checked system calls must execute. These safeguards were added after the first successful FIFO case; the SCHED_OTHER case uses the final probe. The earlier FIFO binary had the same field collection and one-second sleep but buffered the whole sweep. Its missing partial tail therefore cannot establish that no incomplete final sweep began.

Build and run the final probe with the installed cross compiler:

```sh
aarch64-linux-musl-gcc -std=c11 -O2 -Wall -Wextra -Werror -static tests/bench/signoz/observe_clickhouse.c -o /tmp/observe-clickhouse
python3 tests/bench/signoz/run_clickhouse_restart.py --fixture /absolute/fixture.json --case contention --native-observer /tmp/observe-clickhouse --observer-policy other --output /absolute/new-case
python3 tests/bench/signoz/analyze_clickhouse_samples.py /absolute/new-case /absolute/analysis.json
```

Use `--observer-policy fifo` for the FIFO arm. Keep assertions enabled. Each case requires a new output directory and verified stopped seed identities.

## First completed pair

| Case | Observer policy | Query success / host timeout | Complete sweeps | Observer guest CPU | Longest sweep | Longest gap between sweeps |
|---|---|---:|---:|---:|---:|---:|
| real-only-native-03 | FIFO, priority 1 | 60 / 0 | 292 | 5.951737 s | 2.186250 s | 1.064923 s |
| real-only-native-other-02 | SCHED_OTHER | 56 / 4 | 277 | 2.215476 s | 0.015137 s | 22.837934 s |

Both captures pass strict validation: 151,840 and 144,040 thread records respectively. Each sweep has eight Common and 512 BgSchPool records. Runtime SQL confirms Real=10,000,000,000, CPU=0, and background_schedule_pool_size=512. Both cases complete their 300-second observation and stop their clones without cleanup errors. No cgroup quota throttling occurs.

The SCHED_OTHER case times out on probes 14–17, requested at application start +70, +75, +80, and +85 seconds. The gap from sweep 66 end to sweep 67 begin lasts 22.837934 seconds. Across those sweeps the observer gains only 8.369414 milliseconds of guest runtime but 20.651824 seconds of scheduler delay. Common threads accumulate 17.476735–21.925469 seconds of run delay; these are accumulated delays, not proof of uninterrupted exclusion. The cgroup gains 397.065979 guest CPU seconds, including 300.654186 system seconds. Those values do not independently measure host physical execution.

Across the complete SCHED_OTHER capture, observer runtime accounts for 0.5092% of the 435.085880 guest CPU seconds charged to the cgroup. The low-cost observer therefore reproduces the timeout without the earlier Bash observer's large runtime contribution. The capture still loses the fault interval because the observer is also delayed by the fair scheduler. No known G1/reference-lock PC or repeated R record with unchanged runtime and dispatch count appears in the retained complete samples. The gap prevents exclusion. One post-gap Common record uses an ordinary libc low-level futex lock; the saved syscall alone does not identify the caller or lock owner.

Neither case logs a ClickHouse Warning, Error, or session-expired message. Host probe duration includes Docker exec and client startup; a timeout does not prove that SELECT itself spent four seconds inside the server. Application-thread scheduler counters provide independent evidence that application threads also experienced delay.

The first pair does not establish FIFO as a cure. Policy changes observer interference, and the cases are sequential rather than simultaneous. The observation remains compatible with multiple initiating causes, including handler work, guest scheduling/accounting, and host vCPU preemption.

## Failed preparations and verification

The first native preflight fails because musl's sched_setscheduler wrapper returns ENOSYS without entering the kernel. Retained disassembly confirms that behavior. The probe uses the Linux task-policy syscall directly. The second preparation hits a five-second Docker logs timeout before observation; an escalated retry of the exact logs command succeeds. Neither attempt contributes application results.

The first SCHED_OTHER preparation exhausts Docker's default network address pools. Three networks from this continuation's already-stopped cases are inspected, archived, and removed after confirming experiment labels and empty endpoint sets. Containers and volumes remain. The failed attempt and network inspection/removal records are retained; no network workaround or retry is added to the driver.

Validation includes static cross compilation with warnings as errors, intentional rejection of an NDEBUG build, Python syntax, existing strict analysis of real captures, frozen observer hash checks, cargo fmt, and targeted Clippy. No production runtime, kernel, or libc change is made.

## FIFO repetition with the final probe

Real-only-native-fifo-04 uses the final probe and passes strict validation with 290 complete sweeps and 150,800 records. It completes 59 queries and times out on query 17, requested at application start +85 seconds. Runtime SQL verifies the same profiler and pool settings. Cleanup succeeds and cgroup quota throttling remains zero.

Sweep 80 spans 06:39:38.943495–06:39:47.470037 UTC, or 8.526543 seconds. The median sweep is 0.007187 seconds. Compared with the preceding sweep, observer scheduler counters increase by 7.987591 guest runtime seconds, 0.672036 run-delay seconds, and six dispatches. Cgroup BEGIN counters from sweep 80 to 81 gain 146.916962 guest CPU seconds, including 93.645208 system seconds. These are different read boundaries; they must not be treated as an atomic interval or physical CPU measurements.

Common thread run-delay increases during the slow sweep range from 0.001092 to 6.821579 seconds. No known G1/internal-condition-lock PC or repeated R record with unchanged runtime and dispatch count is captured. The observer itself is outside the fair scheduler, yet it still takes seconds to finish a sweep. Guest RT throttling, blocking inside collection, host vCPU preemption, and charged execution time are not distinguished by these samples. The recorded RT budget is a configured limit, not a measurement of actual RT throttling.

Across the complete capture, observer runtime is 10.203323 guest CPU seconds and cgroup usage is 185.560208 seconds. This repetition refutes the idea that FIFO sampling reliably removes the fault. It also confirms that a FIFO observer alone does not close the observation gap.

## Next capture boundary

The baseline kernel has PERF_EVENTS but lacks FTRACE, EVENT_TRACING, and DEBUG_FS. Enabling a consumer cannot recover absent kernel events. A diagnostic kernel can retain baseline tick accounting and add FTRACE plus FTRACE_SYSCALLS, then record only scheduler, signal, futex, and signal-return events into tracefs per-CPU buffers. Function tracing and kprobes are unnecessary for this first capture.

The next comparison must first check reproduction with events disabled on that diagnostic kernel. During event capture, no continuous user-space consumer is required. Stop recording before exporting buffers. Preserve initial-namespace TID mappings, per-event formats, cross-CPU monotonic clock selection, host/guest clock brackets, and each CPU's loss counters and retained time range. Missing events cannot exclude a path unless the relevant interval is retained without loss. Guest scheduled intervals still do not measure physical host execution.

## Procfs access can perturb the target

The version-exact Linux source reveals an additional observation effect. Reading [proc_pid_syscall](https://github.com/gregkh/linux/blob/v6.18.38/fs/proc/base.c#L682) calls [task_current_syscall](https://github.com/gregkh/linux/blob/v6.18.38/lib/syscall.c#L69), which can call [wait_task_inactive](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/core.c#L2258) twice. A target already in TASK_RUNNING returns an unavailable snapshot. A target with a matching nonzero state and on_cpu set can instead leave the reader waiting until the target stops running. The path includes busy waiting, task/runqueue locks, and a timed sleep branch. It can also process a sched_delayed target. A long access therefore need not be pure busy spinning or passive observation.

Reading wchan does not call wait_task_inactive. [get_wchan](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/core.c#L2066) holds the target's pi_lock while ARM64 performs a bounded stack walk. Wakeup needs the same lock. Host descheduling of a vCPU that holds a lock or must clear on_cpu can extend these waits. These are permitted mechanisms, not an attribution of the recorded slow FIFO sweep. Seven exact source files and verified Git blob/SHA256 identities are retained under proc-syscall-sources.

The earlier references to the final probe describe v4. A subsequent v5 probe times each task-field access with CLOCK_MONOTONIC and reads observer SCHED and raw per-thread PROF clocks. Real-only-native-readtiming-01 completes 60 queries, 294 sweeps, and 152,880 valid records. Median and maximum sweeps are 0.017212 and 0.033120 seconds. No completed timed access exceeds ten milliseconds. Observer runtime is 5.305500 guest seconds, run delay is 0.005055 seconds, and cgroup usage is 43.124448 guest CPU seconds. Cleanup succeeds. This case does not reproduce the stall and does not locate the earlier slow access.

Review identifies a limitation in v5: reading SCHED can itself call task_sched_runtime(current), take the observer's task/runqueue locks, and update scheduler accounting. Its clock brackets also differ from PROF and MONOTONIC. Neither SCHED minus PROF nor wall time minus SCHED measures host stolen time or physical execution. Interrupt-disabled execution can also delay tick accounting. The frozen v5 source and binary remain evidence; a separate source supplement records this clock path.

The current v6 probe removes the per-field CPU clock calls. Its stderr record is `slow_read<TAB>TID<TAB>field<TAB>begin_monotonic_ns<TAB>elapsed_ns` for a completed access exceeding ten milliseconds. The interval covers open, reads, close, and buffer termination, including scheduling or host preemption. It does not identify a particular kernel function. Directory traversal, output flushing, boundary reads, and stderr output are outside these intervals. An absent record does not exclude a stall in those operations or an unfinished access.

Real-only-native-readtiming-02 uses v6 with the same FIFO policy and contention schedule. It completes 60 queries, 299 sweeps, and 155,480 valid records. Median and maximum sweeps are 0.007320 and 0.014401 seconds; no timed field access exceeds ten milliseconds. Observer runtime is 2.384877 guest seconds, run delay is 0.001766 seconds, and cgroup usage is 38.405809 guest CPU seconds. The capture has no partial tail or quota throttling, and cleanup succeeds. The two negative repetitions do not invalidate earlier positive cases or identify the slow field. Kernel events remain necessary to distinguish the candidate paths.

## Retention and cleanup

After the two timing cases, all 14 created clones are verified stopped and both seed identities remain unchanged and stopped. Config hashes and eight retained observer binary hashes match. The first three preparations predate per-case binary freezing; their recorded source-path binaries are retained and verified instead. Their provenance remains weaker than the later frozen-byte cases.

Owned daemon 49006 exits with status zero after SIGTERM, and owned helper 49040 exits. The daemon logs a five-second server-drain timeout during shutdown. The three pre-existing user VM helpers remain present, and the user's Docker endpoint reports version 29.8.2. This is a runtime health check, not an end-to-end ingestion test. No production runtime setting changes. Raw evidence, source snapshots, and committed reports are retained with a verified SHA256 manifest in the task artifact directory `clickhouse-native-2026-10-08`.
