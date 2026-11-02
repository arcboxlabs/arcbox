# 2026-11-02 — Can the daemon serve the System VM's virtio-fs and vsock to QEMU TCG over vhost-user on macOS?

- Type: experiment
- Area: `virt/arcbox-vhost-user` (new); input to the QEMU TCG backend plan (item 1b)
- Outcome: works. [ADR 0010](../adr/0010-vhost-user-protocol-layer-is-arcbox-own.md) records the protocol-layer choice. The 9p + virtio-serial fallback is not needed
- Probes: `virt/arcbox-vhost-user/examples/qemu_fs_probe.rs` (real QEMU), `virt/arcbox-vhost-user/tests/frontend.rs` (QEMU-style fake front-end)
- Host / guest: Amp Mac satellite, Apple M4 Pro (Virtual), 6 vCPU, 14 GB,
  macOS 26.3.1 (25D2128), otherwise idle. QEMU 10.2.1 built from source
  (below), `-accel tcg,thread=multi -cpu max,pauth-impdef=on -smp 4`, 2 GiB
  of guest RAM on `memory-backend-shm,share=on`. Guest: the unmodified
  System VM from boot assets 0.8.12 arm64 — kernel `6.18.38-arcbox`
  (sha256 `a122ce7a…02c4`, VSOCK, VIRTIO_FS and VIRTIO_PCI built in) and
  `rootfs.erofs` (sha256 `ceb81067…fa36`). Release build, Rust 1.96.0.

## Question

On a Mac without hardware virtualization, the QEMU TCG backend has to give
the System VM the same `arcbox`, `users` and `private` virtio-fs tags and
the same host-dialled vsock ports as HV and VZ, without changing the guest.
The plan serves both from the daemon over vhost-user. Unknowns: does QEMU on
macOS run `vhost-user-fs-pci` and `vhost-user-vsock-pci` at all, what does
it pass for notifiers where there is no eventfd, can a back-end map guest
RAM and run `arcbox-virtio-core`'s `SplitQueue` on it, and should the
protocol layer be rust-vmm's `vhost` crate or our own. A "no" on any of the
first three starts the 9p + virtio-serial fallback.

## Hypotheses

1. QEMU 10.2.1 on macOS realizes both vhost-user devices when guest RAM is
   `memory-backend-shm,share=on`, and a back-end can `mmap` the descriptors
   `SET_MEM_TABLE` passes.
2. Without eventfd, QEMU passes pipe ends: the read end for
   `SET_VRING_KICK`, the write end for `SET_VRING_CALL`.
3. rust-vmm `vhost` (`vhost-user-backend` feature) and `vhost-user-backend`
   can be made to work on macOS with a small patch.
4. `SplitQueue` runs unchanged on the mapped guest RAM, and an
   `arcbox_fs::FsServer` behind it serves the unmodified rootfs's `arcbox`
   share correctly, including across a guest reboot.

## Method

QEMU, built on the satellite:

```text
brew install meson ninja libslirp
./configure --target-list=aarch64-softmmu --enable-vhost-user --disable-vhost-net \
  --enable-slirp --disable-docs --disable-cocoa --disable-sdl --disable-gtk \
  --disable-vnc --disable-curses --disable-gnutls --disable-libssh --disable-png \
  --disable-capstone --disable-libusb --disable-tools
make -j6
```

`--enable-vhost-user` alone fails on `linux/vhost_types.h`; vhost-net is
the part that needs it.

Notifier behaviour was read from the QEMU 10.2.1 source
(`util/event_notifier-posix.c`, `hw/virtio/vhost-user.c`,
`system/memory.c`, `hw/virtio/vhost-vsock-common.c`) and then checked by
what the back-end received.

rust-vmm: a clone of `rust-vmm/vhost` main at `ed821c2` (2026-10-26), built
with `cargo build -p vhost --features vhost-user-backend` and its lib tests
run, before and after a patch.

The probe (`qemu_fs_probe`) builds a share directory, starts two back-ends
on socketpairs, and spawns QEMU with `-chardev socket,fd=N` for each:
`vhost-user-fs-pci,tag=arcbox,queue-size=1024` served by `VirtioFs` +
`FsServer` (the HV backend's session and handler) through
`Vring::process_avail`, and `vhost-user-vsock-pci` served by a device that
has only a config space (CID 3). The rootfs's rcS mounts the `arcbox` tag
and runs `/arcbox/bin/arcbox-agent init`; the share holds a shell script
there. On the first boot the script reboots the guest, so QEMU stops and
restarts both devices on the same connections. On the second boot it
reports the mount, the bound vsock driver and a small file, reads a large
random file after `drop_caches` and times it with `/proc/uptime`, checks
its sha256, `cat`s and `ls -l`s a directory of 1 KiB files, writes a file
back, and powers off. The probe checks every report against the host side.

```text
cargo build --release -p arcbox-vhost-user --example qemu_fs_probe
RUST_LOG=warn ./target/release/examples/qemu_fs_probe \
  --qemu qemu-system-aarch64 --bios-dir <qemu>/pc-bios \
  --kernel kernel --rootfs rootfs.erofs --big-mib 256 --small-files 2000
```

## Results

Five consecutive runs of the final code, 256 MiB file, 2000 small files.
Wall time covers QEMU start to exit: two boots, the read, a guest-side
sha256 of 256 MiB under TCG (about 3.5 s of it), and the small-file steps.

| Run | Checks | Wall time | FUSE requests | 256 MiB read | `cat` 2000 files | `ls -l` 2000 files |
|---|---|---|---|---|---|---|
| 1 | 9/9 ok | 10.8 s | 16272 | 656 MiB/s | 673 files/s | 3030 files/s |
| 2 | 9/9 ok | 10.6 s | 16272 | 640 MiB/s | 707 files/s | 3509 files/s |
| 3 | 9/9 ok | 10.2 s | 16271 | 640 MiB/s | 784 files/s | 4167 files/s |
| 4 | 9/9 ok | 9.6 s | 16269 | 776 MiB/s | 893 files/s | 3226 files/s |
| 5 | 9/9 ok | 9.0 s | 16269 | 826 MiB/s | 966 files/s | 4000 files/s |

`/proc/uptime` has 10 ms resolution, so a 0.31–0.40 s read is ±3%. Before
these, every run after the `RESET_DEVICE` fix (finding 4) passed: 15 at
64 MiB and 3 at 256 MiB. The guest's report from run 1:

```text
VHOST-PROBE agent-exec args=init
VHOST-PROBE rebooting
VHOST-PROBE agent-exec args=init
VHOST-PROBE mount=arcbox /arcbox virtiofs rw,relatime 0 0
VHOST-PROBE vsock=virtio3
VHOST-PROBE hello=hello from the host
VHOST-PROBE sha256=c1670b9f9bf0f5da4cb22aea381e3b7ef7289e1405bd1e8cf61e64ab4f9bcb1e
VHOST-PROBE small-read bytes=2048000 start=5.00 end=7.97
VHOST-PROBE small-stat entries=2001 start=7.98 end=8.64
VHOST-PROBE wrote=0
```

rust-vmm at `ed821c2`:

| Build | Result |
|---|---|
| `vhost`, `vhost-user-backend` feature, unpatched | 2 errors: `vmm_sys_util::eventfd` does not exist off Linux (`vhost/src/backend.rs`), `libc::SO_DOMAIN` does not exist on macOS (`backend_req_handler.rs`) |
| same, 14-line patch (`EventNotifier` alias, `getsockname` for the domain) | lib builds; lib tests do not compile (`EventNotifier::new` does not exist; the tests still construct eventfds) |
| `vhost-user-backend` | event loop is `vmm_sys_util::epoll`; queues are `virtio-queue` over `vm-memory`. #308 (merged 2025-08) moved its notifiers to `EventNotifier`/`EventConsumer`; the epoll → mio port (#362) is not on main |

## Findings

1. **vhost-user works on macOS.** QEMU realizes both devices and sends
   the guest RAM in `SET_MEM_TABLE` with the POSIX shared-memory
   descriptor of `memory-backend-shm`; `mmap(MAP_SHARED)` of it shows guest
   RAM. The unmodified rootfs boots, mounts `arcbox` as virtiofs and runs
   the agent path from it. Hypotheses 1 and 4 hold.
2. **Notifiers are pipes; hypothesis 2 holds, with a third case.** Without
   eventfd QEMU falls back to non-blocking pipes. `SET_VRING_KICK` gets the
   read end and the guest's kick writes 8 bytes; under TCG the ioeventfd is
   emulated (`memory_region_dispatch_write_eventfds` when `!kvm_enabled()`).
   `SET_VRING_CALL` gets the write end (`event_notifier_get_wfd`); QEMU
   drains it and injects the MSI-X interrupt. `SET_VRING_ERR` gets the read
   end, which the back-end cannot write, so error reporting is not
   available on macOS.
3. **QEMU replaces call descriptors while a ring runs.** It sends a new
   `SET_VRING_CALL` whenever the guest masks or unmasks the queue's MSI-X
   vector (`vhost_virtqueue_mask`).
   An interrupt written to a replaced pipe is lost, so a device that keeps
   a copy of the call descriptor (the vsock RX path does) must read the
   current one at every signal. `CallNotifier` is a shared slot for that.
4. **A reset must not drop negotiated protocol features.** On a guest
   reboot QEMU sends `RESET_DEVICE`, then `SET_VRING_CALL` with
   `NEED_REPLY` (`vhost_set_vring_file` under `REPLY_ACK`), and waits for
   the ack holding the BQL. Protocol features are negotiated once per
   connection, at `vhost_dev_init`. A back-end that cleared them on reset
   did not ack, and the guest hung on its first reboot. `tests/frontend.rs`
   replays that sequence.
5. **XNU rewrites a knote's `udata` on every `kevent` change**, including
   an `EVFILT_USER` `NOTE_TRIGGER`. A trigger must repeat the `udata` it
   registered with, or the next wakeup arrives with a null token.
6. **One mapping can carry `SplitQueue`.** `GuestMemWriter` takes one base
   pointer and a GPA base, so the back-end reserves the span from the
   lowest to the highest region and maps each region into it with
   `MAP_FIXED | MAP_SHARED`. QEMU resends an identical table on every
   device start; the back-end then keeps the mapping. Hypothesis 4's
   `SplitQueue` half holds without changes to `arcbox-virtio-core`.
7. **vhost-user-vsock has two vhost queues.** QEMU keeps the event queue
   (`vhost_vqs[2]`) and emulates it. It reads the CID with `GET_CONFIG` at
   realize, so a back-end without `CONFIG` is refused. The guest bound
   `vmw_vsock_virtio_transport` and posted RX buffers in every run; no
   vsock traffic was moved.
8. **The guest's `sync` sends `FUSE_SYNCFS` (opcode 50), which `FsServer`
   answers with `ENOSYS`.** The guest tolerates it.
9. **Throughput.** 640–826 MiB/s sequential read, 673–966 files/s for
   `cat` and 3030–4167 files/s for `ls -l` of 1 KiB files, so one small
   file costs 1.0–1.5 ms of lookup, open, read and release. How much of
   that is the TCG guest and how much the back-end was not measured. The
   probe uses one request queue, and the back-end runs all queues of a
   connection on one worker thread, so requests of one share are served
   one at a time.
10. **rust-vmm is not usable as is, and its runtime would be replaced
    anyway.** Hypothesis 3 dies for `vhost-user-backend` (epoll,
    `virtio-queue`/`vm-memory` instead of our `SplitQueue`); the `vhost`
    protocol crate needs a patch upstream has not taken and still fails its
    tests. The back-end needs about 20 messages; implementing them is
    smaller than the patch plus the integration. See ADR 0010.

## Decisions taken / open

- Taken: [ADR 0010](../adr/0010-vhost-user-protocol-layer-is-arcbox-own.md).
- Open: vsock traffic over vhost-user (the vsock task's acceptance test);
  more than one fs request queue and a worker per queue; `SYNCFS` in
  `FsServer`; behaviour when the daemon dies under a running QEMU (no
  reconnect is attempted); an Intel Mac run (P1); a 9p baseline on the
  same host, to compare.
