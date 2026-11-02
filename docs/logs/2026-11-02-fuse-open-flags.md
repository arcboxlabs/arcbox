# 2026-11-02 — FsServer reads the guest's open flags as Linux flags

- Type: change
- Area: `virt/arcbox-fs` (`PassthroughFs::apply_flags`, `FsServer::on_init`); the HV backend's virtio-fs shares
- Commits / PRs: `fix(fs): read guest open flags as Linux flags`
- Related: none

## Problem or trigger

Found while booting the System VM's shares through `FsServer` for the
QEMU TCG backend: the agent's log files on the host lost their earlier
contents each time the guest reopened them for appending.

FUSE_OPEN and FUSE_CREATE carry the guest's `open(2)` flags, which are
Linux values. `apply_flags` tested them against the host's `libc`
constants. On macOS those differ: Linux `O_APPEND` (`0o2000`, 0x400) is
macOS `O_TRUNC` (0x400), and Linux `O_TRUNC` (`0o1000`, 0x200) is macOS
`O_CREAT`. So a guest `open(O_WRONLY | O_APPEND)` of an existing file
truncated it on the host, and a guest `O_TRUNC` did not truncate.

`FsServer` also offers `FUSE_WRITEBACK_CACHE`, and the guest kernel accepts
it. With writeback caching the kernel reads pages back through write-only
handles to fill partial pages, which a host `O_WRONLY` descriptor refuses
(`EBADF`), and it computes append offsets itself, writing whole pages back
at their offsets, which a host `O_APPEND` descriptor moves to the end of
the file.

## What was done

- `fuse::open_flags` holds the Linux `O_ACCMODE`, `O_WRONLY`, `O_RDWR`,
  `O_TRUNC` and `O_APPEND` values, and `apply_flags` uses them.
- `FsServer::on_init` records whether the session negotiated
  `FUSE_WRITEBACK_CACHE`. When it did, a write-only open becomes
  read-write and `O_APPEND` is dropped, as virtiofsd does.

Only HV serves shares through `FsServer` (`vmm/darwin_hv/setup.rs`), so
only VMs on the HV backend were affected: appending opens truncated host
files, for example `log/agent.log` under the data directory and the
`dockerd.log`/`containerd.log` files the agent opens for appending, and
any file a container appended to under `/Users` or `/private`. VZ, the
default backend, shares directories through Virtualization.framework's
own virtio-fs device and never reaches `FsServer`. On a Linux host the
`libc` constants are the Linux values, so the old comparison was right
there. The HV effect was not reproduced on an HV daemon; it follows from
the shared code path and the regression tests below.

## Evidence

- `guest_append_open_keeps_the_file`: without the change the file reads
  `"more\n"`, with it `"kept\nmore\n"`.
- `writeback_handles_read_back_and_write_in_place`: a write-only, append
  handle under writeback reads back its pages and writes in place.
- `guest_truncating_open_empties_the_file`: a Linux `O_WRONLY | O_TRUNC`
  open empties the file; without the change the file keeps its contents.
- `cargo test -p arcbox-fs`: 68 passed.

## Follow-ups

- `PassthroughFs::forget` returns early when `dec_ref` returns 1, so an
  inode is never removed from the table. Not changed here.
