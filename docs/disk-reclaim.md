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

See the "verification" section of the change's report for the re-measured
matrix; the expected shape is the VZ rows unchanged (they already reclaimed)
and the HV System VM row following the VZ System VM row within the same
3-minute Btrfs cadence, with the manual compact returning everything at once
on every backend.
