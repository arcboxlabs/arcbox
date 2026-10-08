//! Authorize the profile's public CA through the current macOS user session.
//!
//! The helper stores only the public certificate in System Keychain. Native
//! trust changes run in this process so macOS can request authorization.
//! The daemon owns the private key. This process publishes public bytes after
//! revoking a previous root. Explicit commands also work for development profiles.
//! Repair restores the current public root without creating a key or changing trust.
//! A supplied fingerprint pins the public certificate before any write or trust change.
//! Native authorization cancellation exits with status 130; other failures exit with status 1.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use arcbox_constants::dns::LOCAL_DOMAIN;
use arcbox_constants::paths::{HostLayout, guest};
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, PublicAuthority, lock_profile};
use clap::Subcommand;

/// Local CA commands.
#[derive(Debug, Subcommand)]
pub enum TlsCommands {
    /// Authorize the profile's CA for system HTTPS through the macOS prompt
    Trust {
        /// Authorize only the certificate with this SHA-256 fingerprint
        #[arg(long)]
        expected_fingerprint: Option<String>,
    },
    /// Restore the current Keychain CA's public file without changing trust
    Repair {
        /// Restore only the certificate with this SHA-256 fingerprint
        #[arg(long)]
        expected_fingerprint: Option<String>,
    },
    /// Revoke CA trust without stopping containers
    Untrust,
    /// Check whether this profile's CA is ready for system HTTPS
    Status {
        /// Print public CA metadata and trust status as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Executes the tls subcommand.
pub async fn execute(cmd: TlsCommands) -> Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        bail!("run `abctl tls` as yourself, not root: the CA belongs to your profile's Keychain");
    }
    let tls_dir = HostLayout::from_env_or_default().data_dir.join(guest::TLS);
    if let TlsCommands::Status { json } = cmd {
        let status = arcbox_core::local_https::Status::inspect(&tls_dir).await?;
        if json {
            println!("{}", serde_json::to_string(&status)?);
        } else {
            println!("{}", status.message);
            if !status.certificate_sha256.is_empty() {
                println!("CA SHA-256: {}", status.certificate_sha256);
                println!(
                    "Valid until (UTC Unix seconds): {}",
                    status.certificate_not_after_unix_seconds
                );
            }
        }
        return Ok(());
    }
    if matches!(cmd, TlsCommands::Untrust) {
        remove_trust(&tls_dir).await?;
        println!(
            "Removed ArcBox CA trust for {}. Containers can run without HTTPS trust.",
            tls_dir.display()
        );
        return Ok(());
    }
    let _operation = lock_profile(&tls_dir)?;

    let der = KeychainAuthority::existing_certificate(&tls_dir)?
        .context("no Keychain CA for this profile: start the ArcBox daemon once to create it")?;
    let authority = PublicAuthority::try_from(der)?;
    check_fingerprint(&cmd, &authority.fingerprint_sha256())?;
    if matches!(cmd, TlsCommands::Repair { .. }) {
        authority.repair_publication(&tls_dir)?;
        println!(
            "Restored the current profile's public CA file: {}",
            tls_dir.join(guest::TLS_CA_CERT).display()
        );
        println!("CA SHA-256: {}", authority.fingerprint_sha256());
        return Ok(());
    }
    ensure!(
        authority
            .validity()
            .is_valid_at(std::time::SystemTime::now().into()),
        "the local CA is outside its validity period; rotate the CA before authorizing HTTPS"
    );
    let client = Client::connect().await?;
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
    let legacy_user_trust = authority.has_user_trust()?;
    let trusted = client.ca_status(authority.der()).await? && !legacy_user_trust;
    ensure!(trusted, "system HTTPS trust was not installed");
    ensure!(
        authority.publish(&tls_dir)? == arcbox_local_ca::Publication::Published,
        "the previous local CA still has trust; revoke it before publishing this CA"
    );
    println!(
        "System HTTPS trust for *.{LOCAL_DOMAIN} ({}): installed.",
        tls_dir.display(),
    );
    Ok(())
}

fn check_fingerprint(cmd: &TlsCommands, actual: &str) -> Result<()> {
    if let TlsCommands::Trust {
        expected_fingerprint: Some(expected),
    }
    | TlsCommands::Repair {
        expected_fingerprint: Some(expected),
    } = cmd
    {
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "the local CA changed since HTTPS was requested; refresh its status and try again"
        );
    }
    Ok(())
}

/// Revoke both identities if migration has not yet replaced the public file.
/// Keep the public file and private identity available until every removal succeeds.
pub(super) async fn remove_trust(tls_dir: &Path) -> Result<()> {
    let _operation = lock_profile(tls_dir)?;
    remove_trust_locked(tls_dir).await
}

/// The caller must hold the profile lock through this call and any subsequent identity deletion.
pub(super) async fn remove_trust_locked(tls_dir: &Path) -> Result<()> {
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
        Ok(pem) => Ok(Some(PublicAuthority::from_pem(&pem).context(
            "invalid public CA file; run `abctl tls repair` to check whether the public file can be restored",
        )?)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_and_repair_reject_stale_or_partial_fingerprints() {
        let current = "AB:CD:".repeat(10) + "EF:01";
        for expected in [
            "",
            "AB:CD",
            &("AB:CD:".repeat(10) + "EF:02"),
            &current.to_ascii_lowercase(),
        ] {
            for cmd in [
                TlsCommands::Trust {
                    expected_fingerprint: Some(expected.into()),
                },
                TlsCommands::Repair {
                    expected_fingerprint: Some(expected.into()),
                },
            ] {
                let result = check_fingerprint(&cmd, &current);
                if expected.eq_ignore_ascii_case(&current) {
                    result.unwrap();
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("refresh its status")
                    );
                }
            }
        }
    }
}
