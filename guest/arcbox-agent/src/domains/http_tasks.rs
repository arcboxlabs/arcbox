//! Retains Hyper's internal connection and stream tasks within one HTTP session.

use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Executor(Arc<Mutex<Option<JoinSet<()>>>>);

impl Default for Executor {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Some(JoinSet::new()))))
    }
}

impl<F> hyper::rt::Executor<F> for Executor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: F) {
        // Closing the scope also rejects tasks spawned by an already-stopping driver.
        if let Some(tasks) = self.0.lock().expect("HTTP tasks lock poisoned").as_mut() {
            tasks.spawn(future);
        }
    }
}

impl Executor {
    pub(super) async fn shutdown(&self) {
        let tasks = self.0.lock().expect("HTTP tasks lock poisoned").take();
        if let Some(mut tasks) = tasks {
            tasks.shutdown().await;
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
