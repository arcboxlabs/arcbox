//! Connection-bound resolver ownership and revision-safe file publication.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arcbox_constants::{dns::Domain, paths::ArcboxProfile};
use arcbox_helper::validate::DnsPort;
use arcbox_helper::{HelperError, ResolverLease};

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
    next_connection: u64,
    connections: BTreeMap<u64, Lease>,
}

#[derive(Default)]
struct Lease {
    nonce: Option<[u8; 16]>,
    revision: u64,
    failed: bool,
    records: Vec<Record>,
}

struct Record {
    roots: Vec<Domain>,
    contents: String,
}

impl State {
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
        let mut state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        if let Some(lease) = state.connections.get_mut(&connection) {
            lease.failed = true;
        }
    }

    fn update_with(
        &self,
        connection: u64,
        uid: u32,
        update: Update,
        write: impl Fn(&Path, &[u8]) -> Result<(), HelperError>,
    ) -> Result<(), HelperError> {
        let mut state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        let current = state
            .connections
            .get_mut(&connection)
            .ok_or_else(|| HelperError::other("resolver connection is closed"))?;
        if current.failed {
            return Err(HelperError::validation("resolver lease connection failed"));
        }
        current.failed = true;
        if update.revision <= current.revision
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
        lease.nonce = Some(update.nonce);
        lease.revision = update.revision;
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
        lease.failed = false;
        Ok(())
    }

    pub fn release(&self, connection: u64) -> Result<(), HelperError> {
        let mut state = self
            .mutation
            .lock()
            .expect("DNS resolver mutation lock poisoned");
        let Some(lease) = state.connections.get_mut(&connection) else {
            return Ok(());
        };
        lease.failed = true;
        remove(&lease.records, &self.directory, &[])?;
        state.connections.remove(&connection);
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
