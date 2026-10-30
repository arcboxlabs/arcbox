//! [`VmSpec`] → a VZ configuration and the host state the VM keeps.
//!
//! Devices are added in bus order: entropy, virtiofs shares, the Rosetta
//! share, disks, NICs, vsock, the two console ports, the balloon. The guest
//! names disks and NICs by that order, so it must not change.

use arcbox_vm_driver::{BootSpec, ConsoleSpec, IsolationSpec, Result, ShareSpec, VmSpec};
use arcbox_vz::{
    EntropyDeviceConfiguration, GenericPlatform, LinuxBootLoader, LinuxRosettaDirectoryShare,
    MemoryBalloonDeviceConfiguration, RosettaAvailability, SharedDirectory, SingleDirectoryShare,
    SocketDeviceConfiguration, StorageDeviceConfiguration, VirtioFileSystemDeviceConfiguration,
    VirtualMachineConfiguration,
};

use crate::console::PipeConsole;
use crate::net::{HostServices, NetResources};
use crate::{driver_error, unsupported};

/// A rendered spec: the configuration to build, and the host state behind
/// its devices.
pub struct Rendered {
    pub config: VirtualMachineConfiguration,
    pub net: NetResources,
    pub console: Option<PipeConsole>,
    pub agent_log: Option<PipeConsole>,
}

/// Renders `spec`. Refuses, with `InvalidSpec`, what this driver cannot
/// run: firmware and macOS boots, isolation, dirty tracking, a debug
/// console, console files and sockets, TAP and bridge NICs.
pub fn render(spec: &VmSpec, host: &HostServices) -> Result<Rendered> {
    spec.validate()?;
    refuse(spec)?;
    let BootSpec::Kernel {
        image,
        cmdline,
        initrd,
    } = &spec.boot
    else {
        return Err(unsupported("only direct kernel boots are supported"));
    };

    let mut config =
        VirtualMachineConfiguration::new().map_err(|e| driver_error("configuration", e))?;
    config.set_cpu_count(spec.cpus as usize);
    config.set_memory_size(u64::from(spec.memory_mib) * 1024 * 1024);

    let platform = GenericPlatform::new().map_err(|e| driver_error("platform", e))?;
    if spec.nested_virt {
        if GenericPlatform::is_nested_virt_supported() {
            platform.set_nested_virt_enabled(true);
            tracing::info!("nested virtualization enabled");
        } else {
            tracing::info!("nested virtualization unsupported on this host; booting without it");
        }
    }
    config.set_platform(platform);

    let mut boot_loader =
        LinuxBootLoader::new(image).map_err(|e| driver_error("boot loader", e))?;
    boot_loader.set_command_line(cmdline);
    if let Some(initrd) = initrd {
        boot_loader.set_initial_ramdisk(initrd);
    }
    config.set_boot_loader(boot_loader);

    if spec.entropy {
        config.add_entropy_device(
            EntropyDeviceConfiguration::new().map_err(|e| driver_error("entropy device", e))?,
        );
    }
    for share in &spec.shares {
        config.add_directory_share(virtiofs(share)?);
    }
    if spec.rosetta
        && let Some(rosetta) = rosetta()
    {
        config.add_directory_share(rosetta);
    }
    for disk in &spec.disks {
        config.add_storage_device(
            StorageDeviceConfiguration::disk_image(&disk.path, disk.read_only)
                .map_err(|e| driver_error(format!("disk `{}`", disk.id), e))?,
        );
    }
    let mut net = NetResources::default();
    for nic in &spec.nics {
        config.add_network_device(net.attach(nic, host)?);
    }
    if spec.vsock.is_some() {
        config.add_socket_device(
            SocketDeviceConfiguration::new().map_err(|e| driver_error("vsock device", e))?,
        );
    }
    let (console, agent_log) = if spec.console == ConsoleSpec::Pipe {
        // The array index is the hvc number: hvc0 first, then hvc1.
        let (console, console_port) = PipeConsole::new("console")?;
        let (agent_log, agent_log_port) = PipeConsole::new("agent-log")?;
        config.add_serial_port(console_port);
        config.add_serial_port(agent_log_port);
        (Some(console), Some(agent_log))
    } else {
        (None, None)
    };
    if spec.balloon {
        config.add_memory_balloon_device(
            MemoryBalloonDeviceConfiguration::new()
                .map_err(|e| driver_error("balloon device", e))?,
        );
    }
    Ok(Rendered {
        config,
        net,
        console,
        agent_log,
    })
}

fn refuse(spec: &VmSpec) -> Result<()> {
    if spec.isolation != IsolationSpec::None {
        return Err(unsupported("isolation is not available"));
    }
    if spec.dirty_tracking {
        return Err(unsupported("dirty tracking is not available"));
    }
    if spec.debug_console.is_some() {
        return Err(unsupported("there is no debug console"));
    }
    if !matches!(spec.console, ConsoleSpec::Off | ConsoleSpec::Pipe) {
        return Err(unsupported(
            "console output goes to pipes; use ConsoleSpec::Pipe or Off",
        ));
    }
    Ok(())
}

fn virtiofs(share: &ShareSpec) -> Result<VirtioFileSystemDeviceConfiguration> {
    let context = || format!("share `{}`", share.tag);
    let directory = SharedDirectory::new(&share.host_path, share.read_only)
        .map_err(|e| driver_error(context(), e))?;
    let single = SingleDirectoryShare::new(directory).map_err(|e| driver_error(context(), e))?;
    let mut device = VirtioFileSystemDeviceConfiguration::new(&share.tag)
        .map_err(|e| driver_error(context(), e))?;
    device.set_share(single);
    Ok(device)
}

/// The Rosetta share, or `None` when the host has no Rosetta to share or
/// the share cannot be set up. Best effort, as `VmSpec::rosetta` says.
fn rosetta() -> Option<VirtioFileSystemDeviceConfiguration> {
    match LinuxRosettaDirectoryShare::availability() {
        RosettaAvailability::Supported => {}
        RosettaAvailability::NotSupported => {
            tracing::warn!("Rosetta is not supported on this host; booting without it");
            return None;
        }
        RosettaAvailability::NotInstalled => {
            tracing::warn!(
                "Rosetta is not installed (`softwareupdate --install-rosetta`); booting without it"
            );
            return None;
        }
    }
    let device = LinuxRosettaDirectoryShare::new().and_then(|share| {
        let mut device = VirtioFileSystemDeviceConfiguration::new(VmSpec::ROSETTA_TAG)?;
        device.set_share(share);
        Ok(device)
    });
    match device {
        Ok(device) => Some(device),
        Err(e) => {
            tracing::warn!("Rosetta share setup failed; booting without it: {e}");
            None
        }
    }
}
