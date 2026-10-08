//! Authorize the profile's public CA through the current macOS user session.
//!
//! The helper stores only the public certificate in System Keychain. Native
//! trust changes run in this process so macOS can request authorization.
//! The daemon owns the private key and publishes the certificate after trust
//! is ready. Explicit commands also work for development profiles.
//! Native authorization cancellation exits with status 130; other failures exit with status 1.

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
    /// Authorize the profile's CA for system HTTPS through the macOS prompt
    Trust,
    /// Revoke CA trust; the next production start requires authorization
    Untrust,
    /// Check whether this profile's CA is ready for system HTTPS
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
            "Removed ArcBox CA trust for {}. The next production start requires authorization.",
            tls_dir.display()
        );
        return Ok(());
    }

    let der = KeychainAuthority::existing_certificate(&tls_dir)?
        .context("no Keychain CA for this profile: start the ArcBox daemon once to create it")?;
    let authority = PublicAuthority::try_from(der)?;
    let client = Client::connect().await?;
    if matches!(cmd, TlsCommands::Trust) {
        if let Some(previous) = published_authority(&tls_dir)?
            && previous.der() != authority.der()
        {
            revoke(&client, &previous).await?;
        }
        client.ca_install(authority.der()).await?;
        arcbox_local_ca::remove_user_trust_der(authority.der())
            .context("revoking legacy user HTTPS trust; approve the macOS prompt to continue")?;
        authority
            .trust_admin()
            .context("authorizing system HTTPS trust; approve the macOS prompt to continue")?;
    }
    let legacy_user_trust = authority.has_user_trust()?;
    let trusted = client.ca_status(authority.der()).await? && !legacy_user_trust;
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
    if legacy_user_trust {
        println!("Legacy user HTTPS trust remains. Run `abctl tls trust` to finish migration.");
    }
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
    if let Some(published) = published_authority(tls_dir)?
        && !authorities.iter().any(|ca| ca.der() == published.der())
    {
        authorities.push(published);
    }
    if authorities.is_empty() {
        return Ok(());
    }
    let client = Client::connect().await?;
    for authority in &authorities {
        revoke(&client, authority).await?;
    }
    Ok(())
}

fn published_authority(tls_dir: &Path) -> Result<Option<PublicAuthority>> {
    let path = tls_dir.join(guest::TLS_CA_CERT);
    match std::fs::read(&path) {
        Ok(pem) => Ok(Some(PublicAuthority::from_pem(&pem)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

async fn revoke(client: &Client, authority: &PublicAuthority) -> Result<()> {
    authority
        .remove_admin_trust()
        .context("revoking system HTTPS trust; approve the macOS prompt to continue")?;
    arcbox_local_ca::remove_user_trust_der(authority.der())
        .context("revoking legacy user HTTPS trust; approve the macOS prompt to continue")?;
    client.ca_remove(authority.der()).await?;
    Ok(())
}
