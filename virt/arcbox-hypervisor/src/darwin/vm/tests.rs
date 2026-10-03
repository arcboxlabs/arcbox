use super::*;
use crate::traits::{Vcpu, VirtualMachine};
use crate::types::{CpuArch, VirtioDeviceConfig};

#[test]
fn test_vm_creation() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    let config = VmConfig {
        vcpu_count: 2,
        memory_size: 512 * 1024 * 1024,
        arch: CpuArch::native(),
        ..Default::default()
    };

    let vm = DarwinVm::new(config).unwrap();
    assert_eq!(vm.state(), VmState::Created);
    assert!(!vm.is_running());
}

#[test]
fn test_vcpu_creation() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    let config = VmConfig {
        vcpu_count: 4,
        memory_size: 512 * 1024 * 1024,
        ..Default::default()
    };

    let mut vm = DarwinVm::new(config).unwrap();

    // Create valid vCPUs
    let vcpu0 = vm.create_vcpu(0);
    assert!(vcpu0.is_ok());
    assert_eq!(vcpu0.unwrap().id(), 0);

    let vcpu1 = vm.create_vcpu(1);
    assert!(vcpu1.is_ok());

    // Try to create same vCPU again
    let vcpu0_again = vm.create_vcpu(0);
    assert!(vcpu0_again.is_err());

    // Try to create vCPU with invalid ID
    let vcpu99 = vm.create_vcpu(99);
    assert!(vcpu99.is_err());
}

#[test]
fn test_vm_lifecycle() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    let config = VmConfig {
        vcpu_count: 1,
        memory_size: 256 * 1024 * 1024,
        ..Default::default()
    };

    let mut vm = DarwinVm::new(config).unwrap();
    assert_eq!(vm.state(), VmState::Created);

    // Note: Actually starting the VM requires:
    // 1. The process to be signed with com.apple.security.virtualization entitlement
    // 2. Running on a thread with an active CFRunLoop
    //
    // For unit tests without proper signing, we can only verify state transitions
    // up to the point of calling start(). The full lifecycle test requires
    // a signed binary run from a GUI or properly configured CLI environment.

    // Attempt to start - will fail without entitlement or kernel
    match vm.start() {
        Ok(()) => {
            // If start succeeds, test full lifecycle
            assert_eq!(vm.state(), VmState::Running);
            assert!(vm.is_running());

            // Pause
            vm.pause().unwrap();
            assert_eq!(vm.state(), VmState::Paused);

            // Resume
            vm.resume().unwrap();
            assert_eq!(vm.state(), VmState::Running);

            // Stop
            vm.stop().unwrap();
            assert_eq!(vm.state(), VmState::Stopped);
            assert!(!vm.is_running());
        }
        Err(e) => {
            // Expected without proper signing/configuration
            println!("VM start failed (expected without entitlement): {}", e);
            // VM should be in Error or Starting state
            let state = vm.state();
            assert!(
                state == VmState::Starting || state == VmState::Error,
                "Unexpected state after failed start: {:?}",
                state
            );
        }
    }
}

#[test]
fn test_invalid_state_transitions() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    // Use a small fixed size — this test only checks state transitions,
    // not actual guest execution.
    let config = VmConfig {
        memory_size: 128 * 1024 * 1024, // 128MB
        ..Default::default()
    };
    let mut vm = DarwinVm::new(config).unwrap();

    // Can't pause if not running
    assert!(vm.pause().is_err());

    // Can't resume if not paused
    assert!(vm.resume().is_err());

    // Can't stop if not running
    assert!(vm.stop().is_err());
}

#[test]
fn test_balloon_device_configuration() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    let config = VmConfig {
        vcpu_count: 1,
        memory_size: 512 * 1024 * 1024,
        ..Default::default()
    };

    let mut vm = DarwinVm::new(config).unwrap();

    // Initially no balloon device
    assert!(!vm.has_balloon_device());

    // Add balloon device
    let balloon_config = VirtioDeviceConfig::balloon();
    vm.add_virtio_device(balloon_config).unwrap();

    // Now balloon should be configured
    assert!(vm.has_balloon_device());
    assert_eq!(vm.configured_memory_size(), 512 * 1024 * 1024);

    // Before starting, balloon target memory should return 0
    // (no running VM to query)
    assert_eq!(vm.get_balloon_target_memory(), 0);
}

#[test]
fn test_balloon_device_pending() {
    if !arcbox_vz::is_supported() {
        println!("Virtualization not supported, skipping");
        return;
    }

    // NOTE: Balloon device configuration is pending arcbox-vz support.
    // Once arcbox-vz adds BalloonDeviceConfiguration, this test should
    // verify that the balloon device can be created and configured.
    println!("Balloon device test skipped: pending arcbox-vz support");
}

/// Feeds `bytes` through a pipe into the serial reader, `chunk` bytes per
/// `read(2)`, and returns what it decoded. No VM is involved. The payload
/// must fit the pipe unread: under host pipe-memory pressure XNU hands out
/// 512-byte pipe buffers (measured 2026-09-29), so keep it under that.
fn read_serial_from_pipe(bytes: &[u8], chunk: usize) -> String {
    assert!(bytes.len() <= 512, "payload must fit a minimum-size pipe");
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for `pipe(2)`; both ends
    // are closed below and never used after.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        let written = libc::write(fds[1], bytes.as_ptr().cast(), bytes.len());
        assert_eq!(usize::try_from(written), Ok(bytes.len()), "pipe write");
        let output = DarwinVm::read_serial_fd_chunked(fds[0], chunk);
        libc::close(fds[0]);
        libc::close(fds[1]);
        output
    }
}

#[test]
fn serial_read_keeps_a_multibyte_char_split_across_read_chunks() {
    // 255 ASCII bytes push the 3-byte '中' across a 256-byte read boundary;
    // a per-chunk UTF-8 check dropped both chunks.
    let mut bytes = vec![b'a'; 255];
    bytes.extend_from_slice("中\n".as_bytes());
    let output = read_serial_from_pipe(&bytes, 256);
    assert_eq!(output.len(), bytes.len(), "every byte reaches the caller");
    assert!(output.starts_with(&"a".repeat(255)));
    assert!(output.ends_with("中\n"));
}

#[test]
fn serial_read_spans_several_read_chunks() {
    let bytes = vec![b'x'; 500];
    let output = read_serial_from_pipe(&bytes, 64);
    assert_eq!(output.len(), 500);
}
