//! Cancels retired guest sessions without dropping active native work.

use std::future::Future;

use super::*;

pub(super) async fn guest_changed<T: Send + Sync>(
    mut changed: watch::Receiver<T>,
    current: impl Fn() -> Result<bool> + Send,
) -> Result<()> {
    loop {
        changed.borrow_and_update();
        if !current()? {
            return Ok(());
        }
        tokio::select! {
            update = changed.changed() => update?,
            () = tokio::time::sleep(IDENTITY_INTERVAL) => {}
        }
    }
}

pub(super) async fn drain_on_change(
    work: impl Future<Output = Result<()>>,
    changed: impl Future<Output = Result<()>>,
    shutdown: &CancellationToken,
) -> Result<()> {
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => result,
        changed = changed => {
            shutdown.cancel();
            let result = work.await;
            changed?;
            result
        }
    }
}
