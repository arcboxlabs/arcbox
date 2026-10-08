//! Prepare the private identity and report HTTPS trust independently of VM startup.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use arcbox_api::SetupState;
use arcbox_connect::v1::{LocalHttpsStatus, local_https_status::TrustState};
use arcbox_constants::paths::guest::TLS;
use arcbox_core::local_https::Status;
use arcbox_local_ca::{KeychainAuthority, Publication};
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

const STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// Keeps CA failures visible without preventing ordinary container startup.
pub async fn prepare(ctx: &DaemonContext) {
    let dir = ctx.layout.data_dir.join(TLS);
    let prepared = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || -> Result<_> {
            std::fs::create_dir_all(&dir)?;
            arcbox_local_ca::remove_shared_private_key(&dir)?;
            let authority = KeychainAuthority::open(&dir)?;
            if authority.publish(&dir)? == Publication::AwaitingRevocation {
                tracing::info!("previous local CA retained until explicit trust revocation");
            }
            Ok(Arc::new(authority))
        }
    })
    .await
    .context("CA preparation task failed")
    .and_then(std::convert::identity);
    match prepared {
        Ok(authority) => {
            assert!(
                ctx.local_ca.set(authority).is_ok(),
                "CA preparation runs once"
            );
            let state = Arc::clone(&ctx.setup_state);
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                monitor(&state, &shutdown, || inspect(&dir)).await;
            });
        }
        Err(error) => {
            tracing::error!(%error, "local HTTPS preparation failed");
            ctx.setup_state
                .set_local_https(failed(format!("{error:#}")));
        }
    }
}

fn failed(error: String) -> LocalHttpsStatus {
    LocalHttpsStatus {
        trust_state: TrustState::Error.into(),
        message: format!("Local HTTPS is unavailable: {error}"),
        ..Default::default()
    }
}

async fn inspect(dir: &Path) -> LocalHttpsStatus {
    match Status::inspect(dir).await {
        Ok(status) => status.into(),
        Err(error) => failed(error.to_string()),
    }
}

async fn monitor<F: Future<Output = LocalHttpsStatus>>(
    state: &SetupState,
    shutdown: &CancellationToken,
    mut inspect: impl FnMut() -> F,
) {
    let mut interval = tokio::time::interval(STATUS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = None;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = interval.tick() => {}
        }
        let status = tokio::select! {
            () = shutdown.cancelled() => return,
            status = inspect() => status,
        };
        if previous.as_ref() != Some(&status) {
            state.set_local_https(status.clone());
            previous = Some(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_api::SetupPhase;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn trust_errors_and_cancellation_leave_docker_ready() {
        let state = Arc::new(SetupState::new());
        state.set_phase(SetupPhase::Ready, "Docker ready");
        state.set_vm_running(true);
        let shutdown = CancellationToken::new();
        let inspections = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            let shutdown = shutdown.clone();
            let inspections = Arc::clone(&inspections);
            async move {
                monitor(&state, &shutdown, || {
                    let status = match inspections.fetch_add(1, Ordering::SeqCst) {
                        0 => LocalHttpsStatus {
                            trust_state: TrustState::Untrusted.into(),
                            ..Default::default()
                        },
                        1 => failed("helper disconnected".into()),
                        _ => LocalHttpsStatus {
                            trust_state: TrustState::Trusted.into(),
                            ..Default::default()
                        },
                    };
                    std::future::ready(status)
                })
                .await;
            }
        });
        for expected in [
            TrustState::Untrusted,
            TrustState::Error,
            TrustState::Trusted,
        ] {
            tokio::task::yield_now().await;
            let current = state.current();
            assert_eq!(current.local_https.trust_state, expected);
            assert_eq!(current.phase, SetupPhase::Ready);
            assert!(current.vm_running);
            tokio::time::advance(STATUS_INTERVAL).await;
        }
        shutdown.cancel();
        task.await.unwrap();
        let stopped_at = inspections.load(Ordering::SeqCst);
        tokio::time::advance(STATUS_INTERVAL).await;
        assert_eq!(inspections.load(Ordering::SeqCst), stopped_at);
        assert_eq!(state.current().phase, SetupPhase::Ready);
    }
}
