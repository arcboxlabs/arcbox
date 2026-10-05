use super::*;
use tempfile::tempdir;

fn test_machine_manager(data_dir: &std::path::Path) -> MachineManager {
    test_machine_manager_with_bus(data_dir, crate::event::EventBus::new())
}

#[tokio::test]
async fn storage_reservation_blocks_direct_mutations_but_allows_recovery_machine() {
    let dir = tempdir().unwrap();
    let manager = Arc::new(test_machine_manager(dir.path()));
    manager.create(MachineConfig::default()).await.unwrap();
    let reservation = manager.reserve_storage().unwrap();
    assert!(manager.reserve_storage().is_err());
    assert!(
        manager
            .start("default")
            .await
            .unwrap_err()
            .to_string()
            .contains("held for recovery")
    );
    assert!(manager.reboot("default").is_err());
    assert!(manager.remove("default", true).is_err());
    assert!(manager.set_resources("default", Some(1), None).is_err());
    manager
        .create(MachineConfig {
            name: "storage-check".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let boot_owner = reservation.clone();
    drop(reservation);
    assert!(manager.ensure_storage_available("default").is_err());
    drop(boot_owner);
    manager.ensure_storage_available("default").unwrap();
}

#[test]
fn storage_hold_survives_manager_restart() {
    let dir = tempdir().unwrap();
    let manager = test_machine_manager(dir.path());
    let hold = manager.storage_hold_path();
    std::fs::create_dir_all(hold.parent().unwrap()).unwrap();
    std::fs::write(&hold, b"check required").unwrap();
    drop(manager);
    let manager = test_machine_manager(dir.path());
    assert!(manager.ensure_storage_available("default").is_err());
    manager.ensure_storage_available("storage-check").unwrap();
}

#[tokio::test]
async fn direct_start_rejects_unpaired_system_vm_before_starting_process() {
    let dir = tempdir().unwrap();
    let manager = Arc::new(test_machine_manager(dir.path()));
    manager.create(MachineConfig::default()).await.unwrap();
    assert!(
        manager
            .start("default")
            .await
            .unwrap_err()
            .to_string()
            .contains("paired data")
    );
    assert_eq!(manager.get("default").unwrap().state, MachineState::Created);
}

fn test_machine_manager_with_bus(
    data_dir: &std::path::Path,
    event_bus: crate::event::EventBus,
) -> MachineManager {
    let vm_manager = Arc::new(VmManager::new(data_dir.join("snapshots")));
    MachineManager::new(
        vm_manager,
        data_dir.to_path_buf(),
        HostNetwork::default(),
        event_bus,
    )
}

#[tokio::test]
async fn create_publishes_machine_created_for_user_machines_only() {
    use crate::event::{Event, EventBus};

    let temp_dir = tempdir().unwrap();
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let manager = test_machine_manager_with_bus(temp_dir.path(), bus);

    manager
        .create(MachineConfig {
            name: "work".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(rx.try_recv(), Ok(Event::MachineCreated { name }) if name == "work"),
        "creating a user machine must publish MachineCreated"
    );

    // The default System VM's lifecycle events come from its own lifecycle
    // actor; MachineManager must not double-publish for it.
    manager
        .create(MachineConfig {
            name: "default".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "creating the default machine must not publish from MachineManager"
    );
}

#[tokio::test]
async fn test_assign_cid_propagates_to_vm_config() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    let name = machine_manager
        .create(MachineConfig {
            name: "cid-test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let (vm_id, cid) = machine_manager.assign_cid_for_start(&name).unwrap();
    assert_eq!(cid, 3);
    assert_eq!(
        machine_manager.vm_manager.guest_cid_for_test(&vm_id),
        Some(cid)
    );
}

#[test]
fn test_register_mock_machine() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    machine_manager
        .register_mock_machine("test-mock", 42)
        .unwrap();

    let machine = machine_manager
        .get("test-mock")
        .expect("machine should exist");
    assert_eq!(machine.name, "test-mock");
    assert_eq!(machine.cid, Some(42));
    assert_eq!(machine.state, MachineState::Running);
}

#[test]
fn test_register_mock_machine_idempotent() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    machine_manager
        .register_mock_machine("test-idempotent", 10)
        .unwrap();
    machine_manager
        .register_mock_machine("test-idempotent", 20)
        .unwrap();

    let machine = machine_manager.get("test-idempotent").unwrap();
    assert_eq!(machine.cid, Some(10));
}

/// A force stop publishes `MachineStopping`, then waits for the host to
/// release what it holds of the machine before it touches the VM: the hold
/// here is dropped by another thread after a delay, and `stop` returns no
/// earlier. The mock machine has no VM, so the stop itself then fails —
/// that failure is the VM's, after the wait, and not what is under test.
#[test]
fn a_force_stop_waits_for_the_host_to_release_the_machine() {
    use crate::event::{Event, EventBus};

    let temp_dir = tempdir().unwrap();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let manager = Arc::new(test_machine_manager_with_bus(temp_dir.path(), bus));
    manager.register_mock_machine("held", 7).unwrap();

    let hold = manager.host_hold("held");
    let release_after = Duration::from_millis(300);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(release_after);
        drop(hold);
    });

    let started = std::time::Instant::now();
    let _ = manager.stop("held");
    let waited = started.elapsed();
    assert!(waited >= release_after, "stop returned after {waited:?}");
    assert!(
        matches!(events.try_recv(), Ok(Event::MachineStopping { name }) if name == "held"),
        "the holder learns of the stop before the wait"
    );
    releaser.join().unwrap();
}

#[tokio::test]
async fn connect_agent_distinguishes_missing_and_stopped_machines() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    let missing = machine_manager
        .connect_agent("missing")
        .err()
        .expect("missing machine must fail");
    assert!(matches!(
        &missing,
        EngineError::Common(arcbox_error::CommonError::NotFound(_))
    ));

    machine_manager
        .create(MachineConfig {
            name: "stopped".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let stopped = machine_manager
        .connect_agent("stopped")
        .err()
        .expect("stopped machine must fail");
    assert!(matches!(
        &stopped,
        EngineError::Common(arcbox_error::CommonError::InvalidState(_))
    ));
    assert!(
        stopped
            .to_string()
            .contains("machine 'stopped' is not running")
    );
}

#[test]
fn test_select_routable_ip_prefers_ipv4() {
    let ips = vec![
        "::1".to_string(),
        "fe80::1".to_string(),
        "2001:db8::10".to_string(),
        "10.0.2.2".to_string(),
    ];
    assert_eq!(select_routable_ip(&ips), Some("10.0.2.2".to_string()));
}

#[test]
fn test_select_routable_ip_falls_back_to_global_ipv6() {
    let ips = vec![
        "::1".to_string(),
        "fe80::2".to_string(),
        "2001:db8::42".to_string(),
    ];
    assert_eq!(select_routable_ip(&ips), Some("2001:db8::42".to_string()));
}

/// Two concurrent `create` calls with the same name must not both succeed.
/// One wins, the other returns `AlreadyExists`, and only one machine is
/// registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_concurrent_same_name_no_duplicate() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = Arc::new(test_machine_manager(temp_dir.path()));

    let name = "race-test";
    let mm1 = machine_manager.clone();
    let mm2 = machine_manager.clone();

    // `create` has no `.await` points, so without a barrier the first-polled
    // task can run to completion before the second is even scheduled.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();

    let t1 = tokio::spawn(async move {
        b1.wait().await;
        mm1.create(MachineConfig {
            name: name.to_string(),
            ..Default::default()
        })
        .await
    });
    let t2 = tokio::spawn(async move {
        b2.wait().await;
        mm2.create(MachineConfig {
            name: name.to_string(),
            ..Default::default()
        })
        .await
    });

    let r1 = t1.await.unwrap();
    let r2 = t2.await.unwrap();

    let (winner, loser) = match (r1, r2) {
        (Ok(n), Err(e)) | (Err(e), Ok(n)) => (n, e),
        (Ok(_), Ok(_)) => panic!("both creates succeeded — TOCTOU regression"),
        (Err(e1), Err(e2)) => panic!("both creates failed: {e1:?} / {e2:?}"),
    };
    assert_eq!(winner, name);
    match loser {
        EngineError::Common(ref c) if c.is_already_exists() => {}
        other => panic!("loser should be AlreadyExists, got {other:?}"),
    }

    let machines = machine_manager.list();
    assert_eq!(
        machines.len(),
        1,
        "exactly one machine should be registered"
    );
    assert_eq!(machines[0].name, name);
}

#[tokio::test]
async fn test_create_with_shim_assembles_boot_contract() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    let rootfs_img = temp_dir.path().join("rootfs.squashfs");
    std::fs::write(&rootfs_img, b"squash").unwrap();
    let shim_kernel = temp_dir.path().join("kernel");
    let shim_rootfs = temp_dir.path().join("shim.erofs");

    machine_manager
        .create(MachineConfig {
            name: "shimmed".to_string(),
            disk_gb: 1,
            rootfs: Some(MachineRootfs {
                path: rootfs_img.clone(),
                format: "squashfs".to_string(),
                shim: Some(BootShim {
                    kernel: shim_kernel.clone(),
                    rootfs: shim_rootfs.clone(),
                }),
            }),
            ..Default::default()
        })
        .await
        .unwrap();

    let machine = machine_manager.get("shimmed").unwrap();

    // Device contract: vda=shim EROFS ro, vdb=distro rootfs ro, vdc=data rw.
    let devices = &machine.block_devices;
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0].path, shim_rootfs.to_string_lossy());
    assert!(devices[0].read_only);
    assert_eq!(devices[1].path, rootfs_img.to_string_lossy());
    assert!(devices[1].read_only);
    assert!(devices[2].path.ends_with("data.img"));
    assert!(!devices[2].read_only);

    // The data disk was provisioned sparse at the requested size.
    let data_disk = machine.disk_path.as_ref().unwrap();
    assert_eq!(
        std::fs::metadata(data_disk).unwrap().len(),
        1024 * 1024 * 1024
    );

    // Kernel comes from the shim; cmdline follows the machine-init contract.
    assert_eq!(
        machine.kernel.as_deref(),
        Some(&*shim_kernel.to_string_lossy())
    );
    let cmdline = machine.cmdline.as_deref().unwrap();
    assert!(
        cmdline.contains("root=/dev/vda ro rootfstype=erofs"),
        "{cmdline}"
    );
    assert!(
        cmdline.contains(&format!(
            "init={}",
            arcbox_constants::cmdline::MACHINE_INIT_PATH
        )),
        "{cmdline}"
    );
    assert!(
        cmdline.contains(&format!(
            "{}/dev/vdb",
            arcbox_constants::cmdline::MACHINE_ROOTFS_KEY
        )),
        "{cmdline}"
    );
    assert!(
        cmdline.contains(&format!(
            "{}squashfs",
            arcbox_constants::cmdline::MACHINE_ROOTFS_TYPE_KEY
        )),
        "{cmdline}"
    );
    assert!(
        cmdline.contains(&format!(
            "{}/dev/vdc",
            arcbox_constants::cmdline::MACHINE_DATA_KEY
        )),
        "{cmdline}"
    );
    // The console is capped so a loud distro (Debian at console_loglevel 7)
    // does not trickle kernel audit records onto hvc0 forever.
    assert!(cmdline.contains("loglevel=4"), "{cmdline}");
    // The shim makes the name the guest's hostname.
    assert!(
        cmdline
            .split_whitespace()
            .any(|t| t == format!("{}shimmed", arcbox_constants::cmdline::MACHINE_NAME_KEY)),
        "{cmdline}"
    );
}

#[tokio::test]
async fn test_create_without_shim_boots_rootfs_directly() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    let rootfs_img = temp_dir.path().join("rootfs.squashfs");
    std::fs::write(&rootfs_img, b"squash").unwrap();

    // Without a shim, the rootfs alone cannot boot: an explicit kernel is
    // required (custom-kernel testing path).
    let err = machine_manager
        .create(MachineConfig {
            name: "plain-distro".to_string(),
            disk_gb: 1,
            rootfs: Some(MachineRootfs {
                path: rootfs_img.clone(),
                format: "squashfs".to_string(),
                shim: None,
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("explicit"), "{err}");

    machine_manager
        .create(MachineConfig {
            name: "plain-distro".to_string(),
            disk_gb: 1,
            kernel: Some("/custom/kernel".to_string()),
            rootfs: Some(MachineRootfs {
                path: rootfs_img.clone(),
                format: "squashfs".to_string(),
                shim: None,
            }),
            ..Default::default()
        })
        .await
        .unwrap();

    let machine = machine_manager.get("plain-distro").unwrap();
    let devices = &machine.block_devices;
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].path, rootfs_img.to_string_lossy());
    assert_eq!(machine.kernel.as_deref(), Some("/custom/kernel"));
    let cmdline = machine.cmdline.as_deref().unwrap();
    assert!(
        cmdline.contains("root=/dev/vda ro rootfstype=squashfs"),
        "{cmdline}"
    );
    assert!(!cmdline.contains("init="), "{cmdline}");
}

#[tokio::test]
async fn test_assign_cid_skips_cids_held_by_other_machines() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    machine_manager
        .register_mock_machine("holder-a", 3)
        .unwrap();
    machine_manager
        .register_mock_machine("holder-b", 5)
        .unwrap();

    let name = machine_manager
        .create(MachineConfig {
            name: "fresh".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let (_, cid) = machine_manager.assign_cid_for_start(&name).unwrap();
    assert_eq!(cid, 4, "lowest CID not held by another machine");
}

#[tokio::test]
async fn test_create_with_mounts_extends_cmdline_and_persists() {
    let temp_dir = tempdir().unwrap();
    let machine_manager = test_machine_manager(temp_dir.path());

    let rootfs_img = temp_dir.path().join("rootfs.squashfs");
    std::fs::write(&rootfs_img, b"squash").unwrap();
    let host_share = temp_dir.path().join("share");
    std::fs::create_dir(&host_share).unwrap();

    let shim = BootShim {
        kernel: temp_dir.path().join("kernel"),
        rootfs: temp_dir.path().join("shim.erofs"),
    };
    let mounts = vec![
        MachineMount {
            host_path: host_share.to_string_lossy().into_owned(),
            guest_path: "/work".to_string(),
            read_only: false,
        },
        MachineMount {
            host_path: host_share.to_string_lossy().into_owned(),
            guest_path: "/data".to_string(),
            read_only: true,
        },
    ];

    machine_manager
        .create(MachineConfig {
            name: "mounted".to_string(),
            disk_gb: 1,
            rootfs: Some(MachineRootfs {
                path: rootfs_img.clone(),
                format: "squashfs".to_string(),
                shim: Some(shim.clone()),
            }),
            mounts: mounts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    let machine = machine_manager.get("mounted").unwrap();
    let cmdline = machine.cmdline.as_deref().unwrap();
    assert!(
        cmdline.contains(&format!(
            "{}m0=/work,m1=/data:ro",
            arcbox_constants::cmdline::MACHINE_MOUNTS_KEY
        )),
        "{cmdline}"
    );
    assert_eq!(machine.mounts.len(), 2);
    assert_eq!(machine.mounts[1].guest_path, "/data");
    assert!(machine.mounts[1].read_only);

    // Mounts are rejected off the shim path and validated for separators.
    let err = machine_manager
        .create(MachineConfig {
            name: "no-shim-mounts".to_string(),
            mounts: mounts.clone(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("shim"), "{err}");

    let err = machine_manager
        .create(MachineConfig {
            name: "bad-guest-path".to_string(),
            disk_gb: 1,
            rootfs: Some(MachineRootfs {
                path: rootfs_img,
                format: "squashfs".to_string(),
                shim: Some(shim),
            }),
            mounts: vec![MachineMount {
                host_path: host_share.to_string_lossy().into_owned(),
                guest_path: "/with,comma".to_string(),
                read_only: false,
            }],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("','"), "{err}");
}

/// A machine whose distro init is still starting is NOT ready, even though
/// the agent answered and reported a usable address. This is the CORE-66
/// gate: the distro reconfigures the network from scratch after the agent
/// comes up, so an address observed before it settles does not mean the
/// machine is usable.
#[test]
fn readiness_waits_for_the_distro_init_to_settle() {
    let info = arcbox_connect::v1::SystemInfo {
        ip_addresses: vec!["10.0.2.2".to_owned()],
        distro_init_pending: true,
        ..Default::default()
    };
    assert_eq!(readiness_addresses(&info, "m", 1), None);
}

#[test]
fn readiness_reports_the_address_once_the_distro_init_has_settled() {
    let info = arcbox_connect::v1::SystemInfo {
        ip_addresses: vec!["10.0.2.2".to_owned()],
        distro_init_pending: false,
        ..Default::default()
    };
    assert_eq!(
        readiness_addresses(&info, "m", 1),
        Some(GuestAddresses {
            ip: "10.0.2.2".to_owned(),
            bridge_ip: None,
        })
    );
}

/// The bridge address rides along when the guest reports one, and an empty
/// report (no bridge NIC, no lease, or an agent predating the field) is
/// `None` rather than a blocker: readiness must not wait for an address
/// that is not coming.
#[test]
fn readiness_carries_the_bridge_address_when_reported() {
    let info = arcbox_connect::v1::SystemInfo {
        ip_addresses: vec!["10.0.2.2".to_owned(), "192.168.64.5".to_owned()],
        bridge_ip_address: "192.168.64.5".to_owned(),
        ..Default::default()
    };
    assert_eq!(
        readiness_addresses(&info, "m", 1),
        Some(GuestAddresses {
            ip: "10.0.2.2".to_owned(),
            bridge_ip: Some("192.168.64.5".to_owned()),
        })
    );
}

/// The proto3 default must be the pre-CORE-66 behaviour: an agent that
/// predates the field leaves `distro_init_pending` false, and readiness must
/// proceed exactly as before rather than waiting out the 60 s timeout on a
/// signal that agent will never send.
#[test]
fn an_agent_without_the_field_is_not_treated_as_pending() {
    let decoded = arcbox_connect::v1::SystemInfo::default();
    assert!(!decoded.distro_init_pending);

    let info = arcbox_connect::v1::SystemInfo {
        ip_addresses: vec!["10.0.2.2".to_owned()],
        ..Default::default()
    };
    assert_eq!(
        readiness_addresses(&info, "m", 1).map(|a| a.ip),
        Some("10.0.2.2".to_owned())
    );
}

/// A settled init with nothing usable to report still is not ready — the
/// gate is additive, it does not replace the address requirement.
#[test]
fn a_settled_init_without_a_usable_address_is_still_not_ready() {
    let info = arcbox_connect::v1::SystemInfo {
        ip_addresses: vec!["127.0.0.1".to_owned()],
        distro_init_pending: false,
        ..Default::default()
    };
    assert_eq!(readiness_addresses(&info, "m", 1), None);
}

/// The name becomes the guest's hostname and the label of its DNS record:
/// `_` and `.` turn into `-`, and what is left has to be a DNS label.
#[test]
fn machine_names_become_hostnames() {
    for (name, hostname) in [
        ("dev", "dev"),
        ("my-box-2", "my-box-2"),
        ("A1", "A1"),
        ("my_box.v2", "my-box-v2"),
        ("a.b_c", "a-b-c"),
    ] {
        assert_eq!(
            machine_hostname(name).ok().as_deref(),
            Some(hostname),
            "{name}"
        );
    }
    assert!(machine_hostname(&"x".repeat(63)).is_ok());
    for bad in [
        "",
        "-dev",
        "dev-",
        "_dev",
        "dev.",
        "a b",
        "a/b",
        &"x".repeat(64),
    ] {
        assert!(machine_hostname(bad).is_err(), "{bad:?}");
    }
}

/// Two names that map to one hostname would share one DNS record, with the
/// table keeping whichever machine started last; the second name is refused
/// and the error names the machine that holds the hostname.
#[tokio::test]
async fn a_name_whose_hostname_another_machine_has_is_refused() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    manager
        .create(MachineConfig {
            name: "my_box".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let err = manager
        .create(MachineConfig {
            name: "my-box".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("'my-box'") && message.contains("'my_box'"),
        "{message}"
    );
    assert!(manager.get("my-box").is_none());

    // A different hostname is still fine.
    manager
        .create(MachineConfig {
            name: "my-box-2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
}

/// A shim-booted distro machine with a data disk that holds some data, the
/// shape clone and export act on.
async fn create_shimmed(
    manager: &MachineManager,
    dir: &std::path::Path,
    name: &str,
) -> MachineInfo {
    let rootfs_img = dir.join("rootfs.squashfs");
    std::fs::write(&rootfs_img, b"squash").unwrap();
    manager
        .create(MachineConfig {
            name: name.to_string(),
            cpus: 2,
            memory_mb: 1536,
            disk_gb: 1,
            distro: Some("alpine".to_string()),
            distro_version: Some("3.24".to_string()),
            rootfs: Some(MachineRootfs {
                path: rootfs_img,
                format: "squashfs".to_string(),
                shim: Some(BootShim {
                    kernel: dir.join("kernel"),
                    rootfs: dir.join("shim.erofs"),
                }),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let machine = manager.get(name).unwrap();
    // Some blocks in the middle of the sparse disk, like a guest would leave.
    let disk = std::fs::OpenOptions::new()
        .write(true)
        .open(machine.disk_path.as_ref().unwrap())
        .unwrap();
    std::os::unix::fs::FileExt::write_all_at(&disk, &[0xAB; 8192], 4 * 1024 * 1024).unwrap();
    machine
}

#[tokio::test]
async fn a_clone_gets_its_own_identity_and_the_sources_data() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    let source = create_shimmed(&manager, temp_dir.path(), "src_box").await;

    assert_eq!(manager.clone_machine("src_box", "copy").unwrap(), "copy");
    let clone = manager.get("copy").unwrap();

    assert_eq!(clone.state, MachineState::Created);
    assert_ne!(clone.vm_id, source.vm_id, "a clone is its own VM");
    assert_eq!((clone.cpus, clone.memory_mb, clone.disk_gb), (2, 1536, 1));
    assert_eq!(clone.distro.as_deref(), Some("alpine"));
    assert_eq!(clone.kernel, source.kernel);

    // Its own data disk, holding the source's blocks, in its own directory.
    let disk = clone.disk_path.clone().unwrap();
    assert_eq!(disk, temp_dir.path().join("machines/copy/data.img"));
    assert_eq!(
        std::fs::read(&disk).unwrap(),
        std::fs::read(source.disk_path.as_ref().unwrap()).unwrap()
    );
    assert_eq!(clone.block_devices.len(), 3);
    assert_eq!(clone.block_devices[2].path, disk.to_string_lossy());
    assert_eq!(clone.block_devices[1].path, source.block_devices[1].path);

    // The guest is told the clone's name, not the source's.
    let cmdline = clone.cmdline.as_deref().unwrap();
    let name_token = format!("{}copy", arcbox_constants::cmdline::MACHINE_NAME_KEY);
    assert!(
        cmdline.split_whitespace().any(|t| t == name_token),
        "{cmdline}"
    );
    assert!(!cmdline.contains("src-box"), "{cmdline}");

    // Persisted, so it survives a daemon restart.
    let persisted = manager.persistence.load("copy").unwrap();
    assert_eq!(
        persisted.disk_path.as_deref(),
        Some(&*disk.to_string_lossy())
    );
    assert_eq!(persisted.cmdline.as_deref(), Some(cmdline));
}

#[tokio::test]
async fn clone_refuses_running_sources_taken_names_and_plain_vms() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    create_shimmed(&manager, temp_dir.path(), "src").await;
    manager
        .create(MachineConfig {
            name: "pl_ain".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let taken = manager.clone_machine("src", "pl_ain").unwrap_err();
    assert!(matches!(taken, EngineError::Common(ref c) if c.is_already_exists()));
    let same_hostname = manager.clone_machine("src", "pl.ain").unwrap_err();
    assert!(
        same_hostname.to_string().contains("hostname"),
        "{same_hostname}"
    );

    let missing = manager.clone_machine("nope", "copy").unwrap_err();
    assert!(matches!(missing, EngineError::Common(ref c) if c.is_not_found()));

    let no_disk = manager.clone_machine("pl_ain", "copy").unwrap_err();
    assert!(no_disk.to_string().contains("data disk"), "{no_disk}");

    manager
        .machines
        .write()
        .unwrap()
        .get_mut("src")
        .unwrap()
        .state = MachineState::Running;
    let running = manager.clone_machine("src", "copy").unwrap_err();
    assert!(running.to_string().contains("stop it first"), "{running}");
    assert!(manager.get("copy").is_none());
    assert!(!temp_dir.path().join("machines/copy").exists());
}

#[test]
fn cmdline_with_hostname_replaces_the_name_token_or_adds_one() {
    let key = arcbox_constants::cmdline::MACHINE_NAME_KEY;
    assert_eq!(
        clone::cmdline_with_hostname(&format!("console=hvc0 {key}old quiet"), "new"),
        format!("console=hvc0 {key}new quiet")
    );
    assert_eq!(
        clone::cmdline_with_hostname("console=hvc0 quiet", "new"),
        format!("console=hvc0 quiet {key}new")
    );
}

#[test]
fn clone_file_keeps_the_image_sparse() {
    let temp_dir = tempdir().unwrap();
    let src = temp_dir.path().join("data.img");
    let dst = temp_dir.path().join("copy.img");
    let file = std::fs::File::create(&src).unwrap();
    file.set_len(256 * 1024 * 1024).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &vec![7u8; 1 << 20], 100 << 20).unwrap();
    drop(file);

    clone_file(&src, &dst).unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dst).unwrap());
    // A clone of a sparse image shares its blocks; the destination's
    // allocation is far from its 256 MiB logical size.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt as _;
        let allocated = std::fs::metadata(&dst).unwrap().blocks() * 512;
        assert!(allocated < 4 << 20, "{allocated} bytes allocated");
    }
    assert!(clone_file(&src, &dst).is_err(), "the destination exists");
}

fn test_image_manifest() -> arcbox_image::machine_image::MachineImageManifest {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "name": "alpine-3.24-arm64",
        "version": "20260716_1300",
        "distro": "alpine",
        "release": "3.24",
        "release_title": "3.24",
        "arch": "arm64",
        "variant": "default",
        "upstream": {
            "server": "https://images.linuxcontainers.org",
            "product": "alpine:3.24:arm64:default",
            "version": "20260716_13:00"
        },
        "rootfs": { "path": "rootfs.squashfs", "format": "squashfs", "size": 6, "sha256": "ab" }
    }))
    .unwrap()
}

#[tokio::test]
async fn export_then_import_restores_the_machine_and_its_data() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    let source = create_shimmed(&manager, temp_dir.path(), "dev").await;
    let source_disk = std::fs::read(source.disk_path.as_ref().unwrap()).unwrap();
    let archive_path = temp_dir.path().join("dev.tar.zst");

    let size = manager
        .export("dev", &archive_path, test_image_manifest())
        .unwrap();
    assert_eq!(size, archive_path.metadata().unwrap().len());
    // The snapshot the archive was written from is gone with the export.
    assert!(
        std::fs::read_dir(temp_dir.path().join("machines"))
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.'))
    );

    let manifest = archive::read_manifest(&archive_path).unwrap();
    assert_eq!(manifest.machine.name, "dev");
    assert_eq!(
        (manifest.machine.cpus, manifest.machine.memory_mb),
        (2, 1536)
    );
    assert_eq!(manifest.machine.distro, "alpine");
    assert_eq!(manifest.image.version, "20260716_1300");

    manager.remove("dev", false).unwrap();
    assert!(manager.get("dev").is_none());

    // Import under another name: the caller turns the manifest into a config
    // the way `create` is called, with the rootfs resolved locally.
    let rootfs_img = temp_dir.path().join("rootfs.squashfs");
    let config = MachineConfig {
        name: "dev2".to_string(),
        cpus: manifest.machine.cpus,
        memory_mb: manifest.machine.memory_mb,
        disk_gb: manifest.machine.disk_gb,
        distro: Some(manifest.machine.distro.clone()),
        distro_version: manifest.machine.distro_version.clone(),
        rootfs: Some(MachineRootfs {
            path: rootfs_img,
            format: "squashfs".to_string(),
            shim: Some(BootShim {
                kernel: temp_dir.path().join("kernel"),
                rootfs: temp_dir.path().join("shim.erofs"),
            }),
        }),
        mounts: manifest.machine.mounts,
        ..Default::default()
    };
    assert_eq!(
        manager.import(config.clone(), &archive_path).unwrap(),
        "dev2"
    );

    let imported = manager.get("dev2").unwrap();
    assert_eq!(imported.state, MachineState::Created);
    assert_eq!(
        (imported.cpus, imported.memory_mb, imported.disk_gb),
        (2, 1536, 1)
    );
    let disk = imported.disk_path.clone().unwrap();
    assert_eq!(disk, temp_dir.path().join("machines/dev2/data.img"));
    assert_eq!(std::fs::read(&disk).unwrap(), source_disk);
    let cmdline = imported.cmdline.as_deref().unwrap();
    let name_token = format!("{}dev2", arcbox_constants::cmdline::MACHINE_NAME_KEY);
    assert!(
        cmdline.split_whitespace().any(|t| t == name_token),
        "{cmdline}"
    );

    // A taken name is refused before anything is extracted.
    let taken = manager.import(config, &archive_path).unwrap_err();
    assert!(matches!(taken, EngineError::Common(ref c) if c.is_already_exists()));
    assert!(
        std::fs::read_dir(temp_dir.path().join("machines"))
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.'))
    );
}

#[tokio::test]
async fn export_refuses_a_running_machine_and_stale_staging_is_swept() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    create_shimmed(&manager, temp_dir.path(), "dev").await;
    manager
        .machines
        .write()
        .unwrap()
        .get_mut("dev")
        .unwrap()
        .state = MachineState::Running;
    let err = manager
        .export(
            "dev",
            &temp_dir.path().join("dev.tar.zst"),
            test_image_manifest(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("stop it first"), "{err}");
    assert!(!temp_dir.path().join("dev.tar.zst").exists());

    // What a crashed daemon could leave behind goes on the next start.
    let stale = temp_dir.path().join("machines/.import-leftover");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("data.img"), b"partial").unwrap();
    drop(manager);
    let manager = test_machine_manager(temp_dir.path());
    assert!(!stale.exists());
    assert!(manager.get("dev").is_some());
}

#[tokio::test]
async fn resize_applies_to_the_next_start_and_says_when_a_restart_is_needed() {
    let temp_dir = tempdir().unwrap();
    let manager = test_machine_manager(temp_dir.path());
    let machine = create_shimmed(&manager, temp_dir.path(), "dev").await;

    // Stopped: the new size is what the next start builds the VM from.
    let resized = manager.set_resources("dev", Some(3), None).unwrap();
    assert_eq!(
        resized,
        MachineResize {
            cpus: 3,
            memory_mb: 1536,
            restart_required: false
        }
    );
    let vm = manager.vm_manager.get(&machine.vm_id).unwrap();
    assert_eq!((vm.cpus, vm.memory_mb), (3, 1536));
    let persisted = manager.persistence.load("dev").unwrap();
    assert_eq!((persisted.cpus, persisted.memory_mb), (3, 1536));
    assert_eq!(manager.get("dev").unwrap().cpus, 3);

    // Running: recorded for the next start, and the caller is told so.
    manager
        .machines
        .write()
        .unwrap()
        .get_mut("dev")
        .unwrap()
        .state = MachineState::Running;
    let resized = manager.set_resources("dev", None, Some(2048)).unwrap();
    assert!(resized.restart_required);
    assert_eq!((resized.cpus, resized.memory_mb), (3, 2048));
    assert_eq!(manager.persistence.load("dev").unwrap().memory_mb, 2048);
    // The same size again is a no-op, but a running machine still has to
    // restart to pick up what was set before.
    assert!(
        manager
            .set_resources("dev", Some(3), Some(2048))
            .unwrap()
            .restart_required
    );

    assert!(manager.set_resources("dev", Some(0), None).is_err());
    assert!(manager.set_resources("nope", Some(1), None).is_err());
    manager
        .machines
        .write()
        .unwrap()
        .get_mut("dev")
        .unwrap()
        .state = MachineState::Stopping;
    let err = manager.set_resources("dev", Some(1), None).unwrap_err();
    assert!(err.to_string().contains("Stopping"), "{err}");
}
