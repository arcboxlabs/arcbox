//! [`VmSpec`] → the [`VmmConfig`] an [`HvMachine`](crate::vmm::darwin_hv::HvMachine)
//! boots from. Pure: it opens no file and starts nothing.
//!
//! What the HV machine always has, whatever the spec says: the
//! virtio-console device (the guest console ABI) and the entropy device
//! (the kernel's crng needs it). The primary NIC's MAC is chosen by the
//! machine, as today; the spec's MAC applies to the host NAT NIC.

use arcbox_vm_driver::{
    BootSpec, CacheMode, ConsoleSpec, IsolationSpec, NicAttachment, Result, VmSpec,
};

use super::unsupported;
use crate::vmm::{BlockDeviceConfig, SharedDirConfig, VmBackend, VmmConfig};

/// The smallest guest the HV machine boots.
const MIN_MEMORY_MIB: u32 = 64;

/// Renders `spec`, or refuses it with `InvalidSpec`.
///
/// Refused is what this driver can never run: firmware and macOS boots, isolation, dirty tracking, Rosetta, nested
/// virtualization, host console output, write-back disks, TAP and
/// file-handle NICs, and NICs other than a `UserNat` optionally followed by
/// one `HostNat`.
///
/// A `HostNat` NIC needs the `vmnet` feature; without it the machine boots
/// without that NIC, as the daemon does today.
pub fn vmm_config(spec: &VmSpec) -> Result<VmmConfig> {
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
    let (networking, bridge_nic_mac) = nics(spec)?;
    Ok(VmmConfig {
        vcpu_count: spec.cpus,
        memory_size: u64::from(spec.memory_mib) * 1024 * 1024,
        kernel_path: image.clone(),
        kernel_cmdline: cmdline.clone(),
        initrd_path: initrd.clone(),
        enable_rosetta: false,
        nested_virt: false,
        serial_console: true,
        virtio_console: true,
        shared_dirs: spec
            .shares
            .iter()
            .map(|share| SharedDirConfig {
                host_path: share.host_path.clone(),
                tag: share.tag.clone(),
                read_only: share.read_only,
            })
            .collect(),
        networking,
        vsock: spec.vsock.is_some(),
        guest_cid: spec.vsock.as_ref().map(|v| v.guest_cid),
        balloon: spec.balloon,
        block_devices: spec
            .disks
            .iter()
            .map(|disk| BlockDeviceConfig {
                path: disk.path.clone(),
                read_only: disk.read_only,
            })
            .collect(),
        bridge_nic_mac,
        backend: VmBackend::Hv,
        debug_console_socket: spec.debug_console.clone(),
    })
}

fn refuse(spec: &VmSpec) -> Result<()> {
    if spec.memory_mib < MIN_MEMORY_MIB {
        return Err(unsupported("memory_mib must be at least 64"));
    }
    if spec.isolation != IsolationSpec::None {
        return Err(unsupported("isolation is not available"));
    }
    if spec.dirty_tracking {
        return Err(unsupported("dirty tracking is not available"));
    }
    if spec.rosetta {
        return Err(unsupported(
            "Rosetta can only be shared under Virtualization.framework",
        ));
    }
    if spec.nested_virt {
        return Err(unsupported("nested virtualization is not available"));
    }
    if spec.console != ConsoleSpec::Off {
        return Err(unsupported(
            "guest console output does not reach the host; use ConsoleSpec::Off",
        ));
    }
    if spec.disks.iter().any(|d| d.cache != CacheMode::Unsafe) {
        return Err(unsupported("disks support only CacheMode::Unsafe"));
    }
    Ok(())
}

/// `(networking, bridge NIC MAC)` from the spec's NICs.
fn nics(spec: &VmSpec) -> Result<(bool, Option<String>)> {
    match spec.nics.as_slice() {
        [] => Ok((false, None)),
        [primary] if primary.attachment == NicAttachment::UserNat => Ok((true, None)),
        [primary, bridge]
            if primary.attachment == NicAttachment::UserNat
                && bridge.attachment == NicAttachment::HostNat =>
        {
            Ok((true, Some(bridge.mac.to_string())))
        }
        _ => Err(unsupported(
            "NICs must be one UserNat, optionally followed by one HostNat",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use arcbox_vm_driver::{DiskSpec, MacAddr, NicSpec, ShareSpec, VmId, VsockSpec};

    use super::*;

    fn spec() -> VmSpec {
        VmSpec {
            id: VmId::new("hv-render").unwrap(),
            cpus: 4,
            memory_mib: 2048,
            boot: BootSpec::Kernel {
                image: PathBuf::from("/boot/kernel"),
                cmdline: "console=hvc0".into(),
                initrd: None,
            },
            disks: Vec::new(),
            nics: Vec::new(),
            vsock: None,
            shares: Vec::new(),
            console: ConsoleSpec::Off,
            balloon: false,
            entropy: false,
            dirty_tracking: false,
            isolation: IsolationSpec::None,
            rosetta: false,
            nested_virt: false,
            debug_console: None,
        }
    }

    fn nic(id: &str, mac: &str, attachment: NicAttachment) -> NicSpec {
        NicSpec {
            id: id.into(),
            mac: mac.parse::<MacAddr>().unwrap(),
            attachment,
        }
    }

    #[test]
    fn renders_the_system_vm_shape_in_bus_order() {
        let mut spec = spec();
        spec.disks = ["data", "swap"]
            .into_iter()
            .map(|id| DiskSpec {
                id: id.into(),
                path: PathBuf::from(format!("/disks/{id}.img")),
                read_only: id == "swap",
                root: false,
                cache: CacheMode::Unsafe,
            })
            .collect();
        spec.shares = vec![ShareSpec {
            tag: "arcbox".into(),
            host_path: PathBuf::from("/Users/me"),
            read_only: true,
        }];
        spec.nics = vec![
            nic("nat", "02:00:00:00:00:01", NicAttachment::UserNat),
            nic("bridge", "02:ab:cd:ef:01:23", NicAttachment::HostNat),
        ];
        spec.vsock = Some(VsockSpec { guest_cid: 7 });
        spec.balloon = true;
        spec.debug_console = Some(PathBuf::from("/tmp/console.sock"));

        let config = vmm_config(&spec).unwrap();

        assert_eq!(config.vcpu_count, 4);
        assert_eq!(config.memory_size, 2048 * 1024 * 1024);
        assert_eq!(config.kernel_cmdline, "console=hvc0");
        let disks: Vec<_> = config
            .block_devices
            .iter()
            .map(|d| (d.path.clone(), d.read_only))
            .collect();
        assert_eq!(
            disks,
            [
                (PathBuf::from("/disks/data.img"), false),
                (PathBuf::from("/disks/swap.img"), true)
            ]
        );
        assert_eq!(config.shared_dirs[0].tag, "arcbox");
        assert!(config.shared_dirs[0].read_only);
        assert!(config.networking);
        assert_eq!(config.bridge_nic_mac.as_deref(), Some("02:ab:cd:ef:01:23"));
        assert_eq!((config.vsock, config.guest_cid), (true, Some(7)));
        assert!(config.balloon);
        assert_eq!(config.backend, VmBackend::Hv);
        assert_eq!(
            config.debug_console_socket,
            Some(PathBuf::from("/tmp/console.sock"))
        );
        assert!(!config.enable_rosetta && !config.nested_virt);
    }

    #[test]
    fn a_vm_without_nics_or_vsock_has_neither() {
        let config = vmm_config(&spec()).unwrap();
        assert!(!config.networking);
        assert_eq!(config.bridge_nic_mac, None);
        assert_eq!((config.vsock, config.guest_cid), (false, None));
    }

    #[test]
    fn refuses_what_the_hv_machine_cannot_run() {
        type Edit = fn(&mut VmSpec);
        let cases: [(&str, Edit); 9] = [
            ("rosetta", |s| s.rosetta = true),
            ("nested", |s| s.nested_virt = true),
            ("console", |s| s.console = ConsoleSpec::Pipe),
            ("dirty", |s| s.dirty_tracking = true),
            ("memory", |s| s.memory_mib = 63),
            ("firmware", |s| {
                s.boot = BootSpec::Firmware {
                    image: PathBuf::from("/fw"),
                };
            }),
            ("bridge first", |s| {
                s.nics = vec![
                    nic("bridge", "02:00:00:00:00:02", NicAttachment::HostNat),
                    nic("nat", "02:00:00:00:00:01", NicAttachment::UserNat),
                ];
            }),
            ("tap", |s| {
                let tap = NicAttachment::Tap {
                    name: "tap0".into(),
                };
                s.nics = vec![nic("tap", "02:00:00:00:00:01", tap)];
            }),
            ("writeback", |s| {
                s.disks = vec![DiskSpec {
                    id: "data".into(),
                    path: PathBuf::from("/disks/data.img"),
                    read_only: false,
                    root: false,
                    cache: CacheMode::Writeback,
                }];
            }),
        ];
        for (name, apply) in cases {
            let mut spec = spec();
            apply(&mut spec);
            assert!(
                matches!(
                    vmm_config(&spec),
                    Err(arcbox_vm_driver::Error::InvalidSpec(_))
                ),
                "{name} was not refused"
            );
        }
    }
}
