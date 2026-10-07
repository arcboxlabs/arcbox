//! Prepare production HTTPS trust before publishing a replacement CA or booting the VM.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use arcbox_constants::paths::guest::{TLS_CA_CERT, TLS_CA_KEY};
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority};

/// Installs the canonical production CA through the privileged helper.
pub async fn prepare(dir: &Path) -> Result<()> {
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

    if let Some(previous) = previous
        && previous.der() != public.der()
    {
        // Keep ca.pem until both Admin and legacy User trust have been revoked.
        client.ca_remove(previous.der()).await?;
    }
    tokio::task::spawn_blocking(move || authority.publish(&dir)).await??;
    client.ca_install(public.der()).await?;
    ensure!(
        client.ca_status(public.der()).await?,
        "the ArcBox local CA is not trusted after helper configuration"
    );
    tracing::info!("Local HTTPS CA trusted through arcbox-helper");
    Ok(())
}
