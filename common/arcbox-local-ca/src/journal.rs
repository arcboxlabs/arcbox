//! One durable record selects the current root and retains unfinished rotation identities.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{Error, PublicAuthority};

const FILE_NAME: &str = "identity.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Primary,
    Replacement,
}

impl Slot {
    pub(crate) fn other(self) -> Self {
        match self {
            Self::Primary => Self::Replacement,
            Self::Replacement => Self::Primary,
        }
    }
}

/// Public DER survives certificate removal so interrupted cleanup retains exact identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub(crate) slot: Slot,
    pub(crate) der: Vec<u8>,
}

impl Root {
    pub(crate) fn public(&self) -> Result<PublicAuthority, Error> {
        PublicAuthority::try_from(self.der.clone())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum Journal {
    Stable {
        current: Root,
    },
    /// Only this phase may create or complete the candidate Keychain identity.
    Preparing {
        current: Root,
        target_slot: Slot,
    },
    Prepared {
        current: Root,
        candidate: Root,
    },
    /// Persist this phase before publication; retain it until old identity cleanup finishes.
    Committing {
        previous: Root,
        current: Root,
    },
}

impl Journal {
    pub(crate) fn load(dir: &Path) -> Result<Option<Self>, Error> {
        let path = dir.join(FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        let journal: Self =
            serde_json::from_slice(&bytes).map_err(|source| Error::Journal { path, source })?;
        journal.validate()?;
        Ok(Some(journal))
    }

    /// A durability error must stop cleanup; replay must persist this record successfully first.
    pub(crate) fn persist(&self, dir: &Path) -> Result<(), Error> {
        self.validate()?;
        let path = dir.join(FILE_NAME);
        let bytes = serde_json::to_vec(self).map_err(|source| Error::Journal {
            path: path.clone(),
            source,
        })?;
        arcbox_atomic_file::write(&path, &bytes)?;
        Ok(())
    }

    pub(crate) fn remove(dir: &Path) -> Result<(), Error> {
        let path = dir.join(FILE_NAME);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path, source }),
        }
        match fs::File::open(dir) {
            Ok(directory) => directory.sync_all().map_err(|source| Error::Io {
                path: dir.to_owned(),
                source,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Io {
                path: dir.to_owned(),
                source,
            }),
        }
    }

    pub(crate) fn current(&self) -> &Root {
        match self {
            Self::Stable { current }
            | Self::Preparing { current, .. }
            | Self::Prepared { current, .. }
            | Self::Committing { current, .. } => current,
        }
    }

    pub(crate) fn owned_roots(&self) -> impl Iterator<Item = &Root> {
        std::iter::once(self.current()).chain(self.other_root())
    }

    fn other_root(&self) -> Option<&Root> {
        match self {
            Self::Prepared { candidate, .. } => Some(candidate),
            Self::Committing { previous, .. } => Some(previous),
            Self::Stable { .. } | Self::Preparing { .. } => None,
        }
    }

    fn validate(&self) -> Result<(), Error> {
        for root in self.owned_roots() {
            root.public()?;
        }
        let current = self.current();
        if matches!(self, Self::Preparing { target_slot, .. } if *target_slot != current.slot.other())
        {
            return Err(Error::InvalidAuthority(
                "rotation must prepare the inactive Keychain slot",
            ));
        }
        if let Some(other) = self.other_root()
            && (other.slot == current.slot || other.der == current.der)
        {
            return Err(Error::InvalidAuthority(
                "rotation requires distinct roots in different Keychain slots",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(slot: Slot) -> Root {
        Root {
            slot,
            der: crate::authority::parameters()
                .self_signed(&rcgen::KeyPair::generate().unwrap())
                .unwrap()
                .der()
                .to_vec(),
        }
    }

    #[test]
    fn interrupted_rotation_retains_both_identities_until_cleanup_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let previous = root(Slot::Primary);
        let candidate = root(Slot::Replacement);
        let initial = Journal::Stable {
            current: previous.clone(),
        };
        initial.persist(dir.path()).unwrap();
        for pending in [
            Journal::Preparing {
                current: previous.clone(),
                target_slot: Slot::Replacement,
            },
            Journal::Prepared {
                current: previous.clone(),
                candidate: candidate.clone(),
            },
        ] {
            pending.persist(dir.path()).unwrap();
            let recovered = Journal::load(dir.path()).unwrap().unwrap();
            assert_eq!(recovered.current(), &previous);
            assert_eq!(recovered, pending);
        }
        let committing = Journal::Committing {
            previous: previous.clone(),
            current: candidate.clone(),
        };
        committing.persist(dir.path()).unwrap();
        let recovered = Journal::load(dir.path()).unwrap().unwrap();
        assert_eq!(recovered.current(), &candidate);
        assert_eq!(
            recovered.owned_roots().collect::<Vec<_>>(),
            vec![&candidate, &previous]
        );
        recovered.persist(dir.path()).unwrap();
        Journal::Stable {
            current: candidate.clone(),
        }
        .persist(dir.path())
        .unwrap();
        assert_eq!(
            Journal::load(dir.path())
                .unwrap()
                .unwrap()
                .owned_roots()
                .collect::<Vec<_>>(),
            vec![&candidate]
        );
    }

    #[test]
    fn malformed_or_invalid_state_never_becomes_an_absent_journal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Journal::load(dir.path()).unwrap(), None);
        for bytes in [
            b"".as_slice(),
            br#"{"phase":"stable"}"#,
            br#"{"phase":"unknown"}"#,
            br#"{"phase":"stable","current":{"slot":"primary","der":[1,2,3]}}"#,
        ] {
            fs::write(dir.path().join(FILE_NAME), bytes).unwrap();
            assert!(Journal::load(dir.path()).is_err());
            assert_eq!(fs::read(dir.path().join(FILE_NAME)).unwrap(), bytes);
        }
    }

    #[test]
    fn invalid_rotation_preserves_the_committed_record() {
        let dir = tempfile::tempdir().unwrap();
        let current = root(Slot::Primary);
        let stable = Journal::Stable {
            current: current.clone(),
        };
        stable.persist(dir.path()).unwrap();
        let mut duplicate = current.clone();
        duplicate.slot = Slot::Replacement;
        for invalid in [
            Journal::Preparing {
                current: current.clone(),
                target_slot: Slot::Primary,
            },
            Journal::Prepared {
                current: current.clone(),
                candidate: root(Slot::Primary),
            },
            Journal::Committing {
                previous: current,
                current: duplicate,
            },
        ] {
            assert!(invalid.persist(dir.path()).is_err());
            assert_eq!(Journal::load(dir.path()).unwrap().unwrap(), stable);
        }
    }
}
