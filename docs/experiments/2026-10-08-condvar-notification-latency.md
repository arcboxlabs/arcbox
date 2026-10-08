# 2026-10-08 — Can a delayed waiter block a condition-variable notifier?

- Status: complete for the libc mechanism and runtime comparison; the original ClickHouse stall remains unresolved.
- Environment: isolated VZ guest, Linux 6.18.38-arcbox, ARM64, guest CPU 2.
- Outcome: glibc 2.35 transfers the waiter's delay to the notifier; the tested glibc 2.41 build removes that dependency.
- Probe: [cond_signal_latency.c](../../tests/bench/signoz/cond_signal_latency.c).

## Question and method

The original incident has five Real samples in `pthread_cond_signal` beneath `MergeTreeBackgroundExecutor::trySchedule`. That [ClickHouse method](https://github.com/ClickHouse/ClickHouse/blob/6b09ccf262fd16345434572fb4d45dfd608f86c2/src/Storages/MergeTree/MergeTreeBackgroundExecutor.cpp#L151) holds its executor mutex across `has_tasks.notify_one()`. Could delay in a different waiter extend the notifier's lock hold?

The probe creates waiter A, waiter B, a notifier, and a controller. The controller uses `/proc/self/task/TID/syscall` to confirm each waiter is blocked in a futex belonging to the condition variable. It then sends SIGUSR1 to A; A's handler waits on a separate pipe. The first notification makes A's group G1. B joins the next group. A second notifier calls `pthread_cond_signal` while holding the application mutex. In the delayed case, the controller keeps A's handler blocked for two seconds before releasing its pipe. The control releases A before creating the second notifier.

The same compiled binary runs in the same container and VM with two runtime libraries. The default is Ubuntu `GLIBC 2.35-0ubuntu3.11` from `clickhouse/clickhouse-server:25.12.5`. The comparison uses Debian `GLIBC 2.41-12+deb13u3`, copied with its loader from the already cached ARM64 `rust:1.96-slim-trixie` image. An explicit loader and `--library-path` select the copied files under `/tmp`; no system library is replaced. Library resolution and hashes are recorded. No VM suspension occurs during these runs.

The final probe uses public pthread operations and Linux syscall observation, without reading or changing private libc fields. A fifteen-second alarm bounds failures. The signal handler uses lock-free atomics and async-signal-safe `read`. Each case asserts the expected completion order, joins every thread, and verifies the notifier's latency.

## Results

| Runtime | Case | Second notification elapsed | Notifier thread CPU | Returned while A was still held? |
|---|---|---:|---:|---|
| glibc 2.35 | Control | 4.250 µs | 3.624 µs | A already released |
| glibc 2.35 | Delayed A | 2.002205 s | 7.042 µs | No |
| glibc 2.41 | Control | 1.750 µs | 3.250 µs | A already released |
| glibc 2.41 | Delayed A | 6.083 µs | 5.292 µs | Yes |

In the old-runtime delayed case, A's gate stays closed for 2.001981 s. The notifier returns 34.000 µs after release. In the new-runtime delayed case, the gate stays closed for 2.000702 s, and the controller observes `signaler_returned=1` with `handler_exited=0` before release. This establishes the removal of the dependency on A's progress; the small microsecond differences are not a general performance benchmark.

An earlier exploratory probe read the old runtime's internal state and observed the G1 reference-wait flag while the notifier was blocked. The shipped libc disassembly also retains that futex wait. Those records are preserved for attribution; the final reproducible A/B probe does not depend on that private layout.

## Explanation and scope

The old glibc algorithm must wait for old-group references to disappear before reusing the group. A waiter delayed in its signal handler retains such a reference, so the notifier blocks too. Upstream [commit 1db84775](https://github.com/bminor/glibc/commit/1db84775f831a1494993ce9c118deaf9537cc50a) removes this wait while rewriting signal counting. Its message explicitly describes reduced average and maximum `pthread_cond_signal` latency. The change first shipped in glibc 2.41 and was also backported to older branches; the exact package and implementation matter, not only the major/minor version.

The same upstream change fixes BZ 25847's lost-wakeup race. This experiment demonstrates delay propagation without requiring that race. It does not demonstrate permanent deadlock, nor that glibc 2.41 removes every possible mutex or condition-variable delay.

The separate [scheduler experiment](2026-10-08-thread-cpu-accounting.md#follow-up-incorrect-runtime-causes-prolonged-scheduling-exclusion) shows that incorrect runtime charging can leave a runnable task without execution for about seventy seconds after a ten-second VM suspension. This libc experiment shows how a delayed waiter can extend a notifier's application lock hold. Both mechanisms are established separately; their composition in the original ClickHouse incident remains unproven. The original capture lacks the notifier's fault-time PC/futex address and the matching waiter's state.

A runtime containing the upstream rewrite is therefore a verified remedy for this specific notification-delay mechanism. It does not correct ArcBox's guest scheduler accounting, and this experiment does not establish end-to-end SigNoz recovery. A production image change still needs ClickHouse workload and compatibility validation.

## Reproduction and validation

Compile dynamically with an existing ARM64 Linux glibc toolchain. Do not use the static musl compiler from the earlier CPU probes or define `NDEBUG`; the probe's assertions perform its checks. The guest must expose `/proc/self/task/TID/syscall` and at least three CPUs.

```sh
gcc -O2 -Wall -Wextra -Werror -fanalyzer -pthread cond_signal_latency.c -o /tmp/cond_signal_latency
/tmp/cond_signal_latency control fast
/tmp/cond_signal_latency delay wait
/tmp/libc-modern/ld-linux-aarch64.so.1 --library-path /tmp/libc-modern /tmp/cond_signal_latency control fast
/tmp/libc-modern/ld-linux-aarch64.so.1 --library-path /tmp/libc-modern /tmp/cond_signal_latency delay fast
```

The first pair assumes the tested unpatched 2.35 runtime. The second pair assumes the copied, matched 2.41 loader and libc. Set the expected `wait` or `fast` behavior from the runtime's known implementation before running the case; an unexpected result must fail.

GCC 12.2.0 passed strict warnings and static analysis. Clang-format, workspace formatting, and the existing `arcbox-hv` CPU accounting example's Clippy check passed. All four runtime assertions passed. The task-owned containers were removed. Raw results, runtime identities, exact source, and validation records are archived with a verified SHA256 manifest under `condvar-latency-2026-10-08`; the final source is named `cond_signal_public.c` in that archive. No production runtime was changed.

After collection, the task daemon exited with status zero, its VZ helper exited, and the builder returned to its stopped state. The three existing user VM helpers remained running. The original ClickHouse, Keeper, and Postgres containers reported healthy; ClickHouse answered `SELECT 1`. This health check does not validate end-to-end ingestion.

## Follow-up: scheduler exclusion propagates into notification latency

The two mechanisms now reproduce in one process. [cond_scheduler_latency.c](../../tests/bench/signoz/cond_scheduler_latency.c) creates eight candidate waiters, each with an independent condition variable and mutex. Each waiter enters its condition-variable futex before SIGUSR1 diverts the waiter into a busy handler. All eight handlers remain runnable on guest CPU 1 with unchanged scheduler policy and affinity. The controller runs on CPU 2.

After passive baseline snapshots, the controller emits a ready record and waits for host input. The host suspends the explicitly identified disposable VZ helper for ten seconds, with the existing detached resume watchdog. After resume, the controller selects the unique candidate whose scheduler-runtime increment exceeds its raw PROF increment by at least eight seconds. Selection changes no scheduling property. Zero or multiple qualifying candidates invalidate the case.

The controller then performs the first notification on the selected candidate's condition variable, adds waiter B on CPU 2, and starts the second notifier on CPU 2. That notifier holds the application mutex while calling `pthread_cond_signal`. After the notifier begins, the controller releases the selected handler's gate. The other seven candidates remain busy until the selected waiter completes its condition wait and the notifier returns. Thus the modern runtime cannot shorten the target's scheduling delay by removing its competitors early.

The same binary, kernel, container, and two previously identified runtimes produced these results:

| Runtime | VM suspension | Gate release to handler exit marker | Second notification elapsed | Notifier thread CPU |
|---|---|---:|---:|---:|
| glibc 2.35 | None | 17.026 ms | 18.365 ms | 13.166 µs |
| glibc 2.41 | None | 0.334 µs | 4.542 µs | 3.042 µs |
| glibc 2.35 | Ten seconds | **70.027811 s** | **70.029045 s** | 14.416 µs |
| glibc 2.41 | Ten seconds | **70.158072 s** | **2.000 µs** | 3.500 µs |

The old-runtime pause case selects candidate 6, TID 71, with 10.015850 seconds of excess scheduler runtime. Seventy snapshots spanning about 69.050 seconds show state `R`, runtime fixed at 10.023743838 seconds, and dispatch count fixed at 5. Peer snapshots strictly inside that observation window show a competing thread on CPU 1 advancing by 9.719518 scheduler seconds and 9.667 raw PROF seconds. The notifier remains pending throughout, then returns 33.084 microseconds after the selected handler's exit marker. The handler records that marker immediately before returning; the recorded time is not an exact kernel signal-return boundary.

The modern-runtime pause case selects candidate 2, TID 83, with 10.023498 seconds of excess scheduler runtime. Seventy-one snapshots spanning about 70.044 seconds show state `R`, runtime fixed at 10.042051919 seconds, and dispatch count fixed at 8. Interior peer snapshots show its competing thread advancing by 9.862452 scheduler seconds and 9.859 raw PROF seconds. The notifier returns before the controller releases the gate, while the selected handler remains active. The scheduling defect therefore persists even though the notification no longer waits for the affected thread.

Snapshots bracket `/proc/self/task/TID/status`, `schedstat`, raw PROF, and the synchronization flags with monotonic timestamps. The probe deliberately avoids `/proc/TID/stat` and remote SCHED-clock reads because those paths can update scheduler accounting. The raw per-thread PROF clock uses `(~(unsigned)tid << 3) | 4U`; all threads belong to the same process. Maximum snapshot spans are 140.458 microseconds in the old pause case and 140.833 microseconds in the modern pause case. No read spans the host suspension. The container has unlimited CPU quota, and throttling counters remain zero.

This controlled experiment connects incorrect guest runtime charging, prolonged scheduling exclusion, and old-glibc notification blocking. The runtime containing upstream commit `1db84775` breaks the final dependency while leaving the accounting defect visible. The microsecond timings are completion-order evidence, not a general libc performance benchmark.

The experiment does **not** identify the original ClickHouse trigger. The original capture lacks a matching blocked notifier futex, the associated delayed waiter, and its scheduling history. The synthetic SIGUSR1 busy handler and controlled whole-VM suspension establish a possible causal chain; they do not establish that those conditions occurred in the original incident.

The probe requires the baseline tick-accounting kernel, at least three guest CPUs, and assertions enabled. Compile with the same strict GCC flags as the earlier probe. Invoke `control fast`, `pause wait` for the tested old runtime, or `pause fast` for the tested modern runtime. Supply `go` on stdin after the ready event and, for pause cases, after the verified helper resumes. A 150-second alarm bounds the process. GCC warnings and static analysis, clang-format, workspace formatting, and the existing HV example's Clippy check pass. All four runtime assertions pass. Source, binary, identities, snapshots, and analysis are preserved under `cond-scheduler-2026-10-08` with a SHA256 manifest.
