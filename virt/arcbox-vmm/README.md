# arcbox-vmm

Virtual Machine Monitor (VMM) for ArcBox.

## Overview

This crate provides high-level VM management on top of the hypervisor abstraction layer. It orchestrates VM lifecycle, vCPU execution, memory layout, device management, and boot process.

## Features

- **VmBuilder**: Fluent API for VM configuration and construction
- **VcpuManager**: Manages vCPU threads and execution
- **MemoryManager**: Memory allocation and guest physical address mapping
- **DeviceManager**: VirtIO device registration and I/O handling
- **FDT generation**: Flattened Device Tree for ARM64 boot

## Architecture

```
+--------------------------------------------------+
|                      VMM                          |
|  +------------+ +-------------+ +-------------+  |
|  |VcpuManager | |MemoryManager| |DeviceManager|  |
|  +------------+ +-------------+ +-------------+  |
|  +------------+ +-------------+ +-------------+  |
|  | EventLoop  | |   IrqChip   | |    Boot     |  |
|  +------------+ +-------------+ +-------------+  |
+--------------------------------------------------+
                       |
                       v
          +------------------------+
          |   arcbox-hypervisor    |
          +------------------------+
```

## Usage

```rust
use arcbox_vmm::builder::VmBuilder;

let vm = VmBuilder::new()
    .name("my-vm")
    .cpus(4)
    .memory_gb(2)
    .kernel("/path/to/vmlinux")
    .cmdline("console=hvc0 root=/dev/vda")
    .block_device("/path/to/disk.img", false)
    .network_device(None, None)
    .build()?;

vm.run().await?;
```

On macOS HV, request guest shutdown before calling `wait_for_stopped(timeout)`. The wait requires exclusive access to the VMM, returns `true` only after the guest issues PSCI `SYSTEM_OFF`, and retains that result until the next start. Call `stop()` afterward to join vCPU and I/O workers and release host resources.

## Memory Layout (ARM64)

```
0x0000_0000 - 0x3FFF_FFFF  : RAM (low memory)
0x4000_0000 - 0x4000_FFFF  : GIC distributor
0x4001_0000 - 0x4001_FFFF  : GIC redistributor
0x4002_0000 - 0x4002_FFFF  : UART (PL011)
0x4003_0000 - ...          : VirtIO MMIO devices
0x8000_0000 - ...          : RAM (high memory)
```

## License

MIT OR Apache-2.0
