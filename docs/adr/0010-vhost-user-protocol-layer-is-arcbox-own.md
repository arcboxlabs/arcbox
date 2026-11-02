# ADR 0010: vhost-user back-ends use ArcBox's own protocol layer, kqueue loop and `SplitQueue`

- Status: accepted (2026-11-02)
- Deciders: QEMU TCG backend plan, task D (item 1b)
- Commits / PRs: branch `feat/vhost-user-framework`
- Evidence: [2026-11-02 — vhost-user on macOS](../experiments/2026-11-02-vhost-user-on-macos.md)

## Context

The QEMU TCG backend serves the System VM's virtio-fs shares and vsock from
the daemon over vhost-user, so the guest stays unchanged. The daemon runs on
macOS. The usual Rust stack for a vhost-user back-end is rust-vmm's `vhost`
crate (protocol) and `vhost-user-backend` (runtime). At upstream main
`ed821c2` (2026-10-26), `vhost` does not compile on macOS with the
`vhost-user-backend` feature, and `vhost-user-backend` runs on epoll with
`virtio-queue` over `vm-memory`. ArcBox's devices already run on
`arcbox-virtio-core`'s `SplitQueue` (HV backend), and the root `AGENTS.md`
keeps VirtIO data paths custom-built.

## What was measured

- A 14-line patch makes `vhost` compile on macOS; its lib tests still do
  not compile. `vhost-user-backend` needs a port from epoll that is not on
  upstream main.
- The back-end side needs about 20 messages. Our implementation, with
  `SplitQueue` unchanged on the mapped guest RAM, boots the unmodified
  System VM rootfs from an `FsServer`-backed `arcbox` share under QEMU TCG
  and reads 640–826 MiB/s through it (five runs, 9/9 checks each).

## Decision

1. `virt/arcbox-vhost-user` implements the vhost-user back-end protocol
   itself and depends on no rust-vmm crate.
2. Guest RAM from `SET_MEM_TABLE` is mapped into one reservation, so one
   `GuestMemWriter` covers it and devices process rings with
   `arcbox_virtio_core::SplitQueue`. The started ring's `SplitQueue` is the
   only owner of the avail cursor.
3. Each connection runs one control thread and one kqueue worker thread.
   Devices implement `VhostUserDevice` and register their own descriptors
   through `Registrar`; they do not start threads to poll.
4. Protocol features negotiated on a connection survive `RESET_DEVICE`.
5. A call notifier is read from its shared slot at every signal and never
   cached, because QEMU replaces it on MSI-X mask and unmask.
6. `SET_VRING_ERR` is accepted and ignored: on macOS QEMU passes the read
   end of a pipe.

## Consequences

- The vhost-user protocol is ArcBox code to maintain, including spec
  messages added later. Unsupported requests close the connection, which
  stops the device in QEMU, so a missing message shows up as a failed
  device, not silent corruption.
- No upstream patch is needed. The macOS fixes for rust-vmm (`vhost`
  compile errors) are not sent upstream.
- A device written for this crate is not portable to rust-vmm back-ends.
- Before switching to rust-vmm, a change must show that upstream `vhost`
  builds and passes its tests on macOS, that the runtime it brings runs on
  kqueue, and that ArcBox devices keep `SplitQueue` or measure faster
  without it.

## Alternatives considered

- Patch rust-vmm `vhost` and use only its protocol layer: needs an upstream
  patch that still leaves its tests broken; the runtime around it is ours
  either way.
- `vhost-user-backend`: epoll-only on main, and replaces `SplitQueue` with
  `virtio-queue`/`vm-memory`, a second queue implementation.
- QEMU's `libvhost-user` (C) through FFI: adds a C build and an FFI
  boundary to the data path; macOS support unverified.
- 9p + virtio-serial multiplexing: the plan's fallback; not needed since
  vhost-user works.
