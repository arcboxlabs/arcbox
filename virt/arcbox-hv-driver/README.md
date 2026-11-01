# arcbox-hv-driver

The Hypervisor.framework adapter for the [`arcbox-vm-driver`](../arcbox-vm-driver) port, one VMM process per VM. Hypervisor.framework allows one VM per process, and the daemon holds the System VM, so each VM this driver runs lives in its own `arcbox-hv-vmm` process ([ADR 0010](../../docs/adr/0010-sandboxes-run-on-the-host.md)). The process runs `arcbox_vmm::hv_driver::HvMachineDriver` on itself and serves it to the driver over `<runtime dir>/vmm.sock`; it logs to `<runtime dir>/vmm.log`.

| Capability | How |
|---|---|
| `Prepare` | Spawns the process with no VM; `boot` and `restore` send the spec. |
| `Adopt` / `Detach` | A detached process keeps running; `adopt` checks its pid and id on the socket. A process that never booted is killed. |
| `Checkpoint`, `restore` | The engine's `arcbox-hv/1` image (`Vmm::hv_checkpoint` plus `vm.json`) and `console.json`. `Resume` continues in the same process; connections into the guest are reset. |
| `Vsock` | The process's socketpair end, passed over the control socket (`IoMode::Blocking`). A guest with no listener reads as EOF. |
| `Console` | `ConsoleSpec::Pipe`: the process copies hvc0 and PL011 lines into a pipe whose read end is passed over. `File` appends to a file. |
| `PortForward`, `Balloon`, `DebugSnapshot` | Requests to the process; forwards are bound there. |

Not available: `Staging` (the process reads host paths), guest-initiated vsock, isolation, incremental checkpoints.

The VM's life is the process's: a guest power-off ends the process with the guest's code, and `shutdown(Kill)` is `SIGKILL`. A restore needs the disks the image names at new paths, cloned from the image's `disk<N>.img`.

## Usage

```rust,ignore
// `arcbox-hv-vmm` must be signed with com.apple.security.hypervisor.
let driver = arcbox_hv_driver::HvDriver::new(vmm_binary, detect_proxy);
let vm = driver.boot(spec, &runtime_dir).await?;
```

## Testing

`tests/hv_contract.rs` runs the port's driver contract and three host-sandbox checks (two VMs at once, restore in a new process mid-I/O, resume with port forwards) against real processes. It needs `HV_KERNEL` and `HV_INITRD`; see the module docs.
