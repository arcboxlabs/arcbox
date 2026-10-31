# 2026-10-31 — Can an HV checkpoint carry a Linux guest's live TCP and vsock connections through a restore?

- Type: experiment
- Area: `virt/arcbox-vmm` (`darwin_hv` checkpoint and lifecycle, `device/resume.rs`), `virt/arcbox-virtio-net`, `virt/arcbox-virtio-vsock`, `common/splicetcp`; input to the host-side sandbox checkpoint design
- Outcome: open (spike). The guest keeps its NIC and vsock device across a restore. Host-side connections do not survive. The guest learns this at once: its stale TCP flows get an RST, and its vsock connections get a TRANSPORT_RESET. Its new connections work within 13–60 ms of the restore call
- Probes: `virt/arcbox-vmm/examples/checkpoint_linux_spike/` with `SPIKE_NET=1`, driven by `tests/bench/hv-checkpoint/run-net.sh`
- Follows: [the Linux block-I/O spike](2026-10-31-hv-checkpoint-linux-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5 (25F71). Release
  build. Guest: the ArcBox kernel from boot assets 0.8.12 on 1–8 vCPUs
  with 1 GiB RAM, the spike 3 initramfs, one 256 MiB disk, virtio-console,
  virtio-rng, virtio-net on the production datapath (splicetcp, GuestNat
  10.0.2.0/24) and virtio-vsock (CID 3). No bridge NIC (vmnet), VirtioFS
  or balloon.

## Question

Spike 3 restored a guest whose only devices kept their state in guest RAM
and on disk. virtio-net and vsock also depend on host state that a new
process does not have: the datapath's TCP flows and the host sockets
behind them, and the vsock connection table with its host file
descriptors. What has to be saved, what can be rebuilt, and what does the
guest see?

## Hypotheses

1. Both devices' queue state is in guest RAM, like virtio-blk's. A device
   can derive its avail cursor from `used.idx` instead of keeping one.
2. The host side of a connection cannot be moved between processes, so it
   is not saved. Instead the restored machine has to tell the guest that
   those connections are gone, and it has to do that promptly. A guest
   that is left to time out keeps a dead connection for minutes.
3. For TCP, an RFC 9293 CLOSED-state reply to any segment of a flow the
   new datapath does not track is enough. For vsock, the device's
   `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` is enough.
4. The I/O worker threads write guest RAM independently of the vCPUs. They
   must stop before the RAM image is written.

## Method

Changes:

- **Queue cursors.** The virtio-net TX path (`drain_tx_queue`) and the
  vsock TX path (`process_queue`) seed their avail cursor from `used.idx`.
  This drops `NetPort.last_avail_tx` and `VirtioVsock.last_avail_idx_{tx,rx}`.
  The net RX inject thread and vsock RX injection already did this.
- **Stopping the I/O workers.** A separate `io_running` flag gates the
  threads that write guest RAM on their own: blk workers, net RX
  (`rx-inject`), `vsock-io` and `console-io`. `start` sets it with
  `running`, and `stop` clears both. A checkpoint clears only
  `io_running`, after the vCPUs park, and joins the workers before it
  captures the GIC, device state and RAM.
- **Restore** (`device/resume.rs`, called from `start`):
  - `resume_net_rx_worker` spawns the net RX worker for a NIC that was
    already at DRIVER_OK. A restore never writes the STATUS register,
    which is what normally spawns it.
  - `reset_vsock_transport` posts TRANSPORT_RESET into the next event
    buffer (queue 2) and sets `INT_VRING`. The line comes up when the
    vCPUs re-sync every device after the GIC restore.
- **TCP** (`splicetcp`): `TcpBridge::untracked_reply` returns the CLOSED
  reply (`closed_reply_ip`) for a guest segment of an untracked flow.
  Bare SYNs, RSTs, and frames received before the guest MAC is learned
  get no reply. `FrameClassifier::drain_tcp` sends that reply for every
  frame that neither the fast path nor a handshake claims; before this,
  such frames were dropped silently. Regression test:
  `drain_resets_segments_of_untracked_flows_only`.
- **vsock injection gate.** The first run showed that a host connection
  made right after the restore died together with the stale ones (see
  results). `post_transport_reset` therefore records the event queue's
  `avail.idx`, and `poll_rx_injection` injects nothing until that index
  moves. Linux refills the event buffer right after handling the event,
  and the index moves whether or not the guest kicks the queue.

Workload (`guest/init.c` with `spike_net=<port>`), on top of spike 3's
four block threads:

- eth0 is configured statically as 10.0.2.2/24.
- A TCP client connects to the probe's echo server at 10.0.2.1:`<port>`
  (the gateway maps it to host loopback). It sends an 8-byte counter
  every 10 ms, checks the echo, and has a 5 s receive timeout. When a
  connection ends, it records whether it ended by ECONNRESET/EPIPE or by
  the timeout, then reconnects.
- A vsock echo server listens on port 5000 and serves one connection at a
  time.

The probe drives the vsock side through `Vmm::connect_vsock` with a 2 s
read timeout, reconnecting whenever a connection ends. `save` waits until
both connections carry traffic before it checkpoints.

A restore passes when everything spike 3 checks still holds and:

- The guest's stale TCP flow ends by exactly one reset and no timeout.
- The stale vsock connection ends exactly once.
- Both the guest and the host count echoes after the restore.

## Results

Baseline, 4 vCPUs. The checkpoint took 315 ms in total:

| Step | Time |
|---|---|
| Stopping all vCPUs | 1.5 ms |
| Stopping the I/O workers | 7.0 ms |
| Disk `fsync` + clone | 6.5 ms |
| RAM, 62 MiB on disk | 300 ms |

`hv_restore` returned in 3.1 ms. The guest's stale TCP flow got
ECONNRESET 13 ms after it resumed, and the stale vsock connection got
ECONNRESET (`vsock-end errno 104`). The probe's echo server accepted the
reconnect 14 ms after the restore call, and the first new vsock echo
arrived at 15 ms. Over the next 5 s the guest counted 573 TCP echoes and
the host 494 vsock echoes. Block I/O was unchanged from spike 3, with 0
failures.

20 save/restore cycles at random points: 20 of 20 passed.

| Measure | min | p50 | max |
|---|---|---|---|
| `hv_restore`, ms | 2.7 | 3.0 | 11.7 |
| TCP reconnect after the restore call, ms | 13 | 19 | 60 |
| First vsock echo after the restore call, ms | 13 | 16 | 60 |
| Stopping the vCPUs, ms | 1.2 | 1.5 | 6.0 |
| Stopping the I/O workers, ms | 0.1 | 6.4 | 11.3 |

vCPU sweep: 1, 2 and 8 vCPUs passed. Spike 3's block-only mode (no net,
no vsock) still passed.

Negative controls. Each one disables one change with a temporary edit,
rebuilds the probe and restores the same checkpoint:

| Disabled | Result |
|---|---|
| RST for untracked flows (`drain_tcp` change reverted) | the stale flow ends only by the guest's own receive timeout: errno 11 after 5,489 and 5,490 ms (2 of 2 runs), and the reconnect follows at 5.49 s. A client without a timeout waits for TCP's retransmission timeout |
| TRANSPORT_RESET (`reset_vsock_transport` skipped) | the stale vsock connection never ends in 8 s, so the one-connection server never serves the host: 0 host echoes |
| vsock injection gate (before the fix) | the first host connection gets no reply and times out after exactly 2.0 s, the probe's read timeout (3 of 3 runs). The second connection works; first echo at 2.1 s |

## Findings

1. **Hypotheses 1, 3 and 4 hold.** Neither device needs saved state
   beyond its transport registers once its cursors come from `used.idx`.
   The I/O workers stop in about 6 ms, bounded by the 10 ms vsock
   backstop.
2. **Hypothesis 2 holds, and "promptly" is not free.** Neither TCP nor
   vsock tells the guest anything by default. A stale TCP segment was
   dropped silently, and the guest vsock driver keeps a connected socket
   for as long as the host does not send RST.
3. **A transport reset races new connections.** Linux resets every
   connected socket when it handles the event. A connection whose
   OP_REQUEST it accepted before handling the event is reset too, and the
   host is not told. Nothing may be injected until the guest has handled
   the event.
4. **The RST covers a flow only when the guest sends on it.** Here the
   client sends every 10 ms, and all 24 restores of the matrix ended the
   flow by reset. A guest blocked in `recv` on a flow whose last segment
   the host already ACKed sends nothing. It learns only from
   retransmission, a keepalive or its own timeout. Closing that gap needs
   the flow table in the checkpoint, so that the restore can send each
   flow an RST at SND.NXT. [The follow-up spike](2026-10-31-hv-checkpoint-tcp-reset-spike.md)
   does that. Its probe and `run-net.sh` now include it, so a rerun
   measures this spike's setup plus the saved resets.

## Not covered

- The bridge NIC (vmnet), DHCP leases and DNS registrations. The guest
  here uses a static address. A real guest renews its lease from a
  datapath that never issued it.
- The restored NIC advertises a new random MAC in config space, while the
  guest keeps the one it read at boot. The datapath learns the MAC from
  frames, so this worked here. The machine's MAC should still be saved
  with the checkpoint.
- Frames queued for the RX inject thread when the checkpoint stops it are
  lost. TCP retransmits them; this spike did not count them.
- Two concurrent clones with networking: the probe's echo server needs a
  fixed port. Each clone has its own datapath, so the same guest IP is not
  a conflict.
- UDP and DNS in flight: their host sockets are per-request, and the guest
  retries them.
