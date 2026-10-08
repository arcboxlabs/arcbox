//! Retains resolver publication ownership across startup cancellation and shutdown.

use std::{future::Future, sync::Arc};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio_util::task::AbortOnDropHandle;

#[derive(Clone, Default)]
pub struct ResolverService {
    task: Arc<Mutex<Option<AbortOnDropHandle<Result<()>>>>>,
}

impl ResolverService {
    pub async fn start(&self, run: impl Future<Output = Result<()>> + Send + 'static) {
        let mut task = self.task.lock().await;
        assert!(task.is_none(), "resolver service must start once");
        *task = Some(AbortOnDropHandle::new(tokio::spawn(run)));
    }

    pub async fn join(&self) -> Result<()> {
        let mut task = self.task.lock().await;
        let Some(handle) = task.as_mut() else {
            return Ok(());
        };
        // Retain ownership if the caller cancels this join before cleanup finishes.
        let result = handle.await.context("resolver service task failed");
        *task = None;
        result?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{pin::pin, task::Poll};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn interrupted_join_retains_cleanup_and_its_error() {
        let service = ResolverService::default();
        let survivor = service.clone();
        let (finish, pending) = oneshot::channel();
        service
            .start(async move {
                pending.await?;
                anyhow::bail!("cleanup failed")
            })
            .await;
        {
            let mut joining = pin!(service.join());
            assert!(matches!(futures::poll!(&mut joining), Poll::Pending));
        }
        drop(service);
        assert!(survivor.task.lock().await.is_some());
        finish.send(()).unwrap();
        assert_eq!(
            survivor.join().await.unwrap_err().to_string(),
            "cleanup failed"
        );
        assert!(survivor.task.lock().await.is_none());
        survivor.join().await.unwrap();
    }
}
