use std::collections::HashMap;
use std::sync::Mutex;

use sha2::Digest as _;

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

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Option<String>)>>,
}

impl signature::Verify for Recorder {
    fn verify(&self, path: &Path, requirement: Option<&str>) -> std::result::Result<(), String> {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        self.calls
            .lock()
            .unwrap()
            .push((name, requirement.map(str::to_owned)));
        if std::fs::read(path).unwrap().starts_with(b"unsigned") {
            return Err("code object is not signed at all".into());
        }
        Ok(())
    }
}

type Entries = Vec<(&'static str, Vec<u8>)>;

fn entries(version: &str) -> Entries {
    let info = format!(
        r#"{{"version":"{version}","arch":"arm64","executable":"bin/qemu-system-aarch64"}}"#
    );
    vec![
        ("BUILD-INFO.json", info.into_bytes()),
        ("bin/qemu-system-aarch64", b"qemu".to_vec()),
        ("lib/libglib-2.0.0.dylib", b"glib".to_vec()),
        ("licenses/qemu/COPYING", b"GPL-2.0".to_vec()),
    ]
}

/// Writes a `.tar.gz` of `entries`, plus a symlink when `link` is set, and
/// returns its SHA-256.
fn write_archive(path: &Path, entries: &Entries, link: Option<(&str, &str)>) -> String {
    let file = std::fs::File::create(path).unwrap();
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::fast(),
    ));
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(if name.starts_with("bin/") {
            0o755
        } else {
            0o644
        });
        builder
            .append_data(&mut header, name, data.as_slice())
            .unwrap();
    }
    if let Some((name, target)) = link {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, name, target).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut std::fs::File::open(path).unwrap(), &mut hasher).unwrap();
    format!("{:x}", hasher.finalize())
}

fn runtime(root: &Path, sha256: &str) -> (QemuRuntime, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let artifact = QemuArtifact {
        version: "0.1.0".to_owned(),
        arch: "arm64".to_owned(),
        sha256: sha256.to_owned(),
        cdn: "http://127.0.0.1:9/qemu".to_owned(),
        signature: SignaturePolicy::DeveloperId,
    };
    let runtime = QemuRuntime {
        root: root.to_owned(),
        artifact,
        verifier: recorder.clone(),
    };
    (runtime, recorder)
}

fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn import_installs_the_pinned_archive_with_its_licenses() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
    let sha256 = write_archive(&archive, &entries("0.1.0"), None);
    let (runtime, recorder) = runtime(&root, &sha256);

    let executable = runtime.import(&archive).await.unwrap();
    assert_eq!(executable, root.join("0.1.0").join(QEMU_EXECUTABLE));
    assert_eq!(runtime.installed_executable(), Some(executable));
    assert_eq!(
        std::fs::read(root.join("0.1.0/licenses/qemu/COPYING")).unwrap(),
        b"GPL-2.0"
    );

    let mut calls = recorder.calls.lock().unwrap().clone();
    calls.sort();
    let [(lib, lib_requirement), (exe, exe_requirement)] = calls.as_slice() else {
        panic!("expected two signature checks, got {calls:?}");
    };
    assert_eq!(
        (lib.as_str(), exe.as_str()),
        ("libglib-2.0.0.dylib", "qemu-system-aarch64")
    );
    let (lib_requirement, exe_requirement) = (
        lib_requirement.as_deref().unwrap(),
        exe_requirement.as_deref().unwrap(),
    );
    assert!(lib_requirement.contains("\"422ACSY6Y5\"") && !lib_requirement.contains("identifier"));
    assert!(
        exe_requirement.contains("\"422ACSY6Y5\"")
            && exe_requirement.ends_with("identifier \"com.arcboxlabs.qemu.system-aarch64\"")
    );

    // Importing again replaces the build in place.
    runtime.import(&archive).await.unwrap();
    assert_eq!(names(&root), [".lock", "0.1.0"]);
}

#[test]
fn development_builds_relax_the_certificate_but_not_the_identifier() {
    assert_eq!(
        SignaturePolicy::Development.requirement(true).as_deref(),
        Some("identifier \"com.arcboxlabs.qemu.system-aarch64\"")
    );
    assert_eq!(SignaturePolicy::Development.requirement(false), None);
}

#[tokio::test]
async fn an_archive_that_does_not_match_the_pin_is_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
    write_archive(&archive, &entries("0.1.0"), None);
    let (runtime, recorder) = runtime(&root, SHA);

    let error = runtime.import(&archive).await.unwrap_err();
    assert!(
        matches!(
            error,
            ImageError::Asset(arcbox_asset::AssetError::ChecksumMismatch { .. })
        ),
        "{error}"
    );
    assert_eq!(names(&root), [".lock"]);
    assert!(recorder.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_build_failing_a_check_leaves_nothing_installed() {
    let mut unsigned = entries("0.1.0");
    unsigned[2].1 = b"unsigned glib".to_vec();
    type Link = Option<(&'static str, &'static str)>;
    let cases: [(&str, Entries, Link, &str); 3] = [
        (
            "unsigned library",
            unsigned,
            None,
            "lib/libglib-2.0.0.dylib fails its code signature check: \
             code object is not signed at all",
        ),
        (
            "another version",
            entries("0.0.9"),
            None,
            "the archive is 0.0.9 arm64",
        ),
        (
            "symlink",
            entries("0.1.0"),
            Some(("lib/libz.dylib", "/usr/lib/libz.dylib")),
            "archive entry lib/libz.dylib is not a file or directory",
        ),
    ];
    for (case, entries, link, reason) in cases {
        let dir = tempfile::tempdir().unwrap();
        let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
        let sha256 = write_archive(&archive, &entries, link);
        let (runtime, _) = runtime(&root, &sha256);

        let error = runtime.import(&archive).await.expect_err(case).to_string();
        assert!(
            error.starts_with(&format!("rejected QEMU build: {reason}")),
            "{case}: {error}"
        );
        assert_eq!(runtime.installed_executable(), None, "{case}");
        assert_eq!(names(&root), [".lock"], "{case} left files behind");
    }
}

/// A malformed requirement fails every install the same way a foreign
/// signer does, so check that `codesign` parses ours and rejects only the
/// signer.
#[cfg(target_os = "macos")]
#[test]
fn codesign_parses_the_developer_id_requirement_and_rejects_other_signers() {
    use signature::Verify as _;
    let apple_signed = Path::new("/bin/ls");
    signature::Codesign.verify(apple_signed, None).unwrap();
    for executable in [true, false] {
        let requirement = SignaturePolicy::DeveloperId
            .requirement(executable)
            .unwrap();
        let error = signature::Codesign
            .verify(apple_signed, Some(&requirement))
            .unwrap_err();
        assert!(error.contains("failed to satisfy"), "{error}");
    }
}
