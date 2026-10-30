//! Retains Hyper's internal connection and stream tasks within one HTTP session.

use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Executor(Arc<Mutex<Scope>>);

#[derive(Default)]
struct Scope {
    closed: bool,
    tasks: JoinSet<()>,
    children: Vec<Executor>,
}

impl Default for Executor {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Scope::default())))
    }
}

impl<F> hyper::rt::Executor<F> for Executor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: F) {
        // Closing the scope also rejects tasks spawned by an already-stopping driver.
        let mut scope = self.0.lock().expect("HTTP tasks lock poisoned");
        if !scope.closed {
            while let Some(result) = scope.tasks.try_join_next() {
                if let Err(error) = result {
                    tracing::warn!(%error, "managed HTTP internal task failed");
                }
            }
            scope.tasks.spawn(future);
        }
    }
}

impl Executor {
    pub(super) fn child(&self) -> Self {
        let mut parent = self.0.lock().expect("HTTP tasks lock poisoned");
        let child = Self(Arc::new(Mutex::new(Scope {
            closed: parent.closed,
            ..Scope::default()
        })));
        parent.children.push(child.clone());
        child
    }

    pub(super) async fn shutdown(&self) {
        let mut pending = vec![self.clone()];
        while let Some(executor) = pending.pop() {
            {
                let mut scope = executor.0.lock().expect("HTTP tasks lock poisoned");
                scope.closed = true;
                scope.tasks.abort_all();
                pending.extend(scope.children.iter().cloned());
            }
            // Keep handles in the shared scope if cancellation interrupts a child's cleanup.
            while std::future::poll_fn(|context| {
                executor
                    .0
                    .lock()
                    .expect("HTTP tasks lock poisoned")
                    .tasks
                    .poll_join_next(context)
            })
            .await
            .is_some()
            {}
        }
    }
}

pub(super) async fn run(
    cancelled: &CancellationToken,
    operation: impl AsyncFnOnce(Executor) -> Result<()>,
) -> Result<()> {
    let executor = Executor::default();
    let result = tokio::select! {
        () = cancelled.cancelled() => Ok(()),
        result = operation(executor.clone()) => result,
    };
    executor.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::rt::Executor as _;

    #[tokio::test]
    async fn cancellation_joins_an_internal_task_before_returning() {
        let cancelled = CancellationToken::new();
        let shutdown = cancelled.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (released, dropped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            run(&cancelled, async move |executor| {
                executor.execute(async move {
                    let _released = released;
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                });
                std::future::pending().await
            })
            .await
        });
        ready.await.unwrap();
        shutdown.cancel();
        task.await.unwrap().unwrap();
        assert!(dropped.await.is_err());
    }
}
