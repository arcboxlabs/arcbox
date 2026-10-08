//! Wait for user-authorized HTTPS trust before publishing a replacement CA or booting the VM.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use arcbox_api::{SetupPhase, SetupState};
use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority};

/// Prepares the identity and waits for Desktop or `abctl tls trust` to authorize trust.
pub async fn prepare(dir: &Path, setup_state: &SetupState) -> Result<()> {
    let client = Client::connect().await.context(
        "automatic HTTPS trust requires a compatible arcbox-helper; reinstall the helper",
    )?;
    let dir = dir.to_path_buf();
    let (authority, public, previous) = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || -> Result<_> {
            std::fs::create_dir_all(&dir)?;
            // Retire an exposed key even if trust configuration subsequently fails.
            match std::fs::remove_file(dir.join(TLS_CA_KEY)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let previous = match std::fs::read(dir.join(TLS_CA_CERT)) {
                Ok(pem) => Some(PublicAuthority::from_pem(&pem)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let authority = KeychainAuthority::open(&dir)?;
            let public = PublicAuthority::from_pem(authority.certificate_pem().as_bytes())?;
            Ok((authority, public, previous))
        }
    })
    .await??;

    let previous = previous.filter(|previous| previous.der() != public.der());
    wait_for_trust(setup_state, || async {
        if public.has_user_trust()? {
            return Ok(false);
        }
        if let Some(previous) = &previous
            && previous.has_trust()?
        {
            return Ok(false);
        }
        Ok(client.ca_status(public.der()).await?)
    })
    .await?;
    if let Some(previous) = previous {
        client.ca_remove(previous.der()).await?;
    }
    tokio::task::spawn_blocking(move || authority.publish(&dir)).await??;
    setup_state.set_phase(SetupPhase::AssetsReady, "Local HTTPS trust ready");
    tracing::info!("Local HTTPS CA trust verified");
    Ok(())
}

async fn wait_for_trust<F: Future<Output = Result<bool>>>(
    setup_state: &SetupState,
    mut ready: impl FnMut() -> F,
) -> Result<()> {
    if ready().await? {
        return Ok(());
    }
    setup_state.set_phase(
        SetupPhase::AwaitingHttpsTrust,
        "Authorize local HTTPS in ArcBox, or run `abctl tls trust` as your login user.",
    );
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if ready().await? {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn existing_trust_skips_authorization_and_missing_trust_waits() {
        let state = Arc::new(SetupState::new());
        wait_for_trust(&state, || async { Ok(true) }).await.unwrap();
        assert_eq!(state.current().phase, SetupPhase::Initializing);

        let trusted = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            let trusted = Arc::clone(&trusted);
            async move { wait_for_trust(&state, || async { Ok(trusted.load(Ordering::SeqCst)) }).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(state.current().phase, SetupPhase::AwaitingHttpsTrust);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(!task.is_finished());
        assert!(!state.current().vm_running);
        trusted.store(true, Ordering::SeqCst);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn inspection_failure_does_not_request_authorization() {
        let state = SetupState::new();
        let error = wait_for_trust(&state, || async { anyhow::bail!("helper disconnected") })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "helper disconnected");
        assert_eq!(state.current().phase, SetupPhase::Initializing);
    }
}
