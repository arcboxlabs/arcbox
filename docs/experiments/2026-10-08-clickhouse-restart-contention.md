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

The driver creates a new network by default. To reuse an empty network retained by an earlier restart experiment, pass its full ID with `--network NETWORK_ID`. The driver inspects the network before copying seed volumes and again before creating containers. Both checks require the exact ID, the `bridge` driver, local scope, no active endpoints, and an `arcbox.stall.experiment` label equal to a name beginning with `stall-warm-`. The capture retains both inspections and records the selected ID and canonical name. The original ClickHouse and Keeper DNS aliases remain unchanged; the driver does not change the subnet or assign fixed IP addresses. Cleanup retains the network. Keep earlier experiment containers stopped while reusing their network; the checks do not reserve the network against concurrent starts.

```bash
python3 tests/bench/signoz/run_clickhouse_restart.py --fixture /absolute/fixture.json --case startup-contention --kernel-trace on --network FULL_NETWORK_ID --output /absolute/new-case
```

Network reuse does not change the 25-second readiness limit, the 30-second setup limit, or the 300-second observation requirement. Run the offline network regressions with `python3 -B tests/bench/signoz/test_clickhouse_network.py`.

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

## Corrected observer: control-03

The [strict analyzer](../../tests/bench/signoz/analyze_clickhouse_samples.py) validates all 106 complete sweeps and 55,120 thread records. There are 45 successful probes and 15 host-side four-second timeouts: queries 43–52 and 55–59. The observation lasts 300.020 seconds. This case adds no owned contention workers, but a fault-time host process inspection shows unrelated Rust/C compilation. It is not an idle-host baseline. Those external processes were not stopped. The saved process snapshot and the [independent host sampler](../../tests/bench/signoz/sample_host_vm.py) start after the fault window; they cannot reconstruct its physical CPU cost.

Sweep 104 spans 05:45:34.476416–05:46:28.198660 UTC, or 53.722244 seconds. Across sweeps 103→104, all eight Common threads accumulate 42.715860–48.340192 seconds of guest scheduling delay, with 12–28 additional dispatches. These are accumulated delays, not proof of uninterrupted runnable exclusion. The observer gains 1.015420 seconds of runtime and 5.207122 seconds of scheduling delay; its own runqueue delay does not explain the entire slow sweep.

Cgroup counters, read before each sweep, advance by 904.880756 CPU seconds across 104→105, including 819.369788 system seconds and no quota throttling. Across complete sweeps 103→105, the 512 BgSchPool threads account for 750.693 guest runtime seconds and have a median scheduling-delay increase of 48.07827 seconds. These guest counters alone do not establish physical CPU consumption.

ClickHouse logs report a 2.782-second executor lock hold and lock acquisition delays of 3.230, 47.650, 45.565, and 47.690 seconds. Recovery includes 56 ZooKeeper session-expired messages. Keeper independently marks session 65 dead at 05:46:02.485, creates session 74 at 05:46:26.724, then marks it dead at 05:46:58.565. Repeated approximately ten-second Keeper receive messages also occur before the fault and are not unique evidence of the stall.

No complete record contains the known G1-reference or internal-condition-lock PC. The long sweep has unknown per-thread read times, and no complete sweep covers 05:46:32.307921 through observation end at 05:47:01.295328. The missing PC therefore cannot exclude the old-glibc path. The incomplete final sweep begins during cleanup and is reported separately.

The stop request exceeds the driver's 25-second host limit. Subsequent inspection confirms ClickHouse exited with code 137 and `OOMKilled=false`; Keeper exited with code 0. No application case starts until both are confirmed stopped. The failed stop remains a cleanup error in the evidence, even though the server eventually exits.

## Corrected observer: contention-03

Contention-02 fails before either server starts because the observer copy encounters the same stopped-container overlay `device or resource busy` error. An escalated retry of the exact copy succeeds. Its failed attempt remains intact; contention-03 uses fresh clones. The experiment does not add a retry or mount workaround to the driver.

Contention-03 completes its 300.010-second observation and cleanly stops both clones with exit code 0. All 95 complete sweeps and 49,400 thread records pass strict validation. Of 60 probes, 43 succeed and 17 hit the four-second host timeout: queries 8–19 and 30–34. All 36 owned workers terminate with code -15 after a 60.095-second contention interval.

| Long sweep | UTC begin → end | Duration | Common accumulated scheduling delay | Cgroup charge across this and the next sweep begin |
|---|---|---:|---:|---:|
| 17 | 05:51:08.945523 → 05:52:15.342631 | 66.397108 s | 58.081633–62.307367 s | 1116.141568 CPU s |
| 38 | 05:53:00.124544 → 05:53:32.405773 | 32.281229 s | 21.724356–28.635720 s | 428.888 CPU s |

The second slow sweep occurs after the owned host load ends at 05:52:03.740781. Total cgroup charge across complete sweep beginnings is 1672.565275 CPU seconds, including 1421.324051 system seconds, with no quota throttling. No known G1-reference or internal-condition-lock PC is captured. No repeated `R` observations have unchanged runtime and dispatch counters. These records do not identify the blocked notifier or establish uninterrupted scheduler exclusion.

The independent host sampler records 371 samples over 380 seconds. Its longest `ps` call is 0.171456 seconds and its largest begin-to-begin gap is 1.177340 seconds. Samples inside and outside the 60.095-second load interval bound the whole helper's CPU increase at 207.12–210.12 seconds, before a 0.02-second endpoint precision allowance. That host interval does not match the whole guest observation, so the totals must not be compared directly.

Observer cost remains material. Control-03 records 64.681 seconds of observer runtime over 269.615 seconds of complete-sweep coverage; contention-03 records 60.704 seconds over 296.983 seconds. These values exclude child processes and transport work. The matching observer reduces a comparison difference but cannot exclude observation-induced changes to the fault.

Run `python3 tests/bench/signoz/analyze_clickhouse_samples.py CASE_DIRECTORY OUTPUT_JSON` after cleanup. The checker rejects the three original captures because required fields are absent; the corrected control and contention captures pass without relaxing those requirements. The checker reports final incomplete sweeps separately. Use `python3 tests/bench/signoz/sample_host_vm.py --pids DAEMON_PID HELPER_PID --seconds 380 --output NEW_JSONL` before starting an application case. Preserve process identities, raw outputs, and failed attempts.

## Profiler isolation with the same contention

Two more cases retain the same image, old glibc, kernel, 512-thread background pool, observer, seed data, and 36-worker contention window. Only the global profiler configuration changes. Runtime reads of `system.server_settings` confirm the intended values and pool size. The configured enabled period is 10,000,000,000 ns; the binary's known period-truncation defect remains present. These cases do not test the corrected upstream binary.

| Case | Real / CPU profiler settings | Successful / timed-out host probes | Complete sweeps / thread records | Longest sweep | Guest cgroup charge |
|---|---|---:|---:|---:|---:|
| Contention-03 | enabled / enabled | 43 / 17 | 95 / 49,400 | 66.397108 s | 1672.565275 CPU s |
| Profiler-off-contention-01 | 0 / 0 | 60 / 0 | 142 / 73,840 | 2.742479 s | 181.625946 CPU s |
| Real-only-contention-02 | enabled / 0 | 49 / 11 | 125 / 65,000 | 59.516850 s | 1146.620916 CPU s |

All three captures pass strict complete-record checks. Each runs for 300 seconds and cleans up without errors. The off case's slowest host probe takes 0.108791 seconds. ClickHouse emits no Warning/Error or session-expired messages. Its observer still accounts for 88.014384 guest CPU seconds, about 48.46% of cgroup charge; this is an intrusive diagnostic configuration, not ordinary application cost.

Restoring only the Real profiler reproduces eleven host timeouts, queries 7–17, and 56 session-expired messages. The slow sweep spans 06:04:52.017427–06:05:51.534277 UTC. Common threads accumulate 50.983225–57.082541 seconds of scheduling delay; the observer accumulates 55.328006 seconds. No quota throttling occurs. The full sweep-boundary cgroup increase is 1146.620916 CPU seconds, including 952.776390 system seconds. Neither this case nor contention-03 logs the executor lock warning seen in control-03 and the original incident. The first real-only attempt fails during setup with an overlay copy error and remains preserved separately; an escalated exact-command retry succeeds before the fresh case starts.

The supplemental Real trace query returns the top 150 `(thread_id, trace)` groups from this restart window, containing 22,865 samples. Of these, 21,648 samples are in `pthread_cond_wait`; no returned group contains `pthread_cond_signal` or `trySchedule`. The top-group limit, delayed trace collection, and query before observation end prevent an exclusion claim. The original notifier stack has not been matched in these application cases.

These results make the Real-profiler-enabled path a strong trigger candidate under this host contention. The CPU profiler is not necessary for this reproduction. Disabling both global profilers avoids the observed stall in this one comparison; it does not repair guest task accounting or establish which Real-handler, scheduler, or glibc operation stalls. This is a sequential enabled → disabled → Real-only comparison, not a randomized repeated trial or a reproduction of every original symptom. No production configuration changes.

Use the [fixture preparation tool](../../tests/bench/signoz/prepare_profiler_fixture.py) to copy verified configs without editing the seeds: `python3 tests/bench/signoz/prepare_profiler_fixture.py FIXTURE NEW_DIRECTORY --disable both`, or `--disable cpu` for the Real-only case. Run the existing contention driver against `NEW_DIRECTORY/fixture.json`. Verify the effective `global_profiler_cpu_time_period_ns`, `global_profiler_real_time_period_ns`, and `background_schedule_pool_size` values before interpreting a result. The generated both-off config is byte-identical to the completed off experiment's config.

## Correction: ps counters are not a physical CPU upper bound

An initial comparison freezes and strictly parses a guest capture before taking the ending host sample. The first host sample precedes the application start request. This causal ordering avoids reliance on guest/host realtime synchronization. The retained contention snapshot records 1663.837122 guest CPU seconds while the wider 299.614306-second host interval reports a helper `ps` CPU increase of 747.67 seconds. The off snapshot records 160.275392 guest CPU seconds and a 431.70-second helper `ps` increase over its wider host interval.

The initial reports incorrectly call the `ps` values plus 0.02 seconds physical CPU upper bounds. That interpretation is withdrawn. The precision allowance covers displayed centiseconds only. Apple [ps reads two Mach counters separately](https://github.com/apple-oss-distributions/adv_cmds/blob/6bed8737a34dbb54782a18f47dccf933a9967a12/ps/tasks.c#L108-L138), then [adds the results](https://github.com/apple-oss-distributions/adv_cmds/blob/6bed8737a34dbb54782a18f47dccf933a9967a12/ps/print.c#L836-L853). Matching XNU sources obtain [terminated-thread time](https://github.com/apple-oss-distributions/xnu/blob/5c306bec31e314fa4d8bbdafb2f6f5a6b7e7b291/osfmk/kern/task.c#L5247-L5260) separately from [live-thread time](https://github.com/apple-oss-distributions/xnu/blob/5c306bec31e314fa4d8bbdafb2f6f5a6b7e7b291/osfmk/kern/task.c#L5406-L5431). A thread exiting between those reads can be absent from both totals. Stable PID/start/command identity and monotonic displayed totals cannot rule out that omission.

The causal ordering and recorded counter differences remain valid. They cannot independently establish physical CPU overcharging. The same qualification applies to the earlier `ps` load-window bounds in this entry. Historical raw data and analysis files retain their original fields, with a separate `host-cpu-measurement-correction.json` identifying the invalid inference. The [comparison tool](../../tests/bench/signoz/compare_host_guest_cpu.py) now labels its outputs as displayed `ps` deltas with a display margin and includes the non-atomic-counter limitation.

A replacement host provider needs new measurements, including an initial record before the application starts. A later API observation cannot reconstruct that missing baseline. The earlier controlled pause, raw PROF, and scheduler-exclusion experiments remain separate evidence; this correction prevents using these application `ps` comparisons as an additional physical-execution proof.

The archived XNU commit matches the running kernel. The installed `ps` binary's mapping to the cited `adv_cmds` commit has not been independently verified. Source inspection and a native API probe identify `proc_pid_rusage` task-lifetime accounting as a better candidate for follow-up collection, but per-CPU aggregation and publication delay still need measurement. It is not adopted as a physical CPU upper bound in this experiment. Exact source files, hashes, links, API unit checks, and remaining limitations are preserved in `host-cpu-sources/manifest.json`.

## Evidence and cleanup

All restart evidence is copied to the task artifact directory `clickhouse-restart-2026-10-08/evidence`, under `/Users/Xuan/.codex-copilot/visualizations/2026/10/07/01a11659-104a-7452-88e3-0d0ad05852c5`. All 807 files, totaling 281,444,621 bytes, match their source SHA256 values. `SHA256SUMS` and `archive-verification.json` sit beside the evidence directory. The archive preserves failed preparation attempts, invalid original captures, corrected captures, queries, configuration hashes, process records, source files, analysis scripts, and the explicit host-counter correction.

The owned daemon PID 45259 and helper PID 45293 are stopped after all application cases and host samplers finish. All successful experiment clones are stopped; failed setup clones remain created but not running. Seed IDs, stopped state, and original config hashes are verified. The kernel builder remains stopped, the three pre-existing user VZ helpers remain present, and the user's ArcBox Docker endpoint responds. Clone volumes and the disposable VM disk are retained. No production runtime, kernel, or container profiler setting changes.
