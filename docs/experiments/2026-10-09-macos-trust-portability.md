# 2026-10-09 — Can a Linux CA bundle preserve macOS SSL trust?

- Type: experiment (documentation and source audit)
- Area: local HTTPS and machine client trust
- Outcome: ordinary PEM synchronization cannot preserve all native trust settings; broader integration remains open
- Probes: inspect the official API, source, and store contracts linked below; no executable probe
- Host / guest: no native trust query, trust mutation, or guest execution

## Question

Can ArcBox make certificates trusted for SSL in macOS work inside distro machines by copying their public certificates into the existing Linux system CA bundle?

## Hypotheses

1. Selecting SSL-trusted certificates and copying their PEM preserves native trust decisions.
2. Excluding roots with a Deny setting prevents the copy from losing native distrust decisions.

## Method

Inspect OrbStack's product contract, Apple's public trust-settings APIs and Security source, and the Linux consumer contracts. Compare the information each interface retains. This audit establishes representational limits; it does not measure installed product behavior or infer OrbStack's implementation.

## Results

| Contract | Evidence | Consequence |
|---|---|---|
| OrbStack machine trust | [Machine networking](https://docs.orbstack.dev/machines/network#self-signed-certificates) promises that certificates marked as SSL-trusted in macOS work in machines. | This requirement is separate from delivery of OrbStack's own CA. The documentation does not specify its synchronization or policy implementation. |
| Native trust settings | [CopyTrustSettings](https://developer.apple.com/documentation/security/sectrustsettingscopytrustsettings%28_%3A_%3A_%3A%29?language=objc) exposes policy, application, hostname, key usage, allowed errors, and trust results. A successful empty array differs from absent settings. | Certificate DER alone does not contain these settings. |
| Chain distrust | [Apple's chain evaluator](https://github.com/apple-oss-distributions/Security/blob/main/trust/trustd/SecPolicyServer.c#L3154) checks usage constraints on certificates throughout the chain. | A leaf or intermediate Deny can reject a chain whose root has no Deny. |
| Orphan settings | [Certificate enumeration](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/SecTrustSettings.cpp#L851) depends on visible certificates. The [security manual](https://github.com/apple-oss-distributions/Security/blob/main/SecurityTool/macOS/security.1#L931) separates certificate removal from trust-setting removal. | Enumerating certificates cannot prove that all distrust records were examined. |
| Complete settings export | [CreateExternalRepresentation](https://developer.apple.com/documentation/security/sectrustsettingscreateexternalrepresentation%28_%3A_%3A%29?language=objc) exports settings. Its [current schema](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/TrustSettingsSchema.h#L29) appears in implementation source. | Parsing that schema introduces an implementation dependency; the public API does not guarantee the XML layout. |
| Caller identity | [trustd](https://github.com/apple-oss-distributions/Security/blob/main/trust/trustd/trustd.c#L559) obtains the caller's effective UID. | The user daemon must read that user's settings. A root helper reads root's User domain. |
| Ordinary distro bundles | [Debian's updater](https://manpages.debian.org/testing/ca-certificates/update-ca-certificates.8.en.html) assembles PEM certificates. [OpenSSL](https://docs.openssl.org/3.5/man1/openssl-x509/#trust-settings) distinguishes ordinary trusted roots from purpose-constrained auxiliary trust. | Copying an SSL-only root as ordinary PEM can broaden its permitted uses. |
| Consumer-specific policy | [p11-kit](https://p11-glue.github.io/p11-glue/p11-kit/manual/trust.html) and OpenSSL AUX represent some purposes and rejection rules. [Go's PEM loader](https://go.dev/src/crypto/x509/cert_pool.go) accepts `CERTIFICATE` blocks, not `TRUSTED CERTIFICATE`. | One purpose-aware export format does not cover every Linux client. |

## Findings

Both hypotheses are rejected. Ordinary PEM loses native restrictions, and checking only a root's settings misses rejection of other certificates in its chain. User, Admin, and System domain precedence also prevents treating trust settings as a union of grants. Unknown higher-priority constraints cannot safely fall through to a lower-priority grant.

ArcBox's current authenticated root delivery has a narrower contract. `PublicAuthority` validates the ArcBox identity and certificate scope. Machine delivery retains its captured VM, cancels pending requests, and joins workers before releasing host holds. These mechanisms can carry future public-root snapshots, but they do not decide which native trust settings are portable.

## Decisions taken / open

- Keep ArcBox-root delivery separate from broader macOS trust synchronization. Do not relax `PublicAuthority` to accept arbitrary roots.
- A restricted custom-root subset would require complete settings inspection, explicit exclusion reasons, an independently owned guest collection, and confirmation after installation and withdrawal. Such a subset would not complete the macOS SSL trust requirement.
- A complete design must resolve purpose, application, hostname, and chain distrust semantics across supported consumers. Ordinary CA-file synchronization does not resolve those semantics.
- Keep the [acceptance matrix](../local-https.md#completion-requirements) incomplete for this feature. No native behavior or OrbStack implementation was validated by this audit.
