# ADR 0004: The macOS Keychain owns the local CA key

- Status: accepted (2026-10-07)
- Deciders: ArcBox maintainers
- Evidence: `common/arcbox-local-ca/src/authority.rs`, `guest/arcbox-agent/src/domains/https.rs`

## Context

The local CA private key was stored in `tls/ca-key.pem` under the data directory. The System VM mounts that directory at `/arcbox`. A guest administrator could copy the CA key and retain signing authority after losing access to the VM.

## Decision

1. The daemon creates a new, per-profile EC CA key in the macOS file Keychain. The application must not export the private key.
2. The guest generates each leaf key in memory and sends a signed CSR over a host-initiated vsock connection. The host verifies the CSR and signs only one DNS name below `arcbox.local`, with server authentication usage and a 30-day lifetime.
3. The CA retains its DNS name constraints, excludes IP addresses, and permits no subordinate CA.
4. TLS termination and container port routing remain in the guest. Port 443 does not require publishing a host port.
5. Migration creates a new CA instead of importing the exposed key. Migration removes the old shared key and retains the old public certificate until its trust is revoked. The new CA requires user trust.
6. Keychain errors propagate. The daemon must not substitute a file-backed CA key.

## Consequences

The Keychain ACL controls access to the signing key. This design does not promise Secure Enclave storage or protection from a compromised host account. A compromised guest can request certificates for ArcBox names while connected, but cannot copy the root key through the shared directory.

## Alternatives considered

- Move the existing PEM into the Keychain: previous copies would remain valid signing keys.
- Move TLS termination to the host: changes the direct container routing path without improving key isolation over host-side CSR signing.
- Require Secure Enclave keys: adds hardware and entitlement requirements beyond Keychain storage.
