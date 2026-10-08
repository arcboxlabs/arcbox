# Can kernel events retain the ClickHouse stall path?

- Date: 2026-10-08
- Previous evidence: [native observer gaps](2026-10-08-clickhouse-native-observer.md)
- Probes: [restart driver](../../tests/bench/signoz/run_clickhouse_restart.py), [recorder controller](../../tests/bench/signoz/clickhouse_trace.py), [guest helper](../../tests/bench/signoz/trace-clickhouse.sh)
- Evidence directory: `/private/tmp/arcbox-stall-trace-20261008`

## Method

The diagnostic Linux 6.18.38 kernel retains baseline tick CPU accounting, HZ=1000, NO_HZ_IDLE, and voluntary preemption. The configuration adds FTRACE, FTRACE_SYSCALLS, and their static-event dependencies. Function tracing, probes, debugfs, and automatic tracefs mounting remain disabled. Build from the retained, already-patched source in the owned kernel builder:

```sh
cp /build/baseline.config .config
scripts/config --enable FTRACE --enable FTRACE_SYSCALLS --disable UPROBE_EVENTS --disable EPROBE_EVENTS --disable TRACEFS_AUTOMOUNT_DEPRECATED
make ARCH=arm64 olddefconfig
scripts/diffconfig /build/baseline.config .config
make ARCH=arm64 -j6 Image
```

Review the configuration diff before compilation. The built image SHA256 is `0523aefc40200a26fb2c82d09108d3739a28aaff80e34ad03b61a1739281628f`. Its embedded configuration matches the reviewed configuration, SHA256 `38112631033bd2037a1c8c6207e8b8f0a018d814074e57a621c98cf023dee10d`. The builder is stopped before application capture. The isolated VZ guest retains 18 vCPUs and 16 GiB configured memory.

The application fixture returns to the original settings with both profilers enabled. Both arms use identical stopped seed copies and a 512-thread background pool. Each observation lasts 300 seconds, with 36 owned host workers scheduled at application +30 through +90 seconds. The driver records the effective runtime profiler settings before the identity snapshot.

```sh
python3 tests/bench/signoz/run_clickhouse_restart.py --fixture /absolute/original-fixture.json --case contention --kernel-trace off --output /absolute/new-off-case
python3 tests/bench/signoz/run_clickhouse_restart.py --fixture /absolute/original-fixture.json --case contention --kernel-trace on --output /absolute/new-on-case
```

Both arms replace the periodic proc observer with a privileged, networkless recorder sidecar in the isolated guest's PID namespace. The sidecar uses the same image as ClickHouse. The application receives no added SYS_PTRACE or SYS_NICE capability. The off arm records only control markers. The on arm additionally records scheduler switches, wakeups, migration and process lifecycle; signal generation/delivery; traditional futex entry/exit; restart_syscall entry/exit; and rt_sigreturn entry. No common_pid or comm filter is applied, because remote wakeups and signals identify their target in event payloads.

Each case mounts tracefs and creates one independent instance with the nop tracer, mono clock, all 18 CPUs, and 32 MiB per CPU. No continuous consumer runs. Prepare rejects leftover instances. Initial namespace, NSpid, stat/starttime, maps, and cgroup snapshots occur in the recorded prelude, before WINDOW_BEGIN. These reads can perturb scheduling and are not passive. The critical window has no periodic task-file reads. A second identity snapshot occurs after freezing.

The first failed host query requests an END marker and freezes recording; otherwise the 300-second deadline requests the freeze. Docker control itself can be delayed. Host request/return brackets and the guest markers therefore define the actual capture boundary. Query cadence can skip scheduled probes while the freeze command waits. Guest scheduled intervals still include possible host vCPU descheduling and are not physical CPU measurements.

Freeze also disables event probes. Export saves each CPU's statistics before and after reading its non-consuming trace, then removes the owned instance. Container cleanup occurs afterward. If collection fails, the controller attempts the remaining evidence operations, reports an incomplete capture, and stops its containers. A retained instance blocks another case until its evidence and cleanup are handled.

## Validation boundary

The kernel's successful build and matching embedded configuration do not establish successful application capture. Validate both arms in the running guest. Check initial and final CPU masks, selected clock, event formats and enables, markers, identity completeness, and every CPU's overrun, commit-overrun, and dropped-event counters. Check trace retention per CPU. An overwrite permits conclusions only about a verified retained suffix. A quiet CPU need not contain an END marker or a late event.

Do not pass these captures to the proc sample analyzer. They have a separate schema. Interpret lifecycle and scheduling events before assigning signal or futex dependencies. Signal generation and delivery need not pair one-to-one. Futex entry does not prove a sleep, and a wake return value does not identify a waiter. ARM64 rt_sigreturn can lack an exit event; a missing restart_syscall does not rule out syscall restart.

## Preparation failures and corrections

`events-off-01` failed before ClickHouse startup because the helper expected an exact 32768 KiB readback. Linux allocates whole ring-buffer payload blocks. The instance's `events/header_page` reports 4080 payload bytes per block; 8225 blocks yield 32771 KiB per CPU after integer truncation, or 589878 KiB across 18 CPUs. The helper now calculates that exact capacity, validates every CPU and the total, and saves `header_page` and `buffer-sizing.tsv`. An offline check accepts the measured capacity and rejects an incorrect CPU capacity, an incorrect total, and a duplicate payload field. This correction follows Linux v6.18.38 `kernel/trace/ring_buffer.c` and `kernel/trace/trace.c`; it does not add a tolerance.

`events-off-02` passed trace preparation and started ClickHouse, but the driver's inherited identity command failed to read `/proc/1/maps` without SYS_PTRACE. The trace mode now collects maps through the existing privileged sidecar. The application-side identity command retains only status and CPU quota reads. The proc-observer mode retains its original maps read. The failed case never reached WINDOW_BEGIN or started host contention.

Both failures preserve their command results, partial traces, final container states, and successful cleanup records. Neither is an application control result. Each owned network was inspected and archived before removal after all its containers stopped. Subsequent cases use fresh output directories.

## First completed control

`events-off-03` completes 300 seconds with 60 successful queries and no failure. Runtime settings report both profiler periods as 10000000000 ns and the background pool as 512. All 36 host workers terminate; contention starts 1.080 ms after its target and finishes 6.356 ms after the scheduled end. The marker-only guest window spans 926.408403–1224.934759. All 18 buffers have zero overrun, commit-overrun, dropped events, and read events; their counters remain stable during export. Both identity snapshots contain 713 threads. Independent offline validation passes, and all three containers stop after export.

This control is a negative application result. It does not rule out intermittent stalls or establish the cost of enabling events. The events-on arm still requires a completed, validated capture.

## First completed events-on capture

`events-on-01` also completes all 60 queries. Its 18 CPU buffers retain 6,732,869 events, including all 15 selected event kinds and five control markers. Every overrun, commit-overrun, dropped-event, and read-event counter is zero. The guest analysis window spans 1404.267918–1702.637915, or 298.369997 seconds. Both identity snapshots again contain 713 threads, and all three containers stop after export. Host contention starts 4.650 ms after its target and finishes 13.041 ms after its scheduled end.

| Case | Query successes / failures | Median host command time | Maximum host command time |
|---|---:|---:|---:|
| events-off-03 | 60 / 0 | 52.200 ms | 107.355 ms |
| events-on-01 | 60 / 0 | 57.651 ms | 190.722 ms |

These command times include Docker exec and client startup. Sequential single cases cannot establish tracing overhead. Neither case logs the earlier lock-delay or Keeper-expiry warnings; both log a merge cancellation during shutdown. No application stall has been attributed by this pair.

The [offline validator](../../tests/bench/signoz/validate_clickhouse_trace.py) checks ownership, helper hash, kernel configuration, runtime settings, identities, contention, query results, stop ordering, trace configuration, per-CPU counters, and retained window bounds. Its first on-case parse rejected Linux's `sys_futex(...)` and `sys_futex -> ...` display forms. The corrected parser normalizes those documented enter/exit forms to their configured event names and retains strict rejection of unknown events. The first rejection is preserved separately; it was a parser defect, not evidence of lost events. Both completed captures pass the corrected validator.

```sh
python3 tests/bench/signoz/validate_clickhouse_trace.py /absolute/completed-case on 2ba51fc323186f9a6c2cd10de093ff46677efba2bd461e8ccfd6e55b8ea3b236
```

## Scheduling in the completed negative capture

The retained `events-on-01` analysis separates sleeping intervals from runnable intervals. All 520 Common/BgSchPool identities retain their start ticks across the two snapshots. The merged event stream contains 119,891 complete sleep → wakeup → switch-in chains and 14 runnable switch-out → switch-in chains, with no state-machine anomalies.

| Threads | Longest runnable switch-out → switch-in | Longest wakeup → switch-in | Longest sleep → wakeup |
|---|---:|---:|---:|
| 8 Common | 50 µs | 867 µs | 1.410474 s |
| 512 BgSchPool | 91 µs | 13.526 ms | 25.002971 s |

The 25-second interval is an untimed `FUTEX_WAIT_BITSET_PRIVATE|CLOCK_REALTIME` sleep, followed by wakeup, switch-in, and return zero. It is not runnable starvation. Three uninterruptible intervals are classified separately; the longest is 50 µs. These intervals use guest mono time and do not measure physical CPU execution. The versioned parser, boundary self-checks, and `scheduling-analysis-v2.json` remain with the raw evidence.

## Startup contention retains a partial stall

The startup variant starts ClickHouse five seconds after all 36 host workers start. `startup-events-on-01` fails earlier: copying configuration into the stopped clone returns overlayfs EBUSY. No application, tracing, or contention starts. After the recorded cleanup, the exact same copy command succeeds in 24.835 ms. The failed result and successful recheck are retained. No driver retry or mount workaround is added, and this preparation failure has no established root cause.

`startup-events-on-02` starts under contention without the periodic proc observer. ClickHouse logs readiness about 0.7 seconds after startup, but the host Docker logs command exceeds its five-second timeout. The driver therefore never reaches the SQL loop or identity prelude. It freezes and exports the trace before stopping the containers. Application logs show Keeper operation timeouts and session expiry before that stop request; they contain no executor lock-delay warning.

The partial capture retains 130,997 events across 18 CPUs from ARM at 2338.049519 to END at 2379.204326. Every loss and consumption counter is zero. Frozen counters remain stable through export, and no event follows END. Preparation, freezing, export, copying, and container cleanup succeed. The separate partial checker passes capture integrity; the full validator correctly rejects the missing 300-second observation. Missing runtime settings, maps, process identity snapshots, WINDOW_BEGIN, and SQL results remain explicit limitations.

The trace contains one `/usr/bin/clickhouse-server` exec, at guest PID 23381. Fork lineage and comm names identify candidate descendants, without proving container or thread-group membership. Two raw chains establish substantial guest scheduling delay:

| Candidate | Retained chain | Elapsed guest mono time |
|---|---|---:|
| BgSchPool 23841 | wakeup at 2347.788279 → switch-in at 2378.867562 | 31.079283 s |
| BgSchPool 23490 | R+ switch-out at 2346.013685 → switch-in at 2375.098330 | 29.084645 s |

For 23490, signal 10 is delivered at 2345.795753. The first subsequent `rt_sigreturn` occurs at 2375.098402, only 72 µs after switch-in. Most of this candidate signal-handler interval is therefore an observed runnable wait. The raw boundaries are `cpu11.trace:3249,3252` and `cpu12.trace:8860,8863`. This does not establish continuous handler execution, a particular glibc wait, or EEVDF debt.

Other CPUs continue producing events during the long waits. The largest gap in the merged selected-event stream is 209.663 ms; the largest single-CPU event gap is 1.952271 s. These observations exclude a continuous whole-VM pause across the 29-second interval, but do not exclude individual host vCPU descheduling, guest execution without selected events, or kernel lock delays.

The freeze request takes 34.596592 seconds to return on the host. This includes Docker dispatch, guest scheduling, helper execution, and response transport. It is not a measurement of the kernel tracing-disable operation. The helper's process creation → Bash exec interval alone spans 7.866037 guest seconds. The FROZEN control row follows disabling both recording and events; its timestamp does not identify when recording stopped.

The next driver revision moves its existing container inspect immediately after successful start and saves `ch-started.json`. If readiness then fails, finalization can request identity-after using the captured guest PID. A failed early inspect still leaves that identity unavailable. The change adds no periodic process reads, and the frozen snapshot remains insufficient by itself to exclude PID reuse.

## Production statistics use a different observation path

Desktop Activity, the menubar, and `abctl top` share the guest WatchStats stream. The collector reads global proc statistics, cgroup counters, and per-container network counters. Docker stats uses Moby 29.8.2, containerd 2.3.6, and cgroups 3.1.3 to read cgroup statistics. Neither path enumerates application TIDs to read syscall, wchan, status, stat, or schedstat as the earlier diagnostic observer did.

Linux `cpu.stat` flushes per-CPU rstat state while holding its base lock with interrupts disabled. The diagnostic syscall snapshot can instead wait for a task to become inactive and acquire task/runqueue locks. These are distinct dependencies, not attributed causes. Production statistics were not added to this capture. Startup contention reproducing delay without periodic proc reads shows that those reads are not necessary for this observed scheduling failure.

## Repetition with early container identity

`startup-events-on-03` completes all 60 queries, with a median host command time of 54.529 ms and a maximum of 83.218 ms. The full validator passes. All 6,641,887 retained events have zero loss or consumption, and both snapshots contain 717 threads. `ch-started.json` identifies guest PID 25223; both snapshots match that PID, NSpid 1, and the same container cgroup. The 36 workers all terminate with SIGTERM, and all three containers stop after trace export. No warning or error appears in the saved ClickHouse log.

This repetition exercises early PID capture and normal frozen identity collection. It does not exercise identity collection after a readiness failure, and query success alone does not establish short scheduling waits. The startup failure remains intermittent; no production repair follows from this repetition.

## Queue pressure and delay in otherwise successful queries

Independent lifecycle analysis of the partial `startup-events-on-02` trace establishes a peak of 644 simultaneously runnable, off-CPU descendants: 594 from observed sleep/wakeup transitions and 50 from runnable switch-outs. Another 13 descendants are guest-current at that point. At least 400 tasks remain queued for a cumulative 29.483906 seconds. At END, 639 are queued, including 412 with waits of at least ten seconds. There are no state anomalies. Recent comm names identify 507 queued Common/BgSchPool tasks at their peak. The earlier exploratory count of 514 used any previously observed name and is superseded. Fork lineage still does not replace the missing container identities.

The completed `startup-events-on-03` capture also contains substantial delays despite successful SQL probes. BgSchPool 25346 waits 4.474759 seconds after a runnable switch-out, BgSchPool 25803 waits 4.438620 seconds after wakeup, and Common 25939 waits 4.063912 seconds after wakeup. For 25346, the signal-delivery-to-return candidate spans 4.545021 seconds; only nine microseconds separate its final switch-in from `rt_sigreturn`. The Common delays precede dispatch rather than appearing as long delivery-to-return candidates. Both identity snapshots retain matching start ticks and NSpid for all 717 threads.

## Lock contention and scheduler runtime in a completed failure

The next helper adds `sched/sched_stat_runtime`, `lock/contention_begin`, and `lock/contention_end` to the same recording method. Its SHA256 is `8b882a42d29639b5a52271c92860b15f2085f3c39aa367965b99c2651f39025d`. The kernel image and configuration are unchanged. The current validator requires all 18 event enables; validate earlier 15-event captures with their retained validator and helper snapshots. Each new case preserves its driver sources.

`startup-locks-on-01`, prefix `stall-warm-startup-contention-fe335e9a`, completes the 300-second observation with 55 successful queries and one timeout. Query 6 takes 4.004790 seconds before the host timeout. Its freeze request takes 20.717278 seconds to return. The recorded analysis window spans guest mono 5221.485726–5273.398767. All loss and consumption counters are zero, and full validation passes. Both identity snapshots contain 706 threads with matching start ticks and NSpid, including 512 BgSchPool and eight Common threads. All 36 workers terminate with SIGTERM; all containers stop after export. Keeper session expiry occurs before cleanup.

The trace retains 170,876 scheduler-runtime events, 7,424 contention-begin events, and 7,409 contention-end events. The longest completed target wakeup-to-dispatch wait is 25.337311 seconds. Two identity-verified chains connect long SPIN contention and scheduler runtime charges to subsequent runnable waits:

| Thread | SPIN interval | Following runtime charge | Runnable wait | Final switch-in → sigreturn |
|---|---:|---:|---:|---:|
| Common 3195 | 349.098 ms | 349.125082 ms | 15.412658 s | 12 µs |
| BgSchPool 2641 | 490.098 ms | 490.121915 ms | 19.683360 s | 228 µs |

For Common 3195, `cpu6.trace:63868–63870` records SPIN begin at 5257.628095, acquisition at 5257.977193, and the runtime charge. The task leaves with state R+ at 5257.977267 (`cpu6.trace:63876`), returns at 5273.389925 (`cpu11.trace:59546`), and reaches `rt_sigreturn` at 5273.389937. Its delivery-to-return interval is 15.761910 seconds. Two earlier acquisitions precede signal delivery and are not part of that interval.

For BgSchPool 2641, `cpu14.trace:67135–67138` contains the SPIN/runtime sample. The task leaves with state R+ at 5253.707533 (`cpu14.trace:67180`), returns at 5273.390893 (`cpu17.trace:65541`), and reaches `rt_sigreturn` at 5273.391121 (`cpu17.trace:65561`). Its delivery-to-return interval is 25.380164 seconds, including repeated kernel activity before the long runnable wait.

Both samples use printed lock hash `0000000048a27d59`. The exact-SPIN analyzer pairs all 4,434 recorded waits for that hash. Their maximum is 504.577 ms, their concurrent recorded waiter peak is 18, and their elapsed-time sum is 443.844729 seconds. That sum overlaps across tasks and is neither wall time nor physical CPU time. Across 64 hashes, all 5,590 exact-SPIN begins pair without duplicates or open records.

```sh
python3 tests/bench/signoz/validate_clickhouse_trace.py /absolute/startup-locks-on-01 on 8b882a42d29639b5a52271c92860b15f2085f3c39aa367965b99c2651f39025d
python3 tests/bench/signoz/analyze_clickhouse_spin.py --self-test
python3 tests/bench/signoz/analyze_clickhouse_spin.py /absolute/startup-locks-on-01
```

The first exploratory lock parser conflated per-CPU PID 0 identities and treated mutex optimistic-spin/sleep events as simple pairs. Its duplicate/unmatched records and reported mutex maximum are not validated evidence. The retained v2 analysis and repository analyzer separate idle tasks by CPU and pair only exact SPIN events. Their results match. Self-checks cover idle keys, cross-thread pairing, negative intervals, and runtime parsing.

Linux 6.18.38 `queued_spin_lock_slowpath` emits these events around the MCS queue path. The pending-bit optimistic path bypasses them. A contention-end event means acquisition, not release; the printed `%p` value is a hash, not a resolved lock identity. `get_signal` emits `signal_deliver` while holding `sighand->siglock`, before entering the user handler. Delivery-to-return therefore bounds a candidate signal path, not continuous user-handler execution. Scheduler runtime charges and guest mono spans can include host vCPU descheduling. These chains do not alone prove EEVDF debt, identify the lock, or establish the original glibc notifier dependency.

The median observed signal-10 generation interval is 1.410079 seconds. This agrees with the previously documented old ClickHouse profiler-period truncation: the configured 10000000000 ns becomes 1410065408 ns. The fixture still contains that known defect; the event count is not evidence for a new ten-second timer mechanism.

## Host scheduling preflight

A ten-second macOS System Trace attached to the owned VZ helper succeeds and exposes named threads `com.apple.virtualization.thread.cpu-0` through `cpu-17`. Context-switch records contain host thread IDs, physical cores, states, and nanosecond timestamps. This provides an independent route to measure host execution. The initial template reports a five-second window, but the actual context-switch coverage is only about 334.6 ms at the recording tail. Long inferred initial thread-state intervals contain sentinel core and CPU-time fields and cannot establish earlier on-core execution. Validate retained coverage and clock alignment before using a host trace to interpret a guest stall.

A second preflight explicitly requests a 30-second window. Its recorded duration is 32.120922 seconds, but context-switch events cover only 29.988445791–32.120742458, or 2.132296667 seconds. CPU samples have the same late start. Extending the requested window therefore does not establish usable coverage. The thread-state and cpu-state tables share a modeler and input stream; agreement is internal consistency, not independent confirmation. Later idle `ps` readings are not contemporaneous counter evidence and cannot establish a discrepancy during either recording. Neither preflight is used to attribute the guest failure. No host recording runs during the next application case.

## Bounded stacks identify the process signal lock

The same diagnostic kernel already enables STACKTRACE, FRAME_POINTER, and KALLSYMS. The next helper retains the primary 18-event instance and adds a separate `INSTANCE-stacks` instance. The additional instance uses nop, mono, all 18 CPUs, and a requested 1024 KiB per CPU, which rounds to 1027 KiB. It records exact-SPIN contention begins and installs `stacktrace:128 if flags == 1` after recording starts. Freezing stops both instances and explicitly removes the conditional trigger. Recording-off and event-off alone do not unregister that trigger. The first freeze preserves its trigger readback; repeated freeze calls do not overwrite that evidence. Both buffers export before either instance is removed.

The kernel decrements the trigger count without atomic synchronization. The requested count is not a strict global limit; count consumption also does not guarantee successful stack storage. Stack collection runs synchronously in the contender's path and can perturb the measured wait. IRQ stacks may contain the interrupted task's frames. A stack record has no lock address, so the analyzer associates only adjacent same-CPU, same-TID, same-context SPIN begins and leaves ambiguous sequences unresolved.

`startup-stacks-on-01` fails only the preparation readback check: the kernel prints `stacktrace:count=128`, whereas the prototype expected `stacktrace:128`. No application or host-pressure workload starts. Export and cleanup succeed. The next helper corrects that exact display format and preserves the failed attempt. The successful capture helper SHA256 is `885af992df01f316aae6cf717854128e626d80629eb7e573640d0f2c59147d96`.

`startup-stacks-on-02`, prefix `stall-warm-startup-contention-2f7f1a34`, fails ClickHouse's 25-second readiness deadline. The final log places readiness 54.778684 seconds after its startup message on the guest realtime clock. This is a partial startup failure: no SQL observation, effective-settings query, before snapshot, or WINDOW_BEGIN exists. Early inspect records guest leader 13098; frozen identity-after contains 712 tasks in that thread group. The after-only snapshot does not establish all earlier identities by itself. All 36 host workers terminate, all three containers stop, and cleanup reports no errors.

Independent capture-integrity checks pass. The primary ring retains 121,001 records. The stack ring retains 7,370 records: 7,241 exact-SPIN begins and 129 stacks. Frame continuation lines do not count as ring records. Every CPU has zero overrun, commit-overrun, dropped-event, and read-event counters, and frozen counters remain stable during export. The armed trigger reads 128 and the frozen trigger reads zero. All 129 stacks have an adjacent begin without a context ambiguity. Stack sampling spans 7485.693604–7492.208201 and exhausts its budget during early startup.

Printed hash `00000000e230f497` accounts for 7,164 stack-instance begins and 88 captured stacks. Seventy stacks have ordinary task context and a TID present in the ClickHouse after snapshot. The group includes `do_sigaction`, `__set_current_blocked`/`sigprocmask`, and POSIX timer paths. Ten stacks have hardirq context, including `posix_timer_fn`/`posixtimer_send_sigqueue`/`__lock_task_sighand`. Eight early runc stacks and one idle IRQ lack an after TID. Those records must not be relabeled as ClickHouse user-thread calls, and equal hashes across object lifetimes do not exclude address reuse or collision.

A concrete ordinary-task chain identifies the lock without relying only on its hash:

| Evidence | Recorded value |
|---|---|
| TID 13843, CPU 1 stack | `queued_spin_lock_slowpath → do_sigaction → __arm64_sys_rt_sigaction` |
| Primary begin → acquisition | 7492.207339 → 7492.253257; 45.918 ms |
| Following scheduler charge | 45.996750 ms |
| Runnable switch-out → next switch-in | 7492.253309 → 7512.239204; 19.985895 s |

The stack-instance begin is at 7492.207343 and its stack at 7492.207344, inside the unique matching primary interval. Raw locations are `stacks/export/cpu1.trace:122–133`, `export/cpu1.trace:2977–2981`, and `export/cpu9.trace:2779`. The event comm is ThreadPool; the after snapshot names the same TID SystemLogFlush, with Tgid 13098 and NSpid `13843 725`.

Version-exact [Linux `do_sigaction`](https://github.com/gregkh/linux/blob/v6.18.38/kernel/signal.c#L4314) takes `current->sighand->siglock` at this call site. This establishes a ClickHouse contender in the guest kernel's process-shared signal lock. [ClickHouse QueryProfiler](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Common/QueryProfiler.cpp#L245) installs a signal action before creating and setting its timer, consistent with the captured startup paths. The kernel stack alone does not identify the user-space caller or signal number. A timer-create stack alone also cannot distinguish its timer lock from the sighand lock.

The result directly connects an identified signal-lock wait, a scheduler charge, and a long runnable interval in a real application startup. It does not identify the lock holder, measure physical execution, establish EEVDF debt, or prove the original glibc notifier chain. The earlier capture's unresolved hash is not retroactively assigned an identity. Production behavior remains unchanged.

The [stack checker](../../tests/bench/signoz/analyze_clickhouse_stacks.py) validates stack-instance settings, both rings' record counts and loss counters, and conservative begin/stack associations. Its nine self-checks exercise frame counting, ambiguous nesting, identity/context changes, timestamps, CPU IDs, and malformed records. Its PASS is not a complete application-case validation. The 13843 chain also has one recorded fork, no exit, exec, or repeated fork, and an after start tick matching that birth. No intervening switch for that TID occurs in its 19.985895-second runnable interval.

```sh
python3 tests/bench/signoz/analyze_clickhouse_stacks.py --self-test
python3 tests/bench/signoz/analyze_clickhouse_stacks.py /absolute/startup-stacks-on-02
```

## The delayed task's assigned CPUs remain in spin contention

The same zero-loss capture narrows the 13843 wait without adding a workload. During 7492.253309–7512.239204, ten migration events move the runnable task along CPU path `1 → 6 → 16 → 2 → 8 → 4 → 17 → 9 → 11 → 0 → 9`. A single-runqueue debt calculation cannot describe this interval.

The retained `analyze-assigned-cpu-overlay-v2.py` pairs exact-SPIN events on each CPU, clips each pair to the target's assignment interval, and takes the interval union. Begin flags for the selected hot-hash pairs report disabled IRQs. All selected pairs close, migration sources match the previous destination, and the assignment intervals partition the complete wait. Union and clipping self-checks cover overlaps, duplicates, empty intersections, and disjoint intervals.

| Guest elapsed coverage | Measured duration |
|---|---:|
| Target runnable wait | 19.985895 s |
| Hot-hash SPIN coverage along assigned CPUs | 19.527241 s; 97.7051% |
| Task-context portion of that coverage | 12.261626 s |
| Hardirq-context portion of that coverage | 7.265615 s |
| Idle-current coverage along assigned CPUs | 0 s |
| All 18 CPUs: union of 2,463 hot-hash SPIN pairs | 348.606252 guest-CPU-s; 96.9034% of 359.746110 guest-CPU-s |

The hot hash is the one associated with the identified `do_sigaction` signal-lock contender in this capture. IRQ common_pid identifies the interrupted current task, not the timer target or lock holder. These are guest mono spans, including possible host descheduling. They do not measure physical CPU execution. The interval union avoids counting overlapping records twice.

The event-derived lower bound on queued ClickHouse after-snapshot identities peaks at 662 and averages 567.55 over the window. At least 400 such tasks remain queued for a total of 16.958680 seconds. These values are not kernel `nr_running` or eligibility measurements. The target's preceding runtime charges total 50.845835 ms, but no event records its virtual lag, deadline, runqueue average, or hierarchical eligibility. The capture also lacks effective ancestor cgroup quota/throttle snapshots. The container configuration itself has no CPU quota.

The exact non-RT `spin_lock_irq()` path disables local IRQs and preemption before entering qspinlock. Long contention can therefore defer local timer ticks and scheduling even when the vCPU executes throughout. The baseline kernel uses tick accounting and disables IRQ_TIME_ACCOUNTING; scheduler runtime records cannot separate task and interrupt execution. A scheduler/tick discrepancy in this path is not by itself stolen-time evidence. The controlled VM-suspension result remains separate.

The direct observation is extensive IRQ-disabled guest lock contention while the task waits across multiple CPUs. EEVDF debt remains unproven in this application capture. Independent host residency is needed to distinguish physical spinning from host scheduling amplification. The detailed results are retained as `runnable-fairness-window-v3.json` and `runnable-assigned-cpu-overlay-v2.json`, with their versioned analysis sources.

## Minimal host tracing still needs a complete source capture

A supported Blank template plus the Thread State Trace instrument reports reaching its 30-second limit. The driver's 300-second total process wait expires before an exit status is collected. The logs do not separately time postprocessing. The recorder subsequently disappears, leaving an approximately 39 GiB package. Export rejects that package with `Document Missing Template Error`. This is an incomplete preflight with unknown recorder exit status, not usable host scheduling evidence or proof of a fixed Instruments defect. The exact command, logs, and incomplete package are retained.

The earlier `ktrace info` message is misleading. A read-only direct library call returns EPERM at effective UID 501. The installed tool's error path passes the fixed value EX_OSERR (71) as the error-code argument to `errc`, producing the displayed EREMOTE text. Running `ktrace info` with administrator authorization in the user's terminal succeeds and reports disabled tracing with no allocated kernel buffer; tailspind is the recorded background owner. The tool process does not share that terminal's sudo authorization. No global tracing reset, daemon termination, or permission change is used to obtain access.

The prepared one-shot `capture-host-root30.sh` verifies the owned daemon/helper identities, records 30 seconds through `ktrace dump`, retains scheduling and trace-metadata classes, and returns ownership of its output files to the user. The raw TRACE_LOST_EVENTS ID is `0x07020008`; the previous Instruments tables do not expose its `7,2` subclass, so those exports cannot establish zero source loss. A new root capture must validate coverage and loss markers before application correlation.

## Prior ClickHouse profiler timer contention

[ClickHouse PR 48778](https://github.com/ClickHouse/ClickHouse/pull/48778), merged on 2023-06-14, explicitly reports heavy kernel lock contention from frequent QueryProfiler timer creation and deletion. The change reuses per-thread timers and reports a 2.5% ClickBench geometric-mean improvement on a 224-vCPU machine. This is primary-source precedent for contention in the same profiler/timer subsystem.

The reproduced ClickHouse commit already contains the thread-local Timer, `createIfNecessary()`, and stop/reuse design. The old PR is therefore not a missing fix for this case. Its report does not name `sighand->siglock`, identify Apple VZ behavior, or establish this capture's scheduling chain. The targeted search finds no exact matching public report. The PR metadata/body, diff, exact current header, and comparison are retained with the experiment evidence.

## Frozen evidence archive

The task artifact `clickhouse-kernel-2026-10-08` preserves 42,727 files and nine symlinks, totaling 46,051,949,850 file bytes. Every copied file passes SHA256 comparison against its source, and every symlink target matches. The archive includes completed guest captures, failed attempts, the incomplete Instruments package, source snapshots, and repository files at `b1c1c595b028373b4c0fd2288272a554f3872c28`. Its `archive-verification.json` and `SHA256SUMS` retain the verification result and manifest.

All 73 containers in the isolated guest are stopped. Both seed container/image/volume identities, three fixture configurations, the diagnostic kernel/configuration, and the daemon binary pass identity or hash checks. The last empty owned stack-case network is archived and removed. The owned daemon 91525 and helper 91560 remain available for the pending user-run root preflight. The archive substitutes a verified snapshot for their live daemon log and excludes `host-ktrace-root30.*`, which belongs to that separate pending capture. No production implementation or configuration is changed.
