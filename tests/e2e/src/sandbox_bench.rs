//! Isolated daemon, admission timing, and RPC helpers for sandbox benchmarks.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use arcbox_grpc::sandbox_v1::sandbox_process_service_client::SandboxProcessServiceClient;
use arcbox_protocol::sandbox_v1::{
    AttachExecutionRequest, SandboxEventKind, StartExecutionRequest, StdioChannel, execution_event,
    exit_status, watch_events_response,
};
use tonic::Streaming;
use tonic::transport::Channel;
use tracing::{info, warn};

use crate::boot_assets::{resolve_boot_version, stage_dev_boot_assets};
use crate::daemon::{DaemonConfig, DaemonHandle, connect_unix};
use crate::metrics::{BootAssets, RunMetrics};
use crate::{env_flag, repo_root};

const SANDBOX_READY_TIMEOUT: Duration = Duration::from_secs(180);
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(60);
const RPC_TIMEOUT: Duration = Duration::from_secs(180);
const ADMISSION_POLL: Duration = Duration::from_millis(50);

/// Runs a benchmark against an isolated VZ daemon and preserves every failed run.
pub fn run(
    name: &str,
    scenario: impl AsyncFnOnce(Channel, &mut RunMetrics) -> Result<()>,
) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();
    if !env_flag("SKIP_BUILD") {
        crate::sandbox::build_binaries()?;
    }
    let root = repo_root();
    let version = resolve_boot_version(&root)?;
    let temp = tempfile::Builder::new()
        .prefix(&format!("arcbox-{name}-"))
        .tempdir()?;
    let mut metrics = RunMetrics::new(
        name,
        Some("vz"),
        BootAssets::Bundle {
            version: version.clone(),
        },
    )?;
    let result = (|| {
        stage_dev_boot_assets(&root, temp.path(), &version)?;
        let mut daemon = DaemonHandle::spawn(DaemonConfig {
            binary: root.join("target/release/arcbox-daemon"),
            data_dir: temp.path().to_owned(),
            args: vec![],
            env: vec![
                ("ARCBOX_BOOT_ASSET_VERSION".into(), version),
                ("ARCBOX_VM_BACKEND".into(), "vz".into()),
            ],
        })?;
        let result = (|| {
            metrics.time("daemon_ready", || {
                daemon.wait_ready_blocking(Duration::from_secs(240))
            })?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            tokio::task::LocalSet::new().block_on(&rt, async {
                let channel = connect_unix(&daemon.grpc_socket()).await?;
                scenario(channel, &mut metrics).await
            })
        })();
        let shutdown = daemon.shutdown().and_then(|status| {
            ensure!(status.success(), "benchmark daemon exited with {status}");
            Ok(())
        });
        finish(result, shutdown)
    })();
    metrics.passed = result.is_ok();
    let written = metrics.write(Some(temp.path())).map(|paths| {
        for path in paths {
            info!(path = %path.display(), "run metrics written");
        }
    });
    let result = finish(result, written);
    if result.is_err() || env_flag("KEEP_TEST_DIR") {
        let path = temp.keep();
        warn!(path = %path.display(), "preserving test directory");
    }
    result
}

/// Preserves both the scenario failure and any later cleanup or artifact failure.
pub fn finish(result: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("cleanup also failed: {cleanup:#}")))
        }
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(cleanup),
    }
}

/// Successful RPC response and the time spent waiting for admission before that attempt.
pub struct Admitted<T> {
    pub response: T,
    pub started: Instant,
    pub wait: Duration,
}

/// Retries only explicit network-cleanup admission rejections for the requested sandbox.
/// Transport failures and uncertain commits must surface without replaying the mutation.
pub async fn admit<T>(
    id: &str,
    mut rpc: impl AsyncFnMut() -> Result<T, tonic::Status>,
) -> Result<Admitted<T>> {
    let first = Instant::now();
    let mut started = first;
    loop {
        match tokio::time::timeout(RPC_TIMEOUT, rpc())
            .await
            .with_context(|| format!("{id}: sandbox RPC timed out"))?
        {
            Ok(response) => {
                return Ok(Admitted {
                    response,
                    started,
                    wait: started.duration_since(first),
                });
            }
            Err(status) => {
                let remaining = ADMISSION_TIMEOUT.saturating_sub(first.elapsed());
                if !retryable_admission(id, &status) || remaining.is_zero() {
                    return Err(status).with_context(|| format!("{id}: sandbox admission failed"));
                }
                tokio::time::sleep(ADMISSION_POLL.min(remaining)).await;
                started = Instant::now();
            }
        }
    }
}

fn retryable_admission(id: &str, status: &tonic::Status) -> bool {
    status.code() == tonic::Code::Unavailable
        && (status
            .message()
            .ends_with("sandbox startup cleanup is awaiting host finalization")
            || status.message().ends_with(&format!(
                "sandbox {id} network cleanup is awaiting host finalization"
            )))
}

/// Routes a sandbox request to the default System VM.
pub fn with_machine<T>(msg: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(msg);
    request.metadata_mut().insert(
        "x-machine",
        tonic::metadata::MetadataValue::from_static("default"),
    );
    request
}

/// Starts one command and drains its attach stream until exit, asserting a
/// zero exit code.
pub async fn run_and_collect(
    client: &mut SandboxProcessServiceClient<Channel>,
    id: &str,
    cmd: &[&str],
) -> Result<String> {
    let execution = client
        .start_execution(with_machine(StartExecutionRequest {
            sandbox_id: id.to_owned(),
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
            stdin: false,
            ..Default::default()
        }))
        .await
        .context("StartExecution failed")?
        .into_inner();

    let mut stream = client
        .attach_execution(with_machine(AttachExecutionRequest {
            sandbox_id: id.to_owned(),
            execution_id: execution.id.clone(),
            stdout_offset: 0,
            stderr_offset: 0,
        }))
        .await
        .context("AttachExecution failed")?
        .into_inner();

    // Deadline the drain like wait_for_ready: a wedged exec must fail the
    // probe (writing metrics + preserving the test dir), not hang it.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut stdout = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, stream.message())
            .await
            .with_context(|| format!("{id}: command {cmd:?} timed out"))?
            .context("attach stream error")?;
        let Some(event) = event else {
            bail!("{id}: attach stream ended without an exit event");
        };
        match event.event {
            Some(execution_event::Event::Output(output)) => {
                if output.channel() != StdioChannel::Stderr {
                    stdout.push_str(&String::from_utf8_lossy(&output.data));
                }
            }
            Some(execution_event::Event::Exited(done)) => {
                let state = done.execution.context("exit event without execution")?;
                return match state.exit_status.as_ref().and_then(|s| s.status) {
                    Some(exit_status::Status::Code(0)) => Ok(stdout),
                    other => bail!("{id}: command {cmd:?} exit status {other:?}"),
                };
            }
            _ => {}
        }
    }
}

/// Consumes the shared event stream until `id` reports READY.
///
/// Frames for other sandboxes are skipped rather than buffered: the bench
/// is strictly serial, so the only live sandbox is the one being timed.
pub async fn wait_for_ready(
    events: &mut Streaming<arcbox_protocol::sandbox_v1::WatchEventsResponse>,
    id: &str,
) -> Result<()> {
    let deadline = Instant::now() + SANDBOX_READY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("{id} did not reach READY within {SANDBOX_READY_TIMEOUT:?}");
        }
        let frame = tokio::time::timeout(remaining, events.message())
            .await
            .with_context(|| format!("{id}: READY timed out"))?
            .context("Events stream failed")?
            .context("Events stream ended before READY")?;
        let Some(watch_events_response::Payload::Event(event)) = frame.payload else {
            continue; // keepalive
        };
        if event.sandbox_id != id {
            continue;
        }
        match event.kind() {
            SandboxEventKind::Ready => return Ok(()),
            SandboxEventKind::Failed => {
                let reason = event
                    .attributes
                    .get("error")
                    .map_or("unknown", String::as_str);
                bail!("{id} failed while starting: {reason}");
            }
            _ => {}
        }
    }
}

/// Reads a positive integer, rejecting malformed or zero values.
pub fn env_usize(name: &str, default: usize) -> Result<usize> {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} must be an integer"))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error).with_context(|| format!("reading {name}")),
    };
    ensure!(value > 0, "{name} must be positive");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_preserves_the_scenario_failure() {
        let error = finish(
            Err(anyhow::anyhow!("restore failed")),
            Err(anyhow::anyhow!("remove failed")),
        )
        .expect_err("both failures must surface");
        let message = format!("{error:#}");
        assert!(message.contains("restore failed"));
        assert!(message.contains("remove failed"));
        assert!(finish(Ok(()), Err(anyhow::anyhow!("remove failed"))).is_err());
    }

    #[tokio::test]
    async fn admission_wait_excludes_the_successful_attempt() {
        let mut calls = 0;
        let admitted = admit("probe", async || {
            calls += 1;
            if calls == 1 {
                Err(tonic::Status::unavailable(
                    "guest: sandbox probe network cleanup is awaiting host finalization",
                ))
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(())
            }
        })
        .await
        .expect("admitted");
        assert_eq!(calls, 2);
        assert!(admitted.wait >= ADMISSION_POLL);
        assert!(admitted.started.elapsed() >= Duration::from_millis(5));
        let immediate = admit("probe", async || Ok(())).await.expect("admitted");
        assert_eq!(immediate.wait, Duration::ZERO);
    }

    #[tokio::test]
    async fn other_failures_are_never_replayed() {
        for status in [
            tonic::Status::unavailable("transport connection closed"),
            tonic::Status::unavailable("restored sandbox is already committed"),
            tonic::Status::unavailable(
                "sandbox other network cleanup is awaiting host finalization",
            ),
            tonic::Status::internal("sandbox startup cleanup is awaiting host finalization"),
        ] {
            let mut calls = 0;
            let result: Result<Admitted<()>> = admit("probe", async || {
                calls += 1;
                Err(status.clone())
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
        assert!(retryable_admission(
            "probe",
            &tonic::Status::unavailable(
                "guest: sandbox startup cleanup is awaiting host finalization"
            )
        ));
    }
}
