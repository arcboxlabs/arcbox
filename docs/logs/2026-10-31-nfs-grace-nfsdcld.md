# 2026-10-31 — nfsdcld lets nfsd skip the NFSv4 grace period

- Type: change
- Area: guest agent NFS export (`guest/arcbox-agent/src/nfs.rs`), boot-assets rootfs
- Commits / PRs: boot-assets `7eff5073` (`feat(rootfs): ship nfsdcld …`), released as [v0.8.12](https://github.com/arcboxlabs/boot-assets/releases/tag/v0.8.12); this change
- Related: [boot assets 0.8.10](2026-10-21-boot-0.8.10-private-mountpoint.md) (same release flow)

## Problem or trigger

The first open under `~/ArcBox` after daemon start waited 6–12 s. e2e
`boot_assets` reported `nfs_export` at 8, 10 or 12 s (HV usually 10, VZ
always 12), on every commit tested.

The agent capped the grace period at the 10 s minimum and wrote `Y` to
`/proc/fs/nfsd/v4_end_grace` after starting nfsd. On kernel 6.18 that write
returns `EBUSY` unless a client tracker is active, and the guest ran none.
Boot dmesg showed `NFSD: Unable to initialize client recovery tracking! (-2)`
and `NFSD: starting 10-second grace period`. The agent logged the `EBUSY` at
debug level only. During grace nfsd answers OPEN with `NFS4ERR_GRACE`, and the
macOS client retries every 2 s, so the open returned on the first retry after
grace ended.

## What was done

- boot-assets adds `nfsdcld` to the staged `nfs-utils` binaries. The rootfs
  then ships `/sbin/nfsdcld` with `libsqlite3` and `libevent_core`. The arm64
  rootfs grows from 7.1 MB to 8.4 MB.
- The agent mounts `rpc_pipefs` at `/var/lib/nfs/rpc_pipefs`. It starts
  `nfsdcld -F` with its database on the `/var` tmpfs, then starts nfsd. On a
  rootfs without `/sbin/nfsdcld` it logs a warning and keeps the old behavior.
- The agent waits until nfsdcld holds its inotify descriptor before it writes
  the thread count. A spawned nfsdcld alone is not enough. On a fresh database
  (every boot), nfsdcld's one-time upgrade reads
  `/proc/fs/nfsd/nfsv4recoverydir`, which takes `nfsd_mutex`. The threads
  write holds that lock while nfsd waits 1 s for nfsdcld to open the `cld`
  pipe. When both start at once, nfsdcld blocks behind nfsd, nfsd times out,
  and dmesg shows the same `-2`. The inotify watch is set up after the upgrade
  (`cld_pipe_init` follows `sqlite_prepare_dbh` in nfs-utils 2.6.4).
- With the tracker up and no clients in its database, nfsd logs
  `no clients to reclaim, skipping NFSv4 grace period`. The `v4_end_grace`
  write stays as a backstop. It is skipped when grace has already ended,
  because the kernel also returns `EBUSY` then. A failed write now logs a
  warning.

## Evidence

Measured on an M-series Mac with a manual daemon from this change. Each boot
used a fresh data dir and the agent from this change. The probe timed
`open()` + `read()` of `ArcBox/docker/engine-id` from daemon spawn. It first
waited until `stat` saw the file, because GETATTR is allowed during grace and
OPEN is not.

| Rootfs | Backend | Boots | Open stall after the file is visible |
|---|---|---|---|
| 0.8.11 (no nfsdcld) | HV | 4 | 10.01, 10.01, 10.01, 12.01 s |
| 0.8.11 (no nfsdcld) | VZ | 3 | 12.02, 12.02, 12.02 s |
| 0.8.11 + nfsdcld | HV | 5 | 0.00 s each |
| 0.8.11 + nfsdcld | VZ | 4 | 0.00 s each |

Every nfsdcld boot logged `NFSD: Using nfsdcld client tracking operations.`
and `no clients to reclaim, skipping NFSv4 grace period`. On HV the guest
console read `v4_end_grace` as `Y`. Before the readiness wait was added, the
first nfsdcld boot still stalled 12 s with `-2`. Starting nfsdcld and nfsd
together from the guest console on a fresh database reproduced that failure
twice. Waiting for the inotify descriptor first (about 20 ms) made the tracker
initialize every time.

e2e `boot_assets` with `boot-assets/dev` holding the rebuilt rootfs and the
`assets.lock` manifest pin removed locally:

| Backend | Runs | `nfs_export` |
|---|---|---|
| HV | 2 | 0.0017, 0.0016 s |
| VZ | 2 | 0.0022, 0.0023 s |

Rerun: build the rootfs with `boot-assets build rootfs --arch arm64`, place it
and a manifest with its digest in `boot-assets/dev`, remove the
`manifest_sha256` pin, and run
`cargo xtask e2e --test boot_assets --backend <hv|vz>`.

### Release 0.8.12

[Release run 41182589898](https://github.com/arcboxlabs/boot-assets/actions/runs/41182589898)
published v0.8.12 from `7eff5073de5738572f0f418cb767fbd475786d86`. The tag
build resolves the kernel tag `latest`, which was v0.0.25, the kernel 0.8.11
ships. Both kernel digests are unchanged, and the manifest records the kernel
version as `latest` instead of `v0.0.25`; only `abctl boot status` displays
that field. The only binary that changed is the per-release FEX artifact.
`assets.lock` pins `[boot]` 0.8.12.

| Input | SHA-256 |
|---|---|
| Merged manifest, pinned by `assets.lock` | `4e1638ce4d9f5fcb4669420bb41481297807bf7f069ea264e54c1e258abc409b` |
| ARM64 rootfs | `ceb8106753ab22f5a772f03bd1f541bf5e6edb47e36b4a4b42cae56e4b02fa36` |
| x86_64 rootfs | `b6c81dc7906690e79a9135bc5b3e17f3ff03055e6488e1195939c998d6afe7bb` |
| ARM64 kernel | `a122ce7a69a77408eeef9423afd9f6915828ba74a767e3e1eace635f913e02c4` |

The CDN and GitHub release manifests are byte-identical. Both published
rootfs images contain `nfsdcld` and `libsqlite3`.

Fresh daemons built from this change downloaded 0.8.12 from the CDN
into data dirs with no boot assets: 3 HV and 3 VZ boots. Every boot logged
`no clients to reclaim, skipping NFSv4 grace period`, and the first open of
`ArcBox/docker/engine-id` returned as soon as the file was visible. e2e
`boot_assets` on the published assets passed once per backend, with
`nfs_export` at 0.0015 s on HV and 0.0025 s on VZ.

## Follow-ups

- The x86_64 rootfs was built by the release but not booted.
