//! Host sandbox e2e: a sandbox in a VM of its own on the Mac (ADR 0010).
//!
//! Per backend, against an isolated `--no-linux-vm` daemon: GetCapabilities
//! reports the host location → Create (`RUN_LOCATION_HOST`, a `docker:<ref>`
//! template the host pulls from the registry) → exec → Pause → Resume →
//! exec again, proving a file written before the pause survived → Remove.
//!
//! HV and VZ both run (`ARCBOX_E2E_HOST_BACKENDS` narrows that). VZ runs when the daemon says a pause could run now: VZ
//! refuses to save a VM while the macOS session is locked, so on a locked
//! Mac the VZ half is skipped with that reason rather than failed.
//!
//! The built-in template needs a static Linux busybox the host does not
//! ship, so the sandbox boots `ARCBOX_E2E_IMAGE` (default
//! `public.ecr.aws/docker/library/alpine:latest`) instead.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_grpc::sandbox_v1::sandbox_process_service_client::SandboxProcessServiceClient;
use arcbox_grpc::sandbox_v1::sandbox_service_client::SandboxServiceClient;
use arcbox_protocol::sandbox_v1::{
    AttachExecutionRequest, CreateSandboxRequest, ExecutionEvent, GetCapabilitiesRequest,
    InspectSandboxRequest, LocationCapability, PauseSandboxRequest, RemoveSandboxRequest,
    ResourceLimits, ResumeSandboxRequest, RunLocation, SandboxState, StartExecutionRequest,
    StdioChannel, execution_event, exit_status,
};
use tonic::transport::Channel;
use tracing::{info, warn};

use crate::daemon::{DaemonConfig, DaemonHandle, connect_unix};
use crate::metrics::{BootAssets, RunMetrics};
use crate::repo_root;
use crate::sandbox::SandboxSmokeConfig;

/// A `--no-linux-vm` daemon is ready once boot assets are staged; no
/// System VM boots.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// The first create pulls and converts the image.
const SANDBOX_READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const DEFAULT_IMAGE: &str = "public.ecr.aws/docker/library/alpine:latest";
const ID: &str = "host1";

/// Runs the scenario on HV, then on VZ unless the session is locked.
pub fn run(config: SandboxSmokeConfig) -> Result<()> {
    if !config.skip_build {
        crate::sandbox::build_binaries()?;
    }
    let root = repo_root();
    let version = match config.version {
        Some(v) => v,
        None => crate::boot_assets::boot_version(&root.join("assets.lock"))?,
    };
    // `ARCBOX_E2E_HOST_BACKENDS=vz` (comma-separated) narrows the run.
    let backends = std::env::var("ARCBOX_E2E_HOST_BACKENDS").unwrap_or_else(|_| "hv,vz".into());
    for backend in backends.split(',').map(str::trim).filter(|b| !b.is_empty()) {
        run_backend(&root, &version, backend, config.keep_test_dir)
            .with_context(|| format!("host sandbox on {backend}"))?;
    }
    Ok(())
}

fn run_backend(root: &Path, version: &str, backend: &str, keep: bool) -> Result<()> {
    info!(backend, "starting host sandbox scenario");
    // Under /tmp, not $TMPDIR: the HV VMM's control socket lives in the
    // sandbox's runtime dir, and a /var/folders path overflows sun_path.
    let temp_dir = tempfile::Builder::new()
        .prefix("arcbox-hs-")
        .tempdir_in("/tmp")
        .context("creating test directory")?;
    let data_dir = temp_dir.path().to_owned();
    crate::boot_assets::stage_dev_boot_assets(root, &data_dir, version)?;

    let mut metrics = RunMetrics::new(
        &format!("host_sandbox_{backend}"),
        Some(backend),
        BootAssets::Bundle {
            version: version.to_owned(),
        },
    )?;
    let mut daemon = DaemonHandle::spawn(DaemonConfig {
        binary: root.join("target/release/arcbox-daemon"),
        data_dir: data_dir.clone(),
        args: vec!["--no-linux-vm".to_owned()],
        env: vec![
            ("ARCBOX_BOOT_ASSET_VERSION".to_owned(), version.to_owned()),
            ("ARCBOX_SANDBOX_BACKEND".to_owned(), backend.to_owned()),
        ],
    })?;
    let result = (|| {
        daemon.wait_ready_blocking(READY_TIMEOUT)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let channel = connect_unix(&daemon.grpc_socket()).await?;
            let result = drive(channel.clone(), backend, &mut metrics).await;
            if result.is_err() {
                // A daemon that stops hands its HV VMs to the next one
                // instead of killing them; a failed run must not leave its
                // VMM process behind.
                drop(
                    SandboxServiceClient::new(channel)
                        .remove(RemoveSandboxRequest {
                            id: ID.into(),
                            force: true,
                        })
                        .await,
                );
            }
            result
        })
    })();
    match daemon.shutdown() {
        Ok(status) => info!(%status, "daemon stopped"),
        Err(error) => warn!("daemon shutdown failed: {error:#}"),
    }
    metrics.passed = result.is_ok();
    if let Err(error) = metrics.write(Some(&data_dir)) {
        warn!("writing run metrics failed: {error:#}");
    }
    if result.is_err() || keep {
        let path = temp_dir.keep();
        warn!(path = %path.display(), "preserving test directory");
    }
    result
}

async fn drive(channel: Channel, backend: &str, metrics: &mut RunMetrics) -> Result<()> {
    let mut sandboxes = SandboxServiceClient::new(channel.clone());
    let mut processes = SandboxProcessServiceClient::new(channel);

    let host = host_capability(&mut sandboxes).await?;
    if !host.available {
        bail!("host location unavailable: {}", host.unavailable_reason);
    }
    if host.backend != backend {
        bail!("host location runs {:?}, asked for {backend}", host.backend);
    }
    if !host.pause_resume {
        bail!("host location on {backend} reports no pause/resume");
    }
    if !host.pause_available {
        if backend == "vz" {
            warn!(
                reason = %host.pause_unavailable_reason,
                "skipping the VZ host sandbox scenario: a pause cannot run now"
            );
            return Ok(());
        }
        bail!("HV refuses a pause: {}", host.pause_unavailable_reason);
    }

    let image = std::env::var("ARCBOX_E2E_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    let started = Instant::now();
    sandboxes
        .create(CreateSandboxRequest {
            id: ID.into(),
            template: format!("docker:{image}"),
            limits: Some(ResourceLimits {
                vcpus: 1,
                memory_mib: 256,
            }),
            run_location: RunLocation::Host.into(),
            ..Default::default()
        })
        .await
        .context("Create failed")?;
    wait_for_state(&mut sandboxes, SandboxState::Ready, SANDBOX_READY_TIMEOUT).await?;
    metrics.record("sandbox_ready", started.elapsed().as_secs_f64());

    let os = exec(&mut processes, &["cat", "/etc/os-release"]).await?;
    if !os.contains("Alpine") && image == DEFAULT_IMAGE {
        bail!("the sandbox is not running {image}: {os:?}");
    }
    exec(
        &mut processes,
        &["sh", "-c", "echo before-pause > /marker && sync"],
    )
    .await?;
    wait_for_state(&mut sandboxes, SandboxState::Ready, Duration::from_secs(30)).await?;

    let started = Instant::now();
    sandboxes
        .pause(PauseSandboxRequest { id: ID.into() })
        .await
        .context("Pause failed")?;
    metrics.record("sandbox_pause", started.elapsed().as_secs_f64());
    let state = inspect(&mut sandboxes).await?.state();
    if state != SandboxState::Paused {
        bail!("expected PAUSED after Pause, got {state:?}");
    }

    let started = Instant::now();
    sandboxes
        .resume(ResumeSandboxRequest { id: ID.into() })
        .await
        .context("Resume failed")?;
    wait_for_state(&mut sandboxes, SandboxState::Ready, Duration::from_secs(60)).await?;
    metrics.record("sandbox_resume", started.elapsed().as_secs_f64());

    let marker = exec(&mut processes, &["cat", "/marker"]).await?;
    if marker.trim() != "before-pause" {
        bail!("the file written before the pause did not survive: {marker:?}");
    }
    // The clock push after restore: the guest must not sit at the kernel
    // epoch (it did before the push, which failed every TLS handshake).
    let now = exec(&mut processes, &["date", "+%s"]).await?;
    let guest: u64 = now.trim().parse().context("parsing guest date")?;
    let host = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    if guest.abs_diff(host) > 60 {
        bail!(
            "guest clock {guest} is {}s off the host after resume",
            guest.abs_diff(host)
        );
    }

    sandboxes
        .remove(RemoveSandboxRequest {
            id: ID.into(),
            force: true,
        })
        .await
        .context("Remove failed")?;
    match sandboxes
        .inspect(InspectSandboxRequest { id: ID.into() })
        .await
    {
        Err(status) if status.code() == tonic::Code::NotFound => {}
        other => bail!("removed sandbox still inspects: {other:?}"),
    }
    info!(backend, "host sandbox scenario passed");
    Ok(())
}

async fn host_capability(
    sandboxes: &mut SandboxServiceClient<Channel>,
) -> Result<LocationCapability> {
    let caps = sandboxes
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .context("GetCapabilities failed")?
        .into_inner();
    caps.locations
        .into_iter()
        .find(|l| l.location() == RunLocation::Host)
        .context("GetCapabilities lists no host location")
}

async fn inspect(
    sandboxes: &mut SandboxServiceClient<Channel>,
) -> Result<arcbox_protocol::sandbox_v1::SandboxInfo> {
    Ok(sandboxes
        .inspect(InspectSandboxRequest { id: ID.into() })
        .await
        .context("Inspect failed")?
        .into_inner())
}

async fn wait_for_state(
    sandboxes: &mut SandboxServiceClient<Channel>,
    state: SandboxState,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let info = inspect(sandboxes).await?;
        if info.state() == state {
            return Ok(());
        }
        if info.state() == SandboxState::Failed {
            bail!("sandbox failed: {}", info.error);
        }
        if Instant::now() > deadline {
            bail!(
                "sandbox did not reach {state:?} within {timeout:?} ({:?})",
                info.state()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Runs `cmd` to completion and returns its stdout, requiring exit code 0.
async fn exec(
    processes: &mut SandboxProcessServiceClient<Channel>,
    cmd: &[&str],
) -> Result<String> {
    let execution = processes
        .start_execution(StartExecutionRequest {
            sandbox_id: ID.into(),
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        })
        .await
        .context("StartExecution failed")?
        .into_inner();
    let mut stream = processes
        .attach_execution(AttachExecutionRequest {
            sandbox_id: ID.into(),
            execution_id: execution.id,
            ..Default::default()
        })
        .await
        .context("AttachExecution failed")?
        .into_inner();
    let mut stdout = String::new();
    while let Some(ExecutionEvent { event, .. }) = stream.message().await? {
        match event {
            Some(execution_event::Event::Output(output))
                if output.channel() != StdioChannel::Stderr =>
            {
                stdout.push_str(&String::from_utf8_lossy(&output.data));
            }
            Some(execution_event::Event::Exited(done)) => {
                let status = done
                    .execution
                    .and_then(|e| e.exit_status)
                    .and_then(|s| s.status);
                return match status {
                    Some(exit_status::Status::Code(0)) => Ok(stdout),
                    other => bail!("{cmd:?} exited {other:?}: {stdout:?}"),
                };
            }
            _ => {}
        }
    }
    bail!("{cmd:?}: attach stream ended without an exit event")
}
