# 2026-10-08 — Can host CPU contention stall real ClickHouse threads?

- Type: experiment
- Area: VZ guest scheduling, ClickHouse, diagnostic collection
- Outcome: open; initial application captures have a reproduced status-reader defect
- Probes: [restart driver](../../tests/bench/signoz/run_clickhouse_restart.py), [observer](../../tests/bench/signoz/observe-clickhouse.sh), [procfs comparison](../../tests/bench/signoz/probe_status_read.sh), [comparison checks](../../tests/bench/signoz/analyze_status_read.py)
- Host / guest: Apple M5 Max, macOS 26.4, isolated 18-vCPU VZ guest, baseline tick-accounting kernel `6.18.38-arcbox`, ClickHouse `25.12.5.44`, retained unpatched glibc 2.35

## Question

The [combined synthetic probe](2026-10-08-condvar-notification-latency.md) connects incorrect CPU charging, scheduling exclusion, and old-glibc notifier blocking. Does ordinary host CPU contention reproduce those mechanisms in the actual ClickHouse server, without a synthetic signal handler or explicit VM suspension?

## Method

Each case copies the same stopped ClickHouse and Keeper seed volumes into new owned volumes. Source mounts are read-only. The driver verifies seed IDs, image IDs, Compose project labels, volume destinations, and config hashes. Config files are copied into new containers. The UDF volume is mounted read-only. The experiment starts Keeper first and waits for health, then starts ClickHouse. No original seed container is resumed.

Each successful case observes ClickHouse for 300 seconds. `SELECT 1` runs through Docker exec about every five seconds, with a three-second guest timeout and a four-second host timeout. These limits bound the client once it executes; a delayed Docker exec can postpone guest client creation. Query success does not validate the Common executor's background work.

The observer reads eight Common and 512 BgSchPool threads through one persistent Docker exec. Records include status, schedstat, syscall, wchan, sweep timestamps, and cgroup CPU counters. SYS_PTRACE is added only to the owned ClickHouse clones because ordinary exec cannot read another process's syscall state. Each record contains separate reads; neither a row nor a sweep is an atomic snapshot. Observer scheduler counters are read at sweep end; cgroup counters are read before sweep begin.

Control adds no host load. Contention runs 36 owned `yes` processes from start +30 to +90 seconds. Startup contention starts ClickHouse five seconds after all 36 workers start; the supervisor still runs for 60 seconds. The supervisor is detached and cleans up its own children. Cases run sequentially. Cleanup stops ClickHouse before Keeper and retains the clones, data, and evidence.

Runtime queries in the first contention case confirm both global profiler settings at 10,000,000,000 ns and the scheduling pool at 512. The known period-truncation defect remains in this binary. A later workload snapshot contains ten active SigNoz parts, 114 rows, zero active merges, and zero unfinished mutations. This is a small retained dataset, not production ingestion load.

## Correction: warm restart was already covered

The retained seed logs disprove the initial hypothesis that no warm restart had been tried. A previous run remained active for about 328 seconds, recorded 56 leader acquisitions and six initial mutation loads, and emitted no long-lock warning. Activation timing differs between runs:

| Run | First leader after ready | First mutation load after ready |
|---|---:|---:|
| Original incident | 0.112193 s | 2.589076 s |
| Retained warm restart | 60.028044 s | 60.054646 s |
| New control-02 | 0.024547 s | 0.031976 s |

The new clone activates much earlier than the retained warm restart. Thus +30 to +90 seconds covers steady operation in the new clone. The original incident's first long-lock warning was 206.338484 seconds after readiness; activation remains a candidate factor, not an established trigger.

## Initial captures: preserve the failure and the integrity limit

Control-01 failed before either server started: Docker cp returned an overlay mount `device or resource busy` error. The exact failing copy succeeded when repeated with escalation. The experiment did not silently retry; a fresh control-02 preserved the failed attempt separately.

| Initial case | Host probes successful / timed out | Longest sweep | Missing status fields |
|---|---:|---:|---:|
| Control-02 | 60 / 0 | 1.857 s | 2 |
| Contention-01 | 53 / 7 | 39.415 s | 3 |
| Startup-contention-01 | 52 / 8 | 42.861 s | 2 |

The first contention case's eight Common threads show accumulated guest scheduling delays of 31.424–36.949 seconds across the long sweep; startup contention shows 38.229–40.904 seconds. These are descriptive values from retained core fields, not a claim that either full capture passed validation. The first contention case also records Keeper session expiry and a burst of later SQL completions taking zero to two milliseconds. Host exec timeouts must not be reported as four-second SQL execution times. The probes did not assign unique query IDs, so individual timed-out host requests cannot be paired unambiguously with those server rows.

No known G1-reference wait PC or long executor-lock warning was captured in these three cases. Their long observation gaps cannot exclude a wait during the gap. Every initial capture fails strict full-status validation; the records remain preserved, including the malformed fields and cleanup tails. They must not be relabeled as fully validated after the reader repair.

## Status-reader defect reproduced and repaired

The old shell observer used `while read` directly on `/proc/TID/status`. Bash reads ahead on a seekable descriptor and seeks back after each line, as described by its [maintainer](https://lists.gnu.org/r/bug-bash/2022-04/msg00072.html). A procfs seek can regenerate the text. If a preceding field changes length, a later read can begin inside a field name. The observer then filters out that damaged name.

Linux `v6.18.38` unconditionally emits both Uid and SigBlk in [fs/proc/array.c](https://github.com/gregkh/linux/blob/v6.18.38/fs/proc/array.c). The missing rows therefore require investigation; those fields are not optional. Independent raw extraction confirms four missing SigBlk rows, one missing Uid row, and two missing nonvoluntary context-switch rows across the three cases.

The dynamic-status probe repeatedly reads a shell alternating runnable work and short sleeps. It saves unfiltered records inside a disposable container, then verifies their SHA256 hashes after copying them to the host. This separates reader corruption from transport loss.

| Reader | Records | Records missing a required field |
|---|---:|---:|
| Direct line reads from procfs | 10,000 | 321 |
| Read full status text, then parse the saved text | 10,000 | 0 |

One raw damaged line is `onvoluntary_ctxt_switches`, missing the initial `n`. The observer now reads status into a variable before parsing. The strict checks remain unchanged. The repair fixes record completeness; it does not make the sequential observations atomic or remove observer overhead.

To repeat the reader comparison, copy `probe_status_read.sh` into an isolated ARM64 Linux container with the tested Bash runtime. Run the script under `timeout --kill-after=1 90`; it requires a new `/tmp/proc-read-check` directory. Copy that directory out and run `python3 tests/bench/signoz/analyze_status_read.py DIRECTORY` with assertions enabled. The comparison requires a failing direct-read baseline and no damaged buffered-read records. Stop and remove only the owned container.
