//! Instance authority, resource claims, addresses, and revisions in one caller-locked snapshot.

use std::collections::HashMap;
use std::net::IpAddr;

use arcbox_constants::dns::Domain;

use crate::claims::{self, Boundary, Claim, Resolution};

/// A validated namespace from the complete instance authority snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceAuthority {
    /// Inclusive namespace root; snapshot roots are unique and canonically sorted.
    pub root: Domain,
    /// The current route for this namespace.
    pub target: InstanceTarget,
}

/// Effective instance state, independent of helper transport revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceTarget {
    /// The current connection actively owns the namespace.
    Current,
    /// Another connection serves DNS on the host loopback interface.
    Other {
        /// Validated, nonzero host DNS port.
        port: u16,
    },
    /// The namespace remains reserved without an available DNS server.
    Unavailable,
}

/// One resource remains registered even when its last network disappears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// Claims supplied by the resource owner.
    pub claims: Vec<Claim>,
    /// No address excludes every claim from ownership selection.
    pub address: Option<IpAddr>,
    /// Registry epoch at the last change to this registration.
    pub revision: u64,
}

/// A selected claim carries its address and revision from the same snapshot.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// One current resource owns the selected name and has a network address.
    Matched {
        /// The selected name claim and its trusted owner identities.
        claim: &'a Claim,
        /// The owner's current network address.
        address: IpAddr,
        /// Resource and instance revision for checks after asynchronous work.
        revision: u64,
        /// The selected instance actively permits host publication of this name.
        publishable: bool,
    },
    /// Instance routing terminates lookup, including an owned namespace without a resource.
    Instance {
        /// The most specific matching instance namespace.
        authority: &'a InstanceAuthority,
        /// The instance snapshot revision for checks after forwarding.
        revision: u64,
    },
    /// Equally specific claims or boundaries have incompatible identities.
    Conflict,
    /// A local authority reserves the name without an available matching resource.
    Reserved(&'a Boundary),
    /// Another instance owns this namespace.
    Delegated(&'a Boundary),
    /// No available resource or namespace boundary matches.
    Unmatched,
}

/// A supplied claim belongs to a different registration owner.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("claim owner does not match registration owner")]
pub struct OwnerMismatch;

/// Callers hold one lock across ownership lookup and dependent state reads.
#[derive(Debug, Default)]
pub struct Registry {
    entries: HashMap<String, Registration>,
    boundaries: Vec<Boundary>,
    instances: Vec<InstanceAuthority>,
    authority_revision: u64,
    epoch: u64,
}

impl Registry {
    /// Creates an empty registry with trusted namespace boundaries.
    #[must_use]
    pub fn new(boundaries: Vec<Boundary>) -> Self {
        Self {
            boundaries,
            ..Self::default()
        }
    }

    /// Reserves exact host aliases and the index apex before resources register.
    ///
    /// # Errors
    /// Returns an error if the suffix replaces a host alias or exceeds its name budget.
    pub fn with_builtin_names(domain: &Domain) -> Result<Self, String> {
        let names = [
            "host".to_owned(),
            format!("host.{domain}"),
            "host.docker.internal".to_owned(),
            "gateway.docker.internal".to_owned(),
        ];
        if names.iter().any(|name| name == domain.as_str()) {
            return Err(format!(
                "DNS domain '{domain}' conflicts with a built-in host name"
            ));
        }
        let mut boundaries = names
            .into_iter()
            .map(|name| {
                name.parse().map(|name| Boundary::ReservedName {
                    name,
                    authority: "system".into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        boundaries.push(Boundary::ReservedName {
            name: domain.clone(),
            authority: "index".into(),
        });
        Ok(Self::new(boundaries))
    }

    /// Every effective mutation advances the epoch, including removal.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Replaces validated, canonically sorted instance roots without changing resource claims.
    /// Equivalent effective snapshots preserve every revision.
    pub fn replace_instance_authorities(&mut self, instances: Vec<InstanceAuthority>) -> bool {
        if self.instances == instances {
            return false;
        }
        self.authority_revision = self.advance();
        self.instances = instances;
        true
    }

    /// Revokes known roots and starts a new authority lifetime, even without active roots.
    pub fn invalidate_instance_authorities(&mut self) {
        for instance in &mut self.instances {
            instance.target = InstanceTarget::Unavailable;
        }
        self.authority_revision = self.advance();
    }

    /// Returns the owner's complete registration, including an unavailable resource.
    #[must_use]
    pub fn get(&self, owner: &str) -> Option<&Registration> {
        self.entries.get(owner)
    }

    /// Enumerates every registered owner, including resources without an address.
    pub fn owners(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Replaces all claims atomically; identical registrations keep their revision.
    ///
    /// # Errors
    /// Returns `OwnerMismatch` without changing state if any claim names another owner.
    pub fn replace(
        &mut self,
        owner: String,
        claims: Vec<Claim>,
        address: Option<IpAddr>,
    ) -> Result<u64, OwnerMismatch> {
        if claims.iter().any(|claim| claim.owner() != owner) {
            return Err(OwnerMismatch);
        }
        if let Some(entry) = self.entries.get(&owner)
            && entry.claims == claims
            && entry.address == address
        {
            return Ok(entry.revision);
        }
        let revision = self.advance();
        self.entries.insert(
            owner,
            Registration {
                claims,
                address,
                revision,
            },
        );
        Ok(revision)
    }

    /// Removes every claim for an owner and reports whether the registry changed.
    pub fn remove(&mut self, owner: &str) -> bool {
        if self.entries.remove(owner).is_none() {
            return false;
        }
        self.advance();
        true
    }

    /// Resolves ownership, address, and revision from the same current registration.
    #[must_use]
    pub fn resolve(&self, name: &Domain) -> Lookup<'_> {
        let instance = self
            .instances
            .iter()
            .filter(|instance| {
                name == &instance.root
                    || name
                        .as_str()
                        .strip_suffix(instance.root.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
            .max_by_key(|instance| instance.root.as_str().len());
        if let Some(authority) = instance
            && authority.target != InstanceTarget::Current
        {
            return Lookup::Instance {
                authority,
                revision: self.authority_revision,
            };
        }
        let claims = self
            .entries
            .values()
            .filter(|entry| entry.address.is_some())
            .flat_map(|entry| entry.claims.iter());
        match claims::resolve(name, claims, &self.boundaries) {
            Resolution::Matched(claim) => {
                let entry = &self.entries[claim.owner()];
                Lookup::Matched {
                    claim,
                    address: entry.address.expect("selected claims have an address"),
                    revision: entry.revision.max(self.authority_revision),
                    publishable: instance.is_some(),
                }
            }
            Resolution::Conflict => Lookup::Conflict,
            Resolution::Reserved(boundary) => Lookup::Reserved(boundary),
            Resolution::Delegated(boundary) => Lookup::Delegated(boundary),
            Resolution::Unmatched => {
                instance.map_or(Lookup::Unmatched, |authority| Lookup::Instance {
                    authority,
                    revision: self.authority_revision,
                })
            }
        }
    }

    fn advance(&mut self) -> u64 {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("DNS registry epoch exhausted");
        self.epoch
    }
}
