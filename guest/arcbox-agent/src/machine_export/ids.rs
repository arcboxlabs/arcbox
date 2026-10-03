//! Stable ids for the objects the export names.
//!
//! NFSv3 identifies every object by a 64-bit `fileid` inside an opaque
//! handle the client keeps across requests, while the machine's filesystem
//! only has paths: inode numbers repeat across the mounts under the root
//! (`/proc`, `/sys`, the overlay, each VirtioFS share) and overlayfs
//! renumbers a file on copy-up, so neither can serve as the id. The table
//! issues ids as objects are first seen and remembers each as `(parent,
//! name)`, a tree: a path is the walk up to the root, and renaming a
//! directory moves one entry while every descendant keeps resolving under
//! the new name.
//!
//! Entries are forgotten when the host removes or renames over them, and
//! only then: an object deleted inside the machine keeps its id until the
//! host next asks for it and gets `NFS3ERR_STALE`. Memory therefore grows
//! with the number of distinct paths the host has touched, about a hundred
//! bytes each.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The id of the export root. `0` is reserved by the protocol.
pub const ROOT: u64 = 1;

#[derive(Debug)]
struct Entry {
    parent: u64,
    name: OsString,
}

/// The id ↔ `(parent, name)` table; see the module docs.
#[derive(Debug)]
pub struct IdTable {
    next: u64,
    entries: HashMap<u64, Entry>,
    /// The children each directory has had looked up, by name.
    children: HashMap<u64, HashMap<OsString, u64>>,
}

impl IdTable {
    pub fn new() -> Self {
        Self {
            next: ROOT + 1,
            entries: HashMap::new(),
            children: HashMap::new(),
        }
    }

    /// The path of `id` under `root`, or `None` for an id the table never
    /// issued or has forgotten.
    pub fn path(&self, root: &Path, id: u64) -> Option<PathBuf> {
        let mut names: Vec<&OsStr> = Vec::new();
        let mut current = id;
        while current != ROOT {
            let entry = self.entries.get(&current)?;
            names.push(&entry.name);
            current = entry.parent;
        }
        let mut path = root.to_path_buf();
        path.extend(names.into_iter().rev());
        Some(path)
    }

    /// The directory holding `id`; the root is its own parent.
    pub fn parent(&self, id: u64) -> Option<u64> {
        if id == ROOT {
            return Some(ROOT);
        }
        self.entries.get(&id).map(|entry| entry.parent)
    }

    /// The id of `name` under `parent`, issued on first sight.
    pub fn child(&mut self, parent: u64, name: &OsStr) -> u64 {
        if let Some(id) = self.lookup(parent, name) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.entries.insert(
            id,
            Entry {
                parent,
                name: name.to_owned(),
            },
        );
        self.children
            .entry(parent)
            .or_default()
            .insert(name.to_owned(), id);
        id
    }

    /// The id of `name` under `parent`, if it has one.
    pub fn lookup(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.children.get(&parent)?.get(name).copied()
    }

    /// Forgets `name` under `parent` and everything below it.
    pub fn forget(&mut self, parent: u64, name: &OsStr) {
        let Some(id) = self
            .children
            .get_mut(&parent)
            .and_then(|children| children.remove(name))
        else {
            return;
        };
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            self.entries.remove(&id);
            if let Some(children) = self.children.remove(&id) {
                pending.extend(children.into_values());
            }
        }
    }

    /// Moves the entry `from_name` under `from_parent` to `to_name` under
    /// `to_parent`, forgetting whatever `to_name` named there before. The
    /// moved object keeps its id, and so does everything below it.
    pub fn rename(&mut self, from_parent: u64, from_name: &OsStr, to_parent: u64, to_name: &OsStr) {
        if from_parent == to_parent && from_name == to_name {
            return;
        }
        self.forget(to_parent, to_name);
        let Some(id) = self
            .children
            .get_mut(&from_parent)
            .and_then(|children| children.remove(from_name))
        else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.parent = to_parent;
            to_name.clone_into(&mut entry.name);
        }
        self.children
            .entry(to_parent)
            .or_default()
            .insert(to_name.to_owned(), id);
    }

    /// Number of objects with an id, the root excluded.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn ids_are_stable_and_resolve_to_paths() {
        let mut table = IdTable::new();
        let etc = table.child(ROOT, name("etc"));
        let hosts = table.child(etc, name("hosts"));
        assert_eq!(
            table.child(ROOT, name("etc")),
            etc,
            "a second sight reuses the id"
        );
        assert_ne!(etc, hosts);
        assert_eq!(
            table.path(Path::new("/"), hosts),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(table.path(Path::new("/"), ROOT), Some(PathBuf::from("/")));
        assert_eq!(table.parent(hosts), Some(etc));
        assert_eq!(table.parent(ROOT), Some(ROOT));
        assert_eq!(table.path(Path::new("/"), 99), None, "never issued");
    }

    /// Renaming a directory moves one entry; the ids below it keep
    /// resolving under the new name, which is what keeps the client's open
    /// handles valid across `mv`.
    #[test]
    fn renaming_a_directory_carries_its_subtree() {
        let mut table = IdTable::new();
        let src = table.child(ROOT, name("src"));
        let main = table.child(src, name("main.rs"));
        table.rename(ROOT, name("src"), ROOT, name("lib"));
        assert_eq!(
            table.path(Path::new("/"), main),
            Some(PathBuf::from("/lib/main.rs"))
        );
        assert_eq!(table.lookup(ROOT, name("lib")), Some(src));
        assert_eq!(table.lookup(ROOT, name("src")), None);
    }

    #[test]
    fn renaming_over_an_entry_forgets_it_and_its_children() {
        let mut table = IdTable::new();
        let old = table.child(ROOT, name("old"));
        let old_child = table.child(old, name("x"));
        let new = table.child(ROOT, name("new"));
        table.rename(ROOT, name("new"), ROOT, name("old"));
        assert_eq!(table.lookup(ROOT, name("old")), Some(new));
        assert_eq!(table.path(Path::new("/"), old), None);
        assert_eq!(table.path(Path::new("/"), old_child), None);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn renaming_onto_itself_keeps_the_entry() {
        let mut table = IdTable::new();
        let file = table.child(ROOT, name("f"));
        table.rename(ROOT, name("f"), ROOT, name("f"));
        assert_eq!(table.lookup(ROOT, name("f")), Some(file));
    }

    #[test]
    fn forgetting_drops_the_whole_subtree() {
        let mut table = IdTable::new();
        let dir = table.child(ROOT, name("dir"));
        let sub = table.child(dir, name("sub"));
        let leaf = table.child(sub, name("leaf"));
        let other = table.child(ROOT, name("other"));
        table.forget(ROOT, name("dir"));
        for id in [dir, sub, leaf] {
            assert_eq!(table.path(Path::new("/"), id), None);
        }
        assert_eq!(
            table.path(Path::new("/"), other),
            Some(PathBuf::from("/other"))
        );
        assert_eq!(table.len(), 1);
        // Forgetting an unknown name is a no-op, not a panic.
        table.forget(ROOT, name("missing"));
        assert_eq!(table.len(), 1);
    }
}
