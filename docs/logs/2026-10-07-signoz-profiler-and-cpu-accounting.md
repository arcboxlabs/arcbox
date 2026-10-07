# 2026-10-07 — SigNoz remains idle after ClickHouse stalls

- Type: investigation
- Area: VZ guest CPU accounting, ClickHouse, SigNoz Collector
- Application changes: none
- Status: Collector failure and profiler defect confirmed; the cause of guest accounting loss remains open

## Problem

SigNoz started under ArcBox, but ClickHouse and Keeper stalled, Docker commands became slow, and the Collector failed to initialize. Docker reported more than 50,000% CPU on an 18-vCPU guest. ClickHouse later recovered, but the Collector still did not listen on OTLP ports.

All times below use UTC on 2026-10-07. Add eight hours for Asia/Singapore.

## Environment and method

- Host: Apple M5 Max, 18 cores, macOS 26.4.
- VM: Apple Virtualization.framework, 18 vCPUs, 16 GiB RAM, boot bundle 0.8.7.
- Running kernel: `6.18.38-arcbox`, aarch64; Docker 29.7.2, cgroup v2.
- ClickHouse: `25.12.5.44`, commit `6b09ccf262fd16345434572fb4d45dfd608f86c2`.
- SigNoz: `v0.145.0`; Collector: `v0.144.12`.

The investigation used the original Docker API sample, complete container logs, read-only `/proc` snapshots, and ClickHouse history tables. The SQL queries limited execution time to 10 seconds and query threads to two. No service was restarted, no deployed configuration was edited, and no stress workload was started.

The original diagnostic package is named `arcbox-signoz-diagnostics-2026-10-07`. Additional command records contain the command, UTC start time, duration, exit code, stdout, and stderr. The records remain local because complete configurations and logs are deployment-specific.

## Findings

### 1. The Collector rolled back to nop pipelines

At about 11:29:29, applying remote OpAMP configuration failed. `clickhouselogsexporter` timed out while reading from ClickHouse at `172.25.0.3:9000`.

At 12:50:46, the actual `/var/tmp/collector-config.yaml` had four pipelines: logs, metrics, metrics/meter, and traces. Every pipeline used only the `nop` receiver and exporter. None referenced the configured OTLP receiver.

The Collector listened on health and diagnostic ports 13133, 1777, and 8888, but not 4317 or 4318. Its OpAMP connection remained established. At 12:54:47, a request from the Collector network namespace to ClickHouse `/ping` succeeded.

The exact [Collector source](https://github.com/SigNoz/signoz-otel-collector/blob/v0.144.12/opamp/server_client.go) explains this state:

1. `Start` creates and starts nop pipelines while keeping extensions available.
2. `reload` backs up the current configuration before applying a replacement.
3. A failed start restores the previous nop configuration and restarts the Collector with that configuration.
4. `onRemoteConfigHandler` reports the configuration failure. The inspected client/configuration path has no periodic local retry after this failure.

This confirms why a running container and a working health endpoint did not imply a working ingestion pipeline. The evidence does not show a subsequent successful remote configuration application. Similar reports exist in [SigNoz issue 11620](https://github.com/SigNoz/signoz/issues/11620) and [issue 10235](https://github.com/SigNoz/signoz/issues/10235).

Evidence: `collector-effective.json`, `collector-clickhouse-ping.json`, and the original `logs-otel.json`.

### 2. The deployed ClickHouse build truncates global profiler periods

Both global profiler settings were `10000000000` ns, or 10 seconds. The deployed `Server.cpp` passes setting wrappers through a conditional expression with the integer literal `0`, without extracting `.value`.

[ClickHouse PR 96048](https://github.com/ClickHouse/ClickHouse/pull/96048) fixes this exact expression. The [25.12.6.38 stable changelog](https://github.com/ClickHouse/ClickHouse/blob/master/docs/changelogs/v25.12.6.38-stable.md) includes the fix. The deployed [source at lines 1315–1316](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/programs/server/Server.cpp#L1315) still contains the defect.

The runtime evidence independently confirms the truncation:

| Observation | Value |
|---|---:|
| Configured period | 10,000,000,000 ns |
| Value modulo 2^32 | 1,410,065,408 ns |
| Median of 34 consecutive Real trace intervals, thread 309, 11:19 | 1.410093 s |
| Increase in sampling frequency | approximately 7.09 times |

The process had 512 `BgSchPool` threads and about 768 threads in total. `/proc/1/timers` contained 1,514 POSIX timers: 757 monotonic timers and 757 thread CPU timers. The [profiler implementation](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Common/QueryProfiler.cpp) delivers these timers through SIGUSR1 and SIGUSR2.

During 11:18–11:40, 7,862 of 8,118 CPU samples had `DB::QueryProfilerReal::signalHandler` as the top symbol: 96.85%. The largest single stack group contained 6,049 samples, with `pthread_cond_wait` and `BackgroundSchedulePool::threadFunction` below that handler.

The metric history recorded 303,105 `QueryProfilerSignalOverruns` increments during 11:18–11:45. This counter is not an exact count of lost samples. At 11:28 alone, the counter increased by 46,252 while the metric log produced only four rows.

The first long executor-lock warning appeared at 11:21:30: 22,837 ms holding the lock and 22,689 ms acquiring it. Keeper connection errors appeared later, around 11:25. A later lock hold reached 92,725 ms. The load average peaked at 591.63.

This identifies the profiler signal path as the strongest lead for the stalls. User-space samples cannot identify a kernel spinlock or measure kernel execution time precisely. The profiler samples themselves may also be biased by the failure.

The defect alone does not explain the entire incident. After 11:40, CPU samples became sparse, and current samples still showed the approximately 1.410-second period. The same faulty period can coexist with low load. A trigger or amplification mechanism remains unverified.

Evidence: `trace-stacks.json`, `profiler-interval.json`, `profiler-history.json`, `correlation.json`, `timers.json`, `thread-a.json`, and `clickhouse-logs.json`.

### 3. Docker CPU inflation comes from inconsistent guest accounting

The original Docker sample covers 11:39:07.076310294–11:39:08.560355086:

| Counter | Delta |
|---|---:|
| Elapsed time | 1.484045 s |
| Container CPU | 23.111331 CPU s |
| System CPU | 0.760000 CPU s |
| Online CPUs | 18 |

Docker's calculation is `23.111331 / 0.76 * 18 * 100 = 54737.36%`. Dividing container CPU by elapsed time instead gives about 1557.32%. That second result is compatible with the guest CPU count, but it still depends on guest scheduler accounting. It is not an independent measurement of physical host CPU consumption.

ArcBox forwards the Docker stats response through the Docker API fallback and proxy. `abctl top` calculates container CPU from cgroup `usage_usec` divided by guest elapsed time. Its machine CPU calculation also uses `/proc/stat`, so its machine percentage cannot independently validate that counter.

At 12:40:36, the mismatch also existed in cumulative counters: ClickHouse cgroup CPU usage was 13,198.25 seconds, while all guest busy fields in `/proc/stat` summed to 2,967.06 seconds. This large cumulative gap cannot be explained only by a slightly misaligned one-second sample.

During the low-load interval 12:40:36–12:41:01, `/proc/stat` accumulated 445.44 CPU seconds over 25.02 seconds: 17.803 cores. ClickHouse accumulated 3.230522 CPU seconds: 0.1291 cores. The current system accounting rate was close to the expected 18 cores.

The running kernel uses tick-based accounting: `CONFIG_HZ=1000`, `CONFIG_NO_HZ_IDLE=y`, with `CONFIG_VIRT_CPU_ACCOUNTING_GEN` and `CONFIG_IRQ_TIME_ACCOUNTING` disabled. `/proc/stat` exports USER_HZ units, normally 100; dividing by 100 is correct. `CONFIG_HZ=1000` does not justify changing Docker's conversion factor.

`CONFIG_POSIX_CPU_TIMERS_TASK_WORK=y` defers CPU timer expiry work to task context. A hypothesis about CPU timer work blocking the timer interrupt must account for this setting.

The reviewed [Moby implementation](https://github.com/moby/moby/blob/master/daemon/stats_unix.go) reads `/proc/stat` for system CPU and converts cgroup microseconds for container CPU. The exact installed Moby tag was unavailable, so this source review is not a version-exact audit. The raw counters independently establish the mismatch.

The [Linux tick accounting implementation](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/cputime.c) and scheduler runtime use different accounting mechanisms. Delayed ticks and vCPU descheduling are candidate explanations. No failure-window kernel stack or host vCPU trace was captured, so a specific Linux or VZ cause is not established. The current kernel log contains no usable failure-window stall stack.

Evidence: original `docker-raw-clickhouse-stats.json`, `cpu-a.json`, `cpu-b.json`, `guest-kernel-config.json`, and `guest-dmesg.json`.

## Verification commands

Run the checked-in [read-only SQL probe](../../tests/bench/signoz/diagnose.sql) against the existing diagnostic history:

```sh
docker exec -i signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < tests/bench/signoz/diagnose.sql
```

Run `abctl debug` from a terminal because the CLI requires a TTY:

```sh
abctl debug otel /bin/busybox cat /proc/1/root/var/tmp/collector-config.yaml /proc/net/tcp /proc/net/tcp6
abctl debug signoz-telemetrystore-clickhouse-0-0 /bin/busybox cat /proc/uptime /proc/stat /proc/1/root/sys/fs/cgroup/cpu.stat /proc/interrupts
abctl debug signoz-telemetrystore-clickhouse-0-0 /bin/busybox cat /proc/1/timers
abctl debug otel /bin/busybox zcat /proc/config.gz
```

Take the CPU snapshot twice. Divide the cgroup usage delta by elapsed microseconds. Divide the sum of the first seven `/proc/stat` CPU fields by 100, then divide that delta by elapsed seconds. Preserve the raw snapshots and sampling timestamps.

## Next steps

The following changes and experiments were not performed:

1. In a disposable VZ guest, compare the affected ClickHouse build with global profilers enabled and disabled. Keep the kernel, vCPU count, data, and thread settings identical.
2. If the affected case reproduces, repeat with a compatible ClickHouse release containing PR 96048. Verify that the measured global profiler interval becomes 10 seconds.
3. Capture kernel stacks, timer interrupt deltas, cgroup runtime, `/proc/stat`, and host vCPU scheduling during a recurrence. Use those observations to distinguish tick loss from vCPU descheduling or other accounting faults.
4. After ClickHouse connectivity is stable, reapply the desired Collector configuration or restart the Collector. Verify active OTLP pipelines and an end-to-end telemetry write. A health endpoint alone is insufficient.

Do not treat a lower Docker percentage alone as proof that the stalls are fixed. Do not assign the entire incident to ArcBox or ClickHouse without the controlled comparison.

## ArcBox follow-up — 13:35 UTC

### Confirmed parser defect, separate from the incident

`parse_cpu_ticks` in `guest/arcbox-agent/src/stats.rs` sums every field on the aggregate `/proc/stat` line. Linux already includes `guest` in `user` and `guest_nice` in `nice`. Summing those fields again inflates both busy and total CPU time when the Linux guest runs nested virtual machines. The [Linux 6.18.38 implementation](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/cputime.c#L144) updates both counters for the same interval.

The [standalone reproducer](../../tests/bench/signoz/cpu_ticks_repro.rs) imports the actual ArcBox parser. At 13:35:39, the original temporary copy compiled with Rust 1.96.0 and failed its assertion:

```text
Input:    cpu  100 0 0 100 0 0 0 0 80 0
Actual:   Some((180, 280))
Expected: Some((100, 200))
```

For these sample deltas, the parser turns 50% CPU into 64.29%. The only production parser caller builds `MachineStats`; `abctl top` uses the resulting busy/total deltas. Existing parser tests cover zero guest counters and old short records, but do not cover nonzero guest counters.

Run the reproducer from the repository root:

```sh
rustc --edition=2024 --test tests/bench/signoz/cpu_ticks_repro.rs -o /tmp/arcbox-cpu-ticks-repro
/tmp/arcbox-cpu-ticks-repro nested_guest_time_is_not_double_counted --exact --nocapture
```

Before a fix, the expected diagnostic result is an assertion failure with exit code 101. The proposed fix is to sum only the first eight CPU fields. The same assertion must pass after that fix. No parser fix was applied during this investigation.

All incident snapshots had zero `guest` and `guest_nice` counters. This defect therefore does not explain the Docker percentage or the SigNoz stalls.

### Accounting mechanism and remaining uncertainty

The running kernel has `CONFIG_HIGH_RES_TIMERS=y` and `CONFIG_HZ=1000`, with virtual CPU accounting and IRQ time accounting disabled. In [account_process_tick](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/cputime.c#L475), each invocation charges one `TICK_NSEC`, or one millisecond. In [tick_nohz_handler](https://github.com/gregkh/linux/blob/v6.18.38/kernel/time/tick-sched.c#L284), the timer invokes process accounting once, then uses `hrtimer_forward` to advance past elapsed timer periods. This path does not charge every missed period to the busy task. Idle accounting has a separate catch-up path.

Cgroup CPU usage follows scheduler runtime instead of this tick count. This difference provides a mechanism for the observed divergence if tick delivery is delayed. It does not establish why tick delivery was delayed, or prove that tick delay caused this incident. A failure-window kernel stack and host vCPU scheduling trace are still missing.

The ARM64 [paravirtual stolen-time initialization](https://github.com/gregkh/linux/blob/v6.18.38/arch/arm64/kernel/paravirt.c#L156) logs `using stolen time PV` after activation. The captured guest kernel log has no such message, and observed steal counters are zero. Zero steal therefore cannot exclude host vCPU descheduling from this investigation.

The host power log has no recorded sleep/wake transition during the failure window. Entries named `Video Wake Lock` are assertions, not wake transitions. ArcBox's `sync_clock_from_host` sets `CLOCK_REALTIME`; it does not step the monotonic clock or scheduler runtime. The evidence does not support host sleep or this clock synchronization as the direct explanation.

The reviewed ArcBox daemon records contain no fault-window warning or error that identifies the stall. The deployed VM uses VZ, whose vCPU scheduling and timers belong to Apple's framework. ArcBox's HV backend timer code is outside this VM's execution path.

### Evidence and next causal check

New local evidence includes `cpu_ticks_repro.rs`, `cpu-ticks-repro.json`, `host-sleep-history.json`, `arcbox-cpu-now.json`, and `arcbox-linux-source-excerpts.json`. The current CPU snapshot was taken at 13:32:01, after the failure window; it is not a trace of the fault.

Use an isolated reproduction to capture timer interrupt deltas, both CPU accounting sources, kernel stacks, and host vCPU scheduling together. If the divergence reproduces, compare kernel accounting configurations while holding the VZ backend and workload constant. Enabling another accounting mode or reducing vCPUs without that comparison would be an unverified mitigation.

Correction to the earlier snapshot instructions: sum the first eight `/proc/stat` CPU fields for total CPU time, including `steal` and excluding `guest` and `guest_nice`. The earlier first-seven calculation produced the same result for the captured samples because `steal` was zero.
