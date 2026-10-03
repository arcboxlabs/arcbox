//! Machine console back-pressure e2e: a guest that floods `hvc0` must not
//! wedge its own exec channel.
//!
//! On VZ the host side of a machine's serial port is a 64 KiB pipe. With no
//! reader, the guest's virtio-console write never completes once that pipe
//! is full: every vCPU spins on the stalled queue and the machine's vsock
//! stops answering, so exec, ssh and graceful stop all time out
//! (`virt/arcbox-vz/AGENTS.md`, "A VZ console pipe with no reader wedges the
//! whole VM"). `MachineManager::start` drains every machine's console for
//! exactly this reason (`engine/arcbox-engine/src/machine/serial.rs`).
//!
//! The scenario writes 200 KB — three pipes' worth — to `/dev/hvc0` from
//! inside an alpine machine and then requires a fresh `exec` to answer within
//! five seconds. Without the drain the flood itself never returns. The flood
//! is text lines rather than the raw `/dev/zero` reproducer: measured
//! 2026-09-29, a 1-vCPU alpine machine pushes NUL bytes through hvc0 at
//! ~10 KB/s but 100-byte lines at >100 KB/s, and the wedge does not care
//! what the bytes are.
//!
//! Needs a VZ daemon and the alpine image (the live mirror, or a local one
//! via `ARCBOX_MACHINE_IMAGE_BASE`), plus a musl-cross `arcbox-agent`, like
//! `machine.rs`.

use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_e2e::boot_assets::{resolve_boot_version, stage_dev_boot_assets};
use arcbox_e2e::daemon::{DaemonConfig, DaemonHandle, connect_unix};
use arcbox_e2e::metrics::RunMetrics;
use arcbox_grpc::v1::machine_service_client::MachineServiceClient;
use arcbox_protocol::v1::{
    CreateMachineRequest, MachineExecRequest, RemoveMachineRequest, StartMachineRequest,
    StopMachineRequest,
};
use tonic::transport::Channel;

static TRACING: Once = Once::new();

const READY_TIMEOUT: Duration = Duration::from_secs(180);
const CREATE_BUDGET: Duration = Duration::from_secs(120);
const START_BUDGET: Duration = Duration::from_secs(120);
const RPC_BUDGET: Duration = Duration::from_secs(30);
/// The flood blocks in the guest until the host has drained it; the drain
/// polls at 100 ms while output arrives, so this is generous.
const FLOOD_BUDGET: Duration = Duration::from_secs(60);
/// What a wedged machine cannot do: answer an exec promptly.
const EXEC_AFTER_FLOOD_BUDGET: Duration = Duration::from_secs(5);
/// Three host pipes' worth of console output.
const FLOOD_BYTES: usize = 200 * 1024;

const MACHINE: &str = "e2e-console-alpine";

fn init_tracing() {
    TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    });
}

#[test]
#[ignore = "boots a real VZ daemon, pulls a distro image, and floods a machine's console"]
fn machine_console_flood_does_not_wedge_exec() -> Result<()> {
    init_tracing();

    let root = arcbox_e2e::repo_root();
    if !arcbox_e2e::env_flag("SKIP_BUILD") {
        let shell = xshell::Shell::new()?;
        shell.change_dir(&root);
        xshell::cmd!(shell, "cargo build --release -p arcbox-daemon").run()?;
        xshell::cmd!(
            shell,
            "cargo build --release -p arcbox-agent --target aarch64-unknown-linux-musl"
        )
        .run()?;
    }

    let version = resolve_boot_version(&root)?;
    let data_dir = tempfile::Builder::new()
        .prefix("arcbox-machine-console-e2e-")
        .tempdir()?;
    stage_dev_boot_assets(&root, data_dir.path(), &version)?;

    let mut daemon = DaemonHandle::spawn(DaemonConfig {
        binary: root.join("target/release/arcbox-daemon"),
        data_dir: data_dir.path().to_owned(),
        args: vec![],
        env: vec![
            ("ARCBOX_BOOT_ASSET_VERSION".to_owned(), version),
            ("ARCBOX_VM_BACKEND".to_owned(), "vz".to_owned()),
            ("ARCBOX_DNS_PORT".to_owned(), "0".to_owned()),
        ],
    })?;

    let mut metrics = RunMetrics::new("machine_console", Some("vz"));
    let result = scenario(&mut daemon, &mut metrics);
    metrics.passed = result.is_ok();
    if let Err(error) = metrics.write(Some(data_dir.path())) {
        tracing::warn!("writing run metrics failed: {error:#}");
    }
    // `KEEP_TEST_DIR=1` also sidesteps a `TempDir` removal that can hang on
    // a `~/ArcBox` NFS view the daemon left mounted (`tests/e2e/AGENTS.md`).
    if result.is_err() || arcbox_e2e::env_flag("KEEP_TEST_DIR") {
        let kept = data_dir.keep();
        tracing::warn!(path = %kept.display(), "preserving test directory");
    }
    result
}

fn scenario(daemon: &mut DaemonHandle, metrics: &mut RunMetrics) -> Result<()> {
    metrics.time("daemon_ready", || daemon.wait_ready_blocking(READY_TIMEOUT))?;
    let socket = daemon.grpc_socket();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    runtime.block_on(async {
        let channel = connect_unix(&socket).await?;
        let mut machines = MachineServiceClient::new(channel);

        let started = Instant::now();
        tokio::time::timeout(
            CREATE_BUDGET,
            machines.create(CreateMachineRequest {
                name: MACHINE.to_owned(),
                cpus: 1,
                memory: 1024 * 1024 * 1024,
                disk_size: 2 * 1024 * 1024 * 1024,
                distro: "alpine".to_owned(),
                version: "3.24".to_owned(),
                ..Default::default()
            }),
        )
        .await
        .context("create timed out (image pull)")?
        .context("create failed")?;
        metrics.record("machine_create", started.elapsed().as_secs_f64());

        let started = Instant::now();
        tokio::time::timeout(
            START_BUDGET,
            machines.start(StartMachineRequest {
                id: MACHINE.to_owned(),
            }),
        )
        .await
        .context("start timed out")?
        .context("start failed")?;
        metrics.record("machine_start", started.elapsed().as_secs_f64());

        // The flood: a guest write that outlives the host pipe three times
        // over. It returns only once the host has drained every byte.
        let flood =
            format!("head -c {FLOOD_BYTES} /dev/zero | tr '\\0' x | fold -w 100 > /dev/hvc0");
        let started = Instant::now();
        let (_, exit) = exec_capture(&mut machines, &["/bin/sh", "-c", &flood], FLOOD_BUDGET)
            .await
            .context("console flood")?;
        let flood_secs = started.elapsed().as_secs_f64();
        metrics.record("console_flood", flood_secs);
        if exit != 0 {
            bail!("console flood exited {exit}");
        }
        tracing::info!(secs = flood_secs, "console flood drained");

        // The regression: with the pipe full and unread, this never answers.
        let started = Instant::now();
        let (out, exit) = exec_capture(
            &mut machines,
            &["/bin/sh", "-c", "echo console-exec-ok"],
            EXEC_AFTER_FLOOD_BUDGET,
        )
        .await
        .context("exec after the console flood")?;
        metrics.record("exec_after_flood", started.elapsed().as_secs_f64());
        if exit != 0 || !out.contains("console-exec-ok") {
            bail!("exec after the flood: exit {exit}, stdout {out:?}");
        }

        tokio::time::timeout(
            RPC_BUDGET,
            machines.stop(StopMachineRequest {
                id: MACHINE.to_owned(),
                force: false,
            }),
        )
        .await
        .context("graceful stop timed out")?
        .context("stop failed")?;
        machines
            .remove(RemoveMachineRequest {
                id: MACHINE.to_owned(),
                force: false,
                volumes: false,
            })
            .await
            .context("remove failed")?;
        Ok(())
    })
}

/// Runs `cmd` in the machine over the vsock exec channel; returns
/// `(stdout, exit_code)`. Every await is bounded by `budget`, so a wedged
/// machine fails the call instead of hanging the test.
async fn exec_capture(
    machines: &mut MachineServiceClient<Channel>,
    cmd: &[&str],
    budget: Duration,
) -> Result<(String, i32)> {
    let mut stream = tokio::time::timeout(
        budget,
        machines.exec(MachineExecRequest {
            id: MACHINE.to_owned(),
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        }),
    )
    .await
    .context("exec timed out")?
    .context("exec failed")?
    .into_inner();

    let mut stdout = Vec::new();
    let mut exit = None;
    while let Some(out) = tokio::time::timeout(budget, stream.message())
        .await
        .context("exec output timed out")?
        .context("exec stream error")?
    {
        if out.stream == "stdout" {
            stdout.extend_from_slice(&out.data);
        }
        if out.done {
            exit = Some(out.exit_code);
        }
    }
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        exit.context("exec produced no completion frame")?,
    ))
}
