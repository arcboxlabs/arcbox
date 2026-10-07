# ADR 0005: The privileged helper manages local CA trust

- Status: accepted (2026-10-08)
- Deciders: ArcBox maintainers
- Supersedes: the manual user-trust requirement in decision 5 of [ADR 0004](0004-local-ca-signs-in-macos-keychain.md)
- Evidence: Apple Security.framework `SecTrustSettings.h`; `app/arcbox-helper/src/server/mutations/ca.rs`

## Context

The Keychain migration protects the signing key, but requiring `abctl tls trust` leaves HTTPS setup incomplete. macOS requires authentication when changing User-domain trust. A root process can change Admin-domain trust without another authentication prompt.

## Decision

1. The existing signed root helper installs, inspects, and removes the exact public CA certificate in the System Keychain and Admin trust domain. The private key remains in the user's default Keychain.
2. The helper accepts certificate bytes, never caller-selected file paths. The helper verifies the self-signature, ArcBox subject, CA path length, signing usage, and critical DNS/IP constraints. Trust permits TLS servers only.
3. The canonical production daemon completes trust setup before VM boot and READY. Development and isolated instances require explicit `abctl tls trust`.
4. CA rotation revokes the old root before replacing its published certificate. Uninstall revokes trust before deleting the identity or removing the helper. Failed revocation preserves the resources required for retry.
5. Helper installation and upgrades retain the existing administrator authorization flow. Existing User-domain trust can remain until explicit untrust or uninstall; removing that legacy trust can still require user authentication.

## Consequences

Admin trust applies to every account on the Mac. Its certificate constraints limit signing authority to ArcBox DNS names. Each profile retains a separate key and certificate; removal compares the full certificate and preserves other identities.

`abctl tls untrust` revokes current trust. The next production startup restores Admin trust. This decision adds no permanent HTTPS-disable setting.
