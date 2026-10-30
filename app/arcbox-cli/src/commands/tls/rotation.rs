//! Authorize a pinned replacement in the login session, then ask the daemon to commit.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{self as pb, SystemServiceClient};
use arcbox_helper::client::Client;
use arcbox_local_ca::{KeychainAuthority, lock_profile};

use super::{TlsCommands, authorize, check_fingerprint, current_authority, require_valid, revoke};

pub(super) async fn execute(tls_dir: &Path, cmd: &TlsCommands) -> Result<()> {
    let (pending, expected_old_sha256) = {
        let _operation = lock_profile(tls_dir)?;
        let current_sha256 = current_authority(tls_dir)?.fingerprint_sha256();
        check_fingerprint(cmd, &current_sha256)?;
        let pending = KeychainAuthority::pending_rotation(tls_dir)?;
        let fingerprint = if let Some(rotation) = &pending {
            rotation.previous.fingerprint_sha256()
        } else {
            current_sha256
        };
        (pending, fingerprint)
    };
    let (transport, config) = crate::connect::daemon(&super::super::resolve_grpc_socket_path());
    let daemon = SystemServiceClient::new(transport, config);
    let prepared = if let Some(rotation) = pending {
        pb::LocalCaRotation {
            previous_certificate_der: rotation.previous.der().to_vec(),
            candidate_certificate_der: rotation.candidate.der().to_vec(),
            ..Default::default()
        }
    } else {
        daemon
            .prepare_local_ca_rotation(pb::PrepareLocalCaRotationRequest {
                expected_current_sha256: expected_old_sha256.clone(),
                ..Default::default()
            })
            .await
            .context("preparing the replacement CA; the ArcBox daemon must be running")?
            .into_owned()
    };
    let expected_new_sha256 = {
        let _operation = lock_profile(tls_dir)?;
        let rotation = KeychainAuthority::pending_rotation(tls_dir)?
            .context("the pending CA rotation changed; inspect HTTPS status before retrying")?;
        check_pair(
            (rotation.previous.der(), rotation.candidate.der()),
            &prepared,
        )?;
        ensure!(
            rotation.previous.fingerprint_sha256() == expected_old_sha256,
            "the pending CA rotation changed; inspect HTTPS status before retrying"
        );
        check_fingerprint(cmd, &current_authority(tls_dir)?.fingerprint_sha256())?;
        require_valid(&rotation.candidate)?;
        let helper = Client::connect().await?;
        authorize(&helper, &rotation.candidate).await?;
        revoke(&helper, &rotation.previous).await?;
        rotation.candidate.fingerprint_sha256()
    };
    // The daemon takes the same profile lock when it publishes and switches the signer.
    daemon
        .commit_local_ca_rotation(pb::CommitLocalCaRotationRequest {
            expected_old_sha256,
            expected_new_sha256: expected_new_sha256.clone(),
            ..Default::default()
        })
        .await
        .context("committing CA rotation; inspect HTTPS status before retrying")?;
    let status = daemon
        .get_setup_status(pb::Empty::default())
        .await
        .context("the CA rotation committed, but its daemon status could not be confirmed")?
        .into_owned();
    ensure!(
        status.local_https.certificate_sha256 == expected_new_sha256
            && status.local_https.trust_state == pb::local_https_status::TrustState::Trusted,
        "the CA rotation committed, but the daemon has not confirmed the replacement HTTPS trust"
    );
    println!("Rotated the local HTTPS CA for {}.", tls_dir.display());
    println!("CA SHA-256: {expected_new_sha256}");
    Ok(())
}

fn check_pair(recorded: (&[u8], &[u8]), prepared: &pb::LocalCaRotation) -> Result<()> {
    ensure!(
        recorded.0 == prepared.previous_certificate_der
            && recorded.1 == prepared.candidate_certificate_der,
        "the pending CA rotation changed; inspect HTTPS status before retrying"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_or_reversed_rotation_pairs_are_rejected_before_authorization() {
        let prepared = pb::LocalCaRotation {
            previous_certificate_der: vec![1, 2],
            candidate_certificate_der: vec![3, 4],
            ..Default::default()
        };
        check_pair((&[1, 2], &[3, 4]), &prepared).unwrap();
        for recorded in [
            (&[1, 9][..], &[3, 4][..]),
            (&[1, 2][..], &[3, 9][..]),
            (&[3, 4][..], &[1, 2][..]),
            (&[1, 2][..], &[1, 2][..]),
        ] {
            assert!(check_pair(recorded, &prepared).is_err());
        }
    }
}
