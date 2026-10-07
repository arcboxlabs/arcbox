use super::*;

const SIZE: u64 = 128 * 1024;

#[test]
fn interruption_before_manifest_does_not_publish_or_adopt_images() {
    for count in 1..=2 {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data.img");
        let metadata = directory.path().join("meta.img");
        let mut staged = Vec::new();
        create_image(&data, SIZE, &mut staged).unwrap();
        if count == 2 {
            create_image(&metadata, SIZE, &mut staged).unwrap();
        }
        let old: Vec<_> = staged.iter().map(|path| fs::read(path).unwrap()).collect();
        assert!(!data.exists());
        assert!(!metadata.exists());

        let manifest = prepare_pair(&data, &metadata, SIZE, SIZE).unwrap();
        manifest.verify_images(directory.path()).unwrap();
        for (path, bytes) in staged.iter().zip(old) {
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_ne!(ImageIdentity::read(path).unwrap(), manifest.data.image);
            assert_ne!(ImageIdentity::read(path).unwrap(), manifest.metadata.image);
        }
    }
}

#[test]
fn durable_manifest_resumes_each_image_publication_boundary() {
    for published in 0..=2 {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data.img");
        let metadata = directory.path().join("meta.img");
        let mut staged = Vec::new();
        let manifest = StorageManifest {
            version: 1,
            data: create_image(&data, SIZE, &mut staged).unwrap(),
            metadata: create_image(&metadata, SIZE, &mut staged).unwrap(),
            layout: StorageLayout::Fresh,
        };
        manifest.save(&manifest_path(&data)).unwrap();
        for (source, target) in staged.iter().zip([&data, &metadata]).take(published) {
            fs::hard_link(source, target).unwrap();
        }
        File::open(directory.path()).unwrap().sync_all().unwrap();

        assert_eq!(
            prepare_pair(&data, &metadata, SIZE, SIZE).unwrap(),
            manifest
        );
        manifest.verify_images(directory.path()).unwrap();
        assert!(staged.iter().all(|path| !path.exists()));
        assert_eq!(
            prepare_pair(&data, &metadata, SIZE, SIZE).unwrap(),
            manifest
        );
    }
}

#[test]
fn failed_second_image_creation_leaves_no_published_pair() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data.img");
    let metadata = directory.path().join("meta.img");
    assert!(prepare_pair(&data, &metadata, SIZE, u64::MAX).is_err());
    assert_eq!(
        fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        ["data.storage.lock"]
    );
    prepare_pair(&data, &metadata, SIZE, SIZE).unwrap();
}

#[test]
fn naked_provisioning_header_does_not_grant_format_authority() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data.img");
    let metadata = directory.path().join("meta.img");
    let mut staged = Vec::new();
    create_image(&data, SIZE, &mut staged).unwrap();
    fs::rename(&staged[0], &data).unwrap();
    let before = fs::read(&data).unwrap();
    assert!(prepare_pair(&data, &metadata, SIZE, SIZE).is_err());
    assert_eq!(fs::read(data).unwrap(), before);
    assert!(!metadata.exists());
}

#[test]
fn replaced_staging_file_cannot_recreate_a_missing_member() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data.img");
    let metadata = directory.path().join("meta.img");
    let manifest = prepare_pair(&data, &metadata, SIZE, SIZE).unwrap();
    let staged = staging_path(directory.path(), manifest.metadata.filesystem_uuid);
    fs::copy(&metadata, &staged).unwrap();
    fs::remove_file(&metadata).unwrap();
    assert!(prepare_pair(&data, &metadata, SIZE, SIZE).is_err());
    assert!(!metadata.exists());
    assert!(staged.exists());
}

#[test]
fn concurrent_initializers_return_the_same_authoritative_pair() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data.img");
    let metadata = directory.path().join("meta.img");
    let start = std::sync::Barrier::new(8);
    let manifests = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    prepare_pair(&data, &metadata, SIZE, SIZE).unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    let final_pair = verify_pair(&data, &metadata).unwrap();
    assert!(manifests.iter().all(|manifest| *manifest == final_pair));
}
