# arcbox-host-net

The macOS guest network for VMs that run on the host — the adapter of [`arcbox-vm-driver`](../arcbox-vm-driver)'s `GuestNetwork` port for host sandboxes ([ADR 0010](../../docs/adr/0010-sandboxes-run-on-the-host.md)), and the macOS counterpart of [`arcbox-tap-net`](../arcbox-tap-net). One `HostNetwork` owns:

- an IPv4 `Pool` (`.1` is the gateway, `.2` onwards are leased lowest first). The pool address names a VM on the host — DNS, the sandbox API's `ip_address` — and nothing routes to it;
- one userspace NAT datapath per VM (`arcbox_net::darwin::guest_nat`), so every guest boots on the same fixed link, `10.0.2.2/24` behind `10.0.2.1` (also its resolver), with no host bridge between guests (D1). That is `identity` under `AttachMode::Invariant`; `LegacySnapshot` is refused, since no host checkpoint predates the fixed link;
- the quarantine ledger and its token protocol (`NetworkReconcile`), in memory;
- publication of each live lease's name through the composer's `DnsPublisher`: registered on `activate` and `adopt`, withdrawn on `quarantine` and `release`.

## Where the datapath runs

`DatapathPlacement` decides the NIC the driver gets. `HostNetwork::with_placement` gives a second view over the same ledger, so VZ and HV sandboxes never share a pool address.

| Placement | NIC | Datapath | Port forwarding, checkpoint resets | For |
|---|---|---|---|---|
| `Network` | `FileHandle` (the VMM end of a socketpair, open while the lease is live) | this process | `HostNetwork::take_port_forward`, `HostNetwork::datapath_control` | VZ, whose VMs run in the daemon |
| `Driver` | `UserNat` | the VMM's process, built there by `guest_nat` | the driver's `PortForward` and checkpoint | HV, one VMM process per sandbox, which injects RX into guest memory from that process |

Under `Network`, activating a lease again replaces its datapath, and `adopt` is refused: the datapath died with the process that ran it, so the owner tears the VM down.

## Contract notes

- `reserve` offers `NetworkMode::Nat` only (every datapath egresses through the host) and returns `Unavailable` until `replay_complete`, so a fresh lease cannot take the address of a running guest not yet adopted.
- Nothing this network installs outlives its process, so there is no startup sweep: `startup_cleanup_token` is always `None`. Quarantine still holds the address until the composer finalizes it, after clearing its own host state for the lease — a `PortForward` taken from `take_port_forward` included, whose listeners stay bound until `clear`.
- `host_ingress` is an error: there is no address for a DNAT rule to target. Ports reach the guest through a `PortForward`.

## Known gaps

- The datapath delivers an inbound TCP connection or UDP datagram to the guest on the port the host listener bound, not on the rule's guest port: `process_inbound_cmd` (`arcbox-net`, `darwin/datapath_loop/intercept.rs`) passes `host_port` to `TcpBridge::initiate_inbound`, and `InboundRelay::inject_udp` (`arcbox-proxy`) uses the listener's port. The System VM relies on dockerd's DNAT for the rest. This crate's `PortForward` therefore refuses a rule whose host port differs from its guest port, `0` included. Every other `PortForward` over `InboundListenerManager`, `arcbox-vz-driver`'s included, has the same gap without the refusal.
- A guest reaches the Mac's loopback through its gateway (`10.0.2.1` → `127.0.0.1`), so it can reach a port another sandbox exposed on loopback, like any other host service. Guests cannot address each other directly.

## Testing

`cargo test -p arcbox-host-net` on macOS:

- `tests/contract.rs` — the `GuestNetwork` contract, run against `HostNetwork` and against the testkit's `FakeNetwork`, so the checks stay the port's rather than this adapter's.
- `tests/isolation.rs` — two VMs with real datapaths: each guest's DHCP broadcast is answered on its own link with the address `identity` reports, a port forwarded into one guest is delivered there, and the other guest sees nothing in either case.

The crate is empty on other platforms.
