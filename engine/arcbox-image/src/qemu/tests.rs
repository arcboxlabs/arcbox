use std::collections::HashMap;

use super::lockfile::{ArchPin, QemuPin};
use super::*;

const SHA: &str = "53657969b7ffcc5eb68969e1f7377475758d13ce6452fcdbda5fc2a53da092aa";

fn pin(sha256: &str) -> QemuPin {
    QemuPin {
        version: "0.1.0".to_owned(),
        cdn: "https://boot.arcboxcdn.com/qemu".to_owned(),
        arch: HashMap::from([(
            "arm64".to_owned(),
            ArchPin {
                sha256: sha256.to_owned(),
            },
        )]),
    }
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let pairs: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |key| pairs.get(key).cloned()
}

#[test]
fn the_assets_lock_pin_is_well_formed() {
    let pin = lockfile::pin().expect("assets.lock pins a QEMU build");
    semver::Version::parse(&pin.version).expect("the pinned QEMU version is semver");
    assert!(pin.cdn.starts_with("https://"), "{}", pin.cdn);
    for (arch, ArchPin { sha256 }) in &pin.arch {
        assert!(
            sha256.is_empty()
                || (sha256.len() == 64
                    && sha256
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))),
            "{arch}: {sha256:?} is neither empty nor a lowercase SHA-256"
        );
    }
}

#[test]
fn a_pinned_build_is_published_on_the_cdn_and_held_to_developer_id() {
    let artifact = select("arm64", false, Some(&pin(SHA)), vars(&[]))
        .unwrap()
        .unwrap();
    assert_eq!(artifact.signature, SignaturePolicy::DeveloperId);
    assert_eq!(
        artifact.url(),
        "https://boot.arcboxcdn.com/qemu/v0.1.0/qemu-darwin-arm64-v0.1.0.tar.gz"
    );
    // An unpublished pin or another architecture names no build.
    assert_eq!(
        select("arm64", false, Some(&pin("")), vars(&[])).unwrap(),
        None
    );
    assert_eq!(
        select("x86_64", false, Some(&pin(SHA)), vars(&[])).unwrap(),
        None
    );
}

#[test]
fn only_a_development_daemon_honours_the_override() {
    let overridden = vars(&[
        (env::QEMU_VERSION, "0.0.0-dev.0169f2b"),
        (env::QEMU_SHA256, &SHA.to_ascii_uppercase()),
        (env::QEMU_CDN, "http://127.0.0.1:8000/qemu/"),
    ]);
    let artifact = select("arm64", true, Some(&pin("")), &overridden)
        .unwrap()
        .unwrap();
    assert_eq!(artifact.signature, SignaturePolicy::Development);
    assert_eq!(artifact.sha256, SHA);
    assert_eq!(
        artifact.url(),
        "http://127.0.0.1:8000/qemu/v0.0.0-dev.0169f2b/qemu-darwin-arm64-v0.0.0-dev.0169f2b.tar.gz"
    );

    // Production keeps the pin and its signature policy.
    let artifact = select("arm64", false, Some(&pin(SHA)), &overridden)
        .unwrap()
        .unwrap();
    assert_eq!(artifact.version, "0.1.0");
    assert_eq!(artifact.signature, SignaturePolicy::DeveloperId);

    let incomplete = vars(&[(env::QEMU_VERSION, "0.0.0-dev.0169f2b")]);
    assert!(select("arm64", true, Some(&pin(SHA)), incomplete).is_err());
    let malformed = vars(&[(env::QEMU_VERSION, "0.0.0"), (env::QEMU_SHA256, "abc")]);
    assert!(select("arm64", true, Some(&pin(SHA)), malformed).is_err());
}
