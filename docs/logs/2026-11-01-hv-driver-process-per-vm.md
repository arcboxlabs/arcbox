# 2026-11-01 — HV VMs run in one arcbox-hv-vmm process each, with checkpoints

- Type: change
- Area: `virt/arcbox-vmm` (`hv_driver`), `virt/arcbox-hv-driver`
- Commits / PRs: host sandboxes workstream W1, branch `hs-w1-hv-driver`
- Related: [ADR 0010](../adr/0010-sandboxes-run-on-the-host.md),
  [host sandboxes design](../architecture/host-sandboxes.md),
  [plan](../plans/host-sandboxes-v1.md) W1,
  [HV checkpoint spike 3](../experiments/2026-10-31-hv-checkpoint-linux-spike.md)

## Problem or trigger

Hypervisor.framework allows one VM per process, and the daemon holds the
System VM, so a host sandbox on HV cannot be an `HvMachine` in the daemon.
The R4b in-process driver had no checkpoint, and none of the spike entry
points (`Vmm::hv_checkpoint`/`hv_restore`) was behind the driver port.

## What was done

- `arcbox_vmm::hv_driver::HvMachineDriver` (R4b, rebased) owns a `Vmm` and
  gains `Checkpoint` and `restore` over `Vmm::hv_checkpoint`/`hv_restore`.
  An image is the machine's checkpoint plus `vm.json` (format
  `arcbox-hv/1`). `HoldQuiesced` keeps the stopped machine; `Resume`
  restores into a new machine in the same process, and port forwards move
  onto its listener manager.
- `arcbox-hv-driver` spawns one `arcbox-hv-vmm` per VM, which runs
  `HvMachineDriver` on itself and serves it over `<runtime dir>/vmm.sock`
  (length-prefixed JSON, descriptors as `SCM_RIGHTS`). vsock connections and
  the console pipe arrive as descriptors. `Prepare` is the spawn,
  `Adopt`/`Detach` find a process by pid and id on its socket, and the VM's
  exit is the process's.

## Evidence

All runs on an M-series Mac with the session locked, kernel
`~/.arcbox/boot/0.8.12/kernel`, the spike 3 initramfs (`/dev/vda` I/O
checker, vsock echo on 5000, one `tick` line a second on hvc0, read here
through the console pipe), `tests/hv_contract.rs` in `arcbox-hv-driver`.

| Run | Result |
|---|---|
| 18 `driver_contract!` checks + 3 host-sandbox checks, `--test-threads=1` | 21/21 pass, 33.5 s |
| The same, `--test-threads=6`, `8`, `12` (up to 12 VM processes at once) | pass; see the vsock finding below |
| The same, `--test-threads=4`, next to an isolated HV daemon whose System VM was up (`ARCBOX_VM_BACKEND=hv`, own data dir, ports 0) | 21/21 pass; the daemon's dockerd answered `/_ping` before and after |
| Restore in a new process mid-I/O, on an APFS clone of the image's `disk0.img` | ticks continue, `fails 0`, ops grow (`tick 3 … ops 5709` → `tick 6 … ops 10956`), vsock echo answers |
| Negative control: the same restore onto an empty disk | fails as it should: `fails 10691` |

Finding, not fixed here: under load a guest resets the first vsock
connection or two, which reads as EOF on the host. Seen once in five full
runs at `--test-threads=12` (a fresh boot, at its first tick), and in 4 of
18 restores when six ran at once, 3 s after the restore; a retry 200 ms
later succeeded each time. A serial run never showed it. The echo helper
retries up to five times.

## Follow-ups

- The vsock resets after a restore under load: VMM vsock transport reset
  handling (W2's area), or the spike guest's single-threaded accept loop.
- A restore mismatch from another macOS build reaches the daemon as
  `Error::Driver` text; a typed one needs a port variant.
- The port-forward check after `Resume` proves the host listener moved and
  is removed, not that bytes reach the guest: the spike guest has no TCP
  server.
