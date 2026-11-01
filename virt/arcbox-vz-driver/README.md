# arcbox-vz-driver

The Virtualization.framework adapter for the [`arcbox-vm-driver`](../arcbox-vm-driver) port. It renders a `VmSpec` into a VZ configuration through [`arcbox-vz`](../arcbox-vz), runs the VM in this process, and serves the port's capabilities over it: `Vsock` (async), `Balloon`, `Console` for `hvc0` and the agent log on `hvc1`, `DebugSnapshot`, and `PortForward` into the userspace NAT datapath.

VZ is a managed VMM, so there is no `Adopt`, `Prepare` or `Staging`.

## Checkpoints

`Checkpoint` and `VmDriver::restore` use VZ's own `saveMachineStateTo`/`restoreMachineStateFrom`. An image (format `vz/v1`) holds:

| File | What |
|---|---|
| `state.vzvmsave` | VZ's saved state: guest RAM and devices |
| `disks/<id>.img` | each writable disk, flushed and cloned (`clonefile` on APFS) while the guest was paused |
| `checkpoint.json` | the saved `VmSpec`, the platform's machine identifier and the macOS build; written last |

- A save needs the console session unlocked: VZ creates a Secure Enclave key on every save. Ask `VzDriver::checkpoint_blocker()` before scheduling one; a checkpoint taken anyway fails with `Error::Unavailable` and leaves the guest running.
- A restore rebuilds the saved configuration with the saved machine identifier, the `RestoreSpec`'s id, NICs and disk paths. NICs keep their saved MACs (VZ refuses others). The caller names a copy of `disk_clone(image, id)` for each restore, so one image restores into any number of VMs at once.
- An image from another macOS build is refused with `Error::PreconditionFailed` before VZ is asked; its disk clones can still boot cold.

Measurements and the open questions are in `docs/experiments/2026-10-31-vz-checkpoint-spike.md` (follow-up). `examples/checkpoint_probe.rs` is the driver-level probe.

The crate is empty on platforms other than macOS.

## Network attachments

| `NicAttachment` | Host side |
|---|---|
| `UserNat` | `arcbox_net::darwin::guest_nat` on a socketpair: DHCP, DNS shared with the host, egress through host sockets or the proxy, port forwarding. Falls back to Apple NAT when the datapath cannot start. |
| `HostNat` | vmnet shared mode relayed over a socketpair with the `vmnet` feature, so the host bridge interface is known at once; Apple NAT otherwise or when vmnet fails. |
| `FileHandle` | The caller's socket, duplicated for the VM's life. |

## Usage

```rust,ignore
let driver = arcbox_vz_driver::VzDriver::new(Some(shared_dns), proxy);
let vm = driver.boot(spec, &runtime_dir).await?;
let forwards = vm.take_port_forward();
```

## Testing

`tests/contract.rs` runs the port's driver contract against a real VM. It needs `VZ_KERNEL` and a test binary signed with the virtualization entitlement; see the module docs for the commands. The checkpoint checks skip themselves, printing why, while the session is locked.
