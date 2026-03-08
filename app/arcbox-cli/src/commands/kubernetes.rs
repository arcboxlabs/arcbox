//! Kubernetes integration and lifecycle commands.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, bail};
use arcbox_docker_tools::{HostToolManager, ToolGroup, parse_tools_for_group};
use arcbox_grpc::v1::kubernetes_service_client::KubernetesServiceClient;
use arcbox_protocol::v1::{
    KubernetesDeleteRequest, KubernetesKubeconfigRequest, KubernetesStartRequest,
    KubernetesStatusRequest, KubernetesStopRequest,
};
use clap::Subcommand;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::Request;
use tonic::codegen::{Service, http::Uri};
use tonic::transport::{Channel, Endpoint};

/// Embedded `assets.lock` (same copy used by Docker/Kubernetes host tools).
const LOCK_TOML: &str = include_str!("../../../../assets.lock");
const MANAGED_CONTEXT_NAME: &str = "arcbox";

#[derive(Debug, Subcommand)]
pub enum KubernetesCommands {
    /// Start the native Kubernetes cluster
    Start,
    /// Stop the native Kubernetes cluster
    Stop,
    /// Restart the native Kubernetes cluster
    Restart,
    /// Delete the native Kubernetes cluster state
    Delete,
    /// Show cluster and integration status
    Status,
    /// Enable ArcBox Kubernetes host integration
    Enable,
    /// Disable ArcBox Kubernetes host integration
    Disable,
    /// Print the ArcBox-managed kubeconfig
    Kubeconfig,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct KubernetesIntegrationState {
    enabled: bool,
    previous_context: Option<String>,
}

pub async fn execute(cmd: KubernetesCommands) -> Result<()> {
    match cmd {
        KubernetesCommands::Start => execute_start().await,
        KubernetesCommands::Stop => execute_stop().await,
        KubernetesCommands::Restart => execute_restart().await,
        KubernetesCommands::Delete => execute_delete().await,
        KubernetesCommands::Status => execute_status().await,
        KubernetesCommands::Enable => execute_enable().await,
        KubernetesCommands::Disable => execute_disable().await,
        KubernetesCommands::Kubeconfig => execute_kubeconfig().await,
    }
}

fn resolve_grpc_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("ARCBOX_GRPC_SOCKET") {
        return PathBuf::from(path);
    }

    if let Ok(path) = std::env::var("ARCBOX_SOCKET") {
        let docker_socket = PathBuf::from(path);
        if let Some(parent) = docker_socket.parent() {
            let preferred = parent.join("arcbox-grpc.sock");
            if preferred.exists() {
                return preferred;
            }

            let legacy = parent.join("arcbox.sock");
            if legacy.exists() {
                return legacy;
            }

            return preferred;
        }
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".arcbox")
        .join("arcbox.sock")
}

struct UnixConnector {
    socket_path: PathBuf,
}

impl UnixConnector {
    fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }
}

impl Service<Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let socket_path = self.socket_path.clone();
        Box::pin(async move {
            let stream = UnixStream::connect(socket_path).await?;
            Ok(TokioIo::new(stream))
        })
    }
}

async fn kubernetes_client() -> Result<KubernetesServiceClient<Channel>> {
    let socket_path = resolve_grpc_socket_path();
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(UnixConnector::new(socket_path.clone()))
        .await
        .with_context(|| {
            format!(
                "Failed to connect to ArcBox gRPC daemon at {}",
                socket_path.display()
            )
        })?;

    Ok(KubernetesServiceClient::new(channel))
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine home directory")
}

fn managed_kubeconfig_path(home: &Path) -> PathBuf {
    home.join(".arcbox").join("kube").join("arcbox.yaml")
}

fn integration_state_path(home: &Path) -> PathBuf {
    home.join(".arcbox").join("kube").join("state.json")
}

fn user_kubeconfig_path(home: &Path) -> PathBuf {
    home.join(".kube").join("config")
}

fn runtime_bin_dir(home: &Path) -> PathBuf {
    home.join(".arcbox").join("runtime").join("bin")
}

fn kubectl_bin(home: &Path) -> PathBuf {
    runtime_bin_dir(home).join("kubectl")
}

async fn load_state(home: &Path) -> Result<KubernetesIntegrationState> {
    let path = integration_state_path(home);
    if !path.exists() {
        return Ok(KubernetesIntegrationState::default());
    }

    let bytes = tokio::fs::read(&path).await?;
    serde_json::from_slice(&bytes).context("failed to parse Kubernetes integration state")
}

async fn save_state(home: &Path, state: &KubernetesIntegrationState) -> Result<()> {
    let path = integration_state_path(home);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(state)?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

async fn install_kubernetes_tools(home: &Path) -> Result<()> {
    let tools = parse_tools_for_group(LOCK_TOML, ToolGroup::Kubernetes)
        .context("failed to parse assets.lock")?;
    if tools.is_empty() {
        return Ok(());
    }

    let runtime_bin = runtime_bin_dir(home);
    let arch = arcbox_asset::current_arch().to_string();
    let manager = HostToolManager::new(tools, arch, runtime_bin.clone());
    manager
        .install_all(None)
        .await
        .context("failed to install kubectl")?;

    let user_bin = home.join(".arcbox").join("bin");
    tokio::fs::create_dir_all(&user_bin).await?;
    let target = runtime_bin.join("kubectl");
    let link = user_bin.join("kubectl");
    if tokio::fs::symlink_metadata(&link).await.is_ok() {
        tokio::fs::remove_file(&link).await.ok();
    }
    #[cfg(unix)]
    tokio::fs::symlink(&target, &link).await.with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link.display(),
            target.display()
        )
    })?;

    Ok(())
}

async fn current_context(home: &Path) -> Result<Option<String>> {
    let kubectl = kubectl_bin(home);
    let kubeconfig = user_kubeconfig_path(home);
    if !kubectl.exists() || !kubeconfig.exists() {
        return Ok(None);
    }

    let output = tokio::process::Command::new(&kubectl)
        .arg("config")
        .arg("current-context")
        .arg("--kubeconfig")
        .arg(&kubeconfig)
        .output()
        .await
        .context("failed to query current kube context")?;

    if !output.status.success() {
        return Ok(None);
    }

    let context = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if context.is_empty() {
        Ok(None)
    } else {
        Ok(Some(context))
    }
}

async fn merge_managed_kubeconfig(home: &Path) -> Result<()> {
    let kubectl = kubectl_bin(home);
    let managed = managed_kubeconfig_path(home);
    let user = user_kubeconfig_path(home);

    if let Some(parent) = user.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if !user.exists() {
        tokio::fs::copy(&managed, &user).await?;
        return Ok(());
    }

    let output = tokio::process::Command::new(&kubectl)
        .arg("config")
        .arg("view")
        .arg("--flatten")
        .env(
            "KUBECONFIG",
            format!("{}:{}", user.display(), managed.display()),
        )
        .output()
        .await
        .context("failed to merge kubeconfig")?;

    if !output.status.success() {
        bail!(
            "kubectl config view failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    tokio::fs::write(&user, output.stdout).await?;
    Ok(())
}

async fn set_current_context(home: &Path, context: &str) -> Result<()> {
    let kubectl = kubectl_bin(home);
    let kubeconfig = user_kubeconfig_path(home);
    let output = tokio::process::Command::new(&kubectl)
        .arg("config")
        .arg("use-context")
        .arg(context)
        .arg("--kubeconfig")
        .arg(&kubeconfig)
        .output()
        .await
        .context("failed to switch kube context")?;

    if !output.status.success() {
        bail!(
            "kubectl config use-context failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

async fn delete_context_entries(home: &Path) -> Result<()> {
    let kubectl = kubectl_bin(home);
    let kubeconfig = user_kubeconfig_path(home);
    if !kubectl.exists() || !kubeconfig.exists() {
        return Ok(());
    }

    for args in [
        vec!["config", "delete-context", MANAGED_CONTEXT_NAME],
        vec!["config", "delete-cluster", MANAGED_CONTEXT_NAME],
        vec!["config", "delete-user", MANAGED_CONTEXT_NAME],
    ] {
        let _ = tokio::process::Command::new(&kubectl)
            .args(&args)
            .arg("--kubeconfig")
            .arg(&kubeconfig)
            .output()
            .await;
    }

    Ok(())
}

async fn refresh_managed_kubeconfig(home: &Path) -> Result<()> {
    let mut client = kubernetes_client().await?;
    let response = client
        .get_kubeconfig(Request::new(KubernetesKubeconfigRequest {}))
        .await
        .context("failed to get ArcBox kubeconfig; run 'arcbox k8s start' first")?
        .into_inner();

    let managed = managed_kubeconfig_path(home);
    if let Some(parent) = managed.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&managed, response.kubeconfig).await?;
    Ok(())
}

async fn refresh_if_enabled(home: &Path) -> Result<()> {
    let state = load_state(home).await?;
    if !state.enabled {
        return Ok(());
    }

    refresh_managed_kubeconfig(home).await?;
    merge_managed_kubeconfig(home).await?;
    set_current_context(home, MANAGED_CONTEXT_NAME).await?;
    Ok(())
}

async fn execute_start() -> Result<()> {
    let mut client = kubernetes_client().await?;
    let response = client
        .start(Request::new(KubernetesStartRequest {}))
        .await
        .context("failed to start Kubernetes")?
        .into_inner();

    println!(
        "Kubernetes: {}",
        if response.api_ready {
            "ready"
        } else {
            "starting"
        }
    );
    println!("Endpoint: {}", response.endpoint);
    if !response.detail.is_empty() {
        println!("Detail:   {}", response.detail);
    }

    let home = home_dir()?;
    refresh_if_enabled(&home).await?;
    Ok(())
}

async fn execute_stop() -> Result<()> {
    let mut client = kubernetes_client().await?;
    let response = client
        .stop(Request::new(KubernetesStopRequest {}))
        .await
        .context("failed to stop Kubernetes")?
        .into_inner();

    println!("Kubernetes stopped: {}", response.stopped);
    if !response.detail.is_empty() {
        println!("Detail: {}", response.detail);
    }
    Ok(())
}

async fn execute_restart() -> Result<()> {
    execute_stop().await?;
    execute_start().await
}

async fn execute_delete() -> Result<()> {
    let mut client = kubernetes_client().await?;
    let response = client
        .delete(Request::new(KubernetesDeleteRequest {}))
        .await
        .context("failed to delete Kubernetes")?
        .into_inner();

    println!("Kubernetes cluster deleted.");
    if !response.detail.is_empty() {
        println!("Detail: {}", response.detail);
    }
    Ok(())
}

async fn execute_status() -> Result<()> {
    let home = home_dir()?;
    let state = load_state(&home).await?;
    let kubectl_installed = kubectl_bin(&home).exists();

    let mut client = kubernetes_client().await?;
    let status = client
        .status(Request::new(KubernetesStatusRequest {}))
        .await
        .context("failed to get Kubernetes status")?
        .into_inner();

    println!(
        "Cluster:      {}",
        if status.running { "running" } else { "stopped" }
    );
    println!(
        "API:          {}",
        if status.api_ready {
            "reachable"
        } else {
            "not ready"
        }
    );
    println!("Endpoint:     {}", status.endpoint);
    println!(
        "Integration:  {}",
        if state.enabled { "enabled" } else { "disabled" }
    );
    println!(
        "kubectl:      {}",
        if kubectl_installed {
            "installed"
        } else {
            "not installed"
        }
    );
    if !status.detail.is_empty() {
        println!("Detail:       {}", status.detail);
    }
    for svc in status.services {
        println!("Service {}: {} ({})", svc.name, svc.status, svc.detail);
    }

    Ok(())
}

async fn execute_enable() -> Result<()> {
    let home = home_dir()?;
    install_kubernetes_tools(&home).await?;

    let previous_context = current_context(&home).await?;
    refresh_managed_kubeconfig(&home).await?;
    merge_managed_kubeconfig(&home).await?;
    set_current_context(&home, MANAGED_CONTEXT_NAME).await?;

    save_state(
        &home,
        &KubernetesIntegrationState {
            enabled: true,
            previous_context: previous_context.filter(|ctx| ctx != MANAGED_CONTEXT_NAME),
        },
    )
    .await?;

    println!("Kubernetes integration enabled.");
    println!("Current context: {}", MANAGED_CONTEXT_NAME);
    println!("kubectl installed to {}", kubectl_bin(&home).display());
    Ok(())
}

async fn execute_disable() -> Result<()> {
    let home = home_dir()?;
    let state = load_state(&home).await?;

    delete_context_entries(&home).await?;
    if let Some(previous) = state.previous_context.as_deref() {
        let _ = set_current_context(&home, previous).await;
    }

    save_state(
        &home,
        &KubernetesIntegrationState {
            enabled: false,
            previous_context: state.previous_context,
        },
    )
    .await?;

    println!("Kubernetes integration disabled.");
    Ok(())
}

async fn execute_kubeconfig() -> Result<()> {
    let mut client = kubernetes_client().await?;
    let response = client
        .get_kubeconfig(Request::new(KubernetesKubeconfigRequest {}))
        .await
        .context("failed to get kubeconfig")?
        .into_inner();
    print!("{}", response.kubeconfig);
    Ok(())
}
