# 2026-10-09 — Does the Keychain CA reject other application identities?

- Type: experiment
- Area: `arcbox-local-ca` signing ACL
- Outcome: the isolated native identity matrix passes
- Probes: `common/arcbox-local-ca/src/keychain/tests/signing_identity.rs`
- Host / guest: Apple M5 Max, arm64, macOS 26.4 (25E246); no guest

## Question

Does the default file-Keychain ACL permit the signed creator to issue certificates while rejecting different application identities without authorization?

## Hypotheses

1. The creator and a copy of the same signed executable at another path can sign valid leaves.
2. The same Developer ID certificate with a different application identifier cannot sign without authorization.
3. An ad-hoc signature with the creator's application identifier cannot sign without authorization.

## Method

The ignored parent test signs temporary copies of its executable with `codesign --timestamp=none`. The creator uses `dev.arcbox.ca-acl-test`; the second Developer ID fixture uses `dev.arcbox.ca-acl-test.other`. The ad-hoc fixture retains the creator's identifier. The Developer ID identity was `Developer ID Application: ArcBox, Inc. (422ACSY6Y5)`.

Seven separate child processes create one isolated CA, sign as the creator, sign from the relocated copy, attempt both negative identities, sign as the creator again, and delete the identity. Every child disables Keychain user interaction before CA access and holds the profile operation lock. The external `codesign` process uses the existing signing identity's access controls; the child guard does not control that process.

Each negative probe requires the recovery DER, journal, and stored public certificate to agree. The private-key reference's application-label hash must match the recovered certificate's public-key hash. The production CA operation must fail, and a direct `SecKeyCreateSignature` call must report `NSOSStatusErrorDomain` with `-25293` or `-25308`. A missing item, different error, or zero matching child tests fails acceptance.

The test never installs or revokes certificate trust and never exports root private bytes. Cleanup runs through the original creator even after a probe fails. Cleanup verifies that the exact certificate, both key classes, and the journal are absent. A cleanup failure retains the creator and recovery records.

Rerun from the runtime checkout with a configured Developer ID Application identity:

```sh
ARCBOX_CA_TEST_SIGN_IDENTITY='<Developer ID Application identity>' devenv shell -- cargo test -p arcbox-local-ca --lib keychain::tests::signing_identity::native_signing_acl_rejects_other_code_identities -- --exact --ignored --nocapture --test-threads=1
```

## Results

| Probe | Observed result |
| --- | --- |
| Creator creation and initial signing | Valid leaf; chain, requested name, and leaf key verified |
| Identical signed executable at another path | Valid leaf |
| Same Developer ID, different identifier | Production signing returned `RemoteKeyError`; native signing returned `NSOSStatusErrorDomain / -25293` |
| Ad-hoc signature, same identifier | Production access failed at `SecKeyCopyPublicKey`; native signing returned `NSOSStatusErrorDomain / -25293` |
| Creator after both negative probes | Valid leaf |
| Exact identity cleanup | Certificate, public key, private key, and journal removed; temporary directory removed |

The completed matrix ran once with all seven child probes passing. An earlier run exposed an incorrect test assumption: an ad-hoc process cannot necessarily open the CA before signing. That run also cleaned its isolated identity. The corrected probe verifies certificate identity independently of private-key access and retains the mandatory native signing denial.

The crate's 36 regular tests passed; 13 native or child tests remained ignored in that run. Scoped formatting, strict Clippy for all crate targets, and compilation passed. The exact ignored identity test then passed separately.

## Decisions taken / open

The observed ACL behavior needs no production code change. The [local HTTPS matrix](../local-https.md) records this evidence separately from installed-product acceptance.

This experiment covers the two negative identities above. It does not test a changed executable during an installed daemon upgrade, root-key export ACLs, or the browser authorization flow.
