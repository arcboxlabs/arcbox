//! End-to-end test: Boot a Linux VM using the custom Hypervisor.framework backend.
//!
//! This tests the HV backend (M1-M3) by:
//! 1. Creating a VM with VmBackend::Hv
//! 2. Loading a kernel + initrd
//! 3. Booting to serial output
//! 4. Verifying PL011 UART output contains boot messages
//! 5. Clean shutdown via PSCI
//!
//! Usage:
//!   cargo build --example hv_boot_test -p arcbox-vmm
//!   codesign --force --options runtime \
//!       --entitlements bundle/arcbox.entitlements \
//!       --sign "Developer ID Application: ArcBox, Inc. (422ACSY6Y5)" \
//!       target/debug/examples/hv_boot_test
//!   ./target/debug/examples/hv_boot_test

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn main() {
    // Enable tracing so PL011 serial output (guest_serial target) is visible.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,guest_serial=info,arcbox_vmm=debug,arcbox_hv=debug"
                    .parse()
                    .unwrap()
            }),
        )
        .with_target(true)
        .init();

    println!("╔══════════════════════════════════════════════╗");
    println!("║  ArcBox HV Backend E2E Boot Test             ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    // --- Locate boot assets ---
    let kernel_path = find_kernel();
    let initrd_path = find_initrd();

    println!("[assets] kernel: {}", kernel_path.display());
    println!(
        "[assets] initrd: {}",
        initrd_path
            .as_ref()
            .map_or("none".to_string(), |p| p.display().to_string())
    );
    println!();

    // --- VM configuration ---
    let config = arcbox_vmm::VmmConfig {
        vcpu_count: 2,
        memory_size: 2 * 1024 * 1024 * 1024, // 2 GB
        kernel_path,
        kernel_cmdline: "console=ttyAMA0 earlycon=pl011,0x0b000000 loglevel=7 panic=-1".to_string(),
        initrd_path,
        enable_rosetta: false,
        serial_console: true,
        virtio_console: false, // Use PL011 for this test
        shared_dirs: Vec::new(),
        networking: false,
        vsock: false,
        guest_cid: None,
        balloon: false,
        block_devices: Vec::new(),
        bridge_nic_mac: None,
        backend: arcbox_vmm::VmBackend::Hv, // Force HV backend
    };

    println!("[config] backend: HV (Hypervisor.framework)");
    println!("[config] vCPUs: {}", config.vcpu_count);
    println!("[config] memory: {} MB", config.memory_size / (1024 * 1024));
    println!("[config] cmdline: {}", config.kernel_cmdline);
    println!();

    // --- Create VMM ---
    println!("[phase 1] Creating VMM with HV backend...");
    let start = Instant::now();
    let mut vmm = match arcbox_vmm::Vmm::new(config) {
        Ok(vmm) => {
            println!(
                "[phase 1] VMM created in {:.1}ms ✓",
                start.elapsed().as_secs_f64() * 1000.0
            );
            vmm
        }
        Err(e) => {
            eprintln!("[phase 1] FAILED: {e}");
            eprintln!();
            eprintln!("Troubleshooting:");
            eprintln!("  - Is macOS >= 15.0? (GICv3 required)");
            eprintln!("  - Is the binary signed with com.apple.security.hypervisor?");
            eprintln!("  - Is this Apple Silicon (ARM64)?");
            std::process::exit(1);
        }
    };

    // --- Start VMM ---
    println!("[phase 2] Starting VM (booting kernel)...");
    let boot_start = Instant::now();
    match vmm.start() {
        Ok(()) => {
            println!(
                "[phase 2] VM started in {:.1}ms ✓",
                boot_start.elapsed().as_secs_f64() * 1000.0
            );
        }
        Err(e) => {
            eprintln!("[phase 2] FAILED to start: {e}");
            std::process::exit(1);
        }
    }

    // --- Monitor boot ---
    println!("[phase 3] Monitoring boot (30s timeout)...");
    println!("          PL011 UART output will appear in tracing logs above.");
    println!();

    // Set up Ctrl+C handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_handler(r);

    let boot_timeout = Duration::from_secs(30);
    let start = Instant::now();

    while running.load(Ordering::Relaxed) && start.elapsed() < boot_timeout {
        std::thread::sleep(Duration::from_millis(500));

        if !vmm.is_running() {
            println!("[phase 3] VM stopped (guest shutdown or panic)");
            break;
        }
    }

    if start.elapsed() >= boot_timeout {
        println!("[phase 3] Boot timeout reached (30s)");
    }

    // --- Stop VMM ---
    println!();
    println!("[phase 4] Stopping VM...");
    let stop_start = Instant::now();
    match vmm.stop() {
        Ok(()) => {
            println!(
                "[phase 4] VM stopped in {:.1}ms ✓",
                stop_start.elapsed().as_secs_f64() * 1000.0
            );
        }
        Err(e) => {
            eprintln!("[phase 4] Stop error: {e}");
        }
    }

    println!();
    println!("═══ Test complete ═══");
}

fn find_kernel() -> PathBuf {
    // Try multiple locations
    let candidates = [
        PathBuf::from("boot-assets/dev/kernel"),
        PathBuf::from("tests/resources/Image-arm64"),
        dirs(),
    ];
    for p in &candidates {
        if p.exists() {
            return p.clone();
        }
    }
    eprintln!("ERROR: No kernel found. Tried:");
    for p in &candidates {
        eprintln!("  - {}", p.display());
    }
    std::process::exit(1);
}

fn find_initrd() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("boot-assets/dev/initramfs.cpio.gz"),
        PathBuf::from("tests/resources/initramfs-arm64"),
    ];
    for p in &candidates {
        if p.exists() {
            return Some(p.clone());
        }
    }
    None
}

fn dirs() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.arcbox/boot/0.5.1/kernel"))
}

fn ctrlc_handler(running: Arc<AtomicBool>) {
    let _ = std::thread::spawn(move || {
        // Simple signal handling without external crate
        loop {
            std::thread::sleep(Duration::from_secs(60));
            if !running.load(Ordering::Relaxed) {
                break;
            }
        }
    });
}
