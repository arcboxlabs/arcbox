# Custom local domains and deterministic ownership

- Type: change
- Area: local CA and shared DNS policy
- Related: [ADR 0008](../adr/0008-local-ca-covers-local-domains.md), [acceptance matrix](../local-https.md)

## Problem

The CA only covered `arcbox.local`. Host registration selected the latest owner, while guest removal selected an arbitrary surviving owner. That difference cannot safely support exact aliases, default implicit wildcards, and explicit custom wildcards.

## CA scope

New roots permit `.local` descendants and reject IP addresses and subordinate CAs. Root parsing retains the actual scope, including existing `arcbox.local` roots needed for inspection and exact revocation. Signing checks that scope before producing a leaf. Each CSR still contains one concrete DNS name.

No production CA was rotated or trusted during implementation. Custom aliases remain incomplete until shared matching, host and guest registration, resolver ownership, and browser acceptance are connected.

The default CA and constants suites passed 58 tests; 11 native CA tests remained ignored. The CLI, core, daemon, and helper library/binary suites passed another 452 tests; one native helper test remained ignored. Strict all-target Clippy passed for the CA and constants crates. These tests verify CSR rejection, the loaded root's actual scope, leaf verification, and existing host consumers without native trust changes.

The CA crate also passed strict all-target Clippy for `aarch64-unknown-linux-musl`. That check exposed the journal cleanup method in Linux test builds, although all its callers are macOS-only. The method now has the same platform condition as its callers. Workspace formatting passed. Linux execution and native `.local` trust acceptance remain unverified.

## Shared name patterns

`arcbox-dns::NamePattern` reuses `Domain` validation. Custom input accepts a concrete `.local` name or a leading `*.` and normalizes case and outer whitespace. Exact patterns match only one name. Wildcards match descendants at any depth but exclude the apex. Selection ranks exact names before wildcards, then prefers the most specific wildcard base. The parser rejects wildcard patterns too long to have a valid concrete descendant.

The DNS crate passed 17 unit and integration tests, including five name-pattern regressions. Strict all-target Clippy, workspace formatting, and `cargo xtask check-layers` passed. The dependency change adds only the existing workspace constants crate. This first slice does not change runtime registration or system DNS.

## Shared claim selection

`claims::resolve` selects exact names before the most specific wildcard. Independent owners at the winning specificity produce a conflict without falling back to another wildcard. Duplicate claims from the same owner and authority are idempotent. Generated Compose service aliases may share a name within one project, service, and authority; selection uses the smallest complete container ID. Custom aliases remain exclusive, including on Compose containers. The caller must obtain the selected owner's address and revision from the same registry snapshot.

The DNS crate passed 24 tests, including seven ownership regressions. Strict all-target Clippy passed for the host and `aarch64-unknown-linux-musl`. Namespace reservations, cross-instance delegation, and runtime registration remain separate unfinished slices. These checks did not change native trust or system DNS.
