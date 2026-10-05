//! Token cancellation closes the dedicated connection and joins blocking RPC work.

use std::{future::Future, time::Duration};

use arcbox_connect::v1::{
    AgentPingResponse, ReadinessEvent, StorageCheckRequest, StorageCheckResponse,
};
use tokio_util::sync::CancellationToken;

use super::{AgentClient, transport::AgentTransport};
use crate::error::{EngineError, Result};

impl AgentClient {
    /// Cancels a dedicated check through `cancelled` and joins its blocking worker.
    /// The operation owner must await this future after cancellation.
    pub async fn storage_check_with_cancel(
        self,
        request: StorageCheckRequest,
        cancelled: &CancellationToken,
    ) -> Result<StorageCheckResponse> {
        let blocking_request = request.clone();
        self.cancellable(
            cancelled,
            move |client| client.storage_check_blocking(&blocking_request),
            move |client| async move { client.storage_check(&request).await },
        )
        .await
    }

    /// Cancels a dedicated readiness watch and joins its blocking worker.
    /// The operation owner must await this future after cancellation.
    pub async fn watch_readiness_with_cancel(
        self,
        start_runtime: bool,
        timeout: Duration,
        trace_id: &str,
        cancelled: &CancellationToken,
    ) -> Result<ReadinessEvent> {
        let blocking_trace = trace_id.to_owned();
        self.cancellable(
            cancelled,
            move |client| client.watch_readiness_blocking(start_runtime, timeout, &blocking_trace),
            move |client| client.watch_readiness(start_runtime, timeout, trace_id),
        )
        .await
    }

    /// Cancels a dedicated handshake and joins its blocking worker.
    /// The operation owner must await this future after cancellation.
    pub async fn ping_with_cancel(
        self,
        cancelled: &CancellationToken,
    ) -> Result<AgentPingResponse> {
        self.cancellable(
            cancelled,
            |mut client| client.ping_blocking(),
            |mut client| async move {
                tokio::time::timeout(super::transport::BLOCKING_RPC_TIMEOUT, client.ping())
                    .await
                    .map_err(|_| EngineError::Machine("agent handshake timed out".into()))?
            },
        )
        .await
    }

    pub(crate) async fn get_system_info_with_cancel(
        self,
        cancelled: &CancellationToken,
    ) -> Result<arcbox_connect::v1::SystemInfo> {
        self.cancellable(
            cancelled,
            |mut client| client.get_system_info_blocking(),
            |mut client| async move { client.get_system_info().await },
        )
        .await
    }

    async fn cancellable<T: Send + 'static, F: Future<Output = Result<T>>>(
        self,
        cancelled: &CancellationToken,
        blocking: impl FnOnce(Self) -> Result<T> + Send + 'static,
        asynchronous: impl FnOnce(Self) -> F,
    ) -> Result<T> {
        if cancelled.is_cancelled() {
            return Err(cancelled_error());
        }
        if let AgentTransport::Blocking(transport) = &self.transport {
            let shutdown = transport.shutdown_handle().map_err(|error| {
                EngineError::Machine(format!("agent cancellation handle: {error}"))
            })?;
            let mut worker = tokio::task::spawn_blocking(move || blocking(self));
            return tokio::select! {
                biased;
                () = cancelled.cancelled() => {
                    drop(shutdown);
                    // Socket shutdown interrupts poll; join before the VM can be removed.
                    let _ = worker.await.map_err(|error| EngineError::Machine(error.to_string()))?;
                    Err(cancelled_error())
                }
                result = &mut worker => {
                    result.map_err(|error| EngineError::Machine(error.to_string()))?
                }
            };
        }
        tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(cancelled_error()),
            result = asynchronous(self) => result,
        }
    }
}

fn cancelled_error() -> EngineError {
    EngineError::Machine("storage recovery cancelled during daemon shutdown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read as _, os::fd::IntoRawFd as _, os::unix::net::UnixStream};

    #[tokio::test]
    async fn cancellation_joins_the_blocking_worker_after_socket_shutdown() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        // SAFETY: ownership of this connected socket passes to the transport.
        let transport = unsafe {
            arcbox_transport::vsock::BlockingVsockTransport::from_raw_fd(host.into_raw_fd())
                .unwrap()
        };
        let client = AgentClient {
            cid: 3,
            transport: AgentTransport::Blocking(transport),
            connected: true,
            protocol_admitted: false,
        };
        let cancelled = CancellationToken::new();
        let operation_cancelled = cancelled.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, finish) = std::sync::mpsc::channel();
        let operation = tokio::spawn(async move {
            client
                .cancellable(
                    &operation_cancelled,
                    move |_client| {
                        started.send(()).unwrap();
                        finish.recv().unwrap();
                        Ok(())
                    },
                    |_| async { panic!("blocking transport must use its worker") },
                )
                .await
        });
        ready.await.unwrap();
        cancelled.cancel();
        let socket_closed = tokio::task::spawn_blocking(move || {
            guest
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
        });
        socket_closed.await.unwrap();
        assert!(
            !operation.is_finished(),
            "cancellation must wait for the worker"
        );
        release.send(()).unwrap();
        assert!(
            operation
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }
}
