//! Prepare and commit exact Keychain identities without changing native trust.

use std::path::Path;

use crate::journal::{Journal, Root, Slot};
use crate::{Error, KeychainAuthority, PublicAuthority, Publication};

/// The exact roots whose trust must change before a rotation can commit.
pub struct Rotation {
    /// The root to revoke before removing its Keychain identity.
    pub previous: PublicAuthority,
    /// The replacement root to trust for TLS servers.
    pub candidate: PublicAuthority,
}

impl Rotation {
    fn new(previous: &Root, candidate: &Root) -> Result<Self, Error> {
        Ok(Self {
            previous: previous.public()?,
            candidate: candidate.public()?,
        })
    }

    fn require_commit_trust(&self) -> Result<(), Error> {
        require_candidate_trust(&self.candidate)?;
        if self.previous.has_trust()? {
            return Err(Error::InvalidAuthority(
                "revoke the previous CA's User and Admin trust before committing rotation",
            ));
        }
        Ok(())
    }
}

impl KeychainAuthority {
    /// Prepares one replacement identity while preserving the current root and publication.
    /// Retries reuse the recorded candidate. Hold [`crate::lock_profile`] throughout this call.
    pub fn prepare_rotation(dir: &Path, expected_current: &str) -> Result<Rotation, Error> {
        let journal = Self::journal(dir)?.ok_or(Error::InvalidAuthority(
            "initialize the profile CA before preparing rotation",
        ))?;
        match journal {
            Journal::Stable { current } => {
                require_fingerprint(&current, expected_current)?;
                Self::load_root(dir, &current)?;
                let target_slot = current.slot.other();
                if Self::slot_has_keys(dir, target_slot)? {
                    return Err(Error::InvalidAuthority(
                        "the inactive CA slot contains an unrecorded identity",
                    ));
                }
                Self::finish_preparing(dir, current, target_slot)
            }
            Journal::Preparing {
                current,
                target_slot,
            } => {
                require_fingerprint(&current, expected_current)?;
                Self::load_root(dir, &current)?;
                Self::finish_preparing(dir, current, target_slot)
            }
            Journal::Prepared { current, candidate } => {
                require_fingerprint(&current, expected_current)?;
                Self::load_root(dir, &current)?;
                Self::load_root(dir, &candidate)?;
                Rotation::new(&current, &candidate)
            }
            Journal::Committing { .. } => Err(Error::InvalidAuthority(
                "finish the pending CA rotation commit before preparing another rotation",
            )),
        }
    }

    fn finish_preparing(dir: &Path, current: Root, target_slot: Slot) -> Result<Rotation, Error> {
        // Re-persist after an uncertain write before creating or completing native items.
        Journal::Preparing {
            current: current.clone(),
            target_slot,
        }
        .persist(dir)?;
        let candidate = Self::open_slot(dir, target_slot, true)?.root(target_slot);
        let rotation = Rotation::new(&current, &candidate)?;
        Journal::Prepared { current, candidate }.persist(dir)?;
        Ok(rotation)
    }

    /// Reads a prepared or committing pair without creating, signing, publishing, or changing trust.
    /// Hold [`crate::lock_profile`] while using this pair for native authorization.
    /// An unfinished preparation returns `None`; `prepare_rotation` must finish that preparation.
    pub fn pending_rotation(dir: &Path) -> Result<Option<Rotation>, Error> {
        match Self::journal(dir)? {
            Some(Journal::Prepared { current, candidate }) => {
                Self::load_root(dir, &current)?;
                Self::load_root(dir, &candidate)?;
                Ok(Some(Rotation::new(&current, &candidate)?))
            }
            Some(Journal::Committing { previous, current }) => {
                // The previous identity may already be partly deleted; its DER remains recorded.
                Self::load_root(dir, &current)?;
                Ok(Some(Rotation::new(&previous, &current)?))
            }
            None | Some(Journal::Stable { .. } | Journal::Preparing { .. }) => Ok(None),
        }
    }

    /// Publishes a trusted replacement and deletes the explicitly revoked previous identity.
    /// Hold [`crate::lock_profile`] throughout this call. This call never changes trust.
    /// A completed retry matches `expected_new`; the retired root is no longer recorded.
    pub fn commit_rotation(
        dir: &Path,
        expected_old: &str,
        expected_new: &str,
    ) -> Result<Self, Error> {
        let journal = Self::journal(dir)?.ok_or(Error::InvalidAuthority(
            "no CA rotation has been prepared for this profile",
        ))?;
        let (previous, current) = match journal {
            Journal::Stable { current } => {
                require_fingerprint(&current, expected_new)?;
                require_candidate_trust(&current.public()?)?;
                let authority = Self::load_root(dir, &current)?;
                Journal::Stable { current }.persist(dir)?;
                authority.publish_rotation(dir)?;
                return Ok(authority);
            }
            Journal::Prepared { current, candidate } => {
                require_fingerprint(&current, expected_old)?;
                require_fingerprint(&candidate, expected_new)?;
                Self::load_root(dir, &current)?;
                (current, candidate)
            }
            Journal::Committing { previous, current } => {
                require_fingerprint(&previous, expected_old)?;
                require_fingerprint(&current, expected_new)?;
                (previous, current)
            }
            Journal::Preparing { .. } => {
                return Err(Error::InvalidAuthority(
                    "finish preparing the replacement CA before committing rotation",
                ));
            }
        };
        let authority = Self::load_root(dir, &current)?;
        let rotation = Rotation::new(&previous, &current)?;
        rotation.require_commit_trust()?;
        // A successful fsync also confirms any earlier publication rename in this directory.
        Journal::Committing {
            previous: previous.clone(),
            current: current.clone(),
        }
        .persist(dir)?;
        authority.publish_rotation(dir)?;
        Self::delete_root(dir, &previous)?;
        // Retain both public roots if trust changes while native cleanup runs.
        rotation.require_commit_trust()?;
        Journal::Stable { current }.persist(dir)?;
        Ok(authority)
    }

    fn publish_rotation(&self, dir: &Path) -> Result<(), Error> {
        match self.publish(dir)? {
            Publication::Published => Ok(()),
            Publication::AwaitingRevocation => Err(Error::InvalidAuthority(
                "revoke the published CA's trust before committing rotation",
            )),
        }
    }
}

fn require_fingerprint(root: &Root, expected: &str) -> Result<(), Error> {
    if root.public()?.fingerprint_sha256() != expected {
        return Err(Error::InvalidAuthority(
            "CA rotation fingerprint does not match the recorded identity",
        ));
    }
    Ok(())
}

fn require_candidate_trust(root: &PublicAuthority) -> Result<(), Error> {
    if !root.validity().is_valid_at(time::OffsetDateTime::now_utc()) {
        return Err(Error::InvalidAuthority(
            "the replacement CA is outside its validity period",
        ));
    }
    if root.has_user_trust()? || !root.admin_trusted()? {
        return Err(Error::InvalidAuthority(
            "the replacement CA requires TLS-server-only Admin trust without User trust overrides",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_requests_leave_the_journal_unchanged_without_opening_native_items() {
        let dir = tempfile::tempdir().unwrap();
        let current = Root {
            slot: Slot::Primary,
            der: crate::authority::parameters()
                .self_signed(&rcgen::KeyPair::generate().unwrap())
                .unwrap()
                .der()
                .to_vec(),
        };
        let stable = Journal::Stable { current };
        stable.persist(dir.path()).unwrap();
        assert!(KeychainAuthority::prepare_rotation(dir.path(), "stale").is_err());
        assert!(KeychainAuthority::commit_rotation(dir.path(), "old", "stale").is_err());
        assert!(
            KeychainAuthority::pending_rotation(dir.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(Journal::load(dir.path()).unwrap(), Some(stable));
    }
}
