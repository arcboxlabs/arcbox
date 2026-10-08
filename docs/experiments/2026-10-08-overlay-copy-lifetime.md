# 2026-10-08 — Why can copying into a stopped container fail with OverlayFS EBUSY?

- Type: experiment
- Area: guest Docker archive views, OverlayFS mount lifetime
- Outcome: overlapping overlay lifetimes identified; production repair remains open
- Probes: [guest lifecycle recorder](../../tests/bench/signoz/probe-overlay-lifecycle.sh), [lifecycle analyzer](../../tests/bench/signoz/analyze_overlay_lifecycle.py)
- Host / guest: macOS VZ; Linux 6.18.38 arm64, 18 vCPUs, 16 GiB; Docker 29.8.2 with the containerd overlay snapshotter

## Question

The simultaneous host/guest ClickHouse experiment failed during its first configuration copy, before starting ClickHouse, the guest recorder, or the owned host load. Could a previous Docker archive view retain the overlay after the original-path unmount returned?

## Method

Only the isolated VM was rebooted. The diagnostic kernel enables `KPROBES`, `KPROBE_EVENTS`, `FUNCTION_TRACER`, `FUNCTION_GRAPH_TRACER`, and `DYNAMIC_FTRACE`. Kconfig also enables their dependencies and changes function alignment from 4 to 8. The baseline tick CPU accounting configuration is unchanged. The running guest configuration matches the built configuration byte for byte. These changes and the probes can perturb timing; this experiment measures lifecycle ordering, not baseline race frequency.

The kernel Image SHA256 is `4b088a3cd967aa5b4576c8c1e275ff171ef540ff5c97fe502c7c633cfc9bbca1`; configuration SHA256 is `5065e6d540f190797a25c6a1cf5981a042ad88d36ef2ebbb7ec99433111ea57e`. The previous kernel and all 75 existing containers were preserved. Non-target VM process identities did not change.

Create a fresh stopped container from image `sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a`, with network `none`, restart policy `no`, and entrypoint `/bin/true`. Repeatedly copy a local configuration file to `/etc/clickhouse-server/config-0-0.yaml`. Do not start the container. Stop on the first failure, with a ceiling of 100 copies. The observed snapshot mount options include `index=on,nfs_export=on`.

Run the recorder as root inside the guest after mounting tracefs at `/sys/kernel/tracing`. Use a fresh alphanumeric token beginning with a letter. The commands are `sh probe-overlay-lifecycle.sh prepare TOKEN`, then `stop TOKEN`, `export TOKEN`, and `cleanup TOKEN`. The evidence remains under `/run/arcbox/overlay-probe-TOKEN` after cleanup. Export that directory before any reboot. The recorder refuses existing evidence, checks required symbols and tracepoints, and removes only its owned instance and probe definitions.

The recorder pairs entry and return probes for eleven functions, including `ovl_fill_super`, `ovl_inuse_trylock`, `ovl_inuse_unlock`, `cleanup_mnt`, and `put_mnt_ns`. It uses the guest monotonic clock, 4 MiB per CPU, disabled overwrite, and bounded unlock stack traces. `ovl_get_upper` is inlined in this build. Nested trylocks inside `ovl_fill_super` identify the upper and work dentries instead. Return probes retain the original function arguments.

## Results

Case `overlay-probe-01` failed on copy index 2: the third `docker cp`. The container remained created with PID 0 throughout. No ClickHouse process, owned host pressure, or host ktrace ran. Container `1fb7b4c544b93b6d6bea999339d80f95447f870cff30f4a3ec451f84484e391e` and snapshot 330 are retained.

The trace contains 6,563 events. All 18 CPU buffers report zero overrun, commit overrun, and dropped events; every probe reports zero missed hits. Ten unlock stacks were captured. The critical old-mount-to-cleanup interval contains 559 complete probe pairs, with no nesting error, argument mismatch, or censored frame. Global capture edges require separate treatment; entry/return totals alone are not a completeness test.

All timestamps below are guest monotonic seconds. Line numbers refer to `overlay-probe-01/guest-evidence/export/trace.txt`.

| Lines | Time | Thread | Observation |
|---|---:|---:|---|
| 5298–5303 | 487.807768–487.807991 | 396 | Old overlay `S=0xffff0000d53b9000` successfully locks upper `D=0xffff0000e8144e40` and completes `fill_super`. |
| 6045–6094 | 487.809678–487.809991 | 396 | Namespace `N=0xffff0000c1682100` drops mount `M=0xffff0000c4284480`; `exit_task_namespaces` returns. |
| 6053–6069 | 487.809825–487.809849 | 350 | Original-path lazy unmount returns and drops its overlay mount references. |
| 6099–6100 | 487.810001–487.810003 | 396 | `cleanup_mnt(M)` enters `deactivate_super(S)`. |
| 6337–6350 | 487.811356–487.811698 | 387 | New overlay `0xffff0000c5dc2800` tries the same upper D, receives trylock=0, and returns `-16` (`EBUSY`). |
| 6398–6399 | 487.813247–487.813252 | 396 | Old `put_super(S)` enters `free_fs(0xffff0000c1678800)`. |
| 6427–6453 | 487.813274–487.813281 | 396 | The old overlay finally unlocks upper D. |
| 6483 | 487.813453 | 396 | `cleanup_mnt(M)` returns. |

The failed trylock precedes the old upper-unlock entry by 1.886 ms and its return by 1.893 ms. The unlock stack is `do_exit → task_work_run → __cleanup_mnt → cleanup_mnt → deactivate_super → generic_shutdown_super → ovl_put_super → ovl_inuse_unlock`. The final release therefore occurs in the old Docker thread's exit task work. It is not attributed to a delayed-mntput worker or NFS cache eviction. This identifies the observed final release path; it does not enumerate every historical reference.

The same mount address appears in an earlier copy and has already completed cleanup there. Addresses must be associated with lifecycle generations, not joined across the whole trace. `sched_process_exit` also precedes namespace and task-work cleanup; that tracepoint alone is not a completion barrier.

## Findings and repair boundary

Docker 29.8.2's `daemon/containerfs_linux.go` closes the view's `done` channel before the locked goroutine returns. `containerFSView.Close` waits for that channel, then releases volumes and the original rootfs mount. The OS thread still has to exit. `daemon/snapshotter/mount_unix.go` uses `MNT_DETACH`, so removing the visible mount does not wait for all cloned-namespace references or superblock cleanup. The trace captures the next mount reaching the upper lock before that cleanup completes.

This preparation failure is a Docker archive-view lifecycle race enforced by Linux OverlayFS's exclusive upper-directory check. The repair must make view closure acknowledge completed namespace/root/cwd reference release before the shared snapshot can be remounted. A delay, retry, or disabling the upper lock would not establish that ordering. An implementation must preserve the archive view's mount and path isolation, including setup-error cleanup and the Go runtime's locked-thread rules.

For ArcBox's pinned Linux 6.18.38 guest, a candidate is a `PIDFD_THREAD` exit barrier. Acquire the locked worker's pidfd before unshare, then wait for thread exit before releasing shared mounts on both normal completion and setup failure. Linux runs the observed exit task work before publishing pidfd exit readiness. This avoids restoring or weakening the private namespace. The reversible network-namespace caller must retain its existing thread-reuse behavior. `PIDFD_THREAD` requires Linux 6.9, so unconditional use is unsuitable for Moby's older-kernel compatibility. This candidate has source support but has not been implemented or regression-tested.

[Moby PR #45698](https://github.com/moby/moby/pull/45698), merged on 2023-06-08, already describes the stopped-container `HEAD → PUT` sequence racing unfinished lazy-unmount cleanup. That PR adds snapshot mount reference counting for another duplicate-mount path; it does not change the archive-view completion barrier. Its discussion treats the remaining overlap as a warning. Here, `index=on` makes an occupied upper directory a hard error. The exact thread-exit release chain above adds evidence beyond that earlier report. At inspected master [f313c02f1815f938beaeabc0dbae5c88fd0f5dec](https://github.com/moby/moby/commit/f313c02f1815f938beaeabc0dbae5c88fd0f5dec), `containerfs_linux.go` and `unshare_linux.go` are byte-identical to the tested v29.8.2 files. No later completion-barrier fix was found in the inspected file histories and issue/PR search.

The original ClickHouse stall remains a separate investigation. Its guest signal-lock contention and long runnable waits are not explained by this stopped-container copy failure. No production Docker, guest-accounting, scheduler, profiler, or libc change was deployed. The next synchronized application capture still requires new host helper/thread identities and a recording gate after fixture preparation succeeds.

## Preserved evidence

Task artifact `overlay-copy-lifetime-2026-10-08` contains 162 source/copy SHA256-verified files totaling 79,393,054 bytes. It retains the exact driver, probe, kernel/configuration, raw trace, analyses, matching Moby source, and a snapshot of the live daemon log. All 75 pre-existing container identities, image identities, names, and states match the before-reboot baseline; the additional stopped probe container brings the total to 76. All four recorder actions succeeded and the owned probes were removed. The isolated diagnostic VM remains available; the previous frozen archives are unchanged.

Run `python3 tests/bench/signoz/analyze_overlay_lifecycle.py TRACE --output NEW_JSON`; use `--self-test` for recursion, CPU migration, pointer mismatch, truncation, and mount-address reuse checks. On this capture, all 3,214 probe calls pair completely, all ten unlock stacks associate, and exactly one lock-overlap chain appears. Independent analysis matches the checked-in parser's JSON. Inspect header counts, parse/pairing/lifetime errors, CPU loss counters, and probe misses together; `capture.loss_free` alone only describes ring-buffer loss. Profile hit counts span registration and need not equal recorded events. Displayed timestamps have microsecond precision. Check the parser with `ruff check` and `ruff format --check --line-length 120`; those checks and its self-tests pass. Shell syntax and repository formatting also pass.
