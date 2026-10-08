//! Connection-bound resolver ownership and revision-safe file publication.

#[cfg(test)]
mod authority_tests;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arcbox_constants::{dns::Domain, paths::ArcboxProfile};
use arcbox_helper::validate::DnsPort;
use arcbox_helper::{
    HelperError, ResolverAuthorityRoot, ResolverAuthorityState, ResolverLease, ResolverOwner,
};

use super::{
    ResolverStore, contents,
    storage::{self, ManagedFile, open_directory},
};

pub struct Update {
    pub profile: ArcboxProfile,
    nonce: [u8; 16],
    revision: u64,
    port: DnsPort,
    roots: Vec<Domain>,
}

impl TryFrom<ResolverLease> for Update {
    type Error = HelperError;

    fn try_from(request: ResolverLease) -> Result<Self, Self::Error> {
        let mut roots = request
            .roots
            .into_iter()
            .map(|root| root.parse::<Domain>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(HelperError::validation)?;
        if roots.iter().any(|root| root.as_str() == "local") {
            return Err(HelperError::validation(
                "the .local namespace cannot be claimed",
            ));
        }
        roots.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        roots.dedup();
        Ok(Self {
            profile: request.profile.parse().map_err(HelperError::validation)?,
            nonce: request.nonce,
            revision: request.revision,
            port: DnsPort::try_from(request.port).map_err(HelperError::validation)?,
            roots,
        })
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) epoch: u64,
    pub(super) authority_revision: u64,
    next_connection: u64,
    connections: BTreeMap<u64, Lease>,
}

#[derive(Default)]
struct Lease {
    nonce: Option<[u8; 16]>,
    accepted: Option<Accepted>,
    failed: bool,
    retiring: bool,
    records: Vec<Record>,
}

struct Accepted {
    revision: u64,
    port: DnsPort,
}

struct Record {
    roots: Vec<Domain>,
    contents: String,
}

impl State {
    pub(super) fn ownership(&self) -> BTreeMap<String, (u64, Option<u16>)> {
        let mut roots = BTreeMap::new();
        for (&owner, lease) in &self.connections {
            let port = lease.accepted.as_ref().and_then(|accepted| {
                (!lease.failed && !lease.retiring).then_some(accepted.port.value())
            });
            for root in lease.records.iter().flat_map(|record| &record.roots) {
                roots.entry(root.to_string()).or_insert((owner, port));
            }
        }
        roots
    }

    pub(super) fn contains(&self, connection: u64) -> bool {
        self.connections.contains_key(&connection)
    }

    pub(super) fn roots(&self, connection: u64) -> Vec<ResolverAuthorityRoot> {
        let mut roots = BTreeMap::new();
        for (&owner, lease) in &self.connections {
            let state = match &lease.accepted {
                Some(accepted) if !lease.failed && !lease.retiring => {
                    ResolverAuthorityState::Active {
                        port: accepted.port.value(),
                        accepted_revision: accepted.revision,
                    }
                }
                _ => ResolverAuthorityState::RetainedUnavailable,
            };
            for root in lease.records.iter().flat_map(|record| &record.roots) {
                roots
                    .entry(root.as_str().to_owned())
                    .or_insert_with(|| ResolverAuthorityRoot {
                        root: root.as_str().to_owned(),
                        owner: if owner == connection {
                            ResolverOwner::CurrentConnection
                        } else {
                            ResolverOwner::OtherConnection
                        },
                        state: state.clone(),
                    });
            }
        }
        roots.into_values().collect()
    }

    pub(super) fn unleased(&self, root: &Domain) -> Result<(), HelperError> {
        if self
            .connections
            .values()
            .any(|lease| lease.records.iter().any(|r| r.roots.contains(root)))
        {
            return Err(HelperError::other(format!(
                "resolver root {root} has a connection lease"
            )));
        }
        Ok(())
    }
}

impl ResolverStore {
    pub fn connection(&self) -> u64 {
        let mut state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        state.next_connection = state
            .next_connection
            .checked_add(1)
            .expect("resolver connection identity exhausted");
        let connection = state.next_connection;
        state.connections.insert(connection, Lease::default());
        connection
    }

    pub fn update(&self, connection: u64, uid: u32, update: Update) -> Result<(), HelperError> {
        self.update_with(connection, uid, update, |path, contents| {
            arcbox_atomic_file::write_public(path, contents)
                .map_err(|e| HelperError::io("write", path, e))
        })
    }

    pub fn invalidate(&self, connection: u64) {
        self.mutate(|state| {
            if let Some(lease) = state.connections.get_mut(&connection) {
                lease.failed = true;
            }
        });
    }

    pub fn retire(&self, connection: u64) {
        self.mutate(|state| {
            if let Some(lease) = state.connections.get_mut(&connection) {
                lease.retiring = true;
            }
        });
    }

    fn update_with(
        &self,
        connection: u64,
        uid: u32,
        update: Update,
        write: impl Fn(&Path, &[u8]) -> Result<(), HelperError>,
    ) -> Result<(), HelperError> {
        self.mutate(|state| self.publish(state, connection, uid, update, write))
    }

    fn publish(
        &self,
        state: &mut State,
        connection: u64,
        uid: u32,
        update: Update,
        write: impl Fn(&Path, &[u8]) -> Result<(), HelperError>,
    ) -> Result<(), HelperError> {
        let current = state
            .connections
            .get_mut(&connection)
            .ok_or_else(|| HelperError::other("resolver connection is closed"))?;
        if current.failed {
            return Err(HelperError::validation("resolver lease connection failed"));
        }
        current.failed = true;
        if update.revision <= current.accepted.as_ref().map_or(0, |lease| lease.revision)
            || current.nonce.is_some_and(|nonce| nonce != update.nonce)
        {
            return Err(HelperError::validation(
                "resolver lease is invalid, stale, or has a different nonce",
            ));
        }
        for (&owner, lease) in &state.connections {
            if owner != connection
                && lease
                    .records
                    .iter()
                    .any(|record| record.roots.iter().any(|root| update.roots.contains(root)))
            {
                return Err(HelperError::other(
                    "resolver root belongs to another connection",
                ));
            }
        }
        fs::create_dir_all(&self.directory)
            .map_err(|e| HelperError::io("create_dir", &self.directory, e))?;
        let _directory = open_directory(&self.directory)?.ok_or_else(|| {
            HelperError::io("open", &self.directory, "resolver directory disappeared")
        })?;
        let current = state
            .connections
            .get(&connection)
            .expect("connection was checked under lock");
        for root in &update.roots {
            let path = self.directory.join(root.as_str());
            if storage::inspect(&path, |bytes| {
                current.records.iter().any(|record| {
                    record.roots.contains(root) && bytes == record.contents.as_bytes()
                })
            })? == ManagedFile::Foreign
            {
                return Err(HelperError::foreign_managed(&path));
            }
        }
        let record = Record {
            contents: record_contents(
                update.port,
                uid,
                update.profile,
                update.nonce,
                update.revision,
                connection,
            ),
            roots: update.roots,
        };
        let lease = state
            .connections
            .get_mut(&connection)
            .expect("connection was checked under lock");
        lease.records.push(record);
        let result = (|| {
            let next = lease.records.last().expect("attempted record was inserted");
            for root in &next.roots {
                write(
                    &self.directory.join(root.as_str()),
                    next.contents.as_bytes(),
                )?;
            }
            remove(
                &lease.records[..lease.records.len() - 1],
                &self.directory,
                &next.roots,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            return match remove(&lease.records, &self.directory, &[]) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(HelperError::other(format!(
                    "{error}; resolver cleanup failed: {cleanup}"
                ))),
            };
        }
        let next = lease.records.pop().expect("attempted record was inserted");
        lease.records.clear();
        lease.records.push(next);
        lease.nonce = Some(update.nonce);
        lease.accepted = Some(Accepted {
            revision: update.revision,
            port: update.port,
        });
        lease.failed = false;
        Ok(())
    }

    pub fn release(&self, connection: u64) -> Result<(), HelperError> {
        self.mutate(|state| {
            let Some(lease) = state.connections.get_mut(&connection) else {
                return Ok(());
            };
            lease.failed = true;
            remove(&lease.records, &self.directory, &[])?;
            state.connections.remove(&connection);
            Ok(())
        })
    }

    pub fn recover(&self) -> Result<(), HelperError> {
        let _guard = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        let Some(_directory) = open_directory(&self.directory)? else {
            return Ok(());
        };
        for entry in fs::read_dir(&self.directory)
            .map_err(|e| HelperError::io("list", &self.directory, e))?
        {
            let entry = entry.map_err(|e| HelperError::io("list", &self.directory, e))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.parse::<Domain>().is_ok())
                && storage::inspect(&entry.path(), is_record)? == ManagedFile::Owned
            {
                fs::remove_file(entry.path())
                    .map_err(|e| HelperError::io("remove", entry.path(), e))?;
            }
        }
        Ok(())
    }
}

fn remove(records: &[Record], directory: &Path, keep: &[Domain]) -> Result<(), HelperError> {
    if records.is_empty() {
        return Ok(());
    }
    let Some(_directory) = open_directory(directory)? else {
        return Ok(());
    };
    let mut errors = Vec::new();
    for record in records {
        for root in record.roots.iter().filter(|root| !keep.contains(root)) {
            let path = directory.join(root.as_str());
            let result = storage::inspect(&path, |bytes| bytes == record.contents.as_bytes())
                .and_then(|file| {
                    if file == ManagedFile::Owned {
                        fs::remove_file(&path).map_err(|e| HelperError::io("remove", &path, e))?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(HelperError::other(errors.join("; ")))
    }
}

fn record_contents(
    port: DnsPort,
    uid: u32,
    profile: ArcboxProfile,
    nonce: [u8; 16],
    revision: u64,
    connection: u64,
) -> String {
    format!(
        "{}# lease {uid} {profile} {:032x} {revision} {connection}\n",
        contents(port),
        u128::from_be_bytes(nonce)
    )
}

fn is_record(bytes: &[u8]) -> bool {
    let parse = || {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut fields = text.lines().last()?.strip_prefix("# lease ")?.split(' ');
        let uid = fields.next()?.parse::<u32>().ok()?;
        let profile = fields.next()?.parse::<ArcboxProfile>().ok()?;
        let nonce = u128::from_str_radix(fields.next()?, 16).ok()?.to_be_bytes();
        let revision = fields.next()?.parse::<u64>().ok()?;
        let connection = fields.next()?.parse::<u64>().ok()?;
        let port = DnsPort::try_from(
            text.lines()
                .nth(2)?
                .strip_prefix("port ")?
                .parse::<u16>()
                .ok()?,
        )
        .ok()?;
        Some(
            revision > 0
                && connection > 0
                && bytes
                    == record_contents(port, uid, profile, nonce, revision, connection).as_bytes(),
        )
    };
    parse().unwrap_or(false)
}
