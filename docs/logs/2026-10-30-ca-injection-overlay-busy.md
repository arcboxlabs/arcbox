# 2026-10-30 — Container CA injection retries archive requests that hit a busy overlay

- Type: investigation, change
- Area: `app/arcbox-docker` (`handlers/container/ca_injection`), guest dockerd overlay mounts
- Commits / PRs: this change
- Related: `docs/architecture/vm-stack-redesign.md` (R4 baseline runs that exposed it)

## Problem or trigger

During the R4 e2e baseline on 2026-10-30 (M5 Pro, master `cd19fe7d`), `docker start` failed with 500 in `boot_assets` on both backends: 0 of 4 HV runs and 1 of 2 VZ runs passed, and `backend_matrix` blamed HV. `start_container` propagates `ca_injection::reconcile` errors, so a failed archive request fails the start. At create the same error is only a warning.

## What was done

The failing request was a dockerd archive call (`HEAD`/`GET`/`PUT /containers/<id>/archive`), answering 500 with `failed to mount /var/lib/docker/rootfs/overlayfs/<id> ... index=on,nfs_export=on, err: device or resource busy`.

Measurements against a created alpine container through the daemon's Docker socket:

| Pattern | Busy answers |
|---|---|
| 200 sequential `HEAD`, no delay / 20 / 100 / 250 ms (HV) | 0 at every spacing |
| 200 sequential `HEAD`, no delay (VZ) | 0 |
| Concurrent `HEAD`, 2 / 4 / 8 way (HV, 10 rounds) | 0/20, 3/40, 19/80 |
| `HEAD` right after `PUT` archive | HV 3/30 and 19/50, VZ 13/50 |
| `HEAD` right after `GET` archive | HV 3/30 |
| `docker create` + `docker start`, 10 rounds (HV) | 6/10 create warnings, 3/10 failed starts |

The busy window closes 2–8 ms after the colliding request returns. Retrying a busy request succeeded on the first or second immediate retry in every case; the worst observed was 16.7 ms on HV and 11.0 ms on VZ.

Every failure logs `overlayfs: upperdir is in-use as upperdir/workdir of another mount, mount with '-o index=off' to override exclusive upperdir protection.` in the guest kernel log, 170 lines for 170 dockerd errors. With `index=on`, which `nfs_export=on` requires, overlayfs refuses a second mount of the same upperdir. dockerd mounts the container rootfs per archive request and releases it a few milliseconds after replying, either by a lazy unmount or through the archive helper's mount namespace; which one was not determined. CA injection issues its `HEAD`/`GET`/`PUT` calls back to back, so the next call lands in that window.

The fix retries an archive request that dockerd answers with 500: five retries, starting at 5 ms and doubling, 155 ms in total. A `HEAD` answer carries no body, so the busy case cannot be told apart from other server errors; every archive request is idempotent, so retrying any 500 is safe and only delays a real failure. Start stays fail-closed.

The start that follows CA injection can meet the same window: dockerd mounts the rootfs to start the container. With only the archive retry, a tight HV probe still failed 2 of 60 starts with the same busy error. The proxied start and restart are therefore also retried, on the same schedule, but only when dockerd answers 500 with `device or resource busy`; a start that failed this way did not start the container. Other start errors are returned unchanged. `index=off` was rejected because it drops `nfs_export`; spacing every call was rejected because it costs the common case and does not cover concurrent callers.

## Evidence

- Root cause probes: xuan-work-mbp, master `cd19fe7d`, logs `~/.amp/in/logs/11-{hv,vz}-dockerd.log` and `11-hv-dmesg.txt` on that machine.
- Regression test: `archive_requests_outlast_a_busy_overlay_and_then_report_it` — a fake guest answers the first `BUSY_RETRIES` archive requests with 500 and injection still succeeds; one more busy answer fails it.
- Regression test: `start_outlasts_a_busy_overlay_and_returns_other_errors_unchanged` — five busy start answers still start the container, a sixth is returned, and a non-busy 500 is returned without a retry.
- Probe on xuan-work-mbp with the archive retry only (`af4e484a`): 10 rounds 0 create warnings and 0 failed starts (13 busy errors absorbed); 50 more rounds 0 warnings, 2 failed starts from dockerd's own start mount. Before the fix (`cd19fe7d`): 6/10 warnings and 3/10 failed starts.

## Follow-ups

- Any other host-side caller that issues back-to-back archive requests on one container meets the same window.
