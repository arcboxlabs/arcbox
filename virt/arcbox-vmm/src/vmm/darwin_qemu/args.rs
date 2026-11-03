//! The QEMU command line, as a pure function of what the machine decided.
//!
//! Every device sits at a fixed PCI slot (`addr=`) on the virt machine's
//! root bus. Linux probes that bus in slot order, so the block devices'
//! slots alone fix the guest's names: `vda` the read-only rootfs, `vdb` the
//! btrfs data image, `vdc` the ext4 metadata image — the same order VZ and
//! HV present.

use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::Path;

// Each vhost-user-fs device's queue layout is the back-end's.
use arcbox_vhost_user::fs::{QUEUE_SIZE as FS_QUEUE_SIZE, REQUEST_QUEUES as FS_REQUEST_QUEUES};

use crate::vmm::BlockDeviceConfig;

/// PCI slot of NIC1 (`eth0`).
const NET_SLOT: u8 = 0x01;
/// PCI slot of the first block device; the rest follow in config order.
const FIRST_DISK_SLOT: u8 = 0x02;
/// The System VM attaches three disks; leave room for one more.
const MAX_DISKS: u8 = 4;
/// PCI slot of the virtio-serial controller carrying `hvc0` and `hvc1`.
const SERIAL_SLOT: u8 = FIRST_DISK_SLOT + MAX_DISKS;
/// PCI slot of the entropy device.
const RNG_SLOT: u8 = SERIAL_SLOT + 1;
/// PCI slot of the vhost-user vsock device.
const VSOCK_SLOT: u8 = RNG_SLOT + 1;
/// PCI slot of the bridge NIC (`eth1`). Any slot after NIC1's keeps the
/// guest's probe order, and so its interface names.
const BRIDGE_NET_SLOT: u8 = VSOCK_SLOT + 1;
/// PCI slot of the first vhost-user-fs share; the rest follow in order.
const FIRST_SHARE_SLOT: u8 = BRIDGE_NET_SLOT + 1;
/// The root bus ends at slot 0x1f.
const LAST_SLOT: u8 = 0x1f;

/// The guest CPU: its model and every property, in this one place.
///
/// QARMA5 pointer authentication costs every kernel function entry and
/// exit under TCG; the implementation-defined algorithm is far cheaper.
///
/// `dic=off` exists only in ArcBox's QEMU (arcboxlabs/qemu). It clears
/// `CTR_EL0.DIC`, so Linux issues `IC IVAU` for every page-cache page it
/// first maps executable, and that build makes `IC IVAU` drop QEMU's
/// translations of the page. Without both, code the virtio-fs back-end
/// writes into guest RAM, which TCG never sees, runs as whatever the page
/// held before: the agent executed from the `arcbox` share runs stale code.
/// [`cpu_check_args`] lets the machine refuse a QEMU without the property.
const CPU: &str = "max,pauth-impdef=on,dic=off";

/// A virtio-fs share served by a vhost-user back-end over `chardev_fd`.
#[derive(Debug, Clone)]
pub(super) struct FsShare {
    /// The tag the guest mounts.
    pub(super) tag: String,
    /// QEMU's end of the back-end's socketpair.
    pub(super) chardev_fd: RawFd,
}

/// The bridge NIC: frames of the daemon's vmnet relay over `fd`.
#[derive(Debug, Clone)]
pub(super) struct BridgeNet<'a> {
    /// QEMU's end of the relay's socketpair.
    pub(super) fd: RawFd,
    /// The vmnet interface's MAC, which the host looks the guest up by.
    pub(super) mac: &'a str,
}

/// What the command line names. Every fd is one QEMU inherits at that
/// number.
#[derive(Debug, Clone)]
pub(super) struct QemuArgs<'a> {
    pub(super) kernel: &'a Path,
    pub(super) initrd: Option<&'a Path>,
    pub(super) cmdline: &'a str,
    pub(super) vcpus: u32,
    pub(super) memory_mib: u64,
    pub(super) pidfile: &'a Path,
    /// QEMU's end of the QMP socketpair.
    pub(super) qmp_fd: RawFd,
    /// QEMU's ends of the `hvc0` and `hvc1` socketpairs.
    pub(super) console_fds: [RawFd; 2],
    /// NIC1's frame socket; `None` boots without networking.
    pub(super) net_fd: Option<RawFd>,
    /// NIC2; `None` when vmnet is unavailable or networking is off.
    pub(super) bridge_net: Option<BridgeNet<'a>>,
    /// In guest order: `vda`, `vdb`, ...
    pub(super) disks: &'a [BlockDeviceConfig],
    /// QEMU's end of the vhost-user vsock back-end's socketpair.
    pub(super) vsock_fd: Option<RawFd>,
    pub(super) shares: &'a [FsShare],
}

/// Renders `args` as `qemu-system-aarch64`'s argument vector.
///
/// # Errors
///
/// Returns the reason when the devices do not fit the root bus.
pub(super) fn qemu_args(args: &QemuArgs<'_>) -> Result<Vec<OsString>, String> {
    if args.disks.len() > usize::from(MAX_DISKS) {
        return Err(format!(
            "the QEMU backend attaches at most {MAX_DISKS} disks, got {}",
            args.disks.len()
        ));
    }
    if args.shares.len() > usize::from(LAST_SLOT - FIRST_SHARE_SLOT + 1) {
        return Err(format!(
            "too many virtio-fs shares for the QEMU backend: {}",
            args.shares.len()
        ));
    }

    let mut out = Vec::new();
    // Guest RAM is shareable POSIX memory so vhost-user back-ends can map it;
    // TCG runs on it as fast as on anonymous memory.
    flag(
        &mut out,
        "-object",
        format!(
            "memory-backend-shm,id=ram,size={}M,share=on",
            args.memory_mib
        ),
    );
    // GICv3: the virt machine's default GICv2 stops at 8 vCPUs.
    flag(
        &mut out,
        "-machine",
        "virt,gic-version=3,memory-backend=ram",
    );
    flag(&mut out, "-m", format!("{}M", args.memory_mib));
    flag(&mut out, "-accel", "tcg,thread=multi");
    flag(&mut out, "-cpu", CPU);
    flag(&mut out, "-smp", args.vcpus.to_string());
    out.extend(
        [
            "-nodefaults",
            "-no-user-config",
            "-display",
            "none",
            // A guest reset ends the process like HV's PSCI SYSTEM_RESET
            // ends its vCPUs; the lifecycle then reboots the machine.
            "-no-reboot",
        ]
        .map(OsString::from),
    );
    flag(&mut out, "-pidfile", args.pidfile);
    flag(&mut out, "-kernel", args.kernel);
    if let Some(initrd) = args.initrd {
        flag(&mut out, "-initrd", initrd);
    }
    flag(&mut out, "-append", args.cmdline.to_owned());
    flag(
        &mut out,
        "-chardev",
        format!("socket,id=qmp,fd={}", args.qmp_fd),
    );
    flag(&mut out, "-mon", "chardev=qmp,mode=control");

    if let Some(fd) = args.net_fd {
        flag(
            &mut out,
            "-netdev",
            format!("dgram,id=net0,local.type=fd,local.str={fd}"),
        );
        // `romfile=` skips the PXE option ROM, which the QEMU we ship omits.
        flag(
            &mut out,
            "-device",
            format!("virtio-net-pci,netdev=net0,romfile=,addr={NET_SLOT:#04x}"),
        );
    }
    for (index, (slot, disk)) in (FIRST_DISK_SLOT..).zip(args.disks).enumerate() {
        let mut drive = OsString::from("file=");
        drive.push(escape_option(disk.path.as_os_str()));
        drive.push(format!(",if=none,id=disk{index},format=raw"));
        drive.push(if disk.read_only {
            ",readonly=on"
        } else {
            // Guest discards punch holes in the sparse image.
            ",discard=unmap"
        });
        flag(&mut out, "-drive", drive);
        flag(
            &mut out,
            "-device",
            format!("virtio-blk-pci,drive=disk{index},addr={slot:#04x}"),
        );
    }
    flag(
        &mut out,
        "-device",
        format!("virtio-serial-pci,id=serial0,max_ports=2,addr={SERIAL_SLOT:#04x}"),
    );
    for (port, fd) in args.console_fds.iter().enumerate() {
        flag(&mut out, "-chardev", format!("socket,id=hvc{port},fd={fd}"));
        flag(
            &mut out,
            "-device",
            format!("virtconsole,bus=serial0.0,nr={port},chardev=hvc{port}"),
        );
    }
    flag(
        &mut out,
        "-device",
        format!("virtio-rng-pci,addr={RNG_SLOT:#04x}"),
    );
    if let Some(fd) = args.vsock_fd {
        flag(&mut out, "-chardev", format!("socket,id=vsock0,fd={fd}"));
        flag(
            &mut out,
            "-device",
            format!("vhost-user-vsock-pci,chardev=vsock0,addr={VSOCK_SLOT:#04x}"),
        );
    }
    if let Some(bridge) = &args.bridge_net {
        flag(
            &mut out,
            "-netdev",
            format!("dgram,id=net1,local.type=fd,local.str={}", bridge.fd),
        );
        flag(
            &mut out,
            "-device",
            format!(
                "virtio-net-pci,netdev=net1,mac={},romfile=,addr={BRIDGE_NET_SLOT:#04x}",
                bridge.mac
            ),
        );
    }
    for (index, (slot, share)) in (FIRST_SHARE_SLOT..).zip(args.shares).enumerate() {
        flag(
            &mut out,
            "-chardev",
            format!("socket,id=fs{index},fd={}", share.chardev_fd),
        );
        flag(
            &mut out,
            "-device",
            format!(
                "vhost-user-fs-pci,chardev=fs{index},tag={},queue-size={FS_QUEUE_SIZE},\
                 num-request-queues={FS_REQUEST_QUEUES},addr={slot:#04x}",
                escape_option(std::ffi::OsStr::new(&share.tag)).to_string_lossy()
            ),
        );
    }
    Ok(out)
}

/// A command line that only creates [`CPU`], on QEMU's empty `none`
/// machine, and then serves QMP on `qmp_fd`. A QEMU that lacks a property
/// [`CPU`] sets exits at once with "Property ... not found"; one that has
/// them all waits for a QMP `quit`.
pub(super) fn cpu_check_args(qmp_fd: RawFd) -> Vec<OsString> {
    let mut out = Vec::new();
    flag(&mut out, "-machine", "none");
    flag(&mut out, "-accel", "tcg");
    flag(&mut out, "-cpu", CPU);
    out.extend(["-nodefaults", "-no-user-config", "-display", "none", "-S"].map(OsString::from));
    flag(&mut out, "-chardev", format!("socket,id=qmp,fd={qmp_fd}"));
    flag(&mut out, "-mon", "chardev=qmp,mode=control");
    out
}

/// Appends `name` and its value.
fn flag(out: &mut Vec<OsString>, name: &str, value: impl Into<OsString>) {
    out.push(name.into());
    out.push(value.into());
}

/// Doubles every comma, QEMU's escape inside an option value.
fn escape_option(value: &std::ffi::OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut escaped = Vec::with_capacity(value.len());
    for &byte in value.as_bytes() {
        escaped.push(byte);
        if byte == b',' {
            escaped.push(b',');
        }
    }
    OsString::from_vec(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn disk(path: &str, read_only: bool) -> BlockDeviceConfig {
        BlockDeviceConfig {
            path: PathBuf::from(path),
            read_only,
        }
    }

    fn render(args: &QemuArgs<'_>) -> Vec<String> {
        qemu_args(args)
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    /// The value following each occurrence of `flag`.
    fn values<'a>(argv: &'a [String], flag: &str) -> Vec<&'a str> {
        argv.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .collect()
    }

    fn system_vm<'a>(disks: &'a [BlockDeviceConfig], shares: &'a [FsShare]) -> QemuArgs<'a> {
        QemuArgs {
            kernel: Path::new("/boot/kernel"),
            initrd: None,
            cmdline: "console=hvc0 root=/dev/vda ro",
            vcpus: 12,
            memory_mib: 4096,
            pidfile: Path::new("/data/run/qemu/vm.pid"),
            qmp_fd: 7,
            console_fds: [8, 9],
            net_fd: Some(10),
            bridge_net: Some(BridgeNet {
                fd: 13,
                mac: "02:ab:cd:00:00:01",
            }),
            disks,
            vsock_fd: Some(11),
            shares,
        }
    }

    #[test]
    fn disks_take_consecutive_slots_in_guest_order_and_only_the_rootfs_is_read_only() {
        let disks = [
            disk("/boot/rootfs.erofs", true),
            disk("/data/docker.img", false),
            disk("/data/docker-meta.img", false),
        ];
        let argv = render(&system_vm(&disks, &[]));
        assert_eq!(
            values(&argv, "-drive"),
            [
                "file=/boot/rootfs.erofs,if=none,id=disk0,format=raw,readonly=on",
                "file=/data/docker.img,if=none,id=disk1,format=raw,discard=unmap",
                "file=/data/docker-meta.img,if=none,id=disk2,format=raw,discard=unmap",
            ]
        );
        let blk: Vec<_> = values(&argv, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-blk-pci"))
            .collect();
        assert_eq!(
            blk,
            [
                "virtio-blk-pci,drive=disk0,addr=0x02",
                "virtio-blk-pci,drive=disk1,addr=0x03",
                "virtio-blk-pci,drive=disk2,addr=0x04",
            ]
        );
    }

    #[test]
    fn every_device_has_a_distinct_slot() {
        let disks = [
            disk("/a", true),
            disk("/b", false),
            disk("/c", false),
            disk("/d", false),
        ];
        let shares: Vec<_> = (0..3)
            .map(|i| FsShare {
                tag: format!("share{i}"),
                chardev_fd: 20 + i,
            })
            .collect();
        let argv = render(&system_vm(&disks, &shares));
        let mut slots: Vec<_> = values(&argv, "-device")
            .into_iter()
            .filter_map(|d| d.split(",addr=").nth(1))
            .collect();
        let count = slots.len();
        assert_eq!(
            count,
            1 + 4 + 1 + 1 + 1 + 1 + 3,
            "net, disks, serial, rng, vsock, bridge net, shares"
        );
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), count);
    }

    #[test]
    fn inherited_fds_are_named_where_qemu_expects_them() {
        let shares = [FsShare {
            tag: "arcbox".into(),
            chardev_fd: 12,
        }];
        let argv = render(&system_vm(&[], &shares));
        assert_eq!(
            values(&argv, "-chardev"),
            [
                "socket,id=qmp,fd=7",
                "socket,id=hvc0,fd=8",
                "socket,id=hvc1,fd=9",
                "socket,id=vsock0,fd=11",
                "socket,id=fs0,fd=12",
            ]
        );
        assert_eq!(
            values(&argv, "-netdev"),
            [
                "dgram,id=net0,local.type=fd,local.str=10",
                "dgram,id=net1,local.type=fd,local.str=13",
            ]
        );
        let devices = values(&argv, "-device");
        assert!(devices.contains(&"virtconsole,bus=serial0.0,nr=0,chardev=hvc0"));
        assert!(devices.contains(&"virtconsole,bus=serial0.0,nr=1,chardev=hvc1"));
        assert!(devices.contains(&"vhost-user-vsock-pci,chardev=vsock0,addr=0x08"));
        assert!(
            devices.contains(&"vhost-user-fs-pci,chardev=fs0,tag=arcbox,queue-size=1024,num-request-queues=1,addr=0x0a")
        );
    }

    #[test]
    fn guest_ram_is_shareable_memory_of_the_configured_size() {
        let argv = render(&system_vm(&[], &[]));
        assert_eq!(
            values(&argv, "-object"),
            ["memory-backend-shm,id=ram,size=4096M,share=on"]
        );
        assert_eq!(
            values(&argv, "-machine"),
            ["virt,gic-version=3,memory-backend=ram"]
        );
        assert_eq!(values(&argv, "-m"), ["4096M"]);
        assert_eq!(values(&argv, "-smp"), ["12"]);
    }

    #[test]
    fn kernel_cmdline_pidfile_and_reset_behaviour() {
        let argv = render(&system_vm(&[], &[]));
        assert_eq!(values(&argv, "-kernel"), ["/boot/kernel"]);
        assert_eq!(values(&argv, "-append"), ["console=hvc0 root=/dev/vda ro"]);
        assert_eq!(values(&argv, "-pidfile"), ["/data/run/qemu/vm.pid"]);
        assert!(argv.iter().any(|a| a == "-no-reboot"));
        assert!(argv.iter().any(|a| a == "-nodefaults"));
        assert!(values(&argv, "-initrd").is_empty());

        let with_initrd = QemuArgs {
            initrd: Some(Path::new("/boot/initrd")),
            ..system_vm(&[], &[])
        };
        assert_eq!(values(&render(&with_initrd), "-initrd"), ["/boot/initrd"]);
    }

    /// QEMU fetches `efi-virtio.rom` for a NIC unless told otherwise, and
    /// the QEMU ArcBox ships has no firmware directory.
    #[test]
    fn nic_skips_the_option_rom() {
        let argv = render(&system_vm(&[], &[]));
        assert!(
            values(&argv, "-device").contains(&"virtio-net-pci,netdev=net0,romfile=,addr=0x01")
        );

        let mut offline = system_vm(&[], &[]);
        offline.net_fd = None;
        offline.bridge_net = None;
        let argv = render(&offline);
        assert!(values(&argv, "-netdev").is_empty());
        assert!(
            !values(&argv, "-device")
                .iter()
                .any(|d| d.starts_with("virtio-net"))
        );
    }

    /// NIC1 keeps QEMU's generated MAC, which nothing looks up; NIC2 carries
    /// vmnet's, which the host finds the guest's bridge by. It sits after
    /// NIC1 so the guest still names it `eth1`, and a missing vmnet drops
    /// only it.
    #[test]
    fn the_bridge_nic_carries_the_vmnet_mac_after_nic1() {
        let argv = render(&system_vm(&[], &[]));
        let nics: Vec<_> = values(&argv, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-net-pci"))
            .collect();
        assert_eq!(
            nics,
            [
                "virtio-net-pci,netdev=net0,romfile=,addr=0x01",
                "virtio-net-pci,netdev=net1,mac=02:ab:cd:00:00:01,romfile=,addr=0x09",
            ]
        );

        let without_vmnet = QemuArgs {
            bridge_net: None,
            ..system_vm(&[], &[])
        };
        let argv = render(&without_vmnet);
        assert_eq!(
            values(&argv, "-netdev"),
            ["dgram,id=net0,local.type=fd,local.str=10"]
        );
    }

    /// A comma ends a QEMU option value unless doubled; a data directory
    /// containing one must not split the `file=` value.
    #[test]
    fn commas_in_paths_are_escaped() {
        let disks = [disk("/Users/a,b/docker.img", false)];
        let argv = render(&system_vm(&disks, &[]));
        assert_eq!(
            values(&argv, "-drive"),
            ["file=/Users/a,,b/docker.img,if=none,id=disk0,format=raw,discard=unmap"]
        );
    }

    /// The check must try the very CPU the machine boots, `dic=off`
    /// included, or an unpatched QEMU passes it and runs stale code.
    #[test]
    fn the_cpu_check_creates_the_booted_cpu() {
        let booted = render(&system_vm(&[], &[]));
        let check: Vec<String> = cpu_check_args(5)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(values(&check, "-cpu"), values(&booted, "-cpu"));
        assert!(values(&check, "-cpu")[0].split(',').any(|p| p == "dic=off"));
        assert_eq!(values(&check, "-machine"), ["none"]);
        assert_eq!(values(&check, "-chardev"), ["socket,id=qmp,fd=5"]);
    }

    #[test]
    fn too_many_disks_is_an_error() {
        let disks: Vec<_> = (0..5).map(|i| disk(&format!("/d{i}"), false)).collect();
        assert!(qemu_args(&system_vm(&disks, &[])).is_err());
    }
}
