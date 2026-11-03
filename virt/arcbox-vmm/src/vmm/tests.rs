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
fn every_backend_decodes_from_its_discriminant() {
    for backend in [VmBackend::Hv, VmBackend::Vz, VmBackend::Qemu] {
        // Exhaustive on purpose: a new backend fails to compile here until it
        // joins the list above, then fails below until `TryFrom<u8>` decodes it.
        match backend {
            VmBackend::Hv | VmBackend::Vz | VmBackend::Qemu => {}
        }
        assert_eq!(VmBackend::try_from(backend as u8), Ok(backend));
        assert_eq!(VmBackend::from_str_ascii(backend.as_str()), Some(backend));
    }
    assert_eq!(VmBackend::try_from(u8::MAX), Err(u8::MAX));
}

/// Hosts without hardware virtualization include the CI runners (a Linux
/// container without `/dev/kvm`, a macOS VM), so this runs there.
#[test]
fn start_without_hardware_virtualization_fails_before_initializing() {
    let host = arcbox_hypervisor::host_virtualization();
    if host.available {
        return;
    }
    let mut vmm = Vmm::new(VmmConfig {
        guest_cid: Some(3),
        ..Default::default()
    })
    .unwrap();
    assert!(matches!(
        vmm.start(),
        Err(VmmError::PlatformUnsupported(reason)) if reason == host.reason
    ));
    assert_eq!(vmm.state(), VmmState::Created);
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
    let Machine::Hv(ref mut hv) = vmm.machine else {
        panic!("an HV config builds an HV machine");
    };
    hv.hv_poweroff_rx = Some(receiver);
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

/// A `qemu-system-aarch64` that reports the guest's power-off before it
/// answers `qmp_capabilities`, then exits at the host's next command.
#[cfg(target_os = "macos")]
const QEMU_SHUT_DOWN_DURING_START: &str = r#"#!/bin/sh
for arg; do
  case $arg in socket,id=qmp,fd=*) qmp=${arg##*=} ;; esac
done
printf '{"QMP":{"version":{},"capabilities":[]}}\n' >&"$qmp"
read -r command <&"$qmp"
id=$(printf '%s' "$command" | sed 's/.*"id":\([0-9]*\).*/\1/')
printf '{"event":"SHUTDOWN","data":{"guest":true,"reason":"guest-shutdown"}}\n' >&"$qmp"
printf '{"return":{},"id":%s}\n' "$id" >&"$qmp"
read -r _ <&"$qmp"
"#;

/// The guest can be gone before `Vmm::start` returns. QEMU's own handlers
/// lower the run flag, and `start` must not raise it again, or the
/// lifecycle waits on a VM that no longer exists.
#[cfg(target_os = "macos")]
#[test]
fn qemu_start_keeps_the_shutdown_reported_while_starting() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("qemu-system-aarch64");
    std::fs::write(&binary, QEMU_SHUT_DOWN_DURING_START).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vmm = Vmm::new(VmmConfig {
        guest_cid: Some(3),
        backend: VmBackend::Qemu,
        networking: false,
        qemu: Some(QemuHost {
            binary,
            run_dir: dir.path().join("run"),
        }),
        ..Default::default()
    })
    .unwrap();
    vmm.start().unwrap();
    assert!(!vmm.is_running());
    vmm.stop().unwrap();
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
    let Machine::Hv(ref mut hv) = vmm.machine else {
        panic!("an HV config builds an HV machine");
    };
    hv.hv_poweroff_rx = Some(receiver);
    vmm.state = VmmState::Running;
    drop(poweroff);

    assert!(matches!(
        vmm.wait_for_stopped(Duration::ZERO),
        Err(VmmError::Vcpu(message)) if message.contains("without PSCI SYSTEM_OFF")
    ));
}
