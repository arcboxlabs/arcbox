//! Sandbox cold-start bench: `Create` → `READY` over N serial iterations.
//!
//! Measures accepted `Create` → `READY` and synchronous `Restore` latency.
//! Networked Create may restore a cached snapshot; no-network Create always
//! boots a kernel. System VM boot is measured separately. Unique geometry
//! forces networked cache misses with a different memory size per sample.
//!
//! Readiness is taken from the `Events` stream, subscribed *before* the
//! first `Create`, rather than the smoke test's 500ms `Inspect` poll: at
//! the sub-second scale this bench targets, a 500ms poll interval is the
//! measurement.
//!
//! Requires nested virtualization (VZ backend, Apple Silicon M3+ with
//! macOS 15+). Run:
//!
//! ```console
//! cargo test -p arcbox-e2e --test sandbox_coldstart -- --ignored --nocapture
//! ```
//!
//! Knobs: `ARCBOX_COLDSTART_ITERS` (default 10), `ARCBOX_COLDSTART_VCPUS`
//! (1), `ARCBOX_COLDSTART_MEMORY_MIB` (512), `SKIP_BUILD`, `KEEP_TEST_DIR`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_e2e::env_flag;
use arcbox_e2e::metrics::{RunMetrics, percentile};
use arcbox_e2e::sandbox_bench::{
    admit, env_usize, run, run_and_collect, wait_for_ready, with_machine,
};
use arcbox_grpc::sandbox_v1::sandbox_process_service_client::SandboxProcessServiceClient;
use arcbox_grpc::sandbox_v1::sandbox_service_client::SandboxServiceClient;
use arcbox_grpc::sandbox_v1::sandbox_snapshot_service_client::SandboxSnapshotServiceClient;
use arcbox_protocol::sandbox_v1::{
    CheckpointRequest, CreateSandboxRequest, NetworkMode, NetworkSpec, RemoveSandboxRequest,
    ResourceLimits, RestoreRequest, SandboxEventsRequest, SandboxState,
};
use tonic::Streaming;
use tonic::transport::Channel;
use tracing::{info, warn};

const RPC_TIMEOUT: Duration = Duration::from_secs(180);

#[test]
#[ignore = "requires nested virtualization (VZ on M3+), boot assets, and a signed daemon"]
fn sandbox_coldstart() -> Result<()> {
    let params = Params {
        iters: env_usize("ARCBOX_COLDSTART_ITERS", 10)?,
        vcpus: u32::try_from(env_usize("ARCBOX_COLDSTART_VCPUS", 1)?)?,
        memory_mib: u64::try_from(env_usize("ARCBOX_COLDSTART_MEMORY_MIB", 512)?)?,
        unique_geometry: env_flag("ARCBOX_COLDSTART_UNIQUE_GEOMETRY"),
    };
    run("sandbox_coldstart", async move |channel, metrics| {
        drive(channel, metrics, params).await
    })
}

struct Params {
    iters: usize,
    vcpus: u32,
    memory_mib: u64,
    unique_geometry: bool,
}

impl Params {
    fn memory_for(&self, mode: NetworkMode, iteration: usize) -> Result<u64> {
        if self.unique_geometry && mode == NetworkMode::Enabled {
            self.memory_mib
                .checked_add(u64::try_from(iteration)?)
                .context("unique memory geometry overflow")
        } else {
            Ok(self.memory_mib)
        }
    }
}

/// One iteration's timings.
struct Sample {
    /// Rejected-admission attempts and delays before the successful attempt.
    admission_wait: Duration,
    /// `Create` RPC call → response (the sandbox is STARTING at this point).
    create_rpc: Duration,
    /// Successful `Create` attempt → `READY` event, or synchronous `Restore` completion.
    ready: Duration,
    /// `Create` call → stdout of the first command. READY claims the
    /// sandbox accepts executions; this is what makes that claim
    /// falsifiable, and is the number an SDK user actually waits out.
    first_exec: Duration,
    /// The same command again on the already-warm sandbox. Splits the gap
    /// after READY into "the sandbox was not yet executable" (first ≫
    /// second) and "every execution costs this" (first ≈ second).
    second_exec: Duration,
    /// Guest `/proc/uptime` sampled by the first command: how long the
    /// microVM kernel had been up when the first exec landed. Splits the
    /// create→exec gap into host-side cost (create→exec minus uptime) and
    /// in-guest boot (uptime minus the exec round-trip itself).
    guest_uptime: Option<f64>,
    /// `Remove` RPC call → response.
    remove: Duration,
}

async fn drive(channel: Channel, metrics: &mut RunMetrics, params: Params) -> Result<()> {
    let mut client = SandboxServiceClient::new(channel.clone());
    let mut processes = SandboxProcessServiceClient::new(channel.clone());
    let mut snapshots = SandboxSnapshotServiceClient::new(channel);

    // Subscribed before the first Create so no READY can be missed. Kind is
    // left unfiltered: a FAILED frame is what turns a hung bench into a
    // reported cause.
    let mut events = tokio::time::timeout(
        RPC_TIMEOUT,
        client.events(with_machine(SandboxEventsRequest::default())),
    )
    .await
    .context("Events subscribe timed out")?
    .context("Events subscribe failed")?
    .into_inner();

    for (label, mode) in [
        (
            if params.unique_geometry {
                "cold-networked-unique-geometry"
            } else {
                "create-networked"
            },
            NetworkMode::Enabled,
        ),
        ("cold-no-network", NetworkMode::None),
    ] {
        let geometry: Vec<f64> = (0..params.iters)
            .map(|i| params.memory_for(mode, i).map(|memory| memory as f64))
            .collect::<Result<_>>()?;
        metrics.record_distribution(&format!("coldstart_{label}_memory"), "MiB", &geometry);
        let mut samples = Vec::with_capacity(params.iters);
        for i in 0..params.iters {
            let id = format!("cold-{label}-{i}");
            let sample = one_cycle(
                &mut client,
                &mut processes,
                &mut events,
                &id,
                mode,
                &params,
                i,
            )
            .await?;
            log_sample(label, i, &sample);
            samples.push(sample);
        }
        report(label, &samples, metrics);
    }

    // -- Restore group: the snapshot-resume path -------------------------
    // One warm networked template is checkpointed once; each iteration then
    // restores a clone with a fresh TAP (`network_override`) while the
    // template keeps running — the E2B-style "resume, don't boot" shape.
    let template_id = "cold-template";
    one_template(
        &mut client,
        &mut processes,
        &mut events,
        template_id,
        &params,
    )
    .await?;
    let checkpoint_started = Instant::now();
    let snapshot_id = snapshots
        .checkpoint(with_machine(CheckpointRequest {
            sandbox_id: template_id.to_owned(),
            name: "coldstart-bench".into(),
            ..Default::default()
        }))
        .await
        .context("Checkpoint failed")?
        .into_inner()
        .snapshot_id;
    info!(
        %snapshot_id,
        checkpoint_ms = checkpoint_started.elapsed().as_millis(),
        "template checkpointed"
    );

    let mut samples = Vec::with_capacity(params.iters);
    for i in 0..params.iters {
        let id = format!("cold-restore-{i}");
        let sample = one_restore_cycle(
            &mut client,
            &mut processes,
            &mut snapshots,
            &id,
            &snapshot_id,
        )
        .await?;
        log_sample("restore", i, &sample);
        samples.push(sample);
    }
    report("restore", &samples, metrics);

    client
        .remove(with_machine(RemoveSandboxRequest {
            id: template_id.to_owned(),
            force: true,
        }))
        .await
        .context("Remove template failed")?;

    Ok(())
}

/// Create the warm template the restore group snapshots: booted, one
/// execution completed so the exec path is warm in the captured memory.
async fn one_template(
    client: &mut SandboxServiceClient<Channel>,
    processes: &mut SandboxProcessServiceClient<Channel>,
    events: &mut Streaming<arcbox_protocol::sandbox_v1::WatchEventsResponse>,
    id: &str,
    params: &Params,
) -> Result<()> {
    let admitted = admit(id, async || {
        client
            .create(with_machine(CreateSandboxRequest {
                id: id.to_owned(),
                limits: Some(ResourceLimits {
                    vcpus: params.vcpus,
                    memory_mib: params.memory_mib,
                }),
                network: Some(NetworkSpec {
                    mode: NetworkMode::Enabled.into(),
                }),
                ..Default::default()
            }))
            .await
    })
    .await
    .context("Create template failed")?;
    info!(
        admission_wait_ms = admitted.wait.as_millis(),
        "template admitted"
    );
    wait_for_ready(events, id).await?;
    run_and_collect(processes, id, &["/bin/echo", "template-warm"]).await?;
    Ok(())
}

/// Restore a clone from the template snapshot and drive the same back half
/// as a boot cycle. `ready` is the restore RPC completion: the RPC resumes
/// the VM synchronously, so its return is the usability claim.
async fn one_restore_cycle(
    client: &mut SandboxServiceClient<Channel>,
    processes: &mut SandboxProcessServiceClient<Channel>,
    snapshots: &mut SandboxSnapshotServiceClient<Channel>,
    id: &str,
    snapshot_id: &str,
) -> Result<Sample> {
    let admitted = admit(id, async || {
        snapshots
            .restore(with_machine(RestoreRequest {
                id: id.to_owned(),
                snapshot_id: snapshot_id.to_owned(),
                network_override: true,
                ..Default::default()
            }))
            .await
    })
    .await
    .with_context(|| format!("Restore {id} failed"))?;
    let started = admitted.started;
    let restore_rpc = started.elapsed();

    let mut sample = finish_cycle(
        client,
        processes,
        id,
        started,
        restore_rpc,
        restore_rpc,
        false,
    )
    .await?;
    sample.admission_wait = admitted.wait;
    Ok(sample)
}

fn log_sample(label: &str, iteration: usize, sample: &Sample) {
    info!(
        iteration,
        group = label,
        admission_wait_ms = sample.admission_wait.as_millis(),
        create_ms = sample.create_rpc.as_millis(),
        ready_ms = sample.ready.as_millis(),
        first_exec_ms = sample.first_exec.as_millis(),
        second_exec_ms = sample.second_exec.as_millis(),
        guest_uptime_s = sample.guest_uptime,
        remove_ms = sample.remove.as_millis(),
        "sandbox startup"
    );
}

/// Log the interesting lines of a guest boot dmesg: the timeline from
/// kernel entry to init handoff, plus anything that took visibly long.
fn log_boot_timeline(id: &str, dmesg: &str) {
    for line in dmesg.lines() {
        let interesting = ["Linux version", "Freeing unused kernel", "Run /"]
            .iter()
            .any(|m| line.contains(m));
        if interesting {
            info!(%id, "dmesg: {line}");
        }
    }
    // The largest single gap between consecutive timestamped lines — where
    // the boot actually stalled.
    let stamps: Vec<(f64, &str)> = dmesg
        .lines()
        .filter_map(|l| {
            let ts = l.split(']').next()?.trim_start_matches('[').trim();
            Some((ts.parse::<f64>().ok()?, l))
        })
        .collect();
    if let Some((gap, before, line)) = stamps
        .windows(2)
        .map(|w| (w[1].0 - w[0].0, w[0].1, w[1].1))
        .max_by(|a, b| a.0.total_cmp(&b.0))
    {
        info!(%id, gap_s = gap, "dmesg: largest gap between: {before} → {line}");
    }
}

async fn one_cycle(
    client: &mut SandboxServiceClient<Channel>,
    processes: &mut SandboxProcessServiceClient<Channel>,
    events: &mut Streaming<arcbox_protocol::sandbox_v1::WatchEventsResponse>,
    id: &str,
    mode: NetworkMode,
    params: &Params,
    iteration: usize,
) -> Result<Sample> {
    let memory_mib = params.memory_for(mode, iteration)?;
    let admitted = admit(id, async || {
        client
            .create(with_machine(CreateSandboxRequest {
                id: id.to_owned(),
                limits: Some(ResourceLimits {
                    vcpus: params.vcpus,
                    memory_mib,
                }),
                network: Some(NetworkSpec { mode: mode.into() }),
                ..Default::default()
            }))
            .await
    })
    .await
    .with_context(|| format!("Create {id} failed"))?;
    let started = admitted.started;
    let created = admitted.response.into_inner();
    let create_rpc = started.elapsed();
    if created.state() != SandboxState::Starting {
        bail!("{id}: unexpected create state {:?}", created.state());
    }

    wait_for_ready(events, id).await?;
    let ready = started.elapsed();

    let mut sample = finish_cycle(
        client,
        processes,
        id,
        started,
        create_rpc,
        ready,
        iteration == 1,
    )
    .await?;
    sample.admission_wait = admitted.wait;
    Ok(sample)
}

/// The shared back half of a cycle: first exec (sampling guest uptime),
/// warm exec, optional dmesg capture, remove.
async fn finish_cycle(
    client: &mut SandboxServiceClient<Channel>,
    processes: &mut SandboxProcessServiceClient<Channel>,
    id: &str,
    started: Instant,
    create_rpc: Duration,
    ready: Duration,
    capture_dmesg: bool,
) -> Result<Sample> {
    let stdout = run_and_collect(processes, id, &["/bin/cat", "/proc/uptime"]).await?;
    let first_exec = started.elapsed();
    let guest_uptime = stdout
        .split_whitespace()
        .next()
        .and_then(|t| t.parse::<f64>().ok());
    if guest_uptime.is_none() {
        bail!("{id}: unexpected /proc/uptime output: {stdout:?}");
    }

    let second_started = Instant::now();
    let stdout = run_and_collect(processes, id, &["/bin/echo", "warm-ok"]).await?;
    let second_exec = second_started.elapsed();
    if !stdout.contains("warm-ok") {
        bail!("{id}: warm command output missing marker: {stdout:?}");
    }

    if capture_dmesg {
        // One boot timeline per group: where the in-guest time goes.
        match run_and_collect(processes, id, &["/bin/dmesg"]).await {
            Ok(dmesg) => log_boot_timeline(id, &dmesg),
            Err(error) => warn!(%id, "dmesg capture failed: {error:#}"),
        }
    }

    let remove_started = Instant::now();
    client
        .remove(with_machine(RemoveSandboxRequest {
            id: id.to_owned(),
            force: true,
        }))
        .await
        .with_context(|| format!("Remove {id} failed"))?;

    Ok(Sample {
        admission_wait: Duration::ZERO,
        create_rpc,
        ready,
        first_exec,
        second_exec,
        guest_uptime,
        remove: remove_started.elapsed(),
    })
}

fn report(label: &str, samples: &[Sample], metrics: &mut RunMetrics) {
    let admission: Vec<f64> = samples
        .iter()
        .map(|s| s.admission_wait.as_secs_f64())
        .collect();
    metrics.record_distribution(
        &format!("coldstart_{label}_admission_wait"),
        "seconds",
        &admission,
    );
    for (i, sample) in samples.iter().enumerate() {
        metrics.record(
            &format!("coldstart_{label}_{i}_admission_wait"),
            sample.admission_wait.as_secs_f64(),
        );
    }
    let ready: Vec<f64> = samples.iter().map(|s| s.ready.as_secs_f64()).collect();
    let exec: Vec<f64> = samples.iter().map(|s| s.first_exec.as_secs_f64()).collect();
    let warm_exec: Vec<f64> = samples
        .iter()
        .map(|s| s.second_exec.as_secs_f64())
        .collect();
    let uptime: Vec<f64> = samples.iter().filter_map(|s| s.guest_uptime).collect();
    let create: Vec<f64> = samples.iter().map(|s| s.create_rpc.as_secs_f64()).collect();
    let remove: Vec<f64> = samples.iter().map(|s| s.remove.as_secs_f64()).collect();

    // The first iteration of a group carries one-time cost (default
    // template build, pool warm-up) and is reported apart from the steady
    // state rather than folded into a median that hides it.
    let (first_ready, rest_ready) = ready.split_first().expect("at least one iteration");
    let (first_exec, rest_exec) = exec.split_first().expect("at least one iteration");
    info!(
        group = label,
        steady_samples = rest_ready.len(),
        first_ready_ms = (first_ready * 1000.0).round(),
        ready_min_ms = ms(min(rest_ready)),
        ready_p50_ms = ms(percentile(rest_ready, 0.50)),
        ready_p95_ms = ms(percentile(rest_ready, 0.95)),
        ready_max_ms = ms(max(rest_ready)),
        first_exec_ms = (first_exec * 1000.0).round(),
        exec_p50_ms = ms(percentile(rest_exec, 0.50)),
        exec_p95_ms = ms(percentile(rest_exec, 0.95)),
        exec_max_ms = ms(max(rest_exec)),
        warm_exec_p50_ms = ms(percentile(&warm_exec, 0.50)),
        guest_uptime_p50_s = percentile(&uptime, 0.50),
        create_rpc_p50_ms = ms(percentile(&create, 0.50)),
        remove_p50_ms = ms(percentile(&remove, 0.50)),
        "sandbox startup summary"
    );

    metrics.record(
        &format!("coldstart_{label}_warm_exec_p50"),
        percentile(&warm_exec, 0.50).unwrap_or_default(),
    );
    for (metric, first, rest) in [
        ("ready", first_ready, rest_ready),
        ("exec", first_exec, rest_exec),
    ] {
        metrics.record(&format!("coldstart_{label}_{metric}_first"), *first);
        metrics.record_distribution(&format!("coldstart_{label}_{metric}"), "seconds", rest);
        for (suffix, value) in [
            ("p50", percentile(rest, 0.50)),
            ("p95", percentile(rest, 0.95)),
            ("max", max(rest)),
        ] {
            if let Some(value) = value {
                metrics.record(&format!("coldstart_{label}_{metric}_{suffix}"), value);
            }
        }
    }
}

fn ms(value: Option<f64>) -> f64 {
    value.map_or(f64::NAN, |v| (v * 1000.0).round())
}

fn min(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::min)
}

fn max(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

#[test]
fn unique_geometry_only_changes_networked_create_and_rejects_overflow() {
    let params = Params {
        iters: 2,
        vcpus: 1,
        memory_mib: u64::MAX,
        unique_geometry: true,
    };
    assert!(params.memory_for(NetworkMode::Enabled, 1).is_err());
    assert_eq!(params.memory_for(NetworkMode::None, 1).unwrap(), u64::MAX);
    let params = Params {
        memory_mib: 512,
        ..params
    };
    assert_eq!(params.memory_for(NetworkMode::Enabled, 1).unwrap(), 513);
    let params = Params {
        unique_geometry: false,
        ..params
    };
    assert_eq!(params.memory_for(NetworkMode::Enabled, 1).unwrap(), 512);
}
