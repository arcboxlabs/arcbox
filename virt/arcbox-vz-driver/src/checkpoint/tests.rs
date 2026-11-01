use std::path::PathBuf;

use arcbox_vm_driver::{
    BootSpec, CacheMode, CheckpointFormat, CheckpointImage, CheckpointKind, ConsoleSpec, DiskSpec,
    Error, IsolationSpec, MacAddr, NicAttachment, NicSpec, RestoreSpec, VmId, VmSpec,
};

use super::{FORMAT, Manifest, disk_clone};

fn disk(id: &str, path: &str, read_only: bool) -> DiskSpec {
    DiskSpec {
        id: id.into(),
        path: path.into(),
        read_only,
        root: id == "rootfs",
        cache: CacheMode::Unsafe,
    }
}

fn nic(last: u8) -> NicSpec {
    NicSpec {
        id: "eth0".into(),
        mac: MacAddr::new([0x02, 0xab, 0, 0, 0, last]),
        attachment: NicAttachment::UserNat,
    }
}

fn spec() -> VmSpec {
    VmSpec {
        id: VmId::new("src").unwrap(),
        cpus: 2,
        memory_mib: 512,
        boot: BootSpec::Kernel {
            image: "/k".into(),
            cmdline: "console=hvc0".into(),
            initrd: None,
        },
        disks: vec![
            disk("rootfs", "/src/rootfs.img", false),
            disk("base", "/src/base.img", true),
        ],
        nics: vec![nic(1)],
        vsock: None,
        shares: vec![],
        console: ConsoleSpec::Pipe,
        balloon: false,
        entropy: true,
        dirty_tracking: false,
        isolation: IsolationSpec::None,
        rosetta: false,
        nested_virt: false,
        debug_console: None,
    }
}

fn manifest() -> Manifest {
    Manifest::new(&spec(), &[0x00, 0x0f, 0xa0, 0xff]).unwrap()
}

fn restore(disks: Vec<DiskSpec>, nics: Vec<NicSpec>) -> RestoreSpec {
    RestoreSpec {
        id: VmId::new("dst").unwrap(),
        nics,
        disks,
        isolation: IsolationSpec::None,
    }
}

fn image(dir: PathBuf) -> CheckpointImage {
    CheckpointImage {
        dir,
        format: CheckpointFormat::new(FORMAT),
        kind: CheckpointKind::Full,
    }
}

/// Disks are matched by id, not position, and keep their saved flags; the
/// id and NICs are the restore's.
#[test]
fn restore_spec_takes_paths_by_id_and_keeps_the_rest() {
    // Named in the opposite order and with flags the checkpoint did not
    // have: only the paths may change.
    let spec = manifest()
        .restore_spec(restore(
            vec![
                disk("base", "/dst/base.img", false),
                disk("rootfs", "/dst/rootfs.img", true),
            ],
            vec![NicSpec {
                attachment: NicAttachment::HostNat,
                ..nic(1)
            }],
        ))
        .unwrap();
    assert_eq!(spec.id.as_str(), "dst");
    assert_eq!(spec.nics[0].attachment, NicAttachment::HostNat);
    let disks: Vec<_> = spec
        .disks
        .iter()
        .map(|d| (d.id.as_str(), d.path.to_str().unwrap(), d.read_only))
        .collect();
    assert_eq!(
        disks,
        [
            ("rootfs", "/dst/rootfs.img", false),
            ("base", "/dst/base.img", true)
        ]
    );
}

#[test]
fn restore_spec_refuses_missing_extra_disks_other_nics_and_new_macs() {
    let m = manifest();
    let only_root = vec![disk("rootfs", "/dst/rootfs.img", false)];
    let both = || vec![disk("rootfs", "/r", false), disk("base", "/b", true)];
    let mut extra = both();
    extra.push(disk("scratch", "/dst/scratch.img", false));
    for (case, r) in [
        ("missing disk", restore(only_root, vec![nic(1)])),
        ("extra disk", restore(extra, vec![nic(1)])),
        ("no NIC", restore(both(), vec![])),
        ("another MAC", restore(both(), vec![nic(2)])),
    ] {
        assert!(
            matches!(m.restore_spec(r), Err(Error::InvalidSpec(_))),
            "{case}"
        );
    }
}

#[test]
fn manifest_round_trips_through_its_image() {
    let dir = tempfile::tempdir().unwrap();
    let written = manifest();
    written.write(dir.path()).unwrap();
    let read = Manifest::read(&image(dir.path().into())).unwrap();
    assert_eq!(read, written);
    assert_eq!(read.machine_identifier().unwrap(), [0x00, 0x0f, 0xa0, 0xff]);
}

#[test]
fn another_macos_build_is_a_precondition_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = manifest();
    other.os_build = format!("{}X", other.os_build);
    other.write(dir.path()).unwrap();
    match Manifest::read(&image(dir.path().into())) {
        Err(Error::PreconditionFailed(reason)) => {
            assert!(reason.contains(&other.os_build), "{reason}");
        }
        other => panic!("expected PreconditionFailed, got {other:?}"),
    }
}

#[test]
fn foreign_formats_are_refused() {
    let mut fc = image("/nonexistent".into());
    fc.format = CheckpointFormat::new("firecracker/v1");
    assert!(matches!(
        Manifest::read(&fc),
        Err(Error::ForeignCheckpoint(f)) if f.as_str() == "firecracker/v1"
    ));
}

/// Only writable disks are cloned, as of the call; a later call replaces.
#[test]
fn clone_disks_copies_writable_disks_only() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let mut s = spec();
    for d in &mut s.disks {
        d.path = src.path().join(format!("{}.img", d.id));
        std::fs::write(&d.path, d.id.as_bytes()).unwrap();
    }
    super::clone_disks(&s, dst.path()).unwrap();
    std::fs::write(&s.disks[0].path, b"written after").unwrap();
    assert_eq!(
        std::fs::read(disk_clone(dst.path(), "rootfs")).unwrap(),
        b"rootfs"
    );
    assert!(!disk_clone(dst.path(), "base").exists());
    super::clone_disks(&s, dst.path()).unwrap();
    assert_eq!(
        std::fs::read(disk_clone(dst.path(), "rootfs")).unwrap(),
        b"written after"
    );
}
