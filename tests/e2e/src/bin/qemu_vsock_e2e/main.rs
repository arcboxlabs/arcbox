//! End-to-end probe for vsock over vhost-user (QEMU TCG backend plan, 1d).
//!
//! Boots the unmodified System VM (boot-asset kernel and rootfs, the real
//! `arcbox-agent`) under QEMU TCG with `vhost-user-vsock-pci` served by
//! `arcbox_virtio_vsock::vhost_user::VhostUserVsock`, then drives the
//! host-dialled vsock ports with the daemon's own clients: agent RPC on 1024
//! through `AgentClient`, the Docker API on 2375 through `HalfCloseStream`
//! and the Docker CLI.
//!
//! ```text
//! abctl boot prefetch --data-dir "$DATA"          # kernel, rootfs, runtime
//! cargo build --release -p arcbox-agent --target aarch64-unknown-linux-musl
//! cargo run --release -p arcbox-e2e --bin qemu_vsock_e2e -- \
//!     --qemu <qemu>/bin/qemu-system-aarch64 --data-dir "$DATA" \
//!     --agent target/aarch64-unknown-linux-musl/release/arcbox-agent \
//!     [--gvproxy <gvproxy>] [--smp 4] [--memory-mib 4096] [--exec-from-share]
//! ```
//!
//! The data directory becomes the `arcbox` share, as `~/.arcbox` is for the
//! daemon; use a scratch directory. The Docker checks need `--gvproxy` (the
//! guest's NIC, for `docker pull`) and `docker` on `PATH`. Exit status 0
//! means every check passed.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("qemu_vsock_e2e runs on macOS only");
}

#[cfg(target_os = "macos")]
mod agent;
#[cfg(target_os = "macos")]
mod docker;

#[cfg(target_os = "macos")]
const CID: u64 = 3;

/// `p50 … max` of `samples`, in milliseconds.
#[cfg(target_os = "macos")]
fn percentiles(samples: &mut [std::time::Duration]) -> String {
    samples.sort();
    let pct = |p: usize| samples[(samples.len() * p / 100).min(samples.len() - 1)];
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    format!(
        "p50 {:.2} ms, p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        ms(pct(50)),
        ms(pct(90)),
        ms(pct(99)),
        ms(samples[samples.len() - 1])
    )
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,arcbox_virtio_vsock=warn".into()),
        )
        .init();
    match probe::run() {
        Ok(()) => {
            println!("\n=== ALL CHECKS PASSED ===");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\n=== FAIL: {e:#} ===");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
mod probe {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use arcbox_e2e::qemu::{Vm, VmOptions};

    use crate::docker::Docker;
    use crate::{CID, agent};

    fn parse_args() -> Result<VmOptions> {
        let mut args = std::env::args().skip(1);
        let (mut qemu, mut data_dir, mut agent, mut gvproxy) = (None, None, None, None);
        let (mut smp, mut memory_mib) = (4, 4096);
        // Implementation-defined pointer auth is the cheap one in TCG.
        let mut cpu = String::from("max,pauth-impdef=on");
        let mut accel = String::from("tcg,thread=multi");
        let mut exec_from_share = false;
        while let Some(flag) = args.next() {
            if flag == "--exec-from-share" {
                exec_from_share = true;
                continue;
            }
            let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--qemu" => qemu = Some(PathBuf::from(value()?)),
                "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
                "--agent" => agent = Some(PathBuf::from(value()?)),
                "--gvproxy" => gvproxy = Some(PathBuf::from(value()?)),
                "--cpu" => cpu = value()?,
                "--accel" => accel = value()?,
                "--smp" => smp = value()?.parse()?,
                "--memory-mib" => memory_mib = value()?.parse()?,
                other => bail!("unknown argument {other}"),
            }
        }
        let data_dir: PathBuf = data_dir.context("--data-dir is required")?;
        let log_dir = data_dir.join("qemu-vsock-e2e");
        std::fs::create_dir_all(&log_dir)?;
        Ok(VmOptions {
            qemu: qemu.context("--qemu is required")?,
            version: arcbox_e2e::boot_assets::resolve_boot_version(&arcbox_e2e::repo_root())?,
            data_dir,
            agent: agent.context("--agent is required")?,
            cid: CID,
            accel,
            cpu,
            smp,
            memory_mib,
            gvproxy,
            exec_from_share,
            log_dir,
        })
    }

    /// Raises the soft descriptor limit as far as the kernel allows. The
    /// back-end runs in this process, so every open connection holds two
    /// descriptors here, the daemon's end and the device's, and the
    /// 512-connection step needs far more than macOS's default of 256.
    fn raise_fd_limit() -> Result<u64> {
        let mut per_proc: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>();
        // SAFETY: the name is NUL-terminated and `per_proc` is a c_int of
        // `len` bytes, which is what this sysctl writes.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut per_proc).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit/setrlimit read and write one `rlimit`.
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut rl) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            rl.rlim_cur = rl.rlim_max.min(per_proc as libc::rlim_t);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const rl) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(rl.rlim_cur)
    }

    pub fn run() -> Result<()> {
        let options = parse_args()?;
        println!("[fds] soft limit raised to {}", raise_fd_limit()?);
        println!("[logs] {}", options.log_dir.display());
        let docker_socket = options.log_dir.join("docker.sock");
        let with_docker = options.gvproxy.is_some();
        let started = Instant::now();
        let vm = Vm::boot(&options)?;
        let result = checks(&vm, started, with_docker.then_some(docker_socket));
        if result.is_err() {
            println!(
                "[state] {} vsock connections open",
                vm.vsock.connection_count()
            );
        }
        vm.shutdown()?;
        result
    }

    fn checks(vm: &Vm, started: Instant, docker_socket: Option<PathBuf>) -> Result<()> {
        let vsock = &vm.vsock;
        let ready = agent::wait_ready(vsock, Duration::from_secs(300))?;
        println!(
            "[ok] agent answered Ping on vsock 1024 {:.1}s after QEMU started",
            ready.duration_since(started).as_secs_f64()
        );
        agent::system_info(vsock)?;
        agent::latency(vsock, 200)?;
        agent::concurrent(vsock, 512)?;
        agent::burst(vsock, 512)?;
        agent::host_close(vsock)?;

        let docker = match docker_socket {
            Some(socket) => {
                vm.route_guest_dns_to_gvproxy()?;
                let docker = Docker::start(Arc::clone(vsock), socket)?;
                let t = Instant::now();
                agent::ensure_runtime(vsock, Duration::from_secs(300))?;
                let version = docker.wait_ready(Duration::from_secs(60))?;
                println!(
                    "[ok] EnsureRuntime started dockerd {version} in {:.1}s; it answers through vsock 2375",
                    t.elapsed().as_secs_f64()
                );
                docker.scenarios()?;
                docker.cleanup()?;
                agent::drained(vsock, Duration::from_secs(30))?;
                println!("[ok] every Docker connection closed on both sides");
                Some(docker)
            }
            None => {
                println!("[skip] Docker checks need --gvproxy");
                None
            }
        };

        // The guest closes first: the agent exits under an idle connection.
        let idle = agent::idle_connection(vsock, Duration::from_secs(60))?;
        let t = Instant::now();
        vm.console("busybox kill $(busybox pidof arcbox-agent)")?;
        let eof = agent::wait_eof(idle, t)?;
        let t = Instant::now();
        agent::wait_ready(vsock, Duration::from_secs(120))?;
        agent::drained(vsock, Duration::from_secs(30))?;
        println!(
            "[ok] guest close: agent killed, host read EOF after {:.2}s, respawned agent answered {:.1}s later",
            eof.as_secs_f64(),
            t.elapsed().as_secs_f64()
        );

        // Guest reboot: QEMU stops the rings, resets the device and starts
        // them again for the new driver.
        let idle = agent::idle_connection(vsock, Duration::from_secs(120))?;
        let t = Instant::now();
        vm.console("busybox reboot")?;
        let eof = agent::wait_eof(idle, t)?;
        let t = Instant::now();
        agent::wait_ready(vsock, Duration::from_secs(300))?;
        println!(
            "[ok] guest reboot: host read EOF after {:.2}s, agent answered {:.1}s after that",
            eof.as_secs_f64(),
            t.elapsed().as_secs_f64()
        );
        agent::system_info(vsock)?;
        agent::latency(vsock, 50)?;
        if let Some(docker) = docker {
            agent::ensure_runtime(vsock, Duration::from_secs(300))?;
            let version = docker.wait_ready(Duration::from_secs(60))?;
            println!("[ok] dockerd {version} answered again after the reboot");
        }
        agent::drained(vsock, Duration::from_secs(30))?;
        println!("[ok] no vsock connection left open");
        Ok(())
    }
}
