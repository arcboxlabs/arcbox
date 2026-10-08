use super::*;
use tempfile::TempDir;

fn test_vm_manager() -> (VmManager, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let manager = VmManager::new(temp_dir.path().join("snapshots"));
    (manager, temp_dir)
}

#[test]
fn test_set_guest_cid_updates_vm_config() {
    let (manager, _dir) = test_vm_manager();
    let config = VmConfig {
        guest_cid: None,
        ..Default::default()
    };

    let vm_id = manager.create(config).unwrap();

    manager.set_guest_cid(&vm_id, 7).unwrap();
    assert_eq!(manager.guest_cid_for_test(&vm_id), Some(7));
}

#[test]
fn test_build_vmm_config_includes_guest_cid() {
    let (manager, _dir) = test_vm_manager();
    let config = VmConfig {
        guest_cid: Some(9),
        ..Default::default()
    };

    let vm_id = manager.create(config).unwrap();
    let vmm_config = manager.build_vmm_config_for_test(&vm_id).unwrap();
    assert_eq!(vmm_config.guest_cid, Some(9));
    assert_eq!(
        vmm_config.bridge_nic_mac,
        Some(bridge_nic_mac_for_vm_id(&vm_id))
    );
}

#[test]
fn build_vmm_config_gates_rosetta_on_vz_backend() {
    let (manager, _dir) = test_vm_manager();
    // Rosetta-capable machine created on VZ: the share is wired.
    let config = VmConfig {
        rosetta: true,
        backend: arcbox_vmm::VmBackend::Vz,
        ..Default::default()
    };
    let vm_id = manager.create(config).unwrap();
    assert!(
        manager
            .build_vmm_config_for_test(&vm_id)
            .unwrap()
            .enable_rosetta
    );

    // Switching the stopped VM to HV must drop the VZ-only Rosetta share even
    // though `config.rosetta` (the host capability) is unchanged.
    manager
        .set_backend(&vm_id, arcbox_vmm::VmBackend::Hv)
        .unwrap();
    assert!(
        !manager
            .build_vmm_config_for_test(&vm_id)
            .unwrap()
            .enable_rosetta
    );

    // And back: switching to VZ re-wires it.
    manager
        .set_backend(&vm_id, arcbox_vmm::VmBackend::Vz)
        .unwrap();
    assert!(
        manager
            .build_vmm_config_for_test(&vm_id)
            .unwrap()
            .enable_rosetta
    );
}

#[test]
fn test_bridge_nic_mac_is_stable_and_locally_administered() {
    let vm_id = VmId::from_string("00112233-4455-6677-8899-aabbccddeeff".to_string());
    let mac = bridge_nic_mac_for_vm_id(&vm_id);

    assert_eq!(mac, "02:00:11:22:33:44");
}

#[test]
fn test_start_failure_rolls_back_to_created() {
    let (manager, _dir) = test_vm_manager();
    let vm_id = manager.create(VmConfig::default()).unwrap();

    let _ = manager.start(&vm_id, HostNetwork::default());
    let info = manager.get(&vm_id).expect("vm should still exist");
    assert_eq!(info.state, MachineState::Created);
    assert_eq!(manager.vms.read().unwrap()[&vm_id].run_sequence, 1);
    assert!(manager.network_incarnation(&vm_id).is_err());
    assert!(manager.start(&vm_id, HostNetwork::default()).is_err());
    assert_eq!(manager.vms.read().unwrap()[&vm_id].run_sequence, 2);
}

#[cfg(target_os = "macos")]
#[test]
fn replaced_or_dead_incarnations_cannot_take_the_next_manager() {
    use std::sync::atomic::Ordering;
    let (manager, _dir) = test_vm_manager();
    let id = manager
        .create(VmConfig {
            vsock: false,
            ..Default::default()
        })
        .unwrap();
    {
        let mut vms = manager.vms.write().unwrap();
        let entry = vms.get_mut(&id).unwrap();
        entry.begin_run().unwrap();
        let vmm = Vmm::new(VmManager::build_vmm_config(entry)).unwrap();
        vmm.running_flag().store(true, Ordering::SeqCst);
        entry.vmm = Some(vmm);
        entry.info.state = MachineState::Running;
    }
    let old = manager.network_incarnation(&id).unwrap();
    std::thread::scope(|scope| {
        let (begin, begun) = std::sync::mpsc::channel();
        let (checked, check) = std::sync::mpsc::channel();
        let manager = &manager;
        let id = &id;
        let transition = scope.spawn(move || {
            begun.recv().unwrap();
            assert!(manager.vms.try_write().is_err());
            checked.send(()).unwrap();
            manager
                .vms
                .write()
                .unwrap()
                .get_mut(id)
                .unwrap()
                .reboot_with(|_| Ok(()))
                .unwrap();
        });
        manager
            .with_network_incarnation(id, &old, || {
                begin.send(()).unwrap();
                check.recv().unwrap();
            })
            .unwrap();
        transition.join().unwrap();
    });
    assert!(
        manager
            .with_network_incarnation(&id, &old, || panic!("stale publication"))
            .is_err()
    );
    {
        let mut vms = manager.vms.write().unwrap();
        let entry = vms.get_mut(&id).unwrap();
        assert!(
            entry
                .reboot_with(|_| Err(EngineError::invalid_state("injected reboot failure")))
                .is_err()
        );
        assert_eq!(entry.info.state, MachineState::Stopping);
    }
    assert!(manager.network_incarnation(&id).is_err());
    manager
        .vms
        .write()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .reboot_with(|_| Ok(()))
        .unwrap();
    let current = manager.network_incarnation(&id).unwrap();
    assert_ne!(old, current);
    let mut source = Some("replacement manager");
    assert!(
        manager
            .with_expected_vmm(&id, &old, |_| source.take())
            .is_err()
    );
    assert!(source.is_some());
    assert_eq!(
        manager
            .with_expected_vmm(&id, &current, |_| source.take())
            .unwrap(),
        Some("replacement manager")
    );
    let flag = manager.vms.read().unwrap()[&id]
        .vmm
        .as_ref()
        .unwrap()
        .running_flag();
    flag.store(false, Ordering::SeqCst);
    assert!(
        manager
            .take_inbound_listener_manager(&id, &current)
            .is_err()
    );
    manager
        .vms
        .write()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .info
        .state = MachineState::Stopped;
    manager.remove(&id, &current).unwrap();
    assert!(manager.network_incarnation(&id).is_err());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn expected_agent_connection_fences_replacement_and_selects_the_locked_backend() {
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;

    for backend in [arcbox_vmm::VmBackend::Hv, arcbox_vmm::VmBackend::Vz] {
        let (manager, _dir) = test_vm_manager();
        let id = manager
            .create(VmConfig {
                backend,
                vsock: false,
                ..Default::default()
            })
            .unwrap();
        {
            let mut vms = manager.vms.write().unwrap();
            let entry = vms.get_mut(&id).unwrap();
            entry.begin_run().unwrap();
            let vmm = Vmm::new(VmManager::build_vmm_config(entry)).unwrap();
            vmm.running_flag().store(true, Ordering::SeqCst);
            entry.vmm = Some(vmm);
            entry.info.state = MachineState::Running;
        }
        let expected = manager.network_incarnation(&id).unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        let agent = std::thread::scope(|scope| {
            let (begin, begun) = std::sync::mpsc::channel();
            let (checked, check) = std::sync::mpsc::channel();
            let manager = &manager;
            let id = &id;
            let transition = scope.spawn(move || {
                begun.recv().unwrap();
                assert!(manager.vms.try_write().is_err());
                checked.send(()).unwrap();
                let mut vms = manager.vms.write().unwrap();
                let entry = vms.get_mut(id).unwrap();
                entry.reboot_with(|_| Ok(())).unwrap();
                entry.config.backend = if backend == arcbox_vmm::VmBackend::Hv {
                    arcbox_vmm::VmBackend::Vz
                } else {
                    arcbox_vmm::VmBackend::Hv
                };
            });
            let agent = manager
                .connect_agent_using(id, &expected, 7, |_| {
                    begin.send(()).unwrap();
                    check.recv().unwrap();
                    Ok(host.into_raw_fd())
                })
                .unwrap();
            transition.join().unwrap();
            agent
        });
        assert_eq!(agent.cid(), 7);
        assert_eq!(agent.is_blocking(), backend == arcbox_vmm::VmBackend::Hv);
        drop(agent);
        assert_eq!(guest.read(&mut [0]).unwrap(), 0);

        let stale = manager.connect_agent_using(&id, &expected, 7, |_| {
            panic!("a stale incarnation must not open a socket")
        });
        assert!(matches!(
            stale,
            Err(EngineError::Common(
                arcbox_error::CommonError::InvalidState(_)
            ))
        ));
        let current = manager.network_incarnation(&id).unwrap();
        let flag = manager.vms.read().unwrap()[&id]
            .vmm
            .as_ref()
            .unwrap()
            .running_flag();
        flag.store(false, Ordering::SeqCst);
        assert!(
            manager
                .connect_agent_using(&id, &current, 7, |_| {
                    panic!("a dead VM must not open a socket")
                })
                .is_err()
        );
    }
}

fn modeled_stop_vm(manager: &VmManager) -> (VmId, VmIncarnation) {
    use std::sync::atomic::Ordering;
    let id = manager
        .create(VmConfig {
            vsock: false,
            ..Default::default()
        })
        .unwrap();
    let mut vms = manager.vms.write().unwrap();
    let entry = vms.get_mut(&id).unwrap();
    entry.begin_run().unwrap();
    let vmm = Vmm::new(VmManager::build_vmm_config(entry)).unwrap();
    vmm.running_flag().store(true, Ordering::SeqCst);
    entry.vmm = Some(vmm);
    entry.info.state = MachineState::Running;
    (id, entry.run_incarnation())
}

#[test]
fn stale_stop_and_remove_reject_before_touching_the_replacement() {
    let (manager, _dir) = test_vm_manager();
    let (id, old) = modeled_stop_vm(&manager);
    manager
        .vms
        .write()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .reboot_with(|_| Ok(()))
        .unwrap();
    let current = manager.network_incarnation(&id).unwrap();
    for result in [
        manager.stop(&id, &old),
        manager
            .graceful_stop_using(&id, &old, |_| panic!("stale stop reached wait"))
            .map(|_| ()),
        manager.send_shutdown_rpc(&id, &old, 0),
        manager.remove(&id, &old),
    ] {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("incarnation changed")
        );
    }
    #[cfg(target_os = "macos")]
    assert!(manager.force_stop_without_hypervisor(&id, &old).is_err());
    assert_eq!(manager.network_incarnation(&id).unwrap(), current);
    assert!(manager.remove(&id, &current).is_err());
}

#[test]
fn graceful_completion_never_restores_or_stops_a_replacement_run() {
    for stopped in [false, true] {
        let (manager, _dir) = test_vm_manager();
        let (id, expected) = modeled_stop_vm(&manager);
        manager
            .vms
            .write()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .info
            .state = MachineState::Stopping;
        let result = manager.graceful_stop_using(&id, &expected, |_| {
            let mut vms = manager.vms.write().unwrap();
            let entry = vms.get_mut(&id).unwrap();
            assert!(entry.vmm.is_none());
            entry.begin_run().unwrap();
            let vmm = Vmm::new(VmManager::build_vmm_config(entry)).unwrap();
            vmm.running_flag()
                .store(true, std::sync::atomic::Ordering::SeqCst);
            entry.vmm = Some(vmm);
            entry.info.state = MachineState::Running;
            Ok(stopped)
        });
        assert!(result.unwrap_err().to_string().contains("run changed"));
        assert_ne!(manager.network_incarnation(&id).unwrap(), expected);
    }
}

#[test]
fn borrowed_vmm_prevents_force_and_removal_until_graceful_restores_it() {
    let (manager, _dir) = test_vm_manager();
    let (id, expected) = modeled_stop_vm(&manager);
    manager
        .vms
        .write()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .info
        .state = MachineState::Stopping;
    let result = manager.graceful_stop_using(&id, &expected, |_| {
        assert!(manager.stop(&id, &expected).is_err());
        assert!(manager.remove(&id, &expected).is_err());
        #[cfg(target_os = "macos")]
        assert!(
            manager
                .force_stop_without_hypervisor(&id, &expected)
                .is_err()
        );
        assert_eq!(manager.get(&id).unwrap().state, MachineState::Stopping);
        Ok(false)
    });
    assert!(!result.unwrap());
    assert_eq!(manager.network_incarnation(&id).unwrap(), expected);
}
