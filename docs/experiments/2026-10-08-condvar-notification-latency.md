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
