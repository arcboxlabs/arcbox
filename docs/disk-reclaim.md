# Disk reclaim — how freed guest space returns to the host

Measured 2026-09-27 on Apple Silicon, macOS 26.4, boot assets 0.8.6
(kernel 6.18.38-arcbox), APFS host volume. Each cell is the host-side
physical allocation of the image (`stat -f %b` × 512; `du -k` agreed within
1 MiB) after writing 5 GiB of `/dev/urandom` into the guest filesystem and
deleting it. Scripts: `/dev/urandom` through `dd … conv=fsync`, then `rm`
and `sync`, sampling at fixed offsets.

## Before this change

| backend × guest | after write | rm +0 s | +15 s | +30 s | +60 s | +120 s | +180 s | manual `abctl disk compact` |
|---|---|---|---|---|---|---|---|---|
| VZ · System VM (`docker.img`) | 5370 MiB | 5370 | 5370 | 5371 | 1147 | 1147 | **251** | failed: guest has no `fstrim` binary |
| VZ · machine (`data.img`, alpine) | 5154 MiB | 5154 | **34** | 34 | 34 | 34 | 34 | failed: overlay root refuses `FITRIM` |
| HV · System VM (`docker.img`) | 5361 MiB | 5362 | 5362 | 5362 | 5362 | 5362 | **5362** | failed: `fstrim: Not supported` |
| HV · machine | not run: distro machines always boot on VZ (`VmBackend::default()`), so this cell is the VZ machine row | | | | | | | |

What the numbers mean:

- **VZ reclaims on its own.** Apple's virtio-blk advertises DISCARD
  (`discard_max_bytes` 2 TiB on the 8 TiB image) and turns it into a hole
  punch on the backing file; `stat` drops the moment the guest issues the
  discard. Btrfs `discard=async` is what issues it, and its cadence is the
  whole story of the VZ rows: a block group becomes eligible 120 s after its
  last free (`BTRFS_DISCARD_DELAY`, `fs/btrfs/discard.c`), discards are
  paced at 1000 iops with 64 MiB per discard (`max_discard_size`), and a
  block group that is emptied entirely is discarded after only 10 s
  (`BTRFS_DISCARD_UNUSED_DELAY`) — which is why the machine, whose 5 GiB
  file filled fresh block groups, was back to 34 MiB in 15 s, while the
  System VM's file landed in block groups that also hold live data and took
  3 minutes.
- **HV reclaimed nothing.** On the HV backend the System VM's data disk is
  not virtio-blk at all: the agent picks `/dev/arcboxhvc1`, the HVC
  hypercall fast path (`virt/arcbox-vmm/src/vmm/darwin_hv/hvc_blk.rs`, driver
  in `arcboxlabs/kernel`), which knew READ/WRITE/FLUSH/CAPACITY and nothing
  else. The guest saw `discard_max_bytes=0`, so `discard=async` never issued
  a discard and `fstrim` failed with `Not supported`. The virtio-blk worker's
  hole punch (`blk_worker.rs::process_discard`) was correct and unused: it
  served only `vdc` (the 2 GiB ext4 metadata disk), where a 512 MiB
  write + `rm` + `fstrim` did return the space (514 → 2 MiB).
- **`abctl disk compact` was broken on every backend.** The agent ran
  `fstrim`, which the EROFS rootfs does not ship (ENOENT on every hourly tick
  since `c443ed70`); a machine had no mount point to trim even if it had.

Host-side cost of a punch (APFS, `F_PUNCHHOLE`, measured with a C probe
against an 8 TiB sparse file): a range that is already a hole punches in
4–14 µs regardless of size; 5 GiB of live data punches in 57 ms in one call,
0.33 ms per 16 MiB chunk (320 calls, 104 ms total), 1.1 ms per 64 MiB chunk.
A full trim of a mostly empty 8 TiB image at 16 MiB granularity is ~2.4 s of
punches. `FITRIM` inside the guest on the 8 TiB Btrfs data volume returned
in under 1 ms once its free space had already been discarded.

## What changed

1. **HVC block DISCARD** (`ARCBOX_HVC_BLK_DISCARD`, 0xC2000005). The kernel
   driver routes `REQ_OP_DISCARD` to the host, which punches the 4 KiB-aligned
   interior of the range out of the image, exactly as the virtio worker does.
   The driver probes with a zero-length discard at bind time; an older host
   answers the SMCCC "not supported" code and the device stays as before.
   Limits advertised: 1 GiB per request, 4 KiB granularity. This alone makes
   `discard=async` work on HV.
2. **`FITRIM` in the agent instead of an `fstrim` binary.** The System VM
   trims the Btrfs data volume (one call covers every subvolume) and the ext4
   metadata volume; a distro machine mounts its data device again inside a
   private mount namespace (its only mount is the overlay root, which refuses
   `FITRIM`) and trims that. `DiskTrimResponse` now carries `bytes_trimmed`.
3. **Host-driven scheduling** (`app/arcbox-daemon/src/disk_reclaim.rs`). The
   System VM is trimmed each time the lifecycle marks it idle — five minutes
   without Docker traffic — at most every 15 minutes; every running distro
   machine is trimmed hourly, starting a minute after the daemon starts. The
   agent's own hourly loop is gone: a machine's agent serves RPC and nothing
   else, and the System VM trim runs when nothing competes with it. `abctl
   disk compact [machine]` is the on-demand path.

`discard=async` remains the first line: it returns fully freed extents within
minutes without anyone asking. The trim exists for what it never returns —
free space inside a block group that is still partly used, and the queue a
disk drops on unmount.

## After this change

Same procedure, same host, 2026-09-27. The HV row ran the CI-built kernel
from arcboxlabs/kernel PR #20 (merged as `2c041107`; passed with `--kernel`
because boot assets 0.8.6 predate it); the daemon was the rebuilt one.

| backend × guest | after write | rm +0 s | +15 s | +30 s | +60 s | +120 s | +180 s | manual `abctl disk compact` |
|---|---|---|---|---|---|---|---|---|
| VZ · System VM | 5367 MiB | 5367 | 5367 | 5369 | 1145 | 1145 | **249** | 230 |
| VZ · machine (alpine) | 5140 MiB | 5141 | **21** | 21 | 21 | 21 | 21 | 0 |
| HV · System VM | 5356 MiB | 5357 | **1133** | 1133 | 1133 | 237 | **219** | 219 |

The HV System VM now follows the VZ System VM's Btrfs cadence (the first
drop even lands a step earlier, because the HVC path has no per-request
size cap the guest has to split against). The manual compact works on every
backend and returns whatever `discard=async` had not yet: `abctl disk
compact` on the VZ System VM went 249 → 230 MiB, and `abctl disk compact m1`
took the machine from 21 MiB to 0. The trim mount a machine uses is gone
afterwards (`/run/arcbox/trim` is an empty directory in the machine's
namespace; nothing is mounted on it).

The scheduler was observed end to end with `ARCBOX_IDLE_TIMEOUT_SECS=60`:
`VM entered idle state after 64s of inactivity`, then one second later
`trimmed the idle System VM's disks bytes_trimmed=8795751194624` from the
daemon and `trimmed mount="/run/arcbox/data"` / `"/run/arcbox/metadata"`
from the agent. The machine sweep fired at the one-minute mark
(`trimmed the machine's data disk machine=m1 bytes_trimmed=20936884224`).

### I/O while a trim runs

Inside an alpine container on the System VM: 1 GiB sequential write with
fsync, then 256 MiB of 64 KiB random-data rewrites with fsync, both `dd`
(no fio in the image and no network in the dev guests). Two baseline runs,
then the same with six back-to-back `abctl disk compact` calls overlapping
the run — far more trimming than the scheduler ever issues.

| backend | run | 1 GiB seq write | 256 MiB rewrite |
|---|---|---|---|
| VZ | baseline | 3.4 GB/s, 1.9 GB/s | 485 MB/s, 483 MB/s |
| VZ | six trims overlapping | 3.6 GB/s, 2.9 GB/s | 351 MB/s, 335 MB/s |
| VZ | one trim overlapping (the scheduler's shape) | 2.8, 3.2, 3.2 GB/s | 370, 430, 157 MB/s |
| HV | baseline | 1.8 GB/s, 1.7 GB/s | 120 MB/s, 144 MB/s |
| HV | eight trims overlapping | 1.7 GB/s, 1.8 GB/s | 135 MB/s, 113 MB/s |

A full trim of the 8 TiB data volume takes 85–150 ms wall clock end to end
(`abctl disk compact`, including the RPC). The rewrite pass on VZ loses ~25%
while six trims are stacked on it and is within run-to-run noise otherwise;
the one 157 MB/s sample is a single fsync stall of the kind the baseline also
shows between runs. On HV, where the trim rides the same HVC path as the
I/O, eight stacked trims move neither number outside the baseline spread.
The sequential write is unaffected on both. Nothing here
approaches "noticeably slower", and the scheduler runs the trim only when
the VM has been idle for five minutes.
