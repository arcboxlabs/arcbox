//! Recover the published private identity and report HTTPS trust independently of VM startup.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{LocalHttpsStatus, local_https_status::TrustState};
use arcbox_constants::paths::guest::{TLS, TLS_CA_CERT};
use arcbox_core::local_https::Status;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority, Publication};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

const STATUS_INTERVAL: Duration = Duration::from_secs(5);

mod rotation;

/// Keeps CA failures visible without preventing ordinary container startup.
pub fn prepare(ctx: &DaemonContext) {
    let dir = ctx.layout.data_dir.join(TLS);
    let state = Arc::clone(&ctx.setup_state);
    let authority = ctx.local_ca.clone();
    let shutdown = ctx.shutdown.clone();
    let commands = state
        .local_ca
        .register()
        .expect("TLS worker is registered once during runtime initialization");
    state.set_local_https(LocalHttpsStatus {
        message: "Preparing the local HTTPS certificate.".into(),
        ..Default::default()
    });
    tokio::spawn(async move {
        monitor(&shutdown, commands, |command| {
            let dir = dir.clone();
            let authority = authority.clone();
            let state = Arc::clone(&state);
            async move {
                if let Some(command) = command {
                    rotation::execute(&dir, &authority, &state, command).await;
                } else {
                    let status = refresh(&dir, &authority).await;
                    if *state.current().local_https != status {
                        state.set_local_https(status);
                    }
                }
            }
        })
        .await;
    });
}

async fn refresh(
    dir: &Path,
    authority: &watch::Sender<Option<Arc<KeychainAuthority>>>,
) -> LocalHttpsStatus {
    let mut active = authority.borrow().clone();
    let prepared = tokio::task::spawn_blocking({
        let dir = dir.to_owned();
        move || {
            let operation = match arcbox_local_ca::lock_profile(&dir) {
                Ok(operation) => operation,
                Err(error) => return (active, Err(error.into()), None),
            };
            let prepared = (|| -> Result<_> {
                std::fs::create_dir_all(&dir)?;
                arcbox_local_ca::remove_shared_private_key(&dir)?;
                let authority = KeychainAuthority::open(&dir)?;
                let publication = authority.publish(&dir)?;
                Ok((Arc::new(authority), publication))
            })();
            let result = reconcile(&mut active, prepared, |authority| {
                is_published(&dir, authority)
            });
            (active, result, Some(operation))
        }
    })
    .await
    .context("CA preparation task failed");
    let (result, _operation) = match prepared {
        Ok((active, result, operation)) => {
            authority.send_replace(active);
            (result, operation)
        }
        Err(error) => {
            authority.send_replace(None);
            (Err(error), None)
        }
    };
    if let Err(error) = result {
        if matches!(
            error.downcast_ref::<arcbox_local_ca::Error>(),
            Some(arcbox_local_ca::Error::OperationInProgress)
        ) {
            return inspect(dir).await;
        }
        return failed(format!("{error:#}"));
    }
    inspect(dir).await
}

fn is_published(dir: &Path, authority: &KeychainAuthority) -> Result<bool> {
    let public = PublicAuthority::from_pem(authority.certificate_pem().as_bytes())?;
    match std::fs::read(dir.join(TLS_CA_CERT)) {
        Ok(pem) => Ok(PublicAuthority::from_pem(&pem)?.der() == public.der()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn reconcile<A>(
    active: &mut Option<Arc<A>>,
    prepared: Result<(Arc<A>, Publication)>,
    is_published: impl Fn(&A) -> Result<bool>,
) -> Result<()> {
    // Preserve the Arc for the same root so in-flight signatures remain current.
    if let Some(previous) = active.take()
        && is_published(&previous)?
    {
        *active = Some(previous);
    }
    let (candidate, publication) = prepared?;
    if active.is_none() && publication == Publication::Published {
        ensure!(
            is_published(&candidate)?,
            "the published local CA changed during preparation"
        );
        *active = Some(candidate);
    }
    Ok(())
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

async fn monitor<C, F: Future<Output = ()>>(
    shutdown: &CancellationToken,
    mut commands: mpsc::Receiver<C>,
    mut apply: impl FnMut(Option<C>) -> F,
) {
    let mut interval = tokio::time::interval(STATUS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let command = tokio::select! {
            () = shutdown.cancelled() => return,
            Some(command) = commands.recv() => Some(command),
            _ = interval.tick() => None,
        };
        // Finish accepted work before shutdown so blocking mutations retain their lock and result.
        apply(command).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_api::{SetupPhase, SetupState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn preparation_recovers_and_preserves_only_the_published_authority() {
        let mut active = None::<Arc<u8>>;
        assert!(
            reconcile(
                &mut active,
                Err(anyhow::anyhow!("Keychain locked")),
                |_| Ok(false)
            )
            .is_err()
        );
        assert!(active.is_none());

        let original = Arc::new(1);
        reconcile(
            &mut active,
            Ok((Arc::clone(&original), Publication::Published)),
            |authority| Ok(*authority == 1),
        )
        .unwrap();
        assert!(Arc::ptr_eq(active.as_ref().unwrap(), &original));
        reconcile(
            &mut active,
            Ok((Arc::new(1), Publication::Published)),
            |authority| Ok(*authority == 1),
        )
        .unwrap();
        assert!(Arc::ptr_eq(active.as_ref().unwrap(), &original));

        assert!(
            reconcile(
                &mut active,
                Err(anyhow::anyhow!("Keychain locked")),
                |authority| { Ok(*authority == 1) }
            )
            .is_err()
        );
        assert!(Arc::ptr_eq(active.as_ref().unwrap(), &original));
        assert!(
            reconcile(
                &mut active,
                Err(anyhow::anyhow!("Keychain locked")),
                |_| Ok(false)
            )
            .is_err()
        );
        assert!(active.is_none());
    }

    #[test]
    fn unpublished_replacements_never_become_active() {
        let original = Arc::new(1_u8);
        let mut active = Some(Arc::clone(&original));
        reconcile(
            &mut active,
            Ok((Arc::new(2), Publication::AwaitingRevocation)),
            |authority| Ok(*authority == 1),
        )
        .unwrap();
        assert!(Arc::ptr_eq(active.as_ref().unwrap(), &original));

        reconcile(
            &mut active,
            Ok((Arc::new(2), Publication::AwaitingRevocation)),
            |authority| Ok(*authority == 2),
        )
        .unwrap();
        assert!(active.is_none());
        assert!(
            reconcile(
                &mut active,
                Ok((Arc::new(2), Publication::Published)),
                |_| Ok(false),
            )
            .is_err()
        );
        assert!(active.is_none());
        reconcile(
            &mut active,
            Ok((Arc::new(2), Publication::Published)),
            |authority| Ok(*authority == 2),
        )
        .unwrap();
        assert_eq!(active.as_deref(), Some(&2));
    }

    #[tokio::test(start_paused = true)]
    async fn preparation_and_trust_errors_recover_without_changing_docker_readiness() {
        for vm_running in [false, true] {
            let state = Arc::new(SetupState::new());
            state.set_phase(SetupPhase::Ready, "Daemon ready");
            state.set_vm_running(vm_running);
            let shutdown = CancellationToken::new();
            let inspections = Arc::new(AtomicUsize::new(0));
            let (_sender, commands) = mpsc::channel::<()>(1);
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                let inspections = Arc::clone(&inspections);
                async move {
                    monitor(&shutdown, commands, |_| {
                        let status = match inspections.fetch_add(1, Ordering::SeqCst) {
                            0 => failed("Keychain locked".into()),
                            1 => LocalHttpsStatus {
                                trust_state: TrustState::Untrusted.into(),
                                ..Default::default()
                            },
                            2 => failed("helper disconnected".into()),
                            _ => LocalHttpsStatus {
                                trust_state: TrustState::Trusted.into(),
                                ..Default::default()
                            },
                        };
                        state.set_local_https(status);
                        std::future::ready(())
                    })
                    .await;
                }
            });
            for expected in [
                TrustState::Error,
                TrustState::Untrusted,
                TrustState::Error,
                TrustState::Trusted,
            ] {
                tokio::task::yield_now().await;
                let current = state.current();
                assert_eq!(current.local_https.trust_state, expected);
                assert_eq!(current.phase, SetupPhase::Ready);
                assert_eq!(current.vm_running, vm_running);
                tokio::time::advance(STATUS_INTERVAL).await;
            }
            shutdown.cancel();
            task.await.unwrap();
            let stopped_at = inspections.load(Ordering::SeqCst);
            tokio::time::advance(STATUS_INTERVAL).await;
            assert_eq!(inspections.load(Ordering::SeqCst), stopped_at);
            let current = state.current();
            assert_eq!(current.phase, SetupPhase::Ready);
            assert_eq!(current.vm_running, vm_running);
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_an_accepted_mutation() {
        let shutdown = CancellationToken::new();
        let (sender, commands) = mpsc::channel(1);
        let (started, entered) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let (reply, response) = tokio::sync::oneshot::channel();
        sender.send((started, finished, reply)).await.unwrap();
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                monitor(&shutdown, commands, |command| async move {
                    if let Some((started, finished, reply)) = command {
                        started.send(()).unwrap();
                        finished.await.unwrap();
                        reply.send(()).unwrap();
                    }
                })
                .await;
            }
        });
        entered.await.unwrap();
        shutdown.cancel();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        finish.send(()).unwrap();
        response.await.unwrap();
        task.await.unwrap();
    }
}
