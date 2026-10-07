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

## Repair and controlled reproduction

### Parser correction

The parser now sums the first eight fields. A regression covers nonzero `guest`, `guest_nice`, and `steal`, so excluding nested guest time cannot accidentally exclude steal time. The original failing reproducer and the imported parser tests pass: 16 tests on macOS and 16 tests in the isolated Linux guest. The Linux run used a musl executable that imports the actual parser; that run alone does not validate a rebuilt agent or its RPC path.

`cargo fmt --check` and `cargo clippy -p arcbox-agent --target aarch64-unknown-linux-musl --all-targets` passed. Clippy reported only an existing future-incompatibility notice for `proc-macro-error2 v2.0.1`. The parser correction changes ArcBox machine statistics when nested guest time is nonzero. It does not change Docker's forwarded statistics or resolve the original incident.

### Isolated baseline and profiler comparison

The reproduction used a separate data directory and Docker socket, an 18-vCPU VZ VM, the deployed daemon, boot bundle 0.8.7, and kernel `6.18.38-arcbox`. The existing user VMs and original SigNoz deployment were not restarted or reconfigured. The disposable data directory is `/private/tmp/arcbox-cpu-repro-w0v7qz49`.

Both ClickHouse cases used `25.12.5.44` and a background pool of 512 threads. Global CPU and real profiler settings were `10000000000` for the enabled case and zero for the disabled case. Runtime queries verified the settings. Both ran the same [bounded workload](../../tests/bench/signoz/cpu_workload.sql).

The table aggregates sample intervals where ClickHouse cgroup usage exceeded ten CPU seconds per elapsed second:

| Profiler | Elapsed | Guest total CPU | Container CPU | Host VM CPU |
|---|---:|---:|---:|---:|
| Enabled | 89.19 s | 1212.56 CPU s | 1578.094927 CPU s | not captured |
| Disabled | 89.88 s | 1251.28 CPU s | 1598.671693 CPU s | 1015.29 CPU s |

With profilers disabled, those counters represent 13.922 guest total cores, 17.787 container cores, and 11.296 host VM cores. The profiler defect is therefore not necessary for the accounting divergence. Host process CPU time and guest counters are sampled sequentially, so short-interval comparisons have read skew; the longer aggregate also shows the discrepancy.

Idle guest total accounting remained close to 18 cores: 17.715 with profilers enabled and 17.761 with profilers disabled. These runs reproduced counter divergence, but did not reproduce the original prolonged ClickHouse lock stalls.

### Controlled host suspension

While the same workload was active, a probe verified the disposable VZ helper's PID and start identity, sent SIGSTOP, and resumed the helper after ten seconds. A detached watchdog and a `finally` block both provided SIGCONT. The original run used PID 76989, started at `Wed Oct 7 21:51:29 2026` local time. The helper stopped at 14:23:37.596848 UTC and resumed at 14:23:47.654469 UTC. Other VZ helpers were not signaled.

The interval spanning the suspension, 14:23:35.506177–14:23:47.671382 UTC, recorded:

| Counter | Delta |
|---|---:|
| Guest elapsed | 12.15 s |
| Guest total CPU | 29.71 CPU s |
| Container CPU | 218.456426 CPU s |
| Host VM CPU | 29.14 CPU s |
| Blocking sample read | 10.157694 s |

Applying Docker's formula to those captured counters gives `218.456426 / 29.71 * 18 * 100 = 13235.33%`. This is a derived percentage, not a raw Docker API or UI percentage sample. The guest remained responsive after resumption and `SELECT 1` succeeded.

Host descheduling is sufficient to produce this class of amplification. The experiment does not establish that the original incident had the same host trigger. It also corrects the earlier emphasis on guest accounting loss: cgroup task runtime can include elapsed host descheduling, so the entire difference cannot be assigned to `/proc/stat` undercounting.

In Linux v6.18.38, [update_rq_clock_task](https://github.com/gregkh/linux/blob/v6.18.38/kernel/sched/core.c#L776) subtracts stolen time only when `paravirt_steal_rq_enabled` is active. Otherwise, elapsed scheduler-clock time advances task runtime. The deployed guest has no observed stolen-time activation message and reports zero steal. Tick accounting and scheduler runtime can therefore diverge during host suspension without either counter providing an independent measurement of physical host CPU use.

### Fresh SigNoz initialization and remaining work

A fresh isolated SigNoz stack used the original image versions and read-only configuration sources. Migrations and ClickHouse initialization completed without the original long stalls. During 179.4 seconds, guest total CPU advanced 3170.25 seconds and ClickHouse cgroup CPU advanced 40.16827 seconds. The Collector reported OpAMP server errors against the fresh metastore, so this run does not validate the original ingestion scenario or an end-to-end SigNoz recovery.

No kernel configuration change has been applied. `CONFIG_VIRT_CPU_ACCOUNTING_GEN` is an untested candidate for aligning accounting sources. Generic virtual accounting can still charge elapsed virtual time and has context-tracking overhead; a lower Docker percentage would not prove correct physical-host accounting or fixed ClickHouse stalls. An A/B comparison and overhead measurement remain necessary.

The available `rust:1-bookworm` image lacks `bc`, `bison`, `flex`, `cpio`, and `libelf-dev`. Kernel build work is paused pending a user-provided ARM64 Linux builder image, as required by the workspace instructions. The original prolonged stall remains unresolved.

### Reproduction probes and evidence

Run the [CPU sampler](../../tests/bench/signoz/sample_cpu.py) against an explicit disposable Docker socket. The container must provide `cat`, `/proc`, and cgroup v2. On the observed ARM64 Linux guest, `/proc/stat` uses 100 USER_HZ ticks per second.

```sh
python3 tests/bench/signoz/sample_cpu.py --socket "$LAB/run/docker.sock" --container "$CONTAINER" --host-vm-pid "$VM_PID" --seconds 120 --output "$CAPTURE"
docker --host "unix://$LAB/run/docker.sock" exec -i "$CONTAINER" clickhouse-client < tests/bench/signoz/cpu_workload.sql
```

Start the sampler before the workload in separate terminals. For the suspension experiment, first identify the helper belonging to the disposable VM and record its start time with `ps -p "$VM_PID" -o lstart= -o comm=`. After the workload becomes active, run the [bounded suspension probe](../../tests/bench/signoz/pause_vm.py) in another terminal:

```sh
python3 tests/bench/signoz/pause_vm.py --pid "$VM_PID" --expected-start "$VM_START" --output "$PAUSE_EVENTS"
```

Never select a helper from an existing user VM. The start-time check detects a stale PID; the operator must establish which daemon owns that helper. Verify a query succeeds after resumption. Docker masks `/proc/interrupts` inside these containers, so the sampler omits that file; use `abctl debug` for an unmasked IRQ snapshot.

Raw evidence includes `profiler-on-load.jsonl`, `profiler-off-load.jsonl`, `host-pause.jsonl`, `pause-events.jsonl`, their summaries and analyses, `signoz-init.jsonl`, and `cpu-ticks-fixed-linux.json`. The local evidence archive preserves the analysis scripts and configuration records. Image archives, VM disks, and executable artifacts are excluded.

### Final validation and asset blocker

The complete agent built for `aarch64-unknown-linux-musl` in release mode, and the matching host daemon also built successfully. A daemon-level stats check was attempted in the same disposable data directory with the fixed agent. Startup failed before the VM started: the new daemon embeds the boot bundle 0.8.8 manifest hash, while the retained baseline assets are 0.8.7. The expected hash was `e40c7c6a09ddbaedd9862b114db0a44efcdea40889eccc08aeef2cf156cd4e74`; the baseline manifest hash was `a50d07e225ea82b8d63b6bde7825247729e80133a2c3781e2ccd010bb2433f76`. `boot-assets/dev` contains 0.8.6 and cannot supply the required assets.

The manifest check was not bypassed. The workspace requires user setup when prerequisites are missing, so daemon-level validation is paused pending official 0.8.8 assets. The parser change remains uncommitted until that required check passes. Neither a daemon-level pass nor deployment of the parser fix is claimed.

The checked-in suspension probe was separately run against the disposable helper at 14:38:50 UTC. The ten-second watchdog completed, and a subsequent ClickHouse `SELECT 1` succeeded. Python syntax checks, workspace formatting, and `git diff --check` passed. The baseline daemon then stopped cleanly; its helper PID 76989 exited. The new daemon's failed startup also exited, leaving no running VM from this task. The data directory and logs remain available for the next comparison.

## Approved environment preparation and parser validation — 15:11 UTC

The user approved preparing the missing assets and build dependencies. The daemon downloaded boot bundle 0.8.8 into the disposable data directory. Its manifest matched the pinned SHA256 `e40c7c6a09ddbaedd9862b114db0a44efcdea40889eccc08aeef2cf156cd4e74`; the guest Docker version is now 29.8.2.

The first boot reached `VM_READY` but failed while installing its isolated container route. The local daemon signature used identifier `arcbox-daemon`, which the helper does not accept. Re-signing with the existing Makefile's identifier, `com.arcboxlabs.desktop.daemon`, and the ArcBox Developer ID resolved the connection failure. No helper implementation or authentication rule changed.

The matching built daemon and fixed agent then passed a manual daemon-level stats check: `WatchSetupStatus` reported `READY`; `StatsService.Watch` returned six samples with advancing clocks and CPU counters, sane memory counters, and the ClickHouse container present. ClickHouse `SELECT 1` succeeded. The guest agent file SHA256 matched the built binary. The stream probes use a bounded grpcurl deadline because both RPCs stay open after delivering samples; the earlier generic command recorder's 45-second timeout was a probe limitation.

This completes the previously blocked parser validation. The nested-guest regression, Linux Clippy, formatting, full builds, and real daemon stats path have passed. The parser fix is ready to commit; the original CPU accounting amplification and long stall remain separate work.

The dedicated ARM64 image `arcbox-kernel-builder:cpu-accounting-20261007` now contains `bc`, `bison`, `flex`, `cpio`, and `libelf-dev`. It derives from the existing pinned Rust Bookworm image. A container limited to six CPUs and 8 GiB is building Linux 6.18.38 with the deployed kernel repository tag `v0.0.25`. The existing `kernel` checkout remains on its unrelated debug branch.
