use super::*;
#[test]
fn test_vmm_creation() {
    let config = VmmConfig {
        guest_cid: Some(3),
        ..Default::default()
    };
    let vmm = Vmm::new(config).unwrap();
    assert_eq!(vmm.state(), VmmState::Created);
}

#[test]
fn test_vmm_invalid_config() {
    // Zero vCPUs
    let config = VmmConfig {
        vcpu_count: 0,
        guest_cid: Some(3),
        ..Default::default()
    };
    assert!(Vmm::new(config).is_err());

    // Too little memory
    let config = VmmConfig {
        memory_size: 1024, // 1KB
        guest_cid: Some(3),
        ..Default::default()
    };
    assert!(Vmm::new(config).is_err());
}

#[test]
fn test_vmm_requires_guest_cid_when_vsock_enabled() {
    let config = VmmConfig {
        guest_cid: None,
        ..Default::default()
    };
    assert!(Vmm::new(config).is_err());

    let config = VmmConfig {
        vsock: false,
        guest_cid: None,
        ..Default::default()
    };
    assert!(Vmm::new(config).is_ok());
}

#[test]
fn test_vmm_state_transitions() {
    let config = VmmConfig {
        guest_cid: Some(3),
        ..Default::default()
    };
    let mut vmm = Vmm::new(config).unwrap();

    // Can't pause before running
    assert!(vmm.pause().is_err());

    // Can't resume before pausing
    assert!(vmm.resume().is_err());
}

/// With the HV backend resolved, `capture_snapshot_context` must return
/// `VmmError::Unsupported` rather than the old generic `invalid_state
/// ("hypervisor VM handle is unavailable")` error. This covers the
/// ABX-360 correctness gap: HV-backed VMs were silently hitting
/// VZ-only snapshot code paths.
#[cfg(target_os = "macos")]
#[test]
fn test_hv_snapshot_returns_unsupported() {
    let config = VmmConfig {
        guest_cid: Some(3),
        backend: VmBackend::Hv,
        ..Default::default()
    };
    let mut vmm = Vmm::new(config).unwrap();

    // Simulate a started HV VM without actually booting one.
    vmm.state = VmmState::Running;

    match vmm.capture_snapshot_context() {
        Err(VmmError::Unsupported(msg)) => assert!(msg.contains("HV backend")),
        Err(other) => panic!("expected Unsupported for HV capture, got Err({other:?})"),
        Ok(_) => panic!("expected Unsupported for HV capture, got Ok"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn hv_stop_wait_requires_guest_poweroff_and_retains_completion() {
    let mut vmm = Vmm::new(VmmConfig {
        guest_cid: Some(3),
        backend: VmBackend::Hv,
        ..Default::default()
    })
    .unwrap();
    let (poweroff, receiver) = crossbeam_channel::unbounded();
    vmm.hv_poweroff_rx = Some(receiver);
    vmm.state = VmmState::Running;

    // Host stop and vCPU failures also clear running; neither proves guest poweroff.
    vmm.running.store(false, Ordering::SeqCst);
    assert!(!vmm.wait_for_stopped(Duration::ZERO).unwrap());
    vmm.state = VmmState::Stopped;
    assert!(!vmm.wait_for_stopped(Duration::ZERO).unwrap());
    vmm.state = VmmState::Running;

    poweroff.send(()).unwrap();
    assert!(vmm.wait_for_stopped(Duration::ZERO).unwrap());
    assert!(vmm.wait_for_stopped(Duration::ZERO).unwrap());
    vmm.stop().unwrap();
    assert!(vmm.wait_for_stopped(Duration::ZERO).unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn hv_stop_wait_rejects_vcpu_exit_without_guest_poweroff() {
    let mut vmm = Vmm::new(VmmConfig {
        guest_cid: Some(3),
        backend: VmBackend::Hv,
        ..Default::default()
    })
    .unwrap();
    let (poweroff, receiver) = crossbeam_channel::unbounded();
    vmm.hv_poweroff_rx = Some(receiver);
    vmm.state = VmmState::Running;
    drop(poweroff);

    assert!(matches!(
        vmm.wait_for_stopped(Duration::ZERO),
        Err(VmmError::Vcpu(message)) if message.contains("without PSCI SYSTEM_OFF")
    ));
}
