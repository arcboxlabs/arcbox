//! Manage the profile's public CA trust through the privileged helper.
//!
//! The daemon stores its private CA key in the user's Keychain. Canonical
//! production startup installs SSL-only Admin trust automatically. Explicit
//! trust commands also work for development profiles. No private key reaches
//! the helper. Removing legacy User trust can require macOS authentication.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use arcbox_constants::dns::LOCAL_DOMAIN;
use arcbox_constants::paths::{HostLayout, guest};
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority};
use clap::Subcommand;

/// Local CA commands.
#[derive(Debug, Subcommand)]
pub enum TlsCommands {
    /// Trust the profile's local CA for system HTTPS through the installed helper
    Trust,
    /// Remove CA trust; canonical production startup installs trust again
    Untrust,
    /// Check whether the helper has installed system HTTPS trust for this CA
    Status,
}

/// Executes the tls subcommand.
pub async fn execute(cmd: TlsCommands) -> Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        bail!("run `abctl tls` as yourself, not root: the CA belongs to your profile's Keychain");
    }
    let tls_dir = HostLayout::from_env_or_default().data_dir.join(guest::TLS);
    if matches!(cmd, TlsCommands::Untrust) {
        remove_trust(&tls_dir).await?;
        println!(
            "Removed ArcBox CA trust for {}. Canonical production startup installs trust again.",
            tls_dir.display()
        );
        return Ok(());
    }

    let der = KeychainAuthority::existing_certificate(&tls_dir)?
        .context("no Keychain CA for this profile: start the ArcBox daemon once to create it")?;
    let authority = PublicAuthority::try_from(der)?;
    let client = Client::connect().await?;
    if matches!(cmd, TlsCommands::Trust) {
        client.ca_install(authority.der()).await?;
    }
    let trusted = client.ca_status(authority.der()).await?;
    if matches!(cmd, TlsCommands::Trust) {
        ensure!(trusted, "system HTTPS trust was not installed");
    }
    println!(
        "System HTTPS trust for *.{LOCAL_DOMAIN} ({}): {}.",
        tls_dir.display(),
        if trusted {
            "installed"
        } else {
            "not installed"
        }
    );
    Ok(())
}

/// Revoke both identities if migration has not yet replaced the public file.
/// Keep the public file and private identity available until every removal succeeds.
pub(super) async fn remove_trust(tls_dir: &Path) -> Result<()> {
    let mut authorities: Vec<_> = KeychainAuthority::existing_certificate(tls_dir)?
        .map(PublicAuthority::try_from)
        .transpose()?
        .into_iter()
        .collect();
    let path = tls_dir.join(guest::TLS_CA_CERT);
    match std::fs::read(&path) {
        Ok(pem) => {
            let published = PublicAuthority::from_pem(&pem)?;
            if !authorities.iter().any(|ca| ca.der() == published.der()) {
                authorities.push(published);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    if authorities.is_empty() {
        return Ok(());
    }
    let client = Client::connect().await?;
    for authority in &authorities {
        client.ca_remove(authority.der()).await?;
    }
    for authority in &authorities {
        arcbox_local_ca::remove_user_trust_der(authority.der())?;
    }
    Ok(())
}
