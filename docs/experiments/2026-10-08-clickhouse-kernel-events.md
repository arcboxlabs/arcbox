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
