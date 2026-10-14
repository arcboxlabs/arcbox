# arcbox-net

High-performance network stack for ArcBox.

## Overview

This crate provides networking capabilities for VMs including NAT, bridge, and host-only networking modes. It features a zero-copy data path and high-performance NAT engine designed for minimal latency and maximum throughput.

## Features

- **NAT networking**: Default shared network with host
- **Bridge networking**: Direct L2 connectivity
- **Host-only networking**: Isolated VM networks
- **Port forwarding**: Expose guest services to host
- **DHCP server**: Automatic IP assignment for guests
- **DNS forwarding**: Transparent DNS resolution
- **Zero-copy packets**: Direct guest memory access
- **Lock-free data path**: SPSC ring buffers for hot path

## Architecture

```
+--------------------------------------------------+
|                   arcbox-net                      |
|  +--------------------------------------------+  |
|  |             NetworkManager                 |  |
|  |  - Network lifecycle                       |  |
|  |  - IP allocation                           |  |
|  +--------------------------------------------+  |
|  +----------+ +----------+ +-----------------+   |
|  |   NAT    | |  Bridge  | |  Port Forward   |   |
|  | Network  | | Network  | |    Service      |   |
|  +----------+ +----------+ +-----------------+   |
|  +--------------------------------------------+  |
|  |               TAP/vmnet                    |  |
|  +--------------------------------------------+  |
+--------------------------------------------------+
```

## Usage

```rust
use arcbox_net::{NetworkManager, NetConfig, NetworkMode};

let config = NetConfig {
    mode: NetworkMode::Nat,
    mac: None,  // Auto-generated
    mtu: 1500,
    bridge: None,
    multiqueue: false,
    num_queues: 1,
};

let manager = NetworkManager::new(config);
manager.start()?;

// Allocate IP for a VM
let ip = manager.allocate_ip();
```

## Performance Features

- **LockFreeRing**: Single-producer single-consumer queue for hot path
- **PacketPool**: Pre-allocated packet buffers for zero-allocation I/O
- **NAT Engine**: Connection tracking with 256-entry fast-path cache
- **Incremental checksum**: RFC 1624 compliant, no full packet recalculation

The macOS VM datapath and the `tun_proxy` harness use `FrameClassifier::drain_tcp` to process queued TCP frames in arrival order. A handshake completion promotes the flow before later payload in the same input batch. Established uploads use the bridge's bounded vectored writes and cumulative ACKs. VM replies retain reliable queuing and the existing flush points; the harness retains best-effort writes.

## Platform Support

| Platform | Backend |
|----------|---------|
| macOS | vmnet.framework |
| Linux | TAP/bridge/netlink |

## License

MIT OR Apache-2.0
