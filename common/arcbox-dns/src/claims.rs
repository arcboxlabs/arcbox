//! Pure ownership selection from a consistent set of current resource claims.
//!
//! Callers resolve the selected owner to an address and revision in the same snapshot.
//! Reservations remain present when routes disappear; delegated trees belong to another instance.

use std::cmp::Reverse;

use arcbox_constants::dns::Domain;

use crate::NamePattern;

/// A name claim with trusted resource and namespace identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pattern: NamePattern,
    owner: String,
    authority: String,
    compose_service: Option<(String, String)>,
}

impl Claim {
    /// Creates a claim that cannot share its name with another resource.
    pub fn exclusive(pattern: NamePattern, owner: String, authority: String) -> Self {
        Self {
            pattern,
            owner,
            authority,
            compose_service: None,
        }
    }

    /// Generates a default exact name and its implicit descendant wildcard.
    pub fn generated(name: Domain, owner: String, authority: String) -> [Self; 2] {
        [
            Self::exclusive(
                NamePattern::Exact(name.clone()),
                owner.clone(),
                authority.clone(),
            ),
            Self::exclusive(NamePattern::Descendants(name), owner, authority),
        ]
    }

    /// Generates shared service claims for one replica in a `(project, service)` group.
    /// Custom labels must use `exclusive`, even when the container belongs to Compose.
    pub fn compose_service(
        name: Domain,
        owner: String,
        authority: String,
        group: (String, String),
    ) -> [Self; 2] {
        Self::generated(name, owner, authority).map(|mut claim| {
            claim.compose_service = Some(group.clone());
            claim
        })
    }

    /// Returns the canonical name or wildcard base owned by this claim.
    pub fn pattern(&self) -> &NamePattern {
        &self.pattern
    }

    /// Returns the resource identity, such as a complete container ID.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Returns the trusted namespace identity supplied by the resource registry.
    pub fn authority(&self) -> &str {
        &self.authority
    }
}

/// A namespace boundary supplied by the authoritative resource or instance registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// Reserves one name without reserving descendants, such as an index apex.
    ReservedName {
        /// The complete reserved name.
        name: Domain,
        /// The trusted namespace identity permitted to publish this name.
        authority: String,
    },
    /// Reserves the root and every descendant for one local authority.
    ReservedTree {
        /// The inclusive root of the reserved namespace.
        root: Domain,
        /// The trusted namespace identity permitted to publish within the tree.
        authority: String,
    },
    /// Excludes the root and every descendant from local ownership matching.
    DelegatedTree {
        /// The inclusive root of the external namespace.
        root: Domain,
        /// The external instance that owns the namespace.
        instance: String,
    },
}

impl Boundary {
    fn rank(&self, query: &Domain) -> Option<(usize, bool)> {
        let (root, exact) = match self {
            Self::ReservedName { name, .. } => (name, true),
            Self::ReservedTree { root, .. } | Self::DelegatedTree { root, .. } => (root, false),
        };
        let descendant = !exact
            && query
                .as_str()
                .strip_suffix(root.as_str())
                .is_some_and(|prefix| prefix.ends_with('.'));
        (query == root || descendant).then(|| (root.as_str().split('.').count(), exact))
    }

    fn delegated(&self) -> bool {
        matches!(self, Self::DelegatedTree { .. })
    }

    fn permits(&self, claim: &Claim) -> bool {
        let base = match &claim.pattern {
            NamePattern::Exact(base) | NamePattern::Descendants(base) => base,
        };
        match self {
            Self::ReservedName { name, authority } => {
                authority == &claim.authority
                    && name == base
                    && matches!(&claim.pattern, NamePattern::Exact(_))
            }
            Self::ReservedTree { authority, .. } => {
                authority == &claim.authority && self.rank(base).is_some()
            }
            Self::DelegatedTree { .. } => false,
        }
    }

    fn nearest<'a>(
        name: &Domain,
        boundaries: &'a [Self],
        delegated: bool,
    ) -> Result<Option<&'a Self>, ()> {
        let mut ranked = boundaries
            .iter()
            .filter(|boundary| boundary.delegated() == delegated)
            .filter_map(|boundary| boundary.rank(name).map(|rank| (rank, boundary)));
        let Some((rank, nearest)) = ranked.clone().max_by_key(|(rank, _)| *rank) else {
            return Ok(None);
        };
        if ranked.any(|(other_rank, other)| other_rank == rank && other != nearest) {
            return Err(());
        }
        Ok(Some(nearest))
    }
}

/// Every result except `Unmatched` terminates lookup; callers must not retry a broader wildcard.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// One selected resource owns the most specific matching claim.
    Matched(&'a Claim),
    /// Equally specific claims or boundaries have incompatible identities.
    Conflict,
    /// A local authority reserves the name but has no matching claim.
    Reserved(&'a Boundary),
    /// Another instance owns the name, regardless of local claims.
    Delegated(&'a Boundary),
    /// No claim or boundary matches the query name.
    Unmatched,
}

/// Resolves exact names before wildcards, independently of registration order.
///
/// Shared Compose service claims select the smallest complete container ID.
/// Delegations block all local claims; the closest reservation restricts authority and pattern scope.
pub fn resolve<'a, C>(name: &Domain, claims: C, boundaries: &'a [Boundary]) -> Resolution<'a>
where
    C: IntoIterator<Item = &'a Claim>,
    C::IntoIter: Clone,
{
    let Ok(delegated) = Boundary::nearest(name, boundaries, true) else {
        return Resolution::Conflict;
    };
    if let Some(boundary) = delegated {
        return Resolution::Delegated(boundary);
    }
    let Ok(reserved) = Boundary::nearest(name, boundaries, false) else {
        return Resolution::Conflict;
    };
    let ranked = claims
        .into_iter()
        .filter(|claim| reserved.is_none_or(|boundary| boundary.permits(claim)))
        .filter_map(|claim| claim.pattern.rank(name).map(|rank| (rank, claim)));
    let Some((rank, winner)) = ranked.clone().max_by_key(|(rank, claim)| {
        (
            *rank,
            Reverse((&claim.owner, &claim.authority, &claim.compose_service)),
        )
    }) else {
        return reserved.map_or(Resolution::Unmatched, Resolution::Reserved);
    };
    let mut candidates = ranked
        .filter(|(candidate_rank, _)| *candidate_rank == rank)
        .map(|(_, claim)| claim);
    let same_owner = candidates
        .clone()
        .all(|claim| claim.owner == winner.owner && claim.authority == winner.authority);
    let same_service = winner.compose_service.is_some()
        && candidates.all(|claim| {
            claim.compose_service == winner.compose_service && claim.authority == winner.authority
        });
    if same_owner || same_service {
        Resolution::Matched(winner)
    } else {
        Resolution::Conflict
    }
}
