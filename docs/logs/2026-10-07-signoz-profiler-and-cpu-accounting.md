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

## Kernel accounting A/B method

Both local kernels use Linux 6.18.38, kernel repository commit `2c041107e599b5e87bcb41f7638ac38a4fc24ac1` (`v0.0.25`), its system ARM64 configuration and patches, and Debian GCC 12.2.0. The Linux source archive matched kernel.org's SHA256 `ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045`. The comparison uses the same compiler for both kernels; it is not a bit-identical rebuild of the deployed GCC 13.3.0 kernel.

The baseline image SHA256 is `713a47dd27c7d2dda14a2f6cca1d040818b0b5ba520be6a4afcba592177d94b5`. The candidate SHA256 is `87888eaa743228ff47520832512cc49e8c173d87f4381ecc638540896906ad7a`. The complete resolved configuration difference is:

```text
TICK_CPU_ACCOUNTING y -> n
VIRT_CPU_ACCOUNTING_GEN n -> y
+CONTEXT_TRACKING_USER y
+CONTEXT_TRACKING_USER_FORCE y
+VIRT_CPU_ACCOUNTING y
```

Each kernel boots through the daemon's supported `--kernel` option with the same 0.8.8 rootfs and runtime, 18 vCPUs, and 16 GiB RAM. The live `/proc/config.gz` matched the respective build configuration exactly. The builder was stopped before measurements. The other user VMs remained running, so absolute performance measurements can include host contention.

Only the disposable profiler-disabled ClickHouse control and its Keeper run during this comparison. Each case measures five overhead repetitions, 45 seconds of idle accounting, and 115 seconds around the same bounded CPU query. After the query becomes active, the identified disposable VZ helper is suspended for ten seconds. A successful query after resumption checks liveness.

The [overhead probe](../../tests/bench/signoz/accounting_overhead.c) pins its parent and child to guest CPU 0. Each repetition measures two million direct `getpid` system calls and 100,000 pipe round trips. Compile it inside the ARM64 builder:

```sh
gcc -O2 -Wall -Wextra -Werror -static accounting_overhead.c -o accounting_overhead
```

Copy that executable into the disposable container and run it before the CPU workload. Compare medians across kernels. The counter analysis also includes a pause window from the last completed sample before SIGSTOP to the first fast sample at least three seconds after SIGCONT. This window avoids assigning a cross-file read interrupted by suspension to one short Docker interval. The original per-interval counters remain in the evidence.

### A/B results and baseline recheck — 15:26 UTC

The candidate aligns guest total accounting with cgroup runtime in this workload:

| Measurement | Tick baseline | Generic virtual accounting |
|---|---:|---:|
| Busy-window guest total rate | 11.294 cores | 17.990 cores |
| Busy-window container rate | 17.706 cores | 17.913 cores |
| Busy-window host VM rate | 12.934 cores | 12.987 cores |
| Pause-window elapsed time | 16.44 s | 16.27 s |
| Pause-window guest total | 81.89 CPU s | 292.68 CPU s |
| Pause-window container time | 275.287648 CPU s | 292.110267 CPU s |
| Pause-window host VM time | 93.90 CPU s | 93.80 CPU s |
| Pause-window Docker formula | 6051.02% | 1796.50% |
| Highest single-interval Docker formula | 12743.56% | 1797.80% |

The busy windows contain only intervals above ten container cores, totaling 80.04 seconds for the baseline and 80.71 seconds for the candidate. The table's Docker percentages are computed from the captured counters, not sampled from a Docker UI. The candidate still charges elapsed host suspension to virtual CPU accounting: its pause window contains 292.11 container CPU seconds but only 93.80 host VM CPU seconds. Matching guest counters therefore does not establish accurate physical-host usage.

The candidate also introduces a large microbenchmark regression:

| Median of five repetitions | Initial baseline | Candidate | Baseline after reverting |
|---|---:|---:|---:|
| Direct `getpid` syscall | 128.04 ns | 2224.01 ns | 125.49 ns |
| Same-CPU pipe round trip | 3625.09 ns | 15712.04 ns | 3585.43 ns |

The candidate costs 17.37 times the initial syscall latency and 4.33 times the initial pipe latency. Returning to the baseline restores both measurements, which supports a kernel-mode effect rather than a transient host-load explanation. These are microbenchmark ratios, not an estimate that every application slows by those factors.

Both kernels completed the bounded CPU workload and answered `SELECT 1` after suspension. Neither reproduced the original prolonged ClickHouse lock stall. Generic virtual accounting is not adopted as a default fix: it aligns the displayed counters while retaining host-suspension charging and imposing a large syscall/context-switch cost on this VZ guest. No production kernel configuration changed. The parser correction is committed as `a2c8edfe`; the original accounting and stall problem remains unresolved.

The final baseline daemon and the builder container are stopped. The disposable container route is absent, and the three pre-existing user VZ helpers remain running. The prepared image, build container filesystem, boot assets, kernel binaries, and raw evidence are retained for subsequent work. The evidence archive contains the build recipes, resolved configs, source checksum, and analysis scripts; the two kernel binaries are preserved separately from that archive.

## HV feasibility and Colima comparison — 2026-10-08 Singapore time

The [follow-up experiment](../experiments/2026-10-08-hv-cpu-accounting-and-colima.md) confirms that HV execution time excludes suspension, contention, and WFI idle. Suspension does not return control to the userspace VMM before the guest resumes, so updating a shared stolen-time page only at VM exits is insufficient. Elapsed time minus execution time also includes normal idle and is not a valid stolen-time counter.

Colima 0.10.3 on VZ, with 18 vCPUs and Ubuntu 6.8.0-117, reproduces counter divergence using the same ClickHouse image and SQL. Five Docker API counter pairs produce 1979.61%–2070.98%, exceeding 1800%. Colima therefore does not provide a negative control for this accounting defect. OrbStack is not installed on the host, so its runtime comparison remains pending. No production accounting change was made, and the original prolonged stall remains unresolved.

## Thread-clock isolation and stall-evidence correction — 2026-10-08 Singapore time

The [single-thread experiment](../experiments/2026-10-08-thread-cpu-accounting.md) directly separates raw task tick accounting from scheduler runtime. During a 20.046 s busy probe containing a ten-second VM pause, raw PROF advanced 10.025 s while scheduler runtime advanced 20.046 s. A repeat on another CPU agrees. A sleeping thread does not acquire the ten-second charge. This establishes the mechanism independently of ClickHouse, its profiler, and Docker's percentage calculation.

The original `clickhouse-logs.json` contains a larger warning than the previously cited 92.725 s: at 11:33:23.816518 UTC, thread 496 reported `Lock was held for 155326 ms` from `MergeTreeBackgroundExecutor<RoundRobinRuntimeQueue>::trySchedule`. The recorded stack follows `BackgroundJobsAssignee::scheduleCommonTask`, `StorageMergeTree::scheduleDataProcessingJob`, and `BackgroundSchedulePool::threadFunction`.

This is a warning stack recorded after the timed region, not a stack captured during the stall. The exact deployed `LockGuardWithStopWatch.h` uses `CLOCK_MONOTONIC`, calls `lock.reset()` before stopping the watch, and formats the warning later. The duration can include unlock and scheduling delay; it does not prove continuous mutex ownership for 155.326 s. Realtime clock correction cannot explain this monotonic duration.

Within the approximate interval obtained by subtracting that duration from the warning timestamp, 63 thread identities generated 145 log entries. This weighs against one uninterrupted whole-VM freeze, but does not rule out intermittent or partial-vCPU starvation. Application-to-Docker timestamp delay reaches 29.038 s elsewhere in the incident. Logging delays prevent treating the inferred interval as an exact lock timeline.

Version-exact source review confirms that the CPU profiler uses `CLOCK_THREAD_CPUTIME_ID`. Its samples therefore depend on the scheduler accounting under investigation and cannot independently prove physical CPU consumption by the Real profiler handler. Linux folds missed CPU timer periods into overrun counts, and ClickHouse discards some overrun samples. No evidence yet establishes an unbounded signal storm or proves that profiler behavior caused the original prolonged stall.

The subsequent [timer-attribution experiment](../experiments/2026-10-08-thread-cpu-accounting.md#follow-up-cpu-timer-attribution-inside-a-signal-handler) validates this evidence limitation. With a ten-second VM pause inside a synthetic Real handler, scheduler runtime advanced 11.015 s while raw PROF advanced 1.000 s. A four-second SCHED timer delivered a signal inside that handler; the matching PROF timer did not fire. Neither timer fired in the unpaused control. This demonstrates that the CPU profiler can sample a handler because of time charged during host suspension, without the corresponding physical execution. It does not locate the original stall trigger.

## Trace collection times and runnable pressure — 2026-10-08

The retained Real traces for thread 496 contain 71 samples between 11:29 and 11:35 UTC: 66 in `pthread_cond_wait` under `BackgroundSchedulePool::threadFunction`, and five in `pthread_cond_signal` under `MergeTreeBackgroundExecutor::trySchedule`. The signal samples were logged twice at 11:31:33 and three times at 11:33:23. These are stacks captured by the Real profiler, but their database timestamps cannot place the capture precisely within the long lock interval.

Version-exact [TraceCollector.cpp](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Interpreters/TraceCollector.cpp#L181) reads the trace from its pipe before calling `clock_gettime(CLOCK_REALTIME)` and assigning `event_time_microseconds`. The sender does not include a capture timestamp. The 109 whole seconds without Real records, 11:31:34–11:33:22, therefore establish a gap in collector output. Buffered samples can appear after the lock warning without having been captured after it.

MetricLog recorded rows at 11:32:16, 11:33:04, and twice at 11:33:06 inside that gap. [PeriodicLog.cpp](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Interpreters/PeriodicLog.cpp#L49) passes the current clock reading to the metric record, not its scheduled wakeup time. Assuming no realtime rollback, these observations contradict one uninterrupted whole-VM suspension covering the entire trace gap. Shorter pauses, partial-vCPU starvation, and long per-thread scheduling delays remain possible.

The retained 11:29–11:35 metric window contains 107 MetricLog rows and 96 asynchronous snapshots. Guest-wide runnable count reaches 621 in the snapshot logged at 11:30:22, then falls to at most six after 11:33:27. Reported OS CPU wait totals 1817.269701 s; 1815.566352 s appears in rows through 11:33:24. Reported user and system time totals 77.273680 s. These counters are delayed aggregate reports from guest task accounting, not physical-host execution measurements. Zero tracked IO wait and hard faults do not independently exclude host paging or an IO issue.

AsynchronousMetrics reads runnable count from `/proc/loadavg` before calculating other metrics; AsynchronousMetricLog timestamps the result afterward. A delayed calculation can separate the runnable reading from its logged timestamp. Its longest timestamp gap is 172 s, while MetricLog's longest gap is 88 s. The streams did not stop together.

The [history queries](../../tests/bench/signoz/stall_history.sql), original successful outputs, exact source files, and numeric analysis are preserved under the `stall-history-2026-10-08` task artifact directory. A later query returned no rows after the user VM's helper identity changed; that result is missing historical evidence, not zero activity. The archived results remain the evidence for the original incident.

## Scheduling consequence reproduced — 2026-10-08

The [eight-worker experiment](../experiments/2026-10-08-thread-cpu-accounting.md#follow-up-incorrect-runtime-causes-prolonged-scheduling-exclusion) now links the accounting error to real scheduling delay. After a ten-second VM suspension, one runnable task received no execution across snapshots spanning about 69.8 seconds while seven peers on the same guest CPU kept advancing. Its complete sample gap was 80.338 s; the unpaused control's maximum was 1.133 s. The affected task's runtime and dispatch count stayed unchanged across 24 post-resume `R` snapshots, with no CPU quota or throttling.

The measured delay agrees with Linux EEVDF excluding a task whose `vruntime` was advanced by the unexecuted host interval. Missing stolen-time subtraction can therefore affect application progress, not just Docker percentages and profiler attribution. The original ClickHouse stall still lacks the fault-time host/vCPU trace needed to establish that this controlled sequence occurred there. No production accounting or scheduler workaround is deployed.

## Condition-variable delay isolated and removed in a runtime comparison — 2026-10-08

The [libc experiment](../experiments/2026-10-08-condvar-notification-latency.md) confirms a second mechanism relevant to the retained `pthread_cond_signal` stacks. Delaying a waiter in a signal handler for two seconds makes the old glibc 2.35 notifier wait 2.002205 s while holding an application mutex. With the same VM, kernel, and probe binary, the tested glibc 2.41 notifier returns in 6.083 µs while that waiter remains blocked. Both un-delayed controls return within five microseconds. The deployed ClickHouse `trySchedule` calls `notify_one()` inside its executor lock, so this dependency can extend that lock's hold time.

This is a verified removal of one amplification mechanism, not a reproduction of the full ClickHouse incident. The original trace does not identify the precise futex or delayed waiter. The two-second gate is artificial; the experiment does not establish whether a profiler, scheduler exclusion, or another event delayed a real waiter. Updating the runtime cannot itself repair the separate task-accounting defect.

## Matching upstream reports — 2026-10-08

The [2016 Xen report](https://lists.xenproject.org/archives/html/xen-devel/2016-05/msg01361.html) describes the same scheduler mechanism: a VM pause enters the current task's `delta_exec` and `vruntime`, then that task waits while its peers catch up. Its [follow-up and attached patches](https://lists.xenproject.org/archives/html/xen-devel/2016-05/msg01976.html) add hypervisor runstate support and enable Linux runqueue stolen-time subtraction. This is a precedent on Xen/CFS, not proof of the current ArcBox/VZ trigger or a patch that can be transplanted unchanged.

ClickHouse maintainers explicitly cite glibc BZ 25847 in [issue 75847, “Migrate to Musl”](https://github.com/ClickHouse/ClickHouse/issues/75847). The relevant [glibc rewrite](https://github.com/bminor/glibc/commit/1db84775f831a1494993ce9c118deaf9537cc50a) both fixes the lost-wakeup race and removes the wait-for-waiters path reproduced here. The latter effect does not require the race. Existing public reports therefore cover both mechanism classes, but do not establish the complete causal chain of this incident. The source pages and Xen patch attachments are preserved with hashes under `stall-public-reports-2026-10-08`.

## Scheduler and libc mechanisms connected experimentally — 2026-10-08

The [combined experiment](../experiments/2026-10-08-condvar-notification-latency.md#follow-up-scheduler-exclusion-propagates-into-notification-latency) now reproduces both mechanisms in one process. A ten-second suspension overcharges one of eight continuously runnable waiters. After resume, the selected waiter remains runnable without another dispatch for about seventy seconds while a peer on the same CPU continues executing. The old glibc notifier blocks for 70.029045 seconds and returns 33.084 microseconds after the waiter's handler exit marker. The tested modern runtime returns from notification in 2.000 microseconds even though its selected waiter still takes 70.158072 seconds to reach that marker after gate release.

All four old/modern, control/pause cases pass both runtime assertions and the separate evidence checker. The result establishes a complete synthetic accounting-to-notification chain and verifies removal of the notifier dependency. It does not identify the original ClickHouse waiter's state or the event that delayed that waiter. The original incident therefore remains open, and neither production guest accounting nor the production container runtime has changed.

## Reproducible application restart probes — 2026-10-08

The [restart driver](../../tests/bench/signoz/run_clickhouse_restart.py) clones verified, stopped ClickHouse and Keeper seed volumes and freezes configuration files by hash. An explicit fixture supplies Docker and seed identities. Control, delayed contention, and startup contention cases observe each clone for 300 seconds, with owned host load bounded to 60 seconds. The [thread observer](../../tests/bench/signoz/observe-clickhouse.sh) records Common and BgSchPool status, scheduler counters, and syscall state. Cleanup stops only the clones and retains their data and evidence. The observer is intrusive and its sequential reads are not atomic; subsequent results must quantify its cost and observation gaps.

## Application stalls with a validated observer — 2026-10-08

The [application experiment](../experiments/2026-10-08-clickhouse-restart-contention.md) first exposes a procfs reader defect: direct Bash line reads can seek into regenerated status text. A 10,000-read comparison produces 321 damaged direct-read records and no damaged buffered records. The observer now buffers status before parsing. The original application captures remain invalid, and the strict analyzer continues to reject them.

Corrected control-03 and contention-03 pass complete-record validation and produce 15 and 17 host exec timeouts respectively. The control has unrelated host compilation and is not an idle baseline. Its ClickHouse logs include a 47.690-second executor lock acquisition delay and Keeper session expiry. Controlled contention produces a 66.397-second sweep with Common scheduling delays of 58.082–62.307 seconds, followed by a second 32.281-second slow sweep after the owned load ends. No specific old-glibc internal wait is captured; observation gaps and intrusive sampling still limit attribution. No production kernel or container runtime change is made.

## Real profiler isolated as an application trigger candidate — 2026-10-08

The [same contention experiment](../experiments/2026-10-08-clickhouse-restart-contention.md#profiler-isolation-with-the-same-contention) produces 17 host probe timeouts with both global profilers, zero with both disabled, and eleven after restoring only the Real profiler. Runtime settings confirm each case. The Real-only case again expires Keeper sessions and accumulates Common scheduling delays of 50.983–57.083 seconds. CPU profiling is therefore not necessary for this reproduction. No specific old-glibc notifier wait is captured, and the retained top 150 Real-trace groups do not match the original `pthread_cond_signal` stack. The original incident's complete causal chain remains open.

An accompanying metrology review retracts the claim that macOS `ps` process CPU values plus a centisecond allowance establish a physical CPU upper bound. `ps` separately reads terminated-thread and live-thread counters; a concurrent thread exit can cause omission. The recorded wider-window counter discrepancy is retained as an observation, with an explicit correction. A replacement measurement needs a new before-start baseline. Disabling a profiler is not a guest-accounting repair, and no production profiler, libc, or kernel change is deployed.

## Low-overhead sampling still reproduces scheduler delay — 2026-10-08

The [native observer experiment](../experiments/2026-10-08-clickhouse-native-observer.md) reproduces four host probe timeouts with only the Real profiler enabled and a SCHED_OTHER observer. The observer consumes 2.215 guest CPU seconds over the complete capture, yet loses 22.838 seconds between sweeps and accumulates 20.652 seconds of run delay across that gap. Common threads also accumulate 17.477–21.925 seconds of delay. The earlier Bash observer's large runtime cost is therefore not necessary for this reproduction. A separate FIFO observer case completes all 60 probes, but sequential cases do not establish FIFO as a repair.

The lower-overhead capture still misses the fault's execution path. It does not identify a specific glibc notifier wait, unwind lock, or host scheduling event. The original incident remains unresolved; no production scheduling or accounting change is made.

A repetition with the final FIFO probe produces one timeout and an 8.527-second sweep. The observer gains 7.988 guest runtime seconds and 0.672 run-delay seconds across that sweep, despite its usual seven-millisecond collection duration. FIFO sampling is therefore insufficient to remove the fault or preserve the complete execution path. The next useful boundary is kernel scheduler/signal/futex events retained independently of user-space collection.

## Procfs observation adds another possible wait path — 2026-10-08

Exact Linux 6.18.38 source shows that reading a task's syscall snapshot can wait for that task to leave its CPU and acquire task/runqueue locks. Reading wchan holds the target's pi_lock during stack unwinding. Host vCPU descheduling can extend either dependency. These paths make procfs observation intrusive; source review alone does not prove that either caused a measured sweep or the original incident.

A field-timed FIFO repetition completes all 60 probes without a long access. Review also finds that the added SCHED clock reads can take scheduler locks. The next probe retains only MONOTONIC timing around field access and preserves the previous version and its negative result. The [experiment](../experiments/2026-10-08-clickhouse-native-observer.md#procfs-access-can-perturb-the-target) documents these attribution limits. Production code remains unchanged.

The MONOTONIC-only repetition also completes all 60 queries, with a maximum sweep of 14.401 milliseconds. Neither timing case identifies the earlier slow field. Both pass strict analysis and stop their clones. The owned daemon and helper are stopped, seed identities and retained binary hashes are verified, and the native evidence is retained in `clickhouse-native-2026-10-08`. The original incident remains unattributed.

## Kernel event capture replaces periodic proc reads — 2026-10-08

The [kernel-event experiment](../experiments/2026-10-08-clickhouse-kernel-events.md) boots a diagnostic kernel with the baseline accounting configuration and static scheduler, signal, and syscall events. A separate privileged sidecar records an isolated tracefs instance, freezes it on the first failed query, and exports evidence before stopping the application. The application retains its original capabilities. Two preparation failures expose and correct buffer-rounding and inherited identity-read assumptions; neither counts as an application result.

The first valid marker-only control completes 60 queries without failure. Independent validation confirms runtime settings, contention, identities, trace completeness, and cleanup. Event-enabled capture remains to be validated; the original stall and its complete causal chain remain unresolved.

The first events-on capture also completes 60 queries and passes independent validation. Its 18 buffers retain 6,732,869 events without overrun, dropped events, commit-overrun, or consumption. The strict checker normalizes the kernel's syscall display format; the first parser rejection is preserved separately. Both negative cases retain their original runtime settings. Neither reproduces the earlier long lock or Keeper-expiry warning, so this pair does not identify the original stall's cause.

## Startup contention exposes runnable delays without proc polling — 2026-10-08

The [startup kernel capture](../experiments/2026-10-08-clickhouse-kernel-events.md#startup-contention-retains-a-partial-stall) fails its host Docker logs readiness command despite ClickHouse having logged readiness. It retains 130,997 kernel events without loss, then logs Keeper timeout and session expiry before cleanup stops the application. This is a valid partial trace, not a completed application observation; runtime settings, identity snapshots, and SQL results are absent. The full validator continues to reject the case.

Trace-derived ClickHouse descendants include a 31.079283-second wakeup-to-dispatch delay and a 29.084645-second runnable interval inside a candidate signal handler. Other CPUs continue producing events, so these intervals are not a continuous whole-VM pause. The evidence establishes severe guest scheduling delay but does not identify wrong runtime charging, EEVDF exclusion, or an old-glibc notifier dependency in this capture. The driver now obtains the container PID before readiness to retain a frozen identity snapshot when readiness fails.

Production Activity and Docker statistics do not perform the earlier observer's per-thread syscall/wchan reads. Their cgroup accounting flushes have separate locking paths. These source findings do not attribute the measured stall to either collector, and no production behavior is changed.

The next startup repetition validates early PID capture against both frozen identity snapshots and passes the full capture checker. All 60 queries succeed, with 6,641,887 retained events and no loss or application warning. This is a completed negative application result, not proof that all scheduling waits were short. The new failure-path identity collection still needs a live failure case.

## Kernel spin contention precedes long runnable waits — 2026-10-08

The [lock/runtime capture](../experiments/2026-10-08-clickhouse-kernel-events.md#lock-contention-and-scheduler-runtime-in-a-completed-failure) completes an application observation with one host query timeout and Keeper session expiry. Complete identities and zero-loss kernel events connect Common 3195's 349.098 ms SPIN wait to a 349.125082 ms scheduler-runtime charge, followed by a 15.412658-second runnable wait. After dispatch, the task reaches signal return in 12 µs. BgSchPool 2641 has a corresponding 490 ms SPIN/runtime sample and a 19.683360-second runnable wait. Both use the same unresolved lock hash.

This places an observed amplification path inside the guest kernel's spin contention and scheduling. The runtime charge is not physical CPU evidence, the lock hash does not identify `sighand->siglock`, and the capture does not prove EEVDF debt or the original glibc notifier dependency. The exact-SPIN parser corrects exploratory idle-task and mutex-pairing errors. The fixture still has the already-known profiler-period truncation. No production accounting, scheduler, profiler, or libc change is deployed.

Independent analysis also confirms 644 simultaneously queued descendants in the earlier partial failure and four-second background waits in the successful SQL repetition. Query success alone is therefore insufficient to exclude scheduling delay. A host System Trace preflight identifies all 18 VZ vCPU threads, but its short retained tail cannot attribute the application failure; the next host capture must verify coverage and clock alignment.

## A contender stack identifies the guest signal lock — 2026-10-08

The [bounded stack experiment](../experiments/2026-10-08-clickhouse-kernel-events.md#bounded-stacks-identify-the-process-signal-lock) reproduces a ClickHouse readiness failure and retains both trace instances without loss. One ordinary-task stack resolves to `do_sigaction`, whose exact kernel source acquires `current->sighand->siglock`. The same thread's primary trace shows a 45.918 ms wait, a 45.996750 ms scheduler-runtime charge, and then 19.985895 seconds runnable without dispatch. This identifies a signal-lock/scheduling path in the guest Linux kernel; it is not only a Docker CPU-display defect.

The stack budget is nominally 128; the kernel's non-atomic count produces 129 actual stacks. A preparation-only readback-format failure is retained separately. The application capture remains partial, with after-only identities and no SQL observation. IRQ common_pid values are not treated as the timer's user-space caller, and early runc hashes are not treated as proof of the later ClickHouse lock object's identity. Host vCPU preemption, physical execution, EEVDF debt, and the original libc notifier dependency still require separate evidence.

An explicit 30-second host System Trace window retains only 2.13 seconds of scheduler events. No host recording is added to the application case. The two derived host-state tables share a modeler; their agreement cannot independently validate physical execution. No production repair is deployed.

## Assigned CPUs are occupied by IRQ-disabled contention — 2026-10-08

Further analysis of the same capture follows the delayed task through ten migrations. Along its assigned CPU path, captured hot-hash SPIN intervals cover 19.527241 of 19.985895 guest seconds, or 97.7051%; no idle-current interval overlaps that path. Across all 18 CPUs, interval unions cover 96.9034% of the window's guest-CPU time. The queue reconstructed from events reaches at least 662 ClickHouse after-snapshot identities. This identifies widespread guest lock contention during the observed delay, without assuming a single-runqueue fairness calculation.

The signal-lock acquisition path disables IRQs before spinning. Actual spinning can therefore suppress tick accounting and delay scheduling without host preemption. The guest runtime/PROF discrepancy is insufficient to distinguish that mechanism from host descheduling in this capture. The separate controlled suspension experiment still establishes missing-steal consequences. Physical execution, the lock holder, EEVDF eligibility, and the original libc notifier chain remain unmeasured here.

A minimal Thread State Trace attempt reports reaching its 30-second limit, but the driver's 300-second total process wait expires without a recorder exit status. Postprocessing is not timed separately. The retained package cannot be exported and is not used as evidence. Read-only investigation resolves the misleading `ktrace info` error to EPERM plus an error-reporting defect in the installed tool. Administrator execution in the user's terminal succeeds; the tool session still requires separate authorization. A bounded raw scheduler-capture script is prepared. No production configuration or implementation changes are made.

The frozen kernel evidence is archived as `clickhouse-kernel-2026-10-08`: 42,727 files pass source/copy SHA256 comparison and nine symlink targets match. All isolated containers are stopped and seed/configuration/binary checks pass. The owned daemon/helper remain running only to support the pending user-run root preflight; the archive contains a verified daemon-log snapshot and excludes that future capture's output files.

## Raw host scheduler capture passes preflight — 2026-10-08

The user-run administrator capture succeeds. Its complete 31.568085750-second stream contains 50,956,610 events, with no loss/order markers or per-CPU scheduling-chain mismatch. All 18 host CPUs have records. The checked-in [analyzer](../../tests/bench/signoz/analyze_host_ktrace.py) and an independent target-CSV reconstruction agree on 4,545 closed vCPU scheduled-current spans. Seventeen prior-mapped vCPU TIDs appear under helper 91560; the absent eighteenth TID has no established state. Source verification prevents counting post-switch dispatches twice or equating residency with exact guest execution.

The separate `host-scheduler-preflight-2026-10-08` archive verifies 31 files against source SHA256 values; the earlier frozen archive remains unchanged. This is an idle capture without a ClickHouse failure. It establishes a usable host recording path, not host causation for the earlier guest stall. A simultaneous application capture and bounded host/guest clock alignment remain necessary. Production behavior is unchanged.

## Configuration-copy failure isolated to Docker mount lifetime — 2026-10-08

The simultaneous application capture fails before application startup because `docker cp` cannot remount the stopped container's overlay. An authorized diagnostic kernel enables function probes without changing CPU accounting. The [independent minimal reproduction](../experiments/2026-10-08-overlay-copy-lifetime.md) fails on the third copy into a new, never-started container. A complete target-object chain shows a new upper trylock failing 1.886 ms before the old Docker thread begins unlocking that same upper dentry in exit task work.

The preparation blocker is a Docker archive-view teardown race. A goroutine completion signal precedes the OS thread's final mount cleanup, while the snapshotter's lazy unmount permits the next mount to race. All probes report zero missed hits and all 18 buffers report zero loss counters. The original ClickHouse signal-lock stall remains unresolved. Production behavior is unchanged; the diagnostic VM and stopped reproduction container are retained.
