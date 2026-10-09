//! Native signing ACL acceptance without adding or removing certificate trust.

use std::process::Command;

use super::*;
use crate::CertificateRequest;

const PROBE: &str = "keychain::tests::signing_identity::native_identity_probe";
const PROFILE: &str = "ARCBOX_CA_TEST_PROFILE";
const ACTION: &str = "ARCBOX_CA_TEST_ACTION";

fn run(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("{command:?}: {}\n{stdout}{stderr}", output.status));
    }
    eprint!("{stdout}{stderr}");
    Ok(stdout.into_owned())
}

fn probe(binary: &Path, profile: &Path, action: &str) -> Result<(), String> {
    let stdout = run(Command::new(binary)
        .args([
            "--exact",
            PROBE,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROFILE, profile)
        .env(ACTION, action))?;
    // The test harness also succeeds when an exact filter matches no tests.
    if !stdout.contains("test result: ok. 1 passed;") {
        return Err("the child probe did not run exactly one successful test".into());
    }
    Ok(())
}

#[test]
#[ignore = "requires ARCBOX_CA_TEST_SIGN_IDENTITY; signs temporary binaries and creates an untrusted Keychain CA"]
fn native_signing_acl_rejects_other_code_identities() {
    let identity = std::env::var("ARCBOX_CA_TEST_SIGN_IDENTITY")
        .expect("set ARCBOX_CA_TEST_SIGN_IDENTITY to a Developer ID Application identity");
    assert_ne!(
        identity, "-",
        "the creator must have a Developer ID identity"
    );
    let temporary = tempfile::Builder::new()
        .prefix("arcbox-ca-signing-identity-")
        .tempdir()
        .unwrap();
    let creator = temporary.path().join("creator");
    let relocated = temporary.path().join("relocated");
    let different = temporary.path().join("different-identifier");
    let adhoc = temporary.path().join("adhoc");
    let executable = std::env::current_exe().unwrap();
    for (binary, signer, identifier) in [
        (&creator, identity.as_str(), "dev.arcbox.ca-acl-test"),
        (
            &different,
            identity.as_str(),
            "dev.arcbox.ca-acl-test.other",
        ),
        (&adhoc, "-", "dev.arcbox.ca-acl-test"),
    ] {
        std::fs::copy(&executable, binary).unwrap();
        run(Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--timestamp=none",
                "--sign",
                signer,
                "--identifier",
                identifier,
            ])
            .arg(binary))
        .unwrap();
        run(Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(binary))
        .unwrap();
    }
    std::fs::copy(&creator, &relocated).unwrap();

    // Keep the creator and journal available if exact identity cleanup fails.
    let directory = temporary.keep();
    let profile = directory.join("profile");
    std::fs::create_dir(&profile).unwrap();
    eprintln!("Isolated untrusted CA profile: {}", profile.display());
    let result = (|| {
        probe(&creator, &profile, "create")?;
        probe(&creator, &profile, "allow")?;
        probe(&relocated, &profile, "allow")?;
        probe(&different, &profile, "deny")?;
        probe(&adhoc, &profile, "deny")?;
        probe(&creator, &profile, "allow")
    })();
    let cleanup = probe(&creator, &profile, "cleanup");
    if cleanup.is_ok() {
        std::fs::remove_dir_all(&directory).unwrap();
    } else {
        eprintln!(
            "Retained creator and recovery records: {}",
            directory.display()
        );
        eprintln!(
            "Recovery: {PROFILE}={profile:?} {ACTION}=cleanup {creator:?} --exact {PROBE} --ignored --nocapture --test-threads=1"
        );
    }
    assert!(
        result.is_ok() && cleanup.is_ok(),
        "matrix: {result:?}; cleanup: {cleanup:?}"
    );
}

#[test]
#[ignore = "child probe for native_signing_acl_rejects_other_code_identities"]
fn native_identity_probe() {
    let profile =
        std::path::PathBuf::from(std::env::var_os(PROFILE).expect("isolated test profile"));
    let action = std::env::var(ACTION).expect("test action");
    let _no_ui = SecKeychain::disable_user_interaction().unwrap();
    assert!(!SecKeychain::user_interaction_allowed().unwrap());
    let _operation = crate::lock_profile(&profile).unwrap();
    if action == "cleanup" {
        let roots = KeychainAuthority::owned_certificates(&profile).unwrap();
        let public_keys: Vec<_> = roots
            .iter()
            .map(|root| {
                assert!(!root.has_trust().unwrap());
                let key = SecCertificate::from_der(root.der())
                    .unwrap()
                    .public_key()
                    .unwrap();
                (
                    public_key_hash(&key).unwrap(),
                    key.external_representation().unwrap(),
                )
            })
            .collect();
        KeychainAuthority::delete(&profile).unwrap();
        for (hash, bytes) in public_keys {
            assert!(
                certificate_by_public_key(&hash, bytes.bytes())
                    .unwrap()
                    .is_none()
            );
        }
        for slot in [Slot::Primary, Slot::Replacement] {
            assert!(!KeychainAuthority::slot_has_keys(&profile, slot).unwrap());
        }
        assert!(Journal::load(&profile).unwrap().is_none());
        eprintln!("Exact CA certificate, keys, and journal removed");
        return;
    }

    assert!(matches!(action.as_str(), "create" | "allow" | "deny"));
    let der = if action == "create" {
        assert!(
            KeychainAuthority::existing_certificate(&profile)
                .unwrap()
                .is_none()
        );
        let authority = KeychainAuthority::open(&profile).unwrap();
        let der = authority.certificate.to_der();
        std::fs::write(profile.join("recovery.der"), &der).unwrap();
        der
    } else {
        let der = std::fs::read(profile.join("recovery.der")).unwrap();
        assert_eq!(Journal::load(&profile).unwrap().unwrap().current().der, der);
        der
    };
    let public = PublicAuthority::try_from(der).unwrap();
    assert!(!public.has_trust().unwrap());
    let key = SecCertificate::from_der(public.der())
        .unwrap()
        .public_key()
        .unwrap();
    let recovered_public_hash = public_key_hash(&key).unwrap();
    let stored = certificate_by_public_key(
        &recovered_public_hash,
        key.external_representation().unwrap().bytes(),
    )
    .unwrap()
    .expect("the exact public certificate must still exist");
    assert_eq!(stored.to_der(), public.der());
    if action == "create" {
        eprintln!("Untrusted root SHA-256: {}", public.fingerprint_sha256());
    }
    let pending = CertificateRequest::new("identity-test.arcbox.local").unwrap();
    let csr = VerifiedCertificateRequest::try_from(pending.der()).unwrap();
    let signed = KeychainAuthority::open(&profile).and_then(|authority| authority.sign(&csr));
    if action == "deny" {
        // rcgen erases the platform error; classify the same key operation directly.
        let key = find_key(&KeychainAuthority::label(&profile).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            public_key_hash(&key).unwrap().bytes(),
            recovered_public_hash.bytes()
        );
        let error = key
            .create_signature(
                Algorithm::ECDSASignatureMessageX962SHA256,
                b"identity boundary",
            )
            .expect_err("another code identity must not sign without authorization");
        eprintln!(
            "Denied signing: domain={}, code={}",
            error.domain(),
            error.code()
        );
        assert_eq!(error.domain().to_string(), "NSOSStatusErrorDomain");
        assert!(matches!(error.code(), -25293 | -25308), "{error}");
        // macOS can deny an ad-hoc caller before signing, at SecKeyCopyPublicKey.
        let production_error = signed.expect_err("another code identity must not issue a leaf");
        eprintln!("Denied production CA access: {production_error}");
        assert!(
            matches!(
                production_error,
                Error::Certificate(rcgen::Error::RemoteKeyError)
                    | Error::InvalidAuthority("missing EC public key")
            ),
            "{production_error:?}"
        );
    } else {
        pending.complete(signed.unwrap(), &public).unwrap();
        eprintln!("Signed and verified a leaf for the exact root and requested name");
    }
    assert!(!SecKeychain::user_interaction_allowed().unwrap());
}
