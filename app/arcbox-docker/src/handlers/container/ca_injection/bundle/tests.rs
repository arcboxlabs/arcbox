//! Preserve caller roots and reject ambiguous archive ownership.

use super::super::test_support::{root, tar, untar};
use super::*;

#[test]
fn rotation_replaces_only_the_owned_root_and_is_idempotent() {
    let original = [b"# User roots\n".as_slice(), &root()].concat();
    let old = root();
    let new = root();
    let first = reconcile(&original, &old).unwrap();
    let updated = reconcile(&first, &new).unwrap();
    assert!(updated.starts_with(&original));
    assert!(!String::from_utf8_lossy(&updated).contains(std::str::from_utf8(&old).unwrap().trim()));
    assert!(String::from_utf8_lossy(&updated).contains(std::str::from_utf8(&new).unwrap().trim()));
    assert_eq!(reconcile(&updated, &new).unwrap(), updated);
    let foreign_tail = [first.as_slice(), b"# User suffix\n"].concat();
    assert!(
        reconcile(&foreign_tail, &new)
            .unwrap()
            .ends_with(b"# User suffix\n")
    );
}

#[test]
fn damaged_or_ambiguous_ownership_markers_do_not_delete_roots() {
    let pem = root();
    for original in [
        b"\n# BEGIN ARCBOX LOCAL CA\n".as_slice(),
        b"# END ARCBOX LOCAL CA\n",
        b"\n# BEGIN ARCBOX LOCAL CA\nforeign root\n# END ARCBOX LOCAL CA\n",
    ] {
        assert!(reconcile(original, &pem).is_err());
    }
    let once = reconcile(b"original", &pem).unwrap();
    assert!(reconcile(&[once.as_slice(), &once].concat(), &pem).is_err());
}

#[test]
fn archive_retains_owner_mode_and_original_roots() {
    let input = tar(b"original roots\n");
    let output = reconcile_archive(
        &input,
        Path::new("/etc/ssl/certs/ca-certificates.crt"),
        &root(),
    )
    .unwrap()
    .unwrap();
    let (header, content) = untar(&output);
    assert_eq!(
        (
            header.uid().unwrap(),
            header.gid().unwrap(),
            header.mode().unwrap(),
            header.mtime().unwrap()
        ),
        (42, 84, 0o640, 1234)
    );
    assert!(content.starts_with(b"original roots\n"));
    assert!(reconcile_archive(&input, Path::new("/etc/other.pem"), &root()).is_err());
}

#[test]
fn archive_rejects_links_and_extra_files() {
    let path = Path::new("/etc/ssl/certs/ca-certificates.crt");
    for (entry_type, count) in [(tar::EntryType::Symlink, 1), (tar::EntryType::Regular, 2)] {
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(0);
        header.set_mode(0o644);
        if entry_type.is_symlink() {
            header.set_link_name("/caller-mounted/roots.pem").unwrap();
        }
        for _ in 0..count {
            archive
                .append_data(&mut header, "ca-certificates.crt", std::io::empty())
                .unwrap();
        }
        assert!(reconcile_archive(&archive.into_inner().unwrap(), path, &root()).is_err());
    }
}
