//! Resolver lease publication, isolation, cleanup, and restart regressions.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;

use arcbox_atomic_file::{AtomicWriteError, write_public};

use super::{DnsPort, HelperError, ResolverLease, ResolverStore, Update};

fn request(revision: u64, roots: &[&str]) -> ResolverLease {
    ResolverLease {
        profile: "development".into(),
        nonce: [7; 16],
        revision,
        port: 5554,
        roots: roots.iter().map(|root| (*root).into()).collect(),
    }
}

fn update(revision: u64, roots: &[&str]) -> Update {
    Update::try_from(request(revision, roots)).unwrap()
}

fn fixture() -> (tempfile::TempDir, ResolverStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = ResolverStore::new(directory.path().to_path_buf());
    (directory, store)
}

#[test]
fn roots_are_deduplicated_without_collapsing_descendants() {
    let parsed = update(1, &["z.local", "a.z.local", "z.local"]);
    let roots: Vec<_> = parsed.roots.iter().map(|root| root.as_str()).collect();
    assert_eq!(roots, ["a.z.local", "z.local"]);
    for root in ["local", "../escape"] {
        assert!(Update::try_from(request(1, &[root])).is_err());
    }
    let mut invalid = request(1, &["a.local"]);
    invalid.port = 0;
    assert!(Update::try_from(invalid).is_err());
}

#[test]
fn nonce_and_revision_rejections_preserve_the_accepted_files() {
    for (revision, nonce) in [(0, [7; 16]), (1, [7; 16]), (2, [8; 16])] {
        let (dir, store) = fixture();
        let id = store.connection();
        store.update(id, 501, update(1, &["a.local"])).unwrap();
        let path = dir.path().join("a.local");
        let accepted = fs::read(&path).unwrap();
        let mut rejected = request(revision, &[]);
        rejected.nonce = nonce;
        assert!(store.update(id, 501, rejected.try_into().unwrap()).is_err());
        assert_eq!(fs::read(&path).unwrap(), accepted);
        assert!(store.update(id, 501, update(3, &[])).is_err());
        assert_eq!(fs::read(&path).unwrap(), accepted);
        store.release(id).unwrap();
        assert!(!path.exists());
    }
}

#[test]
fn exact_roots_are_exclusive_and_late_release_preserves_the_replacement() {
    let (dir, store) = fixture();
    let first = store.connection();
    let second = store.connection();
    store.update(first, 501, update(1, &["a.local"])).unwrap();
    let metadata = fs::metadata(dir.path().join("a.local")).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
    let accepted = fs::read(dir.path().join("a.local")).unwrap();
    assert!(store.update(second, 501, update(1, &["a.local"])).is_err());
    assert_eq!(fs::read(dir.path().join("a.local")).unwrap(), accepted);
    assert!(
        store
            .update(second, 501, update(2, &["child.a.local"]))
            .is_err()
    );
    let nested = store.connection();
    store
        .update(nested, 501, update(1, &["child.a.local"]))
        .unwrap();
    let foreign_stamp = fs::read(dir.path().join("child.a.local")).unwrap();
    fs::write(dir.path().join("a.local"), &foreign_stamp).unwrap();
    store.release(first).unwrap();
    assert_eq!(fs::read(dir.path().join("a.local")).unwrap(), foreign_stamp);
    let rejected = store.connection();
    let error = store.update(rejected, 501, update(1, &["a.local"]));
    assert!(matches!(error, Err(HelperError::ForeignManagedFile { .. })));
    assert_eq!(fs::read(dir.path().join("a.local")).unwrap(), foreign_stamp);
    fs::remove_file(dir.path().join("a.local")).unwrap();
    assert!(
        store
            .update(rejected, 501, update(2, &["a.local"]))
            .is_err()
    );
    assert!(!dir.path().join("a.local").exists());
    let next = store.connection();
    store.update(next, 501, update(1, &["a.local"])).unwrap();
    let replacement = fs::read(dir.path().join("a.local")).unwrap();
    assert_ne!(replacement, accepted);
    store.release(first).unwrap();
    store.release(first).unwrap();
    assert_eq!(fs::read(dir.path().join("a.local")).unwrap(), replacement);
    store.update(next, 501, update(2, &[])).unwrap();
    assert!(!dir.path().join("a.local").exists());
    assert!(dir.path().join("child.a.local").exists());
}

#[test]
fn complete_preflight_preserves_foreign_files_and_links() {
    let (dir, store) = fixture();
    let path = dir.path().join("z.local");
    let target_dir = tempfile::tempdir().unwrap();
    let target = target_dir.path().join("target");
    fs::write(&target, "nameserver 1.1.1.1\n").unwrap();
    for kind in 0..3 {
        let id = store.connection();
        match kind {
            0 => fs::write(&path, "nameserver 1.1.1.1\n").unwrap(),
            1 => symlink(&target, &path).unwrap(),
            _ => fs::hard_link(&target, &path).unwrap(),
        }
        let error = store.update(id, 501, update(1, &["a.local", "z.local"]));
        assert!(matches!(error, Err(HelperError::ForeignManagedFile { .. })));
        assert!(!dir.path().join("a.local").exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "nameserver 1.1.1.1\n");
        if kind == 1 {
            assert_eq!(fs::read_link(&path).unwrap(), target);
        }
        fs::remove_file(&path).unwrap();
        assert!(store.update(id, 501, update(2, &["a.local"])).is_err());
        assert!(!dir.path().join("a.local").exists());
        store.release(id).unwrap();
    }
    assert_eq!(fs::read_to_string(target).unwrap(), "nameserver 1.1.1.1\n");
}

#[test]
fn partial_publication_cleans_both_revisions_and_invalidates_the_connection() {
    let (dir, store) = fixture();
    let id = store.connection();
    store
        .update(id, 501, update(1, &["a.local", "old.local"]))
        .unwrap();
    let attempted = update(2, &["a.local", "b.local"]);
    let error = store
        .update_with(id, 501, attempted, |path, bytes| {
            write_public(path, bytes).unwrap();
            if path.ends_with("b.local") {
                let failure = AtomicWriteError::DurabilityUncertain {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("injected fsync failure"),
                };
                return Err(HelperError::io("write", path, failure));
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("injected fsync failure"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    for nonce in [[7; 16], [8; 16]] {
        let mut next = request(3, &["a.local"]);
        next.nonce = nonce;
        assert!(store.update(id, 501, next.try_into().unwrap()).is_err());
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    store.release(id).unwrap();
    assert!(store.update(id, 501, update(4, &[])).is_err());
}

#[test]
fn cleanup_aggregates_errors_and_retains_evidence_for_release_retry() {
    let (dir, store) = fixture();
    let id = store.connection();
    store
        .update(id, 501, update(1, &["a.local", "b.local", "c.local"]))
        .unwrap();
    let error = store
        .update_with(id, 501, update(2, &["a.local"]), |_, _| {
            for root in ["a.local", "b.local"] {
                let path = dir.path().join(root);
                fs::remove_file(&path).unwrap();
                drop(UnixListener::bind(path).unwrap());
            }
            Err(HelperError::other("injected publication failure"))
        })
        .unwrap_err()
        .to_string();
    for context in [
        "injected publication failure",
        "resolver cleanup failed",
        "a.local",
        "b.local",
    ] {
        assert!(error.contains(context), "{error}");
    }
    assert!(!dir.path().join("c.local").exists());
    assert!(store.release(id).is_err());
    assert!(store.update(id, 501, update(3, &[])).is_err());
    for root in ["a.local", "b.local"] {
        fs::remove_file(dir.path().join(root)).unwrap();
    }
    store.release(id).unwrap();
    let replacement = store.connection();
    store
        .update(replacement, 501, update(1, &["a.local", "b.local"]))
        .unwrap();
}

#[test]
fn restart_recovery_removes_only_exact_private_unlinked_lease_files() {
    let (dir, store) = fixture();
    store
        .update(store.connection(), 501, update(1, &["owned.local"]))
        .unwrap();
    let owned = fs::read_to_string(dir.path().join("owned.local")).unwrap();
    let preserved = [
        ("extra.local", format!("{owned}\n")),
        ("uid.local", owned.replace("# lease 501 ", "# lease 0501 ")),
        (
            "legacy.local",
            super::contents(DnsPort::try_from(5554).unwrap()),
        ),
        ("writable.local", owned.clone()),
    ];
    for (name, bytes) in &preserved {
        fs::write(dir.path().join(name), bytes).unwrap();
    }
    fs::set_permissions(
        dir.path().join("writable.local"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target");
    fs::write(&target, &owned).unwrap();
    symlink(&target, dir.path().join("symlink.local")).unwrap();
    fs::hard_link(&target, dir.path().join("hardlink.local")).unwrap();
    let restarted = ResolverStore::new(dir.path().to_path_buf());
    restarted.recover().unwrap();
    assert!(!dir.path().join("owned.local").exists());
    for (name, bytes) in preserved {
        assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), bytes);
    }
    assert_eq!(
        fs::read_link(dir.path().join("symlink.local")).unwrap(),
        target
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("hardlink.local")).unwrap(),
        owned
    );
    assert_eq!(fs::read_to_string(target).unwrap(), owned);
    restarted.recover().unwrap();
}

#[test]
fn owned_looking_changes_block_updates_and_survive_cleanup() {
    for kind in 0..4 {
        let (dir, store) = fixture();
        let id = store.connection();
        store.update(id, 501, update(1, &["a.local"])).unwrap();
        let path = dir.path().join("a.local");
        let accepted = fs::read_to_string(&path).unwrap();
        let changed = match kind {
            0 => format!("# copied {accepted}"),
            1 => format!("{accepted}nameserver 1.1.1.1\n"),
            2 => accepted.replace("port 5554", "port 53"),
            _ => accepted.replace("port 5554", "port 05554"),
        };
        fs::write(&path, &changed).unwrap();
        assert!(store.update(id, 501, update(2, &["a.local"])).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        store.release(id).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        ResolverStore::new(dir.path().to_path_buf())
            .recover()
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
    }
}

#[test]
fn unsafe_resolver_slots_block_updates_and_survive_cleanup() {
    for kind in 0..5 {
        let (dir, store) = fixture();
        let id = store.connection();
        store.update(id, 501, update(1, &["a.local"])).unwrap();
        let path = dir.path().join("a.local");
        let accepted = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        if kind != 2 {
            fs::write(&target, &accepted).unwrap();
        }
        match kind {
            0 => {
                fs::write(&path, &accepted).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
            }
            1 | 2 => symlink(&target, &path).unwrap(),
            3 => fs::hard_link(&target, &path).unwrap(),
            _ => fs::create_dir(&path).unwrap(),
        }
        assert!(store.update(id, 501, update(2, &["a.local"])).is_err());
        store.release(id).unwrap();
        ResolverStore::new(dir.path().to_path_buf())
            .recover()
            .unwrap();
        if matches!(kind, 1 | 2) {
            assert_eq!(fs::read_link(&path).unwrap(), target);
        }
        if kind == 4 {
            assert!(path.is_dir());
        } else if kind != 2 {
            assert_eq!(fs::read(&path).unwrap(), accepted);
        }
        if kind != 2 {
            assert_eq!(fs::read(&target).unwrap(), accepted);
        }
    }
}

#[test]
fn unsafe_resolver_directories_block_publication_and_recovery() {
    for linked in [true, false] {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = parent.path().join("resolver");
        if linked {
            symlink(outside.path(), &path).unwrap();
        } else {
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        }
        let store = ResolverStore::new(path.clone());
        assert!(
            store
                .update(store.connection(), 501, update(1, &["a.local"]))
                .is_err()
        );
        assert!(store.recover().is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        if linked {
            assert_eq!(fs::read_link(&path).unwrap(), outside.path());
        }
    }
}

#[test]
fn acl_changes_block_updates_and_preserve_files_and_directories() {
    for directory_acl in [true, false] {
        let (dir, store) = fixture();
        let id = store.connection();
        store.update(id, 501, update(1, &["a.local"])).unwrap();
        let path = dir.path().join("a.local");
        let accepted = fs::read(&path).unwrap();
        let (target, grant) = if directory_acl {
            (dir.path(), "everyone allow add_file")
        } else {
            (path.as_path(), "everyone allow write")
        };
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", grant])
                .arg(target)
                .status()
                .unwrap()
                .success()
        );
        assert!(store.update(id, 501, update(2, &["a.local"])).is_err());
        assert_eq!(fs::read(&path).unwrap(), accepted);
        let restarted = ResolverStore::new(dir.path().to_path_buf());
        if directory_acl {
            assert!(store.release(id).is_err());
            assert!(restarted.recover().is_err());
        } else {
            store.release(id).unwrap();
            restarted.recover().unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), accepted);
    }
}
