# arcbox-vz-driver

The Virtualization.framework adapter for the [`arcbox-vm-driver`](../arcbox-vm-driver) port. It renders a `VmSpec` into a VZ configuration through [`arcbox-vz`](../arcbox-vz), runs the VM in this process, and serves the port's capabilities over it: `Vsock` (async), `Balloon`, `Console` for `hvc0` and the agent log on `hvc1`, `DebugSnapshot`, and `PortForward` into the userspace NAT datapath.

VZ is a managed VMM, so there is no `Adopt`, `Prepare` or `Staging`. `Checkpoint` is absent until the shim binds `saveMachineStateTo` (vm-stack-redesign "Deferred by decision").

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

`tests/contract.rs` runs the port's driver contract against a real VM. It needs `VZ_KERNEL` and a test binary signed with the virtualization entitlement; see the module docs for the commands.
