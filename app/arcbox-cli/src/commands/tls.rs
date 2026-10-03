//! Trusting the local CA behind `https://<name>.arcbox.local`.
//!
//! The daemon creates the CA as `tls/ca.pem` in its data directory on first
//! start, and the guest agent signs a certificate per container domain with
//! it. Browsers and `curl` accept those once the user trusts the CA:
//!
//! - `abctl tls trust`   — add it to the login keychain, trusted for TLS
//!   servers only (`security add-trusted-cert -p ssl`); macOS asks for the
//!   user's password.
//! - `abctl tls untrust` — remove that trust (`security remove-trusted-cert`);
//!   the certificate stays in the keychain, trusted for nothing.
//!
//! Trust is the user's, per data directory: a development profile has its
//! own CA. The CA is name-constrained to `arcbox.local`, so trusting it
//! vouches for no other name (trust model: `arcbox-local-ca`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use arcbox_constants::dns::LOCAL_DOMAIN;
use arcbox_constants::paths::{HostLayout, guest};
use clap::Subcommand;

const SECURITY: &str = "/usr/bin/security";

/// Local CA commands.
#[derive(Debug, Subcommand)]
pub enum TlsCommands {
    /// Trust the local CA for HTTPS on *.arcbox.local (asks for your password)
    Trust,
    /// Stop trusting the local CA
    Untrust,
}

/// Executes the tls subcommand.
pub async fn execute(cmd: TlsCommands) -> Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        bail!("run `abctl tls` as yourself, not root: trust goes into your login keychain");
    }
    let ca = ca_path(&HostLayout::from_env_or_default())?;
    let home = dirs::home_dir().context("cannot resolve the home directory")?;
    let argv = security_argv(&cmd, &ca, &home);
    let status = Command::new(SECURITY)
        .args(&argv)
        .status()
        .with_context(|| format!("running {SECURITY}"))?;
    if !status.success() {
        bail!("{SECURITY} {} failed ({status})", argv[0].display());
    }
    match cmd {
        TlsCommands::Trust => println!(
            "Trusted the ArcBox local CA ({}) for HTTPS on *.{LOCAL_DOMAIN}.",
            ca.display()
        ),
        TlsCommands::Untrust => {
            println!("Removed trust for the ArcBox local CA ({}).", ca.display());
        }
    }
    Ok(())
}

/// The CA certificate of the data directory, which the daemon creates.
fn ca_path(layout: &HostLayout) -> Result<PathBuf> {
    let ca = layout.data_dir.join(guest::TLS).join(guest::TLS_CA_CERT);
    if !ca.is_file() {
        bail!(
            "no local CA at {}: start the ArcBox daemon once to create it",
            ca.display()
        );
    }
    Ok(ca)
}

/// The `security` arguments that carry out `cmd` for the CA at `ca`.
fn security_argv(cmd: &TlsCommands, ca: &Path, home: &Path) -> Vec<OsString> {
    match cmd {
        // The user's trust settings (no -d: not the admin domain), limited
        // to TLS servers, with the certificate added to the login keychain.
        TlsCommands::Trust => ["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k"]
            .into_iter()
            .map(OsString::from)
            .chain([
                home.join("Library/Keychains/login.keychain-db").into(),
                ca.into(),
            ])
            .collect(),
        TlsCommands::Untrust => vec!["remove-trusted-cert".into(), ca.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_the_users_and_for_tls_servers_only() {
        let argv = security_argv(
            &TlsCommands::Trust,
            Path::new("/data/tls/ca.pem"),
            Path::new("/Users/me"),
        );
        let argv: Vec<&str> = argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            argv,
            [
                "add-trusted-cert",
                "-r",
                "trustRoot",
                "-p",
                "ssl",
                "-k",
                "/Users/me/Library/Keychains/login.keychain-db",
                "/data/tls/ca.pem",
            ]
        );
        assert!(!argv.contains(&"-d"), "never the admin trust domain");
    }

    #[test]
    fn a_data_directory_without_a_ca_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let layout = HostLayout::new(dir.path().to_path_buf());
        let err = ca_path(&layout).unwrap_err().to_string();
        assert!(err.contains("start the ArcBox daemon"), "{err}");

        let tls = dir.path().join(guest::TLS);
        std::fs::create_dir_all(&tls).unwrap();
        std::fs::write(tls.join(guest::TLS_CA_CERT), "pem").unwrap();
        assert_eq!(ca_path(&layout).unwrap(), tls.join(guest::TLS_CA_CERT));
    }
}
