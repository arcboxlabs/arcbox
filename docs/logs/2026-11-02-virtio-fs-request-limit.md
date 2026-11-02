# 2026-11-02 — HV's virtio-fs refuses requests longer than any FUSE request

- Type: change
- Area: `virt/arcbox-virtio-fs` (`VirtioFs::answer_chain`, `VirtioDevice::process_queue`, `VirtioFs::process_queue`); the HV backend's virtio-fs shares
- Commits / PRs: `refactor(virtio-fs): answer a descriptor chain in VirtioFs`, `fix(virtio-fs): refuse requests longer than any FUSE request on HV`
- Related: #769, where review found the same gap in the vhost-user fs back-end

## Problem or trigger

Review of the vhost-user fs back-end (#769) found that it copied every
readable descriptor of a request chain into one buffer before parsing it,
with no limit on the total. #769 bounded it there. HV's virtio-fs device
gathered requests the same way, and still does on master before this
change.

Descriptor addresses and lengths are the guest's. `SplitQueue` bounds how
many descriptors a chain has, and `GuestMemWriter::slice` checks each range,
but nothing bounded their sum: up to a queue's worth of descriptors, each up
to 4 GiB and free to cover the same valid range of guest memory, made one
request a host allocation of that size.

## What was done

- `MAX_REQUEST_SIZE` in `arcbox-virtio-fs` is `DEFAULT_MAX_WRITE` plus one
  page of headers: the session advertises no other `max_write`, so no FUSE
  request is longer.
- `VirtioFs::answer_chain` adds up the readable lengths as `u64` before
  copying, answers a longer chain `EINVAL`, and otherwise gathers, answers
  and falls back to `EIO` as before. The vhost-user back-end's own copy of
  these steps moved here, so both back-ends run the same check.
- HV's `VirtioDevice::process_queue`, the path `dispatch.rs` drives, calls
  it. The inherent `VirtioFs::process_queue`, whose only caller is a unit
  test, copies at most `MAX_REQUEST_SIZE` bytes per chain and refuses a
  longer one with the same `EINVAL`.

VZ shares through Virtualization.framework's own virtio-fs device and was
not affected. The HV effect was not reproduced on an HV daemon; it follows
from the code path and the regression test below.

## Evidence

- `hv_queue_refuses_requests_longer_than_a_full_write` drives HV's
  `VirtioDevice::process_queue` with a real split ring: a GETATTR padded to
  exactly `MAX_REQUEST_SIZE` reaches the handler (error 0, 1 call); one byte
  longer is answered `-EINVAL` without reaching it. With the old
  `virtio_device.rs` it fails: `(0, 1)` instead of `(-22, 0)`.
- #769's `requests_longer_than_a_full_write_are_refused` still passes
  through `answer_chain`.
