# ADR 0006: Authorize local CA trust in the login user's session

- Status: accepted (2026-10-08)
- Deciders: ArcBox maintainers
- Supersedes: trust mutation and automatic authorization in [ADR 0005](0005-helper-manages-local-ca-trust.md)
- Evidence: [Native root validation failure](../logs/2026-10-08-helper-ca-trust.md#live-validation-deployment-blocked)

## Context

The root helper cannot silently authorize certificate trust on SIP-enabled macOS. The native lifecycle test failed with `session has no ui access`. Apple documents this requirement from macOS Big Sur onward. Helper installation and certificate trust require separate authorization.

## Decision

1. The daemon creates and uses the private Keychain identity. Desktop and the CLI read only its public certificate.
2. The helper stores, inspects, and removes exact public certificates in the System Keychain. Removal requires prior Admin trust revocation.
3. The login user's process sets or removes trust with Security.framework. Desktop invokes the bundled CLI directly so macOS can display native authorization.
4. Production startup publishes `AWAITING_HTTPS_TRUST` and waits without booting a VM. Correct existing trust passes without a mutation. Cancellation leaves the daemon waiting and Desktop offers explicit retry.
5. Migration removes legacy User trust even when the certificate is unchanged, because User settings override Admin policy. Startup requires its absence and precise Admin TLS trust. Rotation revokes the previous certificate's Admin and User trust before publishing a replacement. Uninstall revokes trust before deleting identities or removing the helper. Failures preserve the resources needed for retry.

## Consequences

First-time trust, trust removal, and CA rotation can require native authentication. A restart with the same trusted CA does not require another prompt. Development and isolated instances retain explicit trust setup. The CLI reports canceled authorization with exit status 130.

## Alternatives considered

- Background root trust mutation: fails the platform's required user authorization.
- Changing the system authorization database: weakens host security and is unnecessary for the supported GUI flow.
