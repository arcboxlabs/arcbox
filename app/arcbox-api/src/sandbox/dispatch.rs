//! Platform-split sandbox dispatch implementation.
//!
//! Each method contains a `#[cfg(target_os = "linux")]` and
//! `#[cfg(not(target_os = "linux"))]` block. On Linux the calls go to
//! `SandboxManager` directly; on macOS they proxy through the guest agent.

use super::{
    CreateSandboxParams, CreateSandboxResult, RunOutputChunk, RunSandboxParams, SandboxError,
    SandboxInfoResult, SandboxSummaryResult,
};
use arcbox_core::Runtime;
use std::sync::Arc;
use tokio::sync::mpsc;

#[cfg(not(target_os = "linux"))]
use arcbox_protocol::sandbox_v1 as proto;

/// Routes sandbox operations to the platform-appropriate backend.
pub struct SandboxDispatcher {
    runtime: Arc<Runtime>,
}

impl SandboxDispatcher {
    /// Creates a new dispatcher backed by the given runtime.
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    // ─────────────────────────────────────────────────────────────────────
    // macOS helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Resolves a machine name, falling back to the runtime default.
    #[cfg(not(target_os = "linux"))]
    fn resolve_machine<'a>(&'a self, name: Option<&'a str>) -> &'a str {
        match name {
            Some(s) if !s.is_empty() => s,
            _ => self.runtime.default_machine_name(),
        }
    }

    /// Formats a Unix timestamp (seconds) as RFC 3339.
    #[cfg(not(target_os = "linux"))]
    fn unix_to_rfc3339(secs: i64) -> String {
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Dispatch methods
    // ─────────────────────────────────────────────────────────────────────

    /// Lists sandboxes.
    pub async fn list(
        &self,
        machine: Option<&str>,
    ) -> Result<Vec<SandboxSummaryResult>, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = machine;
            if let Some(mgr) = self.runtime.sandbox_manager() {
                let sandboxes = mgr
                    .list_sandboxes(None, &std::collections::HashMap::new())
                    .into_iter()
                    .map(|s| SandboxSummaryResult {
                        id: s.id,
                        state: s.state.to_string(),
                        ip_address: s.ip_address,
                        labels: s.labels,
                        created_at: s.created_at.to_rfc3339(),
                    })
                    .collect();
                return Ok(sandboxes);
            }
            Ok(Vec::new())
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let mut agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            let resp = agent
                .sandbox_list(proto::ListSandboxesRequest::default())
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            let sandboxes = resp
                .sandboxes
                .into_iter()
                .map(|s| SandboxSummaryResult {
                    id: s.id,
                    state: s.state,
                    ip_address: s.ip_address,
                    labels: s.labels,
                    created_at: Self::unix_to_rfc3339(s.created_at),
                })
                .collect();
            Ok(sandboxes)
        }
    }

    /// Creates a sandbox.
    pub async fn create(
        &self,
        params: CreateSandboxParams,
        machine: Option<&str>,
    ) -> Result<CreateSandboxResult, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            use arcbox_vm::{SandboxNetworkSpec, SandboxSpec};

            let _ = machine;
            let mgr = self.runtime.sandbox_manager().ok_or_else(|| {
                SandboxError::Unavailable("sandbox subsystem is not enabled".to_string())
            })?;

            let spec = SandboxSpec {
                id: params.id,
                labels: params.labels,
                kernel: params.kernel.unwrap_or_default(),
                rootfs: params.rootfs.unwrap_or_default(),
                vcpus: params.vcpus.unwrap_or(0),
                memory_mib: params.memory_mib.unwrap_or(0),
                cmd: params.cmd,
                env: params.env,
                ttl_seconds: params.ttl_seconds.unwrap_or(0),
                network: SandboxNetworkSpec::default(),
                ..SandboxSpec::default()
            };

            let (id, ip) = mgr
                .create_sandbox(spec)
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            Ok(CreateSandboxResult {
                id,
                ip_address: ip,
                state: "starting".to_string(),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let mut agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            let proto_req = proto::CreateSandboxRequest {
                id: params.id.unwrap_or_default(),
                labels: params.labels,
                kernel: params.kernel.unwrap_or_default(),
                rootfs: params.rootfs.unwrap_or_default(),
                limits: Some(proto::ResourceLimits {
                    vcpus: params.vcpus.unwrap_or(0),
                    memory_mib: params.memory_mib.unwrap_or(0),
                }),
                cmd: params.cmd,
                env: params.env,
                ttl_seconds: params.ttl_seconds.unwrap_or(0),
                ..proto::CreateSandboxRequest::default()
            };

            let resp = agent
                .sandbox_create(proto_req)
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            Ok(CreateSandboxResult {
                id: resp.id,
                ip_address: resp.ip_address,
                state: resp.state,
            })
        }
    }

    /// Inspects a sandbox.
    pub async fn inspect(
        &self,
        id: &str,
        machine: Option<&str>,
    ) -> Result<SandboxInfoResult, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = machine;
            let mgr = self.runtime.sandbox_manager().ok_or_else(|| {
                SandboxError::Unavailable("sandbox subsystem is not enabled".to_string())
            })?;
            let info = mgr
                .inspect_sandbox(id)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            Ok(SandboxInfoResult {
                id: info.id,
                state: info.state.to_string(),
                vcpus: info.vcpus,
                memory_mib: info.memory_mib,
                created_at: info.created_at.to_rfc3339(),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let mut agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            let info = agent
                .sandbox_inspect(proto::InspectSandboxRequest {
                    id: id.to_string(),
                })
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            let limits = info.limits.unwrap_or_default();
            Ok(SandboxInfoResult {
                id: info.id,
                state: info.state,
                vcpus: limits.vcpus,
                memory_mib: limits.memory_mib,
                created_at: Self::unix_to_rfc3339(info.created_at),
            })
        }
    }

    /// Stops a sandbox gracefully.
    pub async fn stop(
        &self,
        id: &str,
        timeout_seconds: u32,
        machine: Option<&str>,
    ) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = machine;
            let mgr = self.runtime.sandbox_manager().ok_or_else(|| {
                SandboxError::Unavailable("sandbox subsystem is not enabled".to_string())
            })?;
            mgr.stop_sandbox(id, timeout_seconds)
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let mut agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            agent
                .sandbox_stop(proto::StopSandboxRequest {
                    id: id.to_string(),
                    timeout_seconds,
                })
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            Ok(())
        }
    }

    /// Removes a sandbox.
    pub async fn remove(
        &self,
        id: &str,
        force: bool,
        machine: Option<&str>,
    ) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = machine;
            let mgr = self.runtime.sandbox_manager().ok_or_else(|| {
                SandboxError::Unavailable("sandbox subsystem is not enabled".to_string())
            })?;
            mgr.remove_sandbox(id, force)
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let mut agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            agent
                .sandbox_remove(proto::RemoveSandboxRequest {
                    id: id.to_string(),
                    force,
                })
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;
            Ok(())
        }
    }

    /// Runs a command inside a sandbox and returns a streaming output receiver.
    pub async fn run(
        &self,
        id: &str,
        params: RunSandboxParams,
        machine: Option<&str>,
    ) -> Result<mpsc::UnboundedReceiver<RunOutputChunk>, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = machine;
            let mgr = self.runtime.sandbox_manager().ok_or_else(|| {
                SandboxError::Unavailable("sandbox subsystem is not enabled".to_string())
            })?;

            let mut rx = mgr
                .run_in_sandbox(
                    id,
                    params.cmd,
                    params.env,
                    params.working_dir.unwrap_or_default(),
                    params.user.unwrap_or_default(),
                    params.tty,
                    None,
                    params.timeout_seconds.unwrap_or(0),
                )
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            // Bridge bounded mpsc::Receiver<Result<OutputChunk>> into
            // unbounded channel of RunOutputChunk.
            let (tx, new_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    match result {
                        Ok(chunk) => {
                            let done = chunk.stream == "exit";
                            let _ = tx.send(RunOutputChunk {
                                stream: chunk.stream,
                                data: chunk.data,
                                exit_code: chunk.exit_code,
                                done,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(RunOutputChunk {
                                stream: "stderr".to_string(),
                                data: e.to_string().into_bytes(),
                                exit_code: -1,
                                done: true,
                            });
                            break;
                        }
                    }
                }
            });
            Ok(new_rx)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let machine = self.resolve_machine(machine);
            let agent = self
                .runtime
                .get_agent(machine)
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            let proto_req = proto::RunRequest {
                id: id.to_string(),
                cmd: params.cmd,
                env: params.env,
                working_dir: params.working_dir.unwrap_or_default(),
                user: params.user.unwrap_or_default(),
                tty: params.tty,
                timeout_seconds: params.timeout_seconds.unwrap_or(0),
            };

            let rx = agent
                .sandbox_run(proto_req)
                .await
                .map_err(|e| SandboxError::Internal(e.to_string()))?;

            // Bridge UnboundedReceiver<Result<RunOutput>> into
            // UnboundedReceiver<RunOutputChunk>.
            let (tx, new_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                use tokio_stream::StreamExt as _;
                let mut stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
                while let Some(result) = stream.next().await {
                    let chunk = result.unwrap_or_else(|e| proto::RunOutput {
                        stream: "stderr".to_string(),
                        data: e.to_string().into_bytes(),
                        exit_code: -1,
                        done: true,
                    });
                    let _ = tx.send(RunOutputChunk {
                        stream: chunk.stream,
                        data: chunk.data,
                        exit_code: chunk.exit_code,
                        done: chunk.done,
                    });
                }
            });
            Ok(new_rx)
        }
    }
}
