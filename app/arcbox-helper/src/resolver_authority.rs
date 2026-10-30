//! Complete connection-relative resolver authority snapshots.

/// Authority state for one helper lifetime, projected for the calling connection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolverAuthoritySnapshot {
    /// Global epoch. A new helper connection must request an initial snapshot.
    pub epoch: u64,
    /// Root ownership, availability, or endpoint lifetime; excludes lease revision metadata.
    pub authority_revision: u64,
    /// Sorted, unique exact roots, including retained unavailable roots.
    pub roots: Vec<ResolverAuthorityRoot>,
}

/// One inclusive root; consumers select the longest matching root first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolverAuthorityRoot {
    /// Canonical DNS root without a trailing dot.
    pub root: String,
    /// Ownership relative to the connection that requested this snapshot.
    pub owner: ResolverOwner,
    /// Only an active authority has a forwarding endpoint.
    pub state: ResolverAuthorityState,
}

/// Connection identity without exposing the helper's private identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolverOwner {
    /// This connection owns the root.
    CurrentConnection,
    /// Another connection owns the root.
    OtherConnection,
}

/// Availability of an exclusively leased resolver root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolverAuthorityState {
    /// A completely published lease on a live connection.
    Active {
        /// Unprivileged DNS listener port on 127.0.0.1.
        port: u16,
        /// The owner's last completely successful lease revision.
        accepted_revision: u64,
    },
    /// Cleanup retains this root; no local claim or forwarding fallback is allowed.
    RetainedUnavailable,
}
