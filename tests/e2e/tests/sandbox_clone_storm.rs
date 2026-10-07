//! Concurrent snapshot restores against an isolated daemon with nested virtualization.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use arcbox_e2e::metrics::RunMetrics;
use arcbox_e2e::sandbox_bench::{
    admit, env_usize, finish, run, run_and_collect, wait_for_ready, with_machine,
};
use arcbox_grpc::sandbox_v1::sandbox_process_service_client::SandboxProcessServiceClient;
use arcbox_grpc::sandbox_v1::sandbox_service_client::SandboxServiceClient;
use arcbox_grpc::sandbox_v1::sandbox_snapshot_service_client::SandboxSnapshotServiceClient;
use arcbox_protocol::sandbox_v1::{
    CheckpointRequest, CreateSandboxRequest, DeleteSnapshotRequest, NetworkMode, NetworkSpec,
    RemoveSandboxRequest, ResourceLimits, RestoreRequest, SandboxEventsRequest,
};
use tokio::task::JoinSet;
use tonic::transport::Channel;
use tracing::info;

const RPC_TIMEOUT: Duration = Duration::from_secs(180);
const TEMPLATE_ID: &str = "storm-template";

#[test]
#[ignore = "requires nested virtualization (VZ on M3+), boot assets, and a signed daemon"]
fn sandbox_clone_storm() -> Result<()> {
    let degrees = match std::env::var("ARCBOX_STORM_DEGREES") {
        Ok(value) => parse_degrees(&value)?,
        Err(std::env::VarError::NotPresent) => vec![1, 2, 4, 8, 16],
        Err(error) => return Err(error).context("reading ARCBOX_STORM_DEGREES"),
    };
    let params = Params {
        degrees,
        rounds: env_usize("ARCBOX_STORM_ROUNDS", 3)?,
        vcpus: u32::try_from(env_usize("ARCBOX_STORM_VCPUS", 2)?)?,
        memory_mib: u64::try_from(env_usize("ARCBOX_STORM_MEMORY_MIB", 2048)?)?,
    };
    run("sandbox_clone_storm", async move |channel, metrics| {
        drive(channel, metrics, params).await
    })
}

struct Params {
    degrees: Vec<usize>,
    rounds: usize,
    vcpus: u32,
    memory_mib: u64,
}

fn parse_degrees(value: &str) -> Result<Vec<usize>> {
    let mut seen = HashSet::new();
    value
        .split(',')
        .map(|part| {
            let degree = part
                .trim()
                .parse::<usize>()
                .context("ARCBOX_STORM_DEGREES must contain comma-separated positive integers")?;
            ensure!(
                degree > 0 && seen.insert(degree),
                "storm degrees must be positive and unique"
            );
            Ok(degree)
        })
        .collect()
}

async fn drive(channel: Channel, metrics: &mut RunMetrics, params: Params) -> Result<()> {
    let mut client = SandboxServiceClient::new(channel.clone());
    let mut processes = SandboxProcessServiceClient::new(channel.clone());
    let mut snapshots = SandboxSnapshotServiceClient::new(channel);
    metrics.record_distribution("storm_memory", "MiB", &[params.memory_mib as f64]);
    metrics.record_distribution("storm_vcpus", "vCPUs", &[f64::from(params.vcpus)]);
    let mut snapshot_id = None;
    let result = async {
        let mut events = client
            .events(with_machine(SandboxEventsRequest::default()))
            .await?
            .into_inner();
        let admitted = admit(TEMPLATE_ID, async || {
            client
                .create(with_machine(CreateSandboxRequest {
                    id: TEMPLATE_ID.into(),
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
        .await?;
        metrics.record("storm_template_admission_wait", admitted.wait.as_secs_f64());
        wait_for_ready(&mut events, TEMPLATE_ID).await?;
        let output =
            run_and_collect(&mut processes, TEMPLATE_ID, &["/bin/echo", "template-warm"]).await?;
        ensure!(
            output.trim() == "template-warm",
            "template execution did not return its marker"
        );
        let checkpoint = tokio::time::timeout(
            RPC_TIMEOUT,
            snapshots.checkpoint(with_machine(CheckpointRequest {
                sandbox_id: TEMPLATE_ID.into(),
                name: "clone-storm".into(),
                ..Default::default()
            })),
        )
        .await
        .context("checkpoint timed out")??
        .into_inner();
        snapshot_id = Some(checkpoint.snapshot_id.clone());
        for degree in params.degrees {
            let mut steady = Measurements::default();
            for round in 0..params.rounds {
                let label = format!("storm_{degree}_round_{round}");
                let (measured, result) = restore_round(
                    &mut client,
                    snapshots.clone(),
                    &checkpoint.snapshot_id,
                    degree,
                    round,
                )
                .await;
                measured.record(&label, metrics);
                result?;
                if round > 0 {
                    steady.extend(measured);
                }
            }
            steady.record(&format!("storm_{degree}_steady"), metrics);
        }
        Ok(())
    }
    .await;
    let result = finish(
        result,
        remove_all(&mut client, &[TEMPLATE_ID.to_owned()]).await,
    );
    match snapshot_id {
        Some(snapshot_id) => {
            let cleanup = async {
                tokio::time::timeout(
                    RPC_TIMEOUT,
                    snapshots.delete_snapshot(with_machine(DeleteSnapshotRequest { snapshot_id })),
                )
                .await
                .context("snapshot deletion timed out")?
                .context("snapshot deletion failed")?;
                Ok(())
            }
            .await;
            finish(result, cleanup)
        }
        None => result,
    }
}

#[derive(Default)]
struct Measurements {
    restore: Vec<f64>,
    admission_wait: Vec<f64>,
    wall: Vec<f64>,
    throughput: Vec<f64>,
}

impl Measurements {
    fn record(&self, label: &str, metrics: &mut RunMetrics) {
        for (name, unit, values) in [
            ("restore", "seconds", &self.restore),
            ("admission_wait", "seconds", &self.admission_wait),
            ("wall", "seconds", &self.wall),
            ("throughput", "per_second", &self.throughput),
        ] {
            metrics.record_distribution(&format!("{label}_{name}"), unit, values);
        }
    }

    fn extend(&mut self, round: Self) {
        self.restore.extend(round.restore);
        self.admission_wait.extend(round.admission_wait);
        self.wall.extend(round.wall);
        self.throughput.extend(round.throughput);
    }
}

async fn restore_round(
    client: &mut SandboxServiceClient<Channel>,
    snapshots: SandboxSnapshotServiceClient<Channel>,
    snapshot_id: &str,
    degree: usize,
    round: usize,
) -> (Measurements, Result<()>) {
    let ids: Vec<_> = (0..degree)
        .map(|i| format!("storm-{degree}-{round}-{i}"))
        .collect();
    let mut tasks = JoinSet::new();
    let started = Instant::now();
    for id in &ids {
        let id = id.clone();
        let snapshot_id = snapshot_id.to_owned();
        let mut snapshots = snapshots.clone();
        tasks.spawn_local(async move {
            let admitted = admit(&id, async || {
                snapshots
                    .restore(with_machine(RestoreRequest {
                        id: id.clone(),
                        snapshot_id: snapshot_id.clone(),
                        network_override: true,
                        ..Default::default()
                    }))
                    .await
            })
            .await;
            (id, Instant::now(), admitted)
        });
    }
    let mut measured = Measurements::default();
    let mut errors = Vec::new();
    let mut ips = HashSet::new();
    let mut last_completed = started;
    // Drain every launched RPC before cleanup, including after the first failure.
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((id, completed, result)) => {
                last_completed = last_completed.max(completed);
                match result {
                    Ok(admitted) => {
                        measured
                            .restore
                            .push(completed.duration_since(admitted.started).as_secs_f64());
                        measured.admission_wait.push(admitted.wait.as_secs_f64());
                        let response = admitted.response.into_inner();
                        if response.id != id
                            || response.ip_address.is_empty()
                            || !ips.insert(response.ip_address)
                        {
                            errors.push(format!(
                                "{id}: Restore returned an invalid identity or duplicate/empty IP"
                            ));
                        }
                    }
                    Err(error) => errors.push(format!("{id}: {error:#}")),
                }
            }
            Err(error) => errors.push(format!("restore task failed: {error}")),
        }
    }
    let wall = last_completed.duration_since(started).as_secs_f64();
    measured.wall.push(wall);
    if errors.is_empty() {
        measured.throughput.push(degree as f64 / wall);
    }
    info!(
        degree,
        round,
        wall_seconds = wall,
        completed = measured.restore.len(),
        "clone round completed"
    );
    let result = if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(errors.join("; ")))
    };
    let result = finish(result, remove_all(client, &ids).await);
    (measured, result)
}

async fn remove_all(client: &mut SandboxServiceClient<Channel>, ids: &[String]) -> Result<()> {
    let mut errors = Vec::new();
    for id in ids {
        match tokio::time::timeout(
            RPC_TIMEOUT,
            client.remove(with_machine(RemoveSandboxRequest {
                id: id.clone(),
                force: true,
            })),
        )
        .await
        {
            Ok(Ok(_)) => {}
            // Failed RPCs can leave no sandbox, or can commit before losing their response.
            Ok(Err(status)) if status.code() == tonic::Code::NotFound => {}
            Ok(Err(status)) => errors.push(format!("Remove {id}: {status}")),
            Err(error) => errors.push(format!("Remove {id} timed out: {error}")),
        }
    }
    if !errors.is_empty() {
        bail!(errors.join("; "));
    }
    Ok(())
}

#[test]
fn degree_parser_rejects_partial_or_duplicate_workloads() {
    assert_eq!(parse_degrees("1, 2,4").unwrap(), [1, 2, 4]);
    for value in ["", "1,", "1,broken,4", "0,2", "1,1"] {
        assert!(parse_degrees(value).is_err(), "{value:?}");
    }
}
