# 2026-10-03 — Can a machine's root be served to the Mac by its agent, read-write, fast enough to work in?

- Type: experiment
- Area: `guest/arcbox-agent/src/machine_export/`, `app/arcbox-daemon/src/machine_mount/`
- Outcome: ADR 0002; the `feat/machine-file-sharing` series
- Probes: `tests/e2e/tests/machine.rs` (`machine_root_mounted_on_the_host`) for the round trip; the throughput commands below are shell one-liners against a dev daemon, recorded here with their exact form
- Host / guest: Apple M5 Max (18 cores, 128 GiB), macOS 26.4 (25E246), VZ backend, boot bundle 0.8.8; daemon and agent built from this branch in release mode; machine images from the local distro mirror

## Question

OrbStack mounts every machine's filesystem under `~/OrbStack/<machine>`.
Can ArcBox do the same without changing boot-assets, with a userspace
NFSv3 server inside the machine's agent, reached over the machine's bridge
NIC — and is that path usable for a developer workflow: reading and
writing files, `ls` of a large directory, `git status`, and a 1 GiB file
in each direction?

## Hypotheses

1. The macOS NFS client mounts `nfs3_server` with `vers=3,tcp,port=P,mountport=P`
   over the vmnet bridge, and reads `/etc/os-release` of alpine, ubuntu and
   fedora machines through it.
2. A file written on either side is visible on the other immediately
   (within the attribute cache window on the Mac).
3. Bulk throughput through the userspace server and its loopback relay is
   within the same order as the `~/ArcBox` NFSv4 export (kernel nfsd over a
   vsock relay), and `ls`/`git status` complete in well under a second.
4. Larger `rsize`/`wsize` than the macOS default (32 KiB) raises bulk
   throughput measurably.

## Method

Isolated VZ dev daemon from this worktree (`dev-daemon.sh`, machine images
from the local mirror). For each of `alpine 3.24`, `ubuntu noble`,
`fedora 44`: `abctl machine create` + `start`, then on the Mac, with
`M=$ARCBOX_MACHINE_MOUNT_DIR/<name>`:

```sh
cat $M/etc/os-release | head -2
echo from-host > $M/root/from-host; abctl machine exec <name> -- cat /root/from-host
abctl machine exec <name> -- sh -c 'echo from-machine > /root/from-machine'; cat $M/root/from-machine
abctl machine exec <name> -- sh -c 'mkdir -p /root/many && cd /root/many && touch $(seq 1 1000)'
time ls $M/root/many | wc -l
time dd if=/dev/zero of=$M/root/1g bs=1m count=1024          # write
abctl machine exec <name> -- sh -c 'echo 3 > /proc/sys/vm/drop_caches'
time dd if=$M/root/1g of=/dev/null bs=1m                     # read
```

Git through the mount: this repository (1 396 tracked files) cloned from
the Mac with `time git clone <worktree> $M/root/repo`, then
`time git -C $M/root/repo status` twice (cold, warm), `git fsck`, and
inside the machine `find /root/repo -name '._*'` and `git status`.
Extended attributes: `xattr -w user.note "from the mac" $M/root/x`, read
back with `xattr -p` on the Mac, and `ls -a /root` inside the machine.
Admission: from a second machine, a TCP connection to the first machine's
export port. Then `abctl machine stop` and `abctl machine remove --force`,
checking `mount` and `pgrep -fl mount_nfs` after each, and a daemon stop
(`dev-daemon-stop.sh`) checking that the process exits.

## Results

| Machine | `machine start` (wall) | `ls` 1 000 entries, cold / warm | 1 GiB write | 1 GiB read (guest caches dropped) |
|---|---|---|---|---|
| alpine 3.24 | 3.1 s | 16 ms / 5 ms | 4.39 s (245 MB/s) | 3.22 s (333 MB/s) |
| ubuntu noble | 7.0 s | 21 ms / 8 ms | 6.20 s (173 MB/s) | 4.37 s (246 MB/s) |
| fedora 44 | 4.3 s | 16 ms / 10 ms | 4.28 s (251 MB/s) | 3.68 s (292 MB/s) |

- `/etc/os-release` read through the mount on all three; a file written on
  the Mac read back by `machine exec` and the reverse, on all three. The
  mount followed the machine's readiness event by 25–50 ms (daemon log).
  The guest saw the full 1 GiB (`stat -c %s`).
- Files created from the Mac are `root:root` in the machine; `chmod`,
  `ln -s`, `mv` and `rmdir` from the Mac behave; `/arcbox` is neither
  listed nor found.
- Git through the mount (ubuntu, with a cargo build running on the host):
  the clone of this repository took 15.9 s; `git status` on it 0.32 s
  cold and 0.08 s warm, with no errors. `git fsck` printed the same two
  commit-graph messages a local clone of the same worktree prints, so
  they are the source's, not the mount's.
- Sidecars: `xattr -w` from the Mac succeeded, read back, and survived a
  `mv` of the file on the Mac; neither `ls -a` on the Mac nor in the
  machine showed a `._x`, and `find / -xdev -name '._*'` in the machine
  found nothing after the `dd` and the clone, both of whose files carry
  `com.apple.provenance` on the Mac.
- `tests/e2e/tests/machine.rs` passes in 31 s without `KEEP_TEST_DIR`:
  the mount appears, both directions read back, the mount point is gone
  after `stop`, and the daemon exits on SIGTERM inside the harness's 15 s
  grace, so the temp dir is removed.
- Admission: a connection from the alpine machine to the ubuntu machine's
  export port was refused; the ubuntu agent logged
  `refused a peer that is not the host peer=192.168.64.3:36627`.
- `machine stop` (graceful) unmounted at the `MachineStopping` edge and
  returned in 3.5 s; `machine remove --force` left the unmount to the
  fallback path (`umount`, then `umount -f`), about 15 s. After both:
  no `mount_nfs` process, mount directory removed. The daemon stopped in
  8 s with `ArcBox daemon stopped` and no mount left under its data
  directory.

## Findings

- H1, H2 confirmed. H3: 173–251 MB/s writing and 246–333 MB/s reading
  through the userspace server and its loopback relay; `ls` and
  `git status` are far under a second. The `~/ArcBox` NFSv4 export was not
  re-measured side by side in this run. H4 not measured: the mount uses the
  client's default `rsize`/`wsize`.
- The macOS NFS client cannot store extended attributes on NFSv3, so it
  writes `._<name>` AppleDouble siblings — and recent macOS stamps
  `com.apple.provenance` on every file a downloaded app's process creates,
  so `dd` left `._1g` and a clone left `._pack-*.idx` in the machine, which
  git then read as a pack index (`non-monotonic index`). The agent now
  keeps `._` files the Mac creates in memory and out of directory listings
  (`vfs/sidecar.rs`). In memory but listed was not enough: git on the Mac
  reads any `._pack-*.idx` it lists as a pack index and the clone still
  failed. Unlisted, the clone is clean on both sides; see the sidecar line
  in Results.
- A forced remove kills the VM before the unmount on the stopping edge
  finishes, so the mount goes through the slow path. Making `remove` wait
  for the unmount the way a graceful `stop` effectively does is an engine
  change, left open.
- The e2e harness exposed a shutdown hang behind the `~/ArcBox` fix: a
  fresh mount under `/var/folders` answered `umount` with "Resource busy",
  and after the VM stopped the second cleanup pass `stat`'d the mount
  point through `canonicalize`, which a dead NFS server never answers.
  `current_mount_info` now resolves a mount point without `stat`'ing it,
  and the shutdown unmount escalates to `umount -f`.

## Decisions taken / open

- ADR 0002: userspace NFSv3 in the agent over the bridge NIC, with the
  relay as the admission control.
- Open: the mount point's name once `~/ArcBox` is reorganized; a `LINK`
  implementation; evicting ids the Mac has not touched for a long time;
  `remove --force` waiting for the unmount; Finder's `.DS_Store` files,
  which are ordinary files and land in the machine as on any network
  mount.
