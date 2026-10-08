# ADR 0008: The local CA covers concrete names under `.local`

- Status: accepted (2026-10-08)
- Deciders: ArcBox maintainers
- Evidence: [Custom local domains](../logs/2026-10-08-custom-local-domains.md)
- Supersedes: the signing namespace in ADR 0004, decisions 2 and 3

## Context

Independent aliases such as `foo.local` and `bar.local` cannot use a root constrained to `arcbox.local`. A configurable instance suffix alone does not provide OrbStack's custom-domain behavior.

## Decision

1. New roots permit the critical DNS subtree `.local`, exclude every IPv4 and IPv6 address, and retain path length zero.
2. Each CSR contains one concrete DNS name. The signer rejects wildcard SANs and names outside the loaded root's actual scope.
3. The host must authorize current domain ownership before and after signing. A CA constraint does not establish ownership.
4. Existing roots remain unchanged. Exact inspection, revocation, and rotation must retain their recorded public certificate and actual scope.
5. Expanding an existing root's scope requires explicit root rotation and macOS trust authorization. Background preparation must not change trust.

## Consequences

A compromised new root key can impersonate other `.local` services, including LAN services. The Keychain boundary and host ownership checks remain necessary. Name constraints continue to exclude public DNS and IP addresses; they do not isolate ArcBox names from every local network service.

The broader CA constraint does not authorize installing a resolver for all `.local`. Custom aliases still need precise resolver ownership, conflict handling, and matching rules.

## Alternatives considered

- Keep `arcbox.local` only: independent custom aliases cannot receive valid certificates.
- Create a root for each alias: each new name requires another native trust operation and identity lifecycle.
- Remove DNS constraints: grants unnecessary authority over public DNS names.
