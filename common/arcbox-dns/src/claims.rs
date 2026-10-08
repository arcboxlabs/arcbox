//! Pure ownership selection from a consistent set of current resource claims.
//! Callers resolve the selected owner to an address and revision in the same snapshot.

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

/// A conflict terminates lookup; callers must not retry a broader wildcard.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// One selected resource owns the most specific matching claim.
    Matched(&'a Claim),
    /// Equally specific claims have incompatible owners or authorities.
    Conflict,
    /// No claim matches the query name.
    Unmatched,
}

/// Resolves exact names before wildcards, independently of registration order.
/// Shared Compose service claims select the smallest complete container ID.
pub fn resolve<'a>(name: &Domain, claims: &'a [Claim]) -> Resolution<'a> {
    let ranked = claims
        .iter()
        .filter_map(|claim| claim.pattern.rank(name).map(|rank| (rank, claim)));
    let Some((rank, winner)) = ranked.clone().max_by_key(|(rank, claim)| {
        (
            *rank,
            Reverse((&claim.owner, &claim.authority, &claim.compose_service)),
        )
    }) else {
        return Resolution::Unmatched;
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
