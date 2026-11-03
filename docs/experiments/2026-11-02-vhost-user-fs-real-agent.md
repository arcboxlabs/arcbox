# 2026-11-02 — Does the real System VM agent run on vhost-user-fs under QEMU TCG?

- Type: experiment
- Area: `virt/arcbox-vhost-user` (`fs`); input to the QEMU TCG backend plan (item 1c, task H)
- Outcome: yes, with one QEMU gap. The agent, its three shares and the runtime work; SYNCFS stays ENOSYS; one request queue stays. QEMU TCG runs stale code from pages the back-end writes, which needs the arcboxlabs/qemu fix
- Probes: `virt/arcbox-vhost-user/examples/qemu_system_vm_probe/` (real QEMU, real agent and runtime)
- Host / guest: Amp Mac satellite, Apple M4 Pro (Virtual), 6 vCPU, 14 GB,
  macOS 26.3.1 (25D2128), otherwise idle. QEMU 10.2.4 from arcboxlabs/qemu
  CI (`qemu-darwin-arm64-v0.0.0-dev.0169f2b`), `-accel tcg,thread=multi
  -cpu max,pauth-impdef=on -smp 4`, 4 GiB on `memory-backend-shm,share=on`.
  Guest: unmodified boot assets 0.8.12 arm64, kernel `6.18.38-arcbox`
  (sha256 `a122ce7a…02c4`), `rootfs.erofs` (sha256 `ceb81067…fa36`); runtime
  0.8.12 from its manifest; `arcbox-agent` 0.10.5 built from this tree.
  Release build, Rust 1.96.0.

## Question

The [first vhost-user experiment](2026-11-02-vhost-user-on-macos.md) booted
the System VM with a probe script in place of the agent. Does the real
agent come up on shares served by `arcbox_vhost_user::fs`: does it survive
its `dax=always` remount of `/arcbox`, mount `users` and `private`, and
materialize the runtime from `/arcbox/runtime` onto Btrfs? Does the
FUSE_SYNCFS that `FsServer` answers with ENOSYS reach anything in the
guest? Is one worker per share fast enough for runtime materialization and
for many small files under `/Users`?

## Hypotheses

1. The agent tolerates a failed `dax=always` remount (the remount is
   already non-fatal in `guest/arcbox-agent/src/mount.rs`).
2. `sync` and `syncfs` succeed in the guest despite ENOSYS, because the
   kernel treats ENOSYS for FUSE_SYNCFS as "not supported" and stops asking.
3. The back-end worker is not the bottleneck under TCG, so one request
   queue per share is enough.

## Method

`qemu_system_vm_probe` stages the data directory the way the daemon does
(boot manifest, runtime, `arcbox_storage::prepare_pair` disks), attaches the
rootfs and both disks as vda/vdb/vdc, the three shares through
`FsShare::spawn`, and a config-only vhost-user-vsock, and boots with the
daemon's cmdline. rcS and inittab run `guest/wrapper.sh` at the agent's
path; it runs the agent from a devtmpfs copy (finding 5) and starts
`guest/driver.sh`, which talks to the agent over vsock loopback with
`guest/agent_rpc.rs`: Ping, then EnsureRuntime polled until ready while
timing the staging directory under `/run/arcbox/data/runtime`, a cold read
of the runtime through `/arcbox`, `sync` and `sync -f` on each share, a tar
of 4000 4 KiB files under `/Users` with one reader and of the same tree
under `/private` with four. `guest/exec_coherence.rs` checks whether code
the back-end writes runs as written. Four runs of the probe (10–13), three
of it as committed after review (14–16), and three control runs with the
agent executed from the share.

## Results

| Run | Materialize (staging → publish) | EnsureRuntime until ready | Runtime cold read (237 MiB) | `/Users` tar, 1 reader | `/private` tar, 4 readers | Wall |
|---|---|---|---|---|---|---|
| 10 | 3.1 s | 6.67 s | 0.30 s (790 MiB/s) | 4.93 s | 2.65 s | 17.2 s |
| 11 | 3.2 s | 6.45 s | 0.38 s (624 MiB/s) | 5.07 s | 2.70 s | 17.4 s |
| 12 | 3.2 s | 6.29 s | 0.30 s (790 MiB/s) | 4.98 s | 2.65 s | 16.9 s |
| 13 | 3.1 s | 6.64 s | 0.28 s (847 MiB/s) | 5.14 s | 2.67 s | 17.6 s |
| 14 | 3.2 s | 7.59 s | 0.54 s (439 MiB/s) | 6.77 s | 3.26 s | 21.1 s |
| 15 | 3.4 s | 7.69 s | 0.30 s (790 MiB/s) | 5.98 s | 3.10 s | 20.1 s |
| 16 | 4.2 s | 9.22 s | 0.34 s (697 MiB/s) | 5.82 s | 3.00 s | 21.7 s |

Runs 14–16 are the probe as committed, rebased onto the merged vhost-user
framework, on a new satellite of the same type. The back-end refuses
requests longer than a full FUSE_WRITE, and each tar now also lists its
archive in the guest to count the files read and checks tar's exit status
(`tarred` in `guest/driver.sh`). That listing is guest CPU under TCG and
is part of the tar times. Every run read 4000 of 4000 files with tar
exit status 0. Worker busy time was 0.57–0.65 s for `users` and
0.51–0.58 s for `private` (run 12: 0.48 s and 0.66 s), and execution from
the share ran stale code in 3, 1 and 2 of 16 rounds.

Back-end load per share (run 12; runs 10, 11 and 13 are within 0.02 s):

| Share | Requests | Worker busy | Deepest batch | Load it served |
|---|---|---|---|---|
| `arcbox` | 5100 | 0.17 s | 2 | boot, materialization, cold read |
| `users` | 24519 | 0.48 s | 8 | the 1-reader tar (5.0 s) |
| `private` | 20434 | 0.66 s | 6 | the 4-reader tar (2.65 s) |

Every run: `sync rc=0`, `syncfs rc=0` on `/arcbox`, `/Users`, `/private`
and `/Users` again, and exactly one FUSE_SYNCFS per share in the back-end
log. The agent logged `remounted with dax=always` for `/arcbox` and went
on to serve; `/proc/mounts` shows `arcbox /arcbox virtiofs rw,relatime`.
dockerd logged `Daemon has completed initialization`.

Execution coherence (run 13): `buffered=2 direct=1 share-stale=5/16`.
Agent executed from the share (controls): 2 of 3 runs never answered
Ping (the serving agent wrote nothing after `init`; killed at 150 s), 1
passed. From the devtmpfs copy: 4 of 4 passed.

## Findings

1. The `dax=always` remount is a silent no-op on every backend.
   `virtio_fs_context_ops` has no `reconfigure` (6.12 and 6.18), so
   `mount(MS_REMOUNT, "dax=always")` parses the option and returns 0
   without touching the superblock; a fresh `dax=always` mount on a device
   without a cache window fails instead (`virtio_fs_fill_super`). The agent
   continues either way. Hypothesis 1 holds, and `mount.rs` needs no
   change. On HV, `/arcbox` therefore never uses its DAX window either;
   only the `arcbox-dax` test fixture mounts with DAX.
2. SYNCFS needs nothing. The kernel turns ENOSYS into success and stops
   sending it, so `sync` and `syncfs` return 0, and the second `/Users`
   `syncfs` sent no request. containerd and dockerd keep their state on
   Btrfs and ext4, not virtiofs. What ENOSYS costs is that a guest `sync`
   does not flush the host's copy of share data; FUSE_FSYNC on a file
   does. macOS has no per-filesystem sync, so implementing SYNCFS would
   mean a host-wide `sync(2)`. Left as is, so HV is unchanged.
   Hypothesis 2 holds.
3. One request queue per share is enough. Under the 1-reader tar the
   worker is busy about 10% of the time (0.48 s of 5.0 s, about 20 µs per
   request against about 200 µs per request end to end), under the
   4-reader tar about 25% (0.66 s of 2.65 s). Four readers ran 1.9 times
   faster than one with the same single worker. The time goes to the
   emulated guest, not the back-end. Hypothesis 3 holds. HV serves FUSE
   with one queue too.
4. Runtime materialization from `/arcbox/runtime` to Btrfs takes 3.1–4.2 s
   for 10 assets (237 MiB), and EnsureRuntime reaches ready, with dockerd
   answering `/_ping`, 6.3–9.2 s after it is first called.
5. QEMU TCG keeps executing its translation of a guest page after a
   vhost-user back-end rewrites that page. QEMU drops translations when
   the guest CPU or one of its own devices writes a page, but the
   back-end writes shared memory directly, and TCG's IC IVAU is a no-op
   while `-cpu max` reports CTR_EL0.DIC=1, so even the maintenance the
   architecture asks for does not help (`direct=1`). Executing a file
   mapped from the share ran stale code in 5 of 16 rounds. That is why
   the agent hung when executed from the share. The file data is correct
   (sha256 matched in earlier runs); the fix belongs in arcboxlabs/qemu,
   and the probe keeps a check that fails until it lands.

## Decisions taken / open

- Commits: the probe commits under `virt/arcbox-vhost-user/examples/`,
  and `fix(fs): read guest open flags as Linux flags`, which this probe
  found ([log](../logs/2026-11-02-fuse-open-flags.md)).
- Closed 2026-11-03: rerun on arcboxlabs/qemu `aa1c259` with `-cpu
  max,pauth-impdef=on,dic=off`, `guest/wrapper.sh` now running the agent
  and helpers straight from the share. Every check passed, exec coherence
  included (`buffered=2 direct=2 share-stale=0/16`); EnsureRuntime until
  ready 7.01 s, QEMU wall time 18.5 s.
- Open: HV's DAX window for `/arcbox` is never used (finding 1).
