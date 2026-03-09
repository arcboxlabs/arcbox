//! REST API route handlers.
//!
//! Sandbox handlers delegate to [`SandboxDispatcher`] which encapsulates all
//! platform-specific dispatch logic, keeping this module `#[cfg]`-free.

use crate::rest::sse;
use crate::rest::types::{ApiError, ApiResult};
use crate::sandbox::{
    AppState, CreateSandboxParams, RunSandboxParams, SandboxInfoResult, SandboxSummaryResult,
};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Builds all v1 routes.
pub fn v1_routes(state: Arc<AppState>) -> Router {
    Router::new()
        // System
        .route("/system/info", get(system_info))
        .route("/system/version", get(system_version))
        // Machines
        .route("/machines", get(list_machines))
        .route("/machines", post(create_machine))
        .route("/machines/{id}", get(inspect_machine))
        .route("/machines/{id}/start", post(start_machine))
        .route("/machines/{id}/stop", post(stop_machine))
        .route("/machines/{id}", delete(remove_machine))
        // Sandboxes
        .route("/sandboxes", get(list_sandboxes))
        .route("/sandboxes", post(create_sandbox))
        .route("/sandboxes/{id}", get(inspect_sandbox))
        .route("/sandboxes/{id}/stop", post(stop_sandbox))
        .route("/sandboxes/{id}", delete(remove_sandbox))
        .route("/sandboxes/{id}/run", post(run_sandbox))
        // Containers (proxy to guest docker)
        .route("/containers", get(list_containers))
        .with_state(state)
}

// =============================================================================
// System
// =============================================================================

#[derive(Serialize)]
struct SystemInfo {
    version: String,
    platform: String,
    arch: String,
    data_dir: String,
    vm_running: bool,
}

async fn system_info(State(state): State<Arc<AppState>>) -> ApiResult<SystemInfo> {
    let config = state.runtime.config();
    Ok(Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        data_dir: config.data_dir.display().to_string(),
        vm_running: state.runtime.vm_lifecycle().is_running().await,
    }))
}

#[derive(Serialize)]
struct VersionInfo {
    version: String,
    api_version: String,
    git_commit: String,
}

async fn system_version() -> Json<VersionInfo> {
    Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: "v1".to_string(),
        git_commit: option_env!("ARCBOX_GIT_COMMIT")
            .unwrap_or("unknown")
            .to_string(),
    })
}

// =============================================================================
// Machines
// =============================================================================

#[derive(Serialize)]
struct MachineSummaryJson {
    id: String,
    name: String,
    state: String,
    cpus: u32,
    memory_mb: u64,
    disk_gb: u64,
    ip_address: Option<String>,
}

#[derive(Serialize)]
struct ListMachinesResponse {
    machines: Vec<MachineSummaryJson>,
}

async fn list_machines(
    State(state): State<Arc<AppState>>,
) -> ApiResult<ListMachinesResponse> {
    let machines = state.runtime.machine_manager().list();
    let items = machines
        .into_iter()
        .map(|m| MachineSummaryJson {
            id: m.name.clone(),
            name: m.name,
            state: format!("{:?}", m.state).to_lowercase(),
            cpus: m.cpus,
            memory_mb: m.memory_mb,
            disk_gb: m.disk_gb,
            ip_address: m.ip_address,
        })
        .collect();
    Ok(Json(ListMachinesResponse { machines: items }))
}

#[derive(Deserialize)]
struct CreateMachineRequest {
    name: String,
    #[serde(default = "default_cpus")]
    cpus: u32,
    #[serde(default = "default_memory_mb")]
    memory_mb: u64,
    #[serde(default = "default_disk_gb")]
    disk_gb: u64,
    #[serde(default)]
    distro: Option<String>,
    #[serde(default)]
    distro_version: Option<String>,
}

fn default_cpus() -> u32 {
    4
}
fn default_memory_mb() -> u64 {
    4096
}
fn default_disk_gb() -> u64 {
    50
}

#[derive(Serialize)]
struct CreateMachineResponse {
    id: String,
}

async fn create_machine(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateMachineRequest>,
) -> Result<Json<CreateMachineResponse>, ApiError> {
    if req.name.is_empty() {
        return Err(ApiError::bad_request("machine name must not be empty"));
    }

    let config = arcbox_core::machine::MachineConfig {
        name: req.name.clone(),
        cpus: req.cpus,
        memory_mb: req.memory_mb,
        disk_gb: req.disk_gb,
        kernel: None,
        cmdline: None,
        distro: req.distro,
        distro_version: req.distro_version,
        block_devices: Vec::new(),
    };

    state
        .runtime
        .machine_manager()
        .create(config)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(CreateMachineResponse { id: req.name }))
}

async fn inspect_machine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MachineSummaryJson>, ApiError> {
    let machine = state
        .runtime
        .machine_manager()
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("machine '{id}' not found")))?;

    Ok(Json(MachineSummaryJson {
        id: machine.name.clone(),
        name: machine.name,
        state: format!("{:?}", machine.state).to_lowercase(),
        cpus: machine.cpus,
        memory_mb: machine.memory_mb,
        disk_gb: machine.disk_gb,
        ip_address: machine.ip_address,
    }))
}

async fn start_machine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .runtime
        .machine_manager()
        .start(&id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": "started"})))
}

async fn stop_machine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .runtime
        .machine_manager()
        .stop(&id)
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": "stopped"})))
}

async fn remove_machine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .runtime
        .machine_manager()
        .remove(&id, false)
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": "removed"})))
}

// =============================================================================
// Sandboxes
// =============================================================================

/// Query parameters for sandbox endpoints.
#[derive(Deserialize)]
struct MachineQuery {
    /// Target VM name (defaults to "default").
    #[serde(default)]
    machine: Option<String>,
}

#[derive(Serialize)]
struct ListSandboxesResponse {
    sandboxes: Vec<SandboxSummaryResult>,
}

async fn list_sandboxes(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MachineQuery>,
) -> ApiResult<ListSandboxesResponse> {
    let sandboxes = state
        .dispatcher
        .list(query.machine.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ListSandboxesResponse { sandboxes }))
}

#[derive(Deserialize)]
struct CreateSandboxRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    kernel: Option<String>,
    #[serde(default)]
    rootfs: Option<String>,
    #[serde(default)]
    vcpus: Option<u32>,
    #[serde(default)]
    memory_mib: Option<u64>,
    #[serde(default)]
    cmd: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    labels: HashMap<String, String>,
    #[serde(default)]
    ttl_seconds: Option<u32>,
    #[serde(default)]
    machine: Option<String>,
}

#[derive(Serialize)]
struct CreateSandboxResponse {
    id: String,
    ip_address: String,
    state: String,
}

async fn create_sandbox(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MachineQuery>,
    Json(req): Json<CreateSandboxRequest>,
) -> Result<Json<CreateSandboxResponse>, ApiError> {
    let machine = req
        .machine
        .as_deref()
        .or(query.machine.as_deref());

    let result = state
        .dispatcher
        .create(
            CreateSandboxParams {
                id: req.id,
                kernel: req.kernel,
                rootfs: req.rootfs,
                vcpus: req.vcpus,
                memory_mib: req.memory_mib,
                cmd: req.cmd,
                env: req.env,
                labels: req.labels,
                ttl_seconds: req.ttl_seconds,
            },
            machine,
        )
        .await?;

    Ok(Json(CreateSandboxResponse {
        id: result.id,
        ip_address: result.ip_address,
        state: result.state,
    }))
}

async fn inspect_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<MachineQuery>,
) -> Result<Json<SandboxInfoResult>, ApiError> {
    let info = state
        .dispatcher
        .inspect(&id, query.machine.as_deref())
        .await?;
    Ok(Json(info))
}

/// Optional body for stop requests.
#[derive(Default, Deserialize)]
struct StopSandboxRequest {
    #[serde(default = "default_stop_timeout")]
    timeout_seconds: u32,
}

fn default_stop_timeout() -> u32 {
    30
}

async fn stop_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<MachineQuery>,
    body: Option<Json<StopSandboxRequest>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout = body.map_or(30, |Json(b)| b.timeout_seconds);
    state
        .dispatcher
        .stop(&id, timeout, query.machine.as_deref())
        .await?;
    Ok(Json(serde_json::json!({"status": "stopped"})))
}

/// Query parameters for sandbox removal.
#[derive(Deserialize)]
struct RemoveSandboxQuery {
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn remove_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<RemoveSandboxQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .dispatcher
        .remove(&id, query.force, query.machine.as_deref())
        .await?;
    Ok(Json(serde_json::json!({"status": "removed"})))
}

#[derive(Deserialize)]
struct RunSandboxRequest {
    cmd: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    timeout_seconds: Option<u32>,
}

async fn run_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<MachineQuery>,
    Json(req): Json<RunSandboxRequest>,
) -> Result<Response, ApiError> {
    let rx = state
        .dispatcher
        .run(
            &id,
            RunSandboxParams {
                cmd: req.cmd,
                env: req.env,
                working_dir: req.working_dir,
                user: req.user,
                tty: req.tty,
                timeout_seconds: req.timeout_seconds,
            },
            query.machine.as_deref(),
        )
        .await?;

    use axum::response::sse::Event;
    use std::convert::Infallible;
    use tokio_stream::StreamExt as _;

    let stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|chunk| {
            let data = serde_json::json!({
                "stream": chunk.stream,
                "data": String::from_utf8_lossy(&chunk.data),
                "exit_code": chunk.exit_code,
                "done": chunk.done,
            });
            Ok::<_, Infallible>(Event::default().data(data.to_string()))
        });

    Ok(sse::sse_response(stream))
}

// =============================================================================
// Containers (proxy to guest docker)
// =============================================================================

async fn list_containers() -> Response {
    // Container management is handled by the Docker-compatible API socket.
    // Clients should use the Docker socket for full container operations.
    ApiError::not_implemented("use the Docker-compatible API socket for container management")
        .into_response()
}
