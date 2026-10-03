//! The mount points this daemon created, by machine name. Only these are
//! ever unmounted: whatever else sits under the root is not ours.
//!
//! An entry lives until its mount is gone — through a failed release and
//! the retries after it — so shutdown and a lagged receiver still find it.
//! A generation tells a retry whether the name was remounted in between:
//! the retry must not unmount a newer machine's root at the same path.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// One mount this daemon made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    /// The mount point.
    pub path: PathBuf,
    /// Identifies this mount among those a name has had.
    pub generation: u64,
    /// A release failed and a retry is pending; a machine starting under
    /// the name replaces the mount rather than reusing it.
    pub stale: bool,
}

/// The table of mounts, keyed by machine name.
pub(super) struct Mounts {
    entries: BTreeMap<String, Entry>,
    generation: u64,
}

impl Mounts {
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            generation: 0,
        }
    }

    /// Records a mount just made for `name`, replacing any earlier entry,
    /// and returns its generation.
    pub fn record(&mut self, name: &str, path: PathBuf) -> u64 {
        self.generation += 1;
        self.entries.insert(
            name.to_owned(),
            Entry {
                path,
                generation: self.generation,
                stale: false,
            },
        );
        self.generation
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    /// Whether `generation` is still the mount recorded for `name`.
    pub fn is_current(&self, name: &str, generation: u64) -> bool {
        self.entries
            .get(name)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// Marks `generation` as a mount whose release failed.
    pub fn mark_stale(&mut self, name: &str, generation: u64) {
        if let Some(entry) = self.entries.get_mut(name)
            && entry.generation == generation
        {
            entry.stale = true;
        }
    }

    /// Forgets `generation`, if it is still the one recorded for `name`.
    pub fn forget(&mut self, name: &str, generation: u64) {
        if self.is_current(name, generation) {
            self.entries.remove(name);
        }
    }

    /// Every name with a mount, for the lagged-receiver repair.
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Every mount, for shutdown.
    pub fn entries(&self) -> Vec<(String, Entry)> {
        self.entries
            .iter()
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remount_outdates_the_generation_a_retry_holds() {
        let mut mounts = Mounts::new();
        let first = mounts.record("dev", PathBuf::from("/m/dev"));
        assert!(mounts.is_current("dev", first));

        // The release failed; the entry stays, marked stale.
        mounts.mark_stale("dev", first);
        assert!(mounts.get("dev").is_some_and(|entry| entry.stale));
        assert_eq!(mounts.names(), vec!["dev".to_owned()]);

        // The machine started again and was mounted anew: the pending retry
        // no longer owns the path, and forgetting its generation is a no-op.
        let second = mounts.record("dev", PathBuf::from("/m/dev"));
        assert!(!mounts.is_current("dev", first));
        assert!(mounts.is_current("dev", second));
        assert!(mounts.get("dev").is_some_and(|entry| !entry.stale));
        mounts.forget("dev", first);
        assert!(mounts.is_current("dev", second));

        mounts.forget("dev", second);
        assert!(mounts.get("dev").is_none());
        assert_eq!(mounts.entries(), Vec::new());
    }

    #[test]
    fn marking_or_forgetting_an_unknown_generation_changes_nothing() {
        let mut mounts = Mounts::new();
        mounts.mark_stale("dev", 7);
        mounts.forget("dev", 7);
        assert!(mounts.get("dev").is_none());
        let generation = mounts.record("dev", PathBuf::from("/m/dev"));
        mounts.mark_stale("dev", generation + 1);
        assert!(mounts.get("dev").is_some_and(|entry| !entry.stale));
    }
}
