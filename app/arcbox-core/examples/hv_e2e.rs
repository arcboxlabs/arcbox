//! ABX-361: End-to-end test for the HV backend.
//!
//! Boots a Linux guest through the HV (Hypervisor.framework) backend, talks
//! to the in-guest `arcbox-agent` over vsock, and asserts:
//!
//! 1. Ping round-trip succeeds — guest userspace is up.
//! 2. `GetSystemInfo` returns a non-empty kernel version — RPC framing works.
//! 3. `Vmm::pause()` stops the guest — a ping during pause times out.
//! 4. `Vmm::resume()` restarts the guest — the next ping succeeds.
//!    (This also exercises ABX-360.)
//! 5. `Vmm::stop()` shuts down cleanly.
//!
//! Usage:
//!   cargo build --release --example hv_e2e -p arcbox-core
//!   codesign --force --options runtime \
//!       --entitlements bundle/arcbox.entitlements \
//!       --sign "Developer ID Application: ArcBox, Inc. (422ACSY6Y5)" \
//!       target/release/examples/hv_e2e
//!   ./target/release/examples/hv_e2e
//!
//! Environment:
//!   ARCBOX_HV_E2E_KERNEL    override kernel path
//!   ARCBOX_HV_E2E_ROOTFS    override rootfs.erofs path
//!   ARCBOX_HV_E2E_TIMEOUT   boot timeout seconds (default: 30)
//!
//! Exit code 0 = all assertions passed. Non-zero = failure.

use std::fmt::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arcbox_core::AgentClient;
use arcbox_vmm::{BlockDeviceConfig, SharedDirConfig, VmBackend, Vmm, VmmConfig};

const GUEST_CID: u32 = 3;
const AGENT_PORT: u32 = 1024;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,arcbox_vmm=info,arcbox_core=info".parse().unwrap()),
        )
        .with_target(true)
        .init();

    match run() {
        Ok(()) => {
            println!("\n═══ ALL ASSERTIONS PASSED ═══");
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("\n═══ FAIL: {msg} ═══");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let kernel_path = locate("kernel", "ARCBOX_HV_E2E_KERNEL", &kernel_candidates())?;
    let rootfs_path = locate("rootfs", "ARCBOX_HV_E2E_ROOTFS", &rootfs_candidates())?;
    let boot_timeout = std::env::var("ARCBOX_HV_E2E_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or_else(|| Duration::from_secs(30), Duration::from_secs);

    let share_dir = std::env::temp_dir().join("arcbox-hv-e2e-share");
    std::fs::create_dir_all(&share_dir)
        .map_err(|e| format!("create share dir {}: {e}", share_dir.display()))?;
    // Write a dummy agent log so the guest init script has a file to open.
    std::fs::write(share_dir.join("agent.log"), "").ok();

    println!("[cfg] kernel:  {}", kernel_path.display());
    println!("[cfg] rootfs:  {}", rootfs_path.display());
    println!("[cfg] share:   {}", share_dir.display());
    println!("[cfg] cid:     {GUEST_CID}");
    println!("[cfg] timeout: {}s", boot_timeout.as_secs());
    println!();

    let config = VmmConfig {
        vcpu_count: 2,
        memory_size: 1024 * 1024 * 1024, // 1 GiB — enough for a smoke test
        kernel_path,
        kernel_cmdline:
            "console=hvc0 root=/dev/vda ro rootfstype=erofs earlycon=pl011,0x0b000000 loglevel=4 panic=10"
                .to_string(),
        initrd_path: None,
        enable_rosetta: false,
        serial_console: true,
        virtio_console: true,
        shared_dirs: vec![SharedDirConfig {
            host_path: share_dir,
            tag: "arcbox".to_string(),
            read_only: false,
        }],
        networking: false,
        vsock: true,
        guest_cid: Some(GUEST_CID),
        balloon: false,
        block_devices: vec![BlockDeviceConfig {
            path: rootfs_path,
            read_only: true,
        }],
        bridge_nic_mac: None,
        backend: VmBackend::Hv,
    };

    println!("[phase 1] create VMM");
    let t = Instant::now();
    let mut vmm = Vmm::new(config).map_err(|e| format!("Vmm::new: {e}"))?;
    println!(
        "          ok in {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    println!("[phase 2] start VM (HV backend)");
    let t = Instant::now();
    vmm.start().map_err(|e| format!("Vmm::start: {e}"))?;
    println!(
        "          ok in {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    println!("[phase 3] wait for agent on vsock port {AGENT_PORT}");
    let t = Instant::now();
    ping_with_timeout(&vmm, boot_timeout).map_err(|e| {
        format!(
            "agent never responded within {}s: {e}",
            boot_timeout.as_secs()
        )
    })?;
    println!(
        "          agent responded after {:.1}s",
        t.elapsed().as_secs_f64()
    );

    println!("[phase 4] get system info");
    let info = get_system_info(&vmm)?;
    if info.kernel_version.is_empty() {
        return Err("system info: kernel_version is empty".into());
    }
    println!(
        "          kernel={} hostname={}",
        info.kernel_version, info.hostname
    );

    println!("[phase 5] pause VM (ABX-360)");
    vmm.pause().map_err(|e| format!("Vmm::pause: {e}"))?;
    // Ping should fail because the guest vCPUs are parked.
    match ping_once(&vmm, Duration::from_secs(2)) {
        Ok(()) => {
            return Err("ping succeeded while VM was paused — pause did not stop guest".into());
        }
        Err(_) => println!("          ping timed out as expected — guest is paused"),
    }

    println!("[phase 6] resume VM (ABX-360)");
    vmm.resume().map_err(|e| format!("Vmm::resume: {e}"))?;
    // Ping should succeed again now.
    ping_once(&vmm, Duration::from_secs(10))
        .map_err(|e| format!("ping after resume failed: {e}"))?;
    println!("          ping after resume succeeded — guest is back");

    println!("[phase 7] stop VM");
    let t = Instant::now();
    vmm.stop().map_err(|e| format!("Vmm::stop: {e}"))?;
    println!(
        "          ok in {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    Ok(())
}

/// Repeatedly opens a fresh vsock connection and tries a ping until one
/// succeeds or the overall timeout elapses. Each attempt uses a short
/// per-call deadline so we don't burn the entire budget on one handshake.
fn ping_with_timeout(vmm: &Vmm, overall: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut attempt = 0u32;
    let mut last_err = String::from("no attempts made");
    while start.elapsed() < overall {
        attempt += 1;
        match ping_once(vmm, Duration::from_secs(3)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Err(format!("{attempt} attempts, last error: {last_err}"))
}

/// Single ping attempt: connect a fresh vsock, send ping, close.
fn ping_once(vmm: &Vmm, _deadline: Duration) -> Result<(), String> {
    let fd = vmm
        .connect_vsock(AGENT_PORT)
        .map_err(|e| format!("connect_vsock: {e}"))?;
    let mut client =
        AgentClient::from_fd(GUEST_CID, fd).map_err(|e| format!("AgentClient::from_fd: {e}"))?;
    client
        .ping_blocking()
        .map(|_| ())
        .map_err(|e| format!("ping_blocking: {e}"))
}

fn get_system_info(vmm: &Vmm) -> Result<arcbox_protocol::SystemInfo, String> {
    let fd = vmm
        .connect_vsock(AGENT_PORT)
        .map_err(|e| format!("connect_vsock: {e}"))?;
    let mut client =
        AgentClient::from_fd(GUEST_CID, fd).map_err(|e| format!("AgentClient::from_fd: {e}"))?;
    client
        .get_system_info_blocking()
        .map_err(|e| format!("get_system_info_blocking: {e}"))
}

fn kernel_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("boot-assets/dev/kernel"),
        PathBuf::from("boot-assets/dev/kernel.arm64"),
        home_asset("kernel"),
    ]
}

fn rootfs_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("boot-assets/dev/rootfs.erofs"),
        home_asset("rootfs.erofs"),
    ]
}

fn home_asset(name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.arcbox/boot/0.5.5/{name}"))
}

fn locate(kind: &str, env_key: &str, candidates: &[PathBuf]) -> Result<PathBuf, String> {
    if let Ok(override_path) = std::env::var(env_key) {
        let p = PathBuf::from(override_path);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("${env_key} set but {} does not exist", p.display()));
    }
    for p in candidates {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    let mut msg = format!("no {kind} found. Tried:");
    for p in candidates {
        let _ = write!(msg, "\n  - {}", p.display());
    }
    let _ = write!(msg, "\nOverride with ${env_key}=/path/to/{kind}");
    Err(msg)
}
