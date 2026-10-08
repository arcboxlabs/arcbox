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

## Invalid container listings preserve host state

The shared host query previously converted malformed JSON into an empty container set. The networking reconciler could then remove every live container's DNS and port state. Listing parsing now rejects malformed JSON, non-array responses, and missing, non-string, or empty IDs. A mixed valid/invalid list also fails instead of publishing a partial snapshot. Only a valid empty array means no running containers.

All 131 Docker library tests and seven handler mock tests passed. The mock suite includes an HTTP 200 response with malformed JSON through the actual pooled guest client. Existing lifecycle fixtures now provide the full inspect fields required by CA injection and canonical-ID requests; response and network-state assertions remain intact. Strict all-target Clippy passed for the Docker crate. This repair does not yet refresh known IDs or add revision-conditional removal.

## Namespace reservations and delegation

The matcher now accepts trusted boundaries alongside claims. An exact reservation protects only its name; a tree reservation includes its root and descendants. A claim must use the reservation's authority and stay within that reservation's pattern scope. A missing resource route remains reserved instead of falling back to an ancestor wildcard, including one from the same authority. Delegated subtrees always exclude local claims. Equally specific incompatible boundaries produce a conflict.

Nine boundary regressions passed, including order independence, duplicate boundaries, nested delegation, and same-authority wildcard exclusion. Strict all-target Clippy passed on the host and Linux musl target. These pure boundaries do not acquire macOS resolver leases or authorize a resource to publish them; runtime and helper integration remain unfinished.

## Shared inspect metadata

`ContainerMetadata` reads complete container identity, default aliases, and IPv4 addresses through one JSON parser for host and guest consumers. It preserves the original alias spelling, including underscores, for validation at the later registration boundary. IPv4 selection uses network-name order rather than JSON field order, deduplicates addresses, and never reads the deprecated top-level `NetworkSettings.IPAddress`. A container without an address retains its identity so callers can remove stale registration.

Six metadata regressions passed within the DNS crate's 39 passing tests. Strict all-target Clippy passed for both host and Linux musl; workspace formatting and layer checks passed. The dependency change reuses workspace `serde_json` without changing package versions. Host and guest consumers still need to adopt this parser. Docker's [API 1.44 schema](https://github.com/moby/moby/blob/v25.0.0/api/swagger.yaml#L1432-L1481) identifies the top-level address as a deprecated default-bridge mirror and provides all network addresses under `Networks`.

## Subscribe before taking the guest snapshot

The guest now confirms a successful Docker event subscription and complete response headers before listing running containers. It retains the reader and consumes events buffered during that listing. Subscription or listing errors propagate to the existing reconnect loop. The guest GET helper rejects unsuccessful HTTP status codes, including HTTP 500 with an otherwise valid empty JSON array.

Five host-executed socket and event tests passed. Strict all-target Clippy passed for the Linux musl guest target. The socket tests exercise the production ordering function without a VM, iptables, or scheduler-dependent worker cancellation. They verify that an incomplete subscription does not start the listing, buffered events survive the listing, and failed listings return while the event stream stays open. Removing owners that disappeared during a disconnected interval still requires the full-ID registry work. VM execution remains unverified.

## Host metadata adoption

All host Docker registration paths now use `ContainerMetadata` through the existing inspect adapter. Start, restart, rename, network changes, recovery, and reconciliation share the same alias and primary-address rules. The adapter no longer reads the deprecated top-level address, including when that field disagrees with the current network addresses.

All 133 Docker library tests and seven API mock tests passed. Strict all-target Clippy, workspace formatting, and the layer check passed; the layer check covers 65 members and 190 edges with two existing exceptions. Regression coverage rejects a top-level-only address and verifies stable selection with multiple networks and invalid or IPv6-only entries. The dependency change adds only the existing workspace DNS crate. Full-ID replacement and production claim selection remain unfinished.
