# 2026-10-08 — Can HV expose correct execution time, and does Colima avoid CPU accounting divergence?

- Type: experiment
- Area: Hypervisor.framework, Linux CPU accounting, Docker statistics
- Outcome: HV execution time excludes suspension and contention; Colima reproduces counter divergence; a complete guest accounting repair remains open.
- Probes: [HV guest](../../virt/arcbox-hv/examples/cpu_accounting.rs), [HV driver](../../tests/bench/signoz/hv_accounting.py), [CPU sampler](../../tests/bench/signoz/sample_cpu.py), [SQL workload](../../tests/bench/signoz/cpu_workload.sql), [overhead probe](../../tests/bench/signoz/accounting_overhead.c).
- Host: M5 Max, 18 cores, macOS 26.4 build 25E246, Xcode 26.6. Existing user VMs and unrelated builds remained active.

## Question

The [SigNoz investigation](../logs/2026-10-07-signoz-profiler-and-cpu-accounting.md) reproduced incompatible guest and container CPU counters. Can ArcBox use the public HV execution-time API to supply stolen time correctly? Does the same workload avoid counter divergence under Colima?

## Hypotheses

1. HV execution time excludes intervals when the host cannot run the vCPU.
2. A host suspension returns control to the userspace VMM before guest execution resumes.
3. Colima's current configuration avoids the accounting divergence observed under ArcBox VZ.

## HV method

The probe creates one VM, an in-framework GIC, one vCPU, and a 16 KiB instruction page. The guest loops over `ADD` or executes `WFI`. A host thread cancels `hv_vcpu_run` after three seconds. The guest has no exit instruction. A third case sleeps on the host without entering the guest.

The driver repeats six cases three times: busy, WFI, host sleep, busy with suspension, WFI with suspension, and busy with host contention. Suspension targets only the newly created probe child. A detached watchdog sends SIGCONT after one second. The recorded stop/resume window includes watchdog completion overhead and is not an exact scheduler suspension duration. Contention starts 36 `yes` processes after the probe becomes ready; the driver terminates those processes after the measurement.

The probe reads `hv_vcpu_get_exec_time` on the owning thread and compares its delta with `CLOCK_THREAD_CPUTIME_ID` and elapsed monotonic time. The ARM64 SDK declares execution time in `mach_absolute_time` units. The measured timebase is 125/3 nanoseconds per tick. Apple's [web description](https://developer.apple.com/documentation/hypervisor/hv_vcpu_get_exec_time(_:_:)) instead says nanoseconds; the probe uses the SDK units.

Run from the repository root after configuring the project's signing identity:

```sh
direnv exec . cargo build -p arcbox-hv --features gic --example cpu_accounting --release
codesign --force --options runtime --identifier com.arcboxlabs.cpu-accounting --entitlements bundle/arcbox.dev.entitlements --sign 'Developer ID Application: ArcBox, Inc. (422ACSY6Y5)' target/release/examples/cpu_accounting
python3 tests/bench/signoz/hv_accounting.py --binary target/release/examples/cpu_accounting --output /tmp/hv-accounting.jsonl
```

The output path must not exist. Run the driver when the host can tolerate three short CPU contention bursts.

## HV results

Values are medians of three repetitions, in seconds. Thread CPU time includes host work that the HV execution counter excludes.

| Case | Elapsed | Host thread CPU | HV execution | Returns from `hv_vcpu_run` |
|---|---:|---:|---:|---:|
| Busy | 3.002779 | 2.916084 | 2.845562 | 1 |
| WFI | 3.003714 | 0.000102 | 0.000001333 | 1 |
| Host sleep | 3.003921 | 0.000024 | 0 | 0 |
| Busy, suspended for about 1 s | 3.005074 | 1.937806 | 1.906084 | 1 |
| WFI, suspended for about 1 s | 3.003048 | 0.000079 | 0.000001417 | 1 |
| Busy, 36 host load processes | 3.013716 | 1.177930 | 1.162838 | 1 |

Hypothesis 1 survives these checks. The API excludes suspension, host contention, WFI wait, and time outside guest execution in these cases. This establishes a useful measurement source, not a complete stolen-time provider.

Hypothesis 2 is false for this probe. Each guest case returns only once, for the final host cancellation. Suspension and resumption do not return control to the userspace VMM. Updating a shared page only after `hv_vcpu_run` returns would miss the guest's first accounting operations after resumption.

Elapsed time minus execution time also includes normal WFI idle. That difference cannot be labeled stolen time without another source of state information. The [ARM PV-time interface](https://docs.kernel.org/virt/kvm/arm/pvtime.html) requires involuntary non-running time and an update before scheduling the vCPU. The public [WFI wait-time query](https://developer.apple.com/documentation/hypervisor/hv_vcpu_get_wait_for_interrupt_time(_:_:)) starts at macOS 27; this host runs 26.4. An idle-time counter alone does not establish the required update timing.

## Colima method

The existing profile had no running containers before this experiment. Two experiment containers supplied the overhead probe and ClickHouse workload. The profile and VM were not restarted, reconfigured, or suspended.

| Property | Observed value |
|---|---|
| Colima | 0.10.3, commit `00f6c297e92a82c04a4ab507db0a61435650d7e8` |
| Backend and VM | VZ, 18 vCPUs, 16 GiB RAM, nested virtualization disabled |
| Guest kernel | Ubuntu `6.8.0-117-generic`, GCC 13.3.0 |
| Docker server | 29.5.2 |
| Kernel configuration | HZ 1000, NO_HZ_FULL, VIRT_CPU_ACCOUNTING_GEN; CONTEXT_TRACKING_USER_FORCE and PARAVIRT_TIME_ACCOUNTING disabled |
| Runtime activation | Empty `/sys/devices/system/cpu/nohz_full`; no `nohz_full` boot argument |

Compiling generic virtual accounting does not activate that accounting on every CPU. Linux [vtime.h](https://github.com/gregkh/linux/blob/v6.8/include/linux/vtime.h) gates the generic path on runtime context tracking. This Colima configuration continues to use tick accounting. It differs from the earlier ArcBox candidate, which forced context tracking on all CPUs.

The local ClickHouse `25.12.5` image was copied from ArcBox into Colima; its image ID was `cacf32d68842`. A config override disabled both global profilers and set `background_schedule_pool_size` to 512. Runtime queries confirmed those settings. The container used no network and exposed no host ports.

The same checked-in SQL ran with 18 threads and a 90-second limit. The sampler collected guest, container, and host VM counters every two seconds. Five Docker stats API responses were collected during the workload. The host helper PID, 50285, was verified through its open Colima EFI and disk files.

To repeat, create an isolated ClickHouse container on an explicitly selected Colima Docker socket. Apply the three server settings above. Run `sample_cpu.py` for 98 seconds, start `cpu_workload.sql` after three seconds, and capture `/containers/NAME/stats?stream=false` during the query. Run `accounting_overhead` before the query. Verify `SELECT 1` after completion. Stop and remove only the experiment containers.

## Colima results

Busy intervals require more than ten container CPU seconds per elapsed second. They total 78.03 seconds.

| Counter | Delta | CPU seconds per elapsed second |
|---|---:|---:|
| Guest total, first seven `/proc/stat` fields | 1219.900 s | 15.634 |
| ClickHouse cgroup | 1356.396 s | 17.383 |
| Host VZ helper | 1017.580 s | 13.041 |
| Guest steal | 0 s | 0 |

The five Docker API counter pairs give 2026.51%, 2018.67%, 2010.50%, 1979.61%, and 2070.98% using Docker's formula and 18 online CPUs. All exceed 1800%. These are calculated percentages from saved API responses, not UI screenshots. The aggregate busy-window formula gives 2001.40%.

Median overhead was 149.57 ns per direct `getpid` and 1863.84 ns per pipe round trip. These microbenchmarks do not measure whole-application performance. Counter reads are sequential; short intervals contain sampling skew. The longer aggregate still shows divergence.

Hypothesis 3 is false. Colima reproduces the same class of accounting mismatch without nested virtualization or ClickHouse profilers. This does not reproduce the original extreme percentage, long lock stalls, or failed SigNoz ingestion. The comparison does not isolate why magnitudes differ between kernels and runs.

## OrbStack evidence boundary

The host retains an OrbStack Docker context, but the usual application paths, `orb` executable, and Docker socket are absent. A same-host experiment is pending installation and first startup by the user.

OrbStack's [architecture documentation](https://docs.orbstack.dev/architecture) describes a shared Linux kernel and custom optimizations. That description does not establish correct stolen-time accounting. The inspected public [linux-macvirt branch](https://github.com/orbstack/linux-macvirt/tree/308681584a45dba25ebd45499136bfec1b829468) is Linux 6.3.9, dated June 25, 2023. It cannot establish current OrbStack behavior. Unrelated reports of high CPU or hangs are not evidence of this defect.

## Validation and remaining work

The release probe built and ran all 18 final cases. The earlier five-case pilot also completed. Formatting and example Clippy passed. The `arcbox-hv` test run passed 26 tests and skipped 15 entitlement-dependent tests; the signed live probe validates the hardware path used here. Python syntax checks passed.

No production accounting code or VM default changed. Experiment containers, probe VMs, and host load processes were stopped and removed. Existing user VMs remain running. Raw counters, API responses, configs, scripts, and a SHA256 manifest are preserved in the task artifact directory `cpu-accounting-2026-10-08`.

Correct cumulative execution time is available through HV, but these results do not prove a public API path for timely standard stolen-time updates. A future guest-demanded query needs separate attribution and overhead validation. The original SigNoz stall remains unresolved, and OrbStack runtime validation remains pending.
