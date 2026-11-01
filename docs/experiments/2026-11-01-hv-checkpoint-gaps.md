# 2026-11-01 — Can the HV checkpoint close the gaps the spikes left: device config, MAC, macOS build, dirty pages, DHCP and handshakes?

- Type: experiment
- Area: `virt/arcbox-vmm` (`darwin_hv` checkpoint, `machine.rs`, `network.rs`), `virt/arcbox-virtio-{core,net,balloon,console,vsock}`, `virt/arcbox-dhcp`, `virt/arcbox-net` (`datapath_loop`); W2 of `docs/plans/host-sandboxes-v1.md`
- Outcome: the gaps in the spikes' "Open" and "Not covered" lists that W2 owns are closed, plus one found on the way: a restored process's first vsock connections could reuse the address of a connection the guest still held and read EOF. Saving a 1 GiB guest that has touched 50–68 MiB writes only those pages. The RAM write takes 51–74 ms at p50 59 ms, and the whole checkpoint 72–103 ms (spike 3: about 300 ms of RAM scan, 315–345 ms in total). A second-generation checkpoint writes 4–15 MiB. Outliers up to 288 ms came from concurrent builds on the host
- Probes: `virt/arcbox-vmm/examples/checkpoint_linux_spike/`, driven by `tests/bench/hv-checkpoint/run-linux.sh` and `run-net.sh`
- Follows: [the Linux spike](2026-10-31-hv-checkpoint-linux-spike.md), [the net and vsock spike](2026-10-31-hv-checkpoint-net-vsock-spike.md), [the TCP reset spike](2026-10-31-hv-checkpoint-tcp-reset-spike.md)
- Host / guest: Apple M5 Pro (Mac17,9), 48 GB, macOS 26.5 (25F71). Release
  build. Six other worktrees were building on the same 15-core machine
  throughout, which is where the max column's outliers come from. Guest: the
  ArcBox kernel from boot assets 0.8.12 on 1–8 vCPUs with 1 GiB RAM, the
  spike 3 initramfs extended below, one 256 MiB disk, virtio-console,
  virtio-rng, virtio-balloon, and in net mode virtio-net on the production
  datapath and virtio-vsock. No bridge NIC (vmnet) or VirtioFS.

## Question

The spikes left a list of state outside the checkpoint:

- the balloon, and the console's RX cursor;
- after a restore, the first host vsock connection or two sometimes read
  EOF (reported by W1 against its HV driver);
- the NIC's MAC, which the restored device re-randomized;
- nothing tied a checkpoint to the macOS build that wrote it (design D6);
- every checkpoint scanned all of RAM for non-zero pages;
- a restored guest renews its lease from a datapath that never issued it;
- no flow had been checkpointed in the middle of a handshake.

Can each be closed in the VMM without changing `Vmm::hv_checkpoint` and
`Vmm::hv_restore`, and what does a checkpoint cost once only touched pages
are written?

## Method

Changes:

- `VirtioDevice` gains `save_config` / `load_config` for the state the host
  owns. virtio-net saves its MAC, the balloon saves `num_pages` and
  `actual`, and vsock saves its connection manager's next host port. The restore loads them before `ack_features`. The balloon
  and console drop their host-side avail cursors and resume at the guest's
  `used.idx`, as blk and rng already do.
- `state.json` records `os_build` (`sysctl kern.osversion`) and the bridge
  NIC's MAC. A restore on another build fails with
  `VmmError::CheckpointBuildMismatch` before it builds anything. A restored
  bridge NIC takes the saved MAC unless the config names a different one,
  which is an error.
- `save::write_ram` asks `mincore` which pages can have changed. A page in no
  anonymous object is still the clean page of the image the machine was
  restored from (mapped `MAP_PRIVATE`). An anonymous page that is resident or
  compressed was written. An anonymous page that is neither was never touched
  since the mapping, or since the balloon refreshed it, and reads zero.
  - With a base image, the new `mem.bin` starts as an `fclonefileat` of it,
    keeps the clean pages, writes the written ones and punches holes over
    the zero ones.
  - Without one, it writes the touched pages that are not zero.
  - Runs of pages that need the same operation are coalesced. Debug builds
    compare the finished image with RAM byte for byte (`verify_ram`).
- `HvMachine::set_balloon_target` raises the config-change interrupt. Before
  this the guest never saw a new target.
- The DHCP server answers a client that has an address (`ciaddr`) by unicast
  to it, and everything else by broadcast (RFC 2131 4.1).
  `DhcpServer::handle_packet` returns a `DhcpReply` that carries the
  destination. A renewal of a lease this server never issued was already
  ACKed with the same address; the broadcast reply was what the client
  never received.

The probe guest gains:

- a console reader that echoes `rx <line>`;
- in net mode, a DHCP client that renews 10.0.2.2's lease every 300 ms from
  a socket bound to 10.0.2.2:68, the way a renewing client does;
- a client that keeps a connect pending against a host listener whose
  backlog is full, so the dial never completes;
- a vsock server on port 5001 that greets each connection with one byte and
  never closes it, as an agent handler busy elsewhere would.

The host side:

- sets the balloon to 64 MiB before every save and flips it on each restore;
- writes a line to the console after the restore;
- in net mode, makes its first vsock connection, before the checkpoint and
  again after the restore, to that holding server;
- with `SPIKE_RESAVE=<dir>`, checkpoints the restored machine again.

A restore now also passes only when:

- the balloon comes back at the saved size and then moves to the new one;
- the console line is echoed;
- the guest's MAC equals the saved one;
- the first vsock connection after the restore is accepted;
- at least one renewal is ACKed after the restore;
- the pending connect, when the checkpoint saved a reset for it, ends with
  ECONNREFUSED within 1 s of the restore.

`run-linux.sh` adds a `build` negative control and a second generation.
`run-net.sh` adds the pending-connect and DHCP columns and a second
generation.

Tests:

- `abort_tcp_saves_no_reset_for_an_active_open_and_its_syn_ack_draws_one`
  (`datapath_loop/tests/control.rs`): a port-forwarded connection still
  waiting for the guest's SYN-ACK is aborted. The checkpoint saves no reset,
  the host side closes, and the SYN-ACK the guest sends afterwards gets RST
  at ISN + 1.
- `renewal_of_an_unknown_lease_is_acked_unicast_with_the_same_address`
  (`arcbox-dhcp`).
- `load_config_restores_the_saved_mac` (virtio-net).
- `load_config_resumes_host_ports_where_the_checkpoint_left_off` (vsock).
- `a_fresh_device_resumes_at_the_guest_used_index` and
  `load_config_restores_target_and_inflation` (balloon).
- The console RX test also continues on a fresh device.

## Results

`run-linux.sh`: exit 0. Baseline, 4 vCPUs: the checkpoint wrote 3495 pages
(16 KiB host pages, 54 MiB) in 59 ms, 86 ms in total. The restore passed
and the guest's first tick came 43 ms after the restore call.

| `run-linux.sh`, 20 cycles at random points (0 of 20 failed) | min | p50 | max |
|---|---|---|---|
| RAM written, MiB | 51 | 54 | 54 |
| RAM write, ms | 51 | 59 | 271 |
| stopping the vCPUs, ms | 1.1 | 1.5 | 6.0 |
| `hv_restore`, ms | 3.1 | 4.4 | 12.3 |

`run-net.sh`: exit 0. Baseline: 4 TCP flows aborted, 65 MiB written in
58 ms, 81 ms in total. After the restore, the stale flows and the pending
connect ended 26 ms after the restore call. 16 renewals were ACKed in 5 s,
and the balloon went from 64 MiB to 0.

| `run-net.sh`, 20 cycles at random points (0 of 20 failed) | min | p50 | max |
|---|---|---|---|
| `hv_restore`, ms | 3.8 | 4.7 | 10.6 |
| TCP reconnect after the restore call, ms | 15 | 25 | 50 |
| first vsock echo, ms | 9 | 19 | 55 |
| pending connect refused after the restore call, ms | 15 | 25 | 50 |
| DHCP renewals ACKed in 3 s after restore | 9 | 10 | 10 |
| stopping the vCPUs, ms | 1.1 | 1.5 | 4.6 |
| stopping the I/O workers, ms | 0.6 | 8.3 | 15.1 |

Every cycle aborted 4 flows, so every checkpoint landed inside a pending
dial. A rerun after the vsock fix (finding 6), with the holding-server check,
passed 20 of 20 with `hv_restore` at 6.7–9.3 ms and the other columns
within the ranges above.

vCPU sweep (fresh save; RAM written, RAM write, checkpoint total):

| vCPUs | `run-linux.sh` | `run-net.sh` |
|---|---|---|
| 1 | 49 MiB, 54 ms, 78 ms | 60 MiB, 102 ms, 172 ms |
| 2 | 50 MiB, 53 ms, 103 ms | 61 MiB, 288 ms, 377 ms |
| 8 | 56 MiB, 241 ms, 382 ms | 68 MiB, 57 ms, 83 ms |

Both scripts passed again after a rebase onto master with the host
sandbox workstreams (W1's HV driver included). `run-linux.sh`'s 20 cycles
wrote 52–54 MiB at p50 50 ms; one write took 848 ms while other worktrees
were building. `run-net.sh`'s 20 cycles passed with every column inside the
ranges above, reconnects up to 83 ms.

The slow cells are not tied to a vCPU count. Each script has one, and they
are at different counts; each coincided with builds in other worktrees.

Second generation, checkpointing a machine restored from the baseline:

| | written | zeroed | kept | RAM write | total |
|---|---|---|---|---|---|
| `run-linux.sh` | 315 pages (4 MiB) | 12,160 | 53,061 | 45 ms | 75 ms |
| `run-net.sh` | 893 pages (13 MiB) | 11,392 | 53,251 | 47 ms | 72 ms |

The three columns add up to all 65,536 pages. The zeroed pages are
those the guest handed back since the restore: the balloon and free page
reporting refresh them to zero in RAM. Both second generations restored and
passed. A debug build ran three generations with `verify_ram` passing on
each.

Negative controls, `run-linux.sh`. `build` is new; the rest are spike 3's,
rerun:

| Tampered | Result |
|---|---|
| `os_build` set to `0A000` | `checkpoint was written on macOS build 0A000, this host runs 25F71` |
| devices, timer, ICC, the other checkpoint's GIC or RAM, disk | fail as in spike 3 |
| GIC from another checkpoint at the same configuration (`gic-swap`) | restored: the two blobs were byte-identical this run (spike 3 finding 3), so the control tested nothing |

Code-level negative controls, each a temporary edit and a release rebuild:

| Disabled | Result |
|---|---|
| writing touched pages (debug build) | `verify_ram`: the image differs from RAM in MiB 16 |
| DHCP reply by broadcast instead of unicast | 0 renewals ACKed, 1 timed out; the restore fails. The client's socket is bound to its address, and Linux does not deliver a broadcast to it |
| sending the saved resets | the idle connection and the pending connect never end |
| `load_config` for the balloon | the balloon comes back at 0, not 64 MiB; the restore fails |
| vsock's `save_config` / `load_config` (the code before this change) | the first vsock connection after the restore reads EOF, 3 of 3 restores; with them, 3 of 3 are accepted |

Two controls showed nothing:

- Skipping the hole punch still passed. The pages it should have zeroed
  happened to be zero in the base image too.
- Restoring the old console RX cursor passed, because the probe's console
  had taken one input before the checkpoint and the stale cursor happened
  to be right. The unit test covers that case.

## Findings

1. **`mincore` is enough to know what to write.** No write protection or
   dirty log is needed. A page outside every anonymous object is still the
   base image's page. An untouched anonymous page reads zero. Neither is
   read or scanned, so the RAM cost follows what the guest touched, not
   its size.
2. **The second generation is cheap because of APFS clones.** Starting
   `mem.bin` as a clone of the image the machine was restored from, opened
   by descriptor so it can since have been replaced, leaves only the
   guest's new writes and frees.
3. **The balloon target never reached an HV guest.**
   `set_balloon_target` changed the config space without raising the
   config interrupt, so the guest's driver never looked. The daemon's
   balloon controller does not shrink on either backend (`app/AGENTS.md`),
   which is why nothing noticed.
4. **The DHCP gap was the reply's destination, not the lease.** The
   server already ACKs a renewal of an address it never issued, if that
   address is free. A renewing client has its socket bound to that address
   and never received the broadcast reply.
5. **A pending dial is reset like an established flow.** The new datapath
   never saw it, so only the saved reset ends it at once. Without the reset,
   the guest retransmits its SYN into a fresh dial that waits out the
   datapath's 5 s timeout. A dial the host accepted but the guest has not
   answered (ActiveOpen) gets no reset by design, and the guest's SYN-ACK
   draws one (unit test).
6. **A restored process reused the vsock addresses the guest still
   held.** Every `VsockConnectionManager` started its host ports at 50,000,
   so the first connections of a restored process took the addresses of
   the checkpointed process's first ones. TRANSPORT_RESET leaves a reset
   socket in Linux's connected table until its owner closes it, and Linux
   looks up the connected table first, so a new OP_REQUEST from the same
   address finds the dead socket and is answered with OP_RST: the host
   reads EOF. A retry gets the next port and works, which is why it showed
   up only as the first one or two connections failing. The probe's echo
   server closed its reset socket at once, so 8 restores without the
   holding server showed nothing. The vsock device now carries the next
   host port in the checkpoint. EOF on the first connections after a
   *boot* is a different case: the guest has no listener yet and answers
   OP_RST, as `connect_vsock_hv` documents, and the caller retries.
7. **The guest-side wait of a pending connect is no evidence.** It
   includes the time before the checkpoint. The probe first checked it
   against the 5 s dial timeout and failed a cycle that checkpointed late in
   a dial. The check now only times the end from the restore, and it is
   skipped when the checkpoint saved no reset for the dial. The pend
   client sleeps 100 ms between dials, so a checkpoint can land outside one.
   No cycle did here.

## Not covered

- The bridge NIC (vmnet) MAC is saved and reused, but the probe has no
  vmnet NIC, so that path is untested.
- Frames queued for the RX inject thread at the checkpoint are still lost;
  TCP retransmits them.
- First-touch cost after a restore: RAM is mapped from `mem.bin` lazily, and
  the probe does not measure the faults.
- VirtioFS (FUSE inode tables, DAX mappings).
- In one `run-net.sh` cycle before the final run, a download started after
  the restore crashed the datapath:
  `build_tcp_data_frame_partial_csum: ip_total_len=262184 overflows u16`.
  splicetcp's retransmit paths (`tcp_bridge/fast_path.rs`) pass every
  in-flight byte to `emit_data_frames`, which in large-frame mode builds one
  frame. Any RTO retransmit with more than about 64 KiB in flight would hit
  it. That is unrelated to the checkpoint and outside W2's paths; it is
  reported, not fixed.

## Decisions taken / open

Taken:

- Host-owned device state travels as an opaque per-device blob
  (`save_config` / `load_config`); a device that has none rejects a
  non-empty one.
- A checkpoint is valid only on the macOS build that wrote it (D6).
- RAM images are written from `mincore`, not from a dirty log.

Open:

- The design's gap table (`docs/architecture/host-sandboxes.md`) should be
  updated to mark these rows closed.
