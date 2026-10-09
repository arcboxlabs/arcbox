//! Close RPC admission and retain handler tasks independently of client requests.

#[cfg(test)]
mod tests;

use bytes::Bytes;
use connectrpc::codec::CodecFormat;
use connectrpc::dispatcher::{MethodDescriptor, RequestStream, StreamingResult, UnaryResult};
use connectrpc::payload::Payload;
use connectrpc::{ConnectError, Dispatcher, RequestContext, Router};
use futures::{StreamExt as _, future::BoxFuture};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Shared RPC admission and handler completion barrier.
#[derive(Clone, Default)]
pub struct Calls {
    tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl Calls {
    /// Reject new handlers, cancel admitted operations, and fail client input streams.
    pub fn close(&self) {
        self.tasks.close();
        self.shutdown.cancel();
    }

    /// Wait for all admitted handler futures after admission closes.
    pub async fn wait(&self) {
        self.tasks.wait().await;
    }

    pub(super) fn dispatcher(self, router: Router) -> impl Dispatcher {
        TrackedDispatcher {
            router,
            calls: self,
        }
    }

    fn track<T: Send + 'static>(
        &self,
        make: impl FnOnce() -> BoxFuture<'static, Result<T, ConnectError>>,
    ) -> BoxFuture<'static, Result<T, ConnectError>> {
        // Register before checking admission so close cannot miss an accepted handler.
        let lease = self.tasks.token();
        if self.tasks.is_closed() {
            return Box::pin(async { Err(ConnectError::unavailable("daemon is shutting down")) });
        }
        let future = make();
        // The handler owns the lease so client cancellation cannot abandon cleanup.
        let handler = tokio::spawn(async move {
            let _lease = lease;
            future.await
        });
        Box::pin(async move {
            handler.await.map_err(|error| {
                ConnectError::internal(format!("RPC handler task failed: {error}"))
            })?
        })
    }

    fn input(&self, requests: RequestStream) -> RequestStream {
        Box::pin(futures::stream::unfold(
            Some((requests, self.shutdown.clone())),
            |state| async move {
                let (mut requests, cancelled) = state?;
                tokio::select! {
                    biased;
                    () = cancelled.cancelled() => Some((
                        Err(ConnectError::unavailable("daemon is shutting down")),
                        None,
                    )),
                    item = requests.next() => item.map(|item| (item, Some((requests, cancelled)))),
                }
            },
        ))
    }
}

struct TrackedDispatcher {
    router: Router,
    calls: Calls,
}

impl Dispatcher for TrackedDispatcher {
    fn lookup(&self, path: &str) -> Option<MethodDescriptor> {
        self.router.lookup(path)
    }

    fn call_unary(
        &self,
        path: &str,
        mut ctx: RequestContext,
        request: Payload,
        format: CodecFormat,
    ) -> UnaryResult {
        ctx.extensions_mut().insert(self.calls.shutdown.clone());
        self.calls
            .track(|| self.router.call_unary(path, ctx, request, format))
    }

    fn call_server_streaming(
        &self,
        path: &str,
        mut ctx: RequestContext,
        request: Bytes,
        format: CodecFormat,
    ) -> StreamingResult {
        ctx.extensions_mut().insert(self.calls.shutdown.clone());
        self.calls.track(|| {
            self.router
                .call_server_streaming(path, ctx, request, format)
        })
    }

    fn call_client_streaming(
        &self,
        path: &str,
        mut ctx: RequestContext,
        requests: RequestStream,
        format: CodecFormat,
    ) -> UnaryResult {
        ctx.extensions_mut().insert(self.calls.shutdown.clone());
        self.calls.track(|| {
            self.router
                .call_client_streaming(path, ctx, self.calls.input(requests), format)
        })
    }

    fn call_bidi_streaming(
        &self,
        path: &str,
        mut ctx: RequestContext,
        requests: RequestStream,
        format: CodecFormat,
    ) -> StreamingResult {
        ctx.extensions_mut().insert(self.calls.shutdown.clone());
        self.calls.track(|| {
            self.router
                .call_bidi_streaming(path, ctx, self.calls.input(requests), format)
        })
    }
}
