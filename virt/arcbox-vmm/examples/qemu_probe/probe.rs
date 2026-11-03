use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arcbox_vmm::{BlockDeviceConfig, QemuHost, VmBackend, Vmm, VmmConfig};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,arcbox_net=warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, work, rest @ ..] = args.as_slice() else {
        return Err("usage: qemu_probe <mode> <work> [mem_mib] [vcpus]".into());
    };
    let work = PathBuf::from(work);
    let run_dir = work.join("run");
    if mode == "reap" {
        let started = Instant::now();
        let reaped = arcbox_vmm::reap_orphaned_qemu(&run_dir);
        println!("PROBE-HOST: reaped {reaped} in {:?}", started.elapsed());
        return Ok(());
    }
    let memory_mib: u64 = rest.first().map_or(Ok(4096), |v| v.parse())?;
    let vcpus: u32 = rest.get(1).map_or(Ok(4), |v| v.parse())?;
    let guest_mode = match mode.as_str() {
        "stop" | "orphan" => "sleep",
        other => other,
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();
    let disk = |path: PathBuf, read_only| BlockDeviceConfig { path, read_only };
    let config = VmmConfig {
        vcpu_count: vcpus,
        memory_size: memory_mib * 1024 * 1024,
        kernel_path: env_path("QEMU_PROBE_KERNEL")?,
        kernel_cmdline: format!("console=hvc0 rdinit=/init probe={guest_mode}"),
        initrd_path: Some(work.join("initramfs.cpio.gz")),
        vsock: false,
        balloon: false,
        block_devices: vec![
            disk(env_path("QEMU_PROBE_ROOTFS")?, true),
            disk(work.join("docker.img"), false),
            disk(work.join("docker-meta.img"), false),
        ],
        backend: VmBackend::Qemu,
        qemu: Some(QemuHost {
            binary: env_path("ARCBOX_QEMU_PATH")?,
            run_dir: run_dir.clone(),
        }),
        ..VmmConfig::default()
    };

    let started = Instant::now();
    let mut vmm = Vmm::new(config)?;
    vmm.start()?;
    println!("PROBE-HOST: QEMU up in {:?}", started.elapsed());
    drain_consoles(&vmm, started, "")?;

    match mode.as_str() {
        "reset" => {
            while vmm.is_running() {
                std::thread::sleep(Duration::from_millis(10));
            }
            println!(
                "PROBE-HOST: guest stopped, reset_requested={}",
                vmm.reset_requested()
            );
            let rebooted = Instant::now();
            vmm.reboot()?;
            println!("PROBE-HOST: rebooted in {:?}", rebooted.elapsed());
            drain_consoles(&vmm, started, "'")?;
            // The second boot resets too; its first lines are the proof.
            std::thread::sleep(Duration::from_secs(3));
        }
        "stop" => std::thread::sleep(Duration::from_secs(5)),
        "orphan" => {
            println!("PROBE-HOST: pid {} idles; kill -9 it", std::process::id());
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        _ => {
            let stopped = vmm.wait_for_stopped(Duration::from_secs(600))?;
            println!(
                "PROBE-HOST: guest stopped={stopped} after {:?}",
                started.elapsed()
            );
        }
    }
    let stopping = Instant::now();
    vmm.stop()?;
    let left: Vec<_> = std::fs::read_dir(&run_dir)?
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    println!(
        "PROBE-HOST: stopped in {:?}; pidfiles left: {left:?}",
        stopping.elapsed()
    );
    Ok(())
}

fn env_path(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is not set").into())
}

/// Prints every line of `hvc0` and `hvc1` with its time since `started`.
fn drain_consoles(vmm: &Vmm, started: Instant, suffix: &'static str) -> Result<()> {
    let readers = vmm.dup_serial_readers()?.ok_or("no console pipes")?;
    for (name, fd) in [("hvc0", readers.console), ("hvc1", readers.agent_log)] {
        std::thread::spawn(move || print_lines(name, suffix, fd, started));
    }
    Ok(())
}

fn print_lines(name: &str, suffix: &str, fd: OwnedFd, started: Instant) {
    let mut file = std::fs::File::from(fd);
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == b'\n' {
                        let elapsed = started.elapsed().as_secs_f64();
                        let text = String::from_utf8_lossy(&line);
                        println!("[{elapsed:7.3}] {name}{suffix}: {}", text.trim_end());
                        line.clear();
                    } else {
                        line.push(byte);
                    }
                }
            }
            // The serial drain's descriptors are non-blocking.
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}
