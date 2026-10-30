//! Consistent authority snapshots and bounded watches over serialized lease mutations.

use std::time::Duration;

use arcbox_helper::{HelperError, ResolverAuthoritySnapshot};
use tokio_util::sync::CancellationToken;

use super::{ResolverStore, lease::State};

impl ResolverStore {
    pub(super) fn mutate<T>(&self, mutation: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        // Connection identities start at one, so this projection compares global authority.
        let previous = state.roots(0);
        let ownership = state.ownership();
        let result = mutation(&mut state);
        let authority_changed = ownership != state.ownership();
        if authority_changed {
            state.authority_revision = state
                .authority_revision
                .checked_add(1)
                .expect("authority revision exhausted");
        }
        if authority_changed || previous != state.roots(0) {
            state.epoch = state
                .epoch
                .checked_add(1)
                .expect("authority epoch exhausted");
            self.authority_epoch.send_replace(state.epoch);
        }
        result
    }

    pub fn snapshot(&self, connection: u64) -> Result<ResolverAuthoritySnapshot, HelperError> {
        let state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        if !state.contains(connection) {
            return Err(HelperError::other("resolver connection is closed"));
        }
        Ok(ResolverAuthoritySnapshot {
            epoch: state.epoch,
            authority_revision: state.authority_revision,
            roots: state.roots(connection),
        })
    }

    pub async fn watch(
        &self,
        connection: u64,
        after: Option<u64>,
        closed: &CancellationToken,
    ) -> Result<ResolverAuthoritySnapshot, HelperError> {
        let mut epochs = self.authority_epoch.subscribe();
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);
        loop {
            if closed.is_cancelled() {
                return Err(HelperError::other("resolver connection is closed"));
            }
            let snapshot = self.snapshot(connection)?;
            if after != Some(snapshot.epoch) {
                return Ok(snapshot);
            }
            tokio::select! {
                biased;
                () = closed.cancelled() => {
                    return Err(HelperError::other("resolver connection is closed"));
                }
                () = &mut deadline => return self.snapshot(connection),
                result = epochs.changed() => {
                    result.map_err(|error| HelperError::other(error.to_string()))?;
                }
            }
        }
    }
}
