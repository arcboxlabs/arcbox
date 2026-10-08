use super::{ArcboxProfile, DnsPort, ResolverStore, Update};

#[test]
fn nonce_and_revision_rejections_preserve_the_accepted_files() {
    let directory = tempfile::tempdir().unwrap();
    let store = ResolverStore::new(directory.path().to_path_buf());
    let connection = store.connection();
    let update = || Update {
        profile: ArcboxProfile::Development,
        nonce: [7; 16],
        revision: 1,
        port: DnsPort::try_from(5554).unwrap(),
        roots: vec!["a.local".parse().unwrap()],
    };
    store.update(connection, 501, update()).unwrap();
    let path = directory.path().join("a.local");
    let accepted = std::fs::read(&path).unwrap();
    assert!(store.update(connection, 501, update()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), accepted);
    store.release(connection).unwrap();
    assert!(!path.exists());
}
