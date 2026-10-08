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

## Guest metadata adoption

Guest DNS, web-service facts, and published-port mirrors now consume the same metadata parser as the host. DNS and discovery select one primary address; domain NAT retains all distinct addresses. Each pinned published binding produces one mirror target at the shared primary address. Duplicate guest address parsers are removed. Existing lowercase alias keys and underscore spelling remain unchanged pending canonical claim registration.

All 18 targeted host-executed tests passed: eight facts tests, six event tests, and four publish-mirror tests. Strict Linux musl all-target Clippy and workspace formatting passed. These tests cover shared primary-address selection, all-address retention, alias spelling, and one target per published binding. The new guest behavior has not run inside a VM.

## Canonical container claims

The shared metadata parser reads `dev.arcbox.domains` as comma-separated exact or explicit wildcard `.local` names. Claim generation canonicalizes generated names under the caller's suffix and uses the complete container ID. Generated names include descendant wildcards when the DNS length limit permits a descendant. Custom exact names do not gain implicit wildcards, and custom names remain exclusive across Compose replicas. One-off Compose containers claim only their unique container name.

Each rejected name produces a diagnostic without discarding other valid names. Invalid underscores are not rewritten as hyphens, and invalid generated names do not acquire an ID fallback. Valid 252-byte and 253-byte generated names retain exact claims without an impossible descendant wildcard. Production registration still needs to consume these claims and report their diagnostics.

All 45 DNS tests passed, including six claim-generation regressions. Strict all-target Clippy passed on the host and Linux musl target, and formatting passed. These tests verify generated and custom ownership rules but do not establish macOS resolver or browser coverage.

## Preserve unknown DNS query types

The shared DNS parser and host forwarder now preserve unknown QTYPE values, including SVCB and HTTPS. Known local names return NODATA when the requested record type has no local data. Missing names under the configured suffix keep the existing host NODATA and guest NXDOMAIN behavior for every query type. External queries reach the existing upstream path with their original question bytes. The host reuses the shared record type instead of maintaining a second enum.

All 50 targeted tests passed: 17 shared parser/response tests, 31 host DNS tests, and two guest DNS tests covering multiple names and query types. A real UDP upstream test verifies forwarding of types 64, 65, and 65280. Strict Clippy passed for the shared parser targets, all host network targets, and all Linux musl guest targets. Formatting passed. These checks do not verify conflict responses, which require the pending ownership-registry integration, or installed VM/browser behavior.

## Shared ownership registry

`registry::Registry` stores each owner's complete claims, optional address, and revision in one caller-locked snapshot. Identical replacements preserve the revision and epoch. Effective replacement and removal advance the epoch, including removal followed by reinsertion. A disconnected owner retains its identity but contributes no claims. A foreign-owner claim rejects the entire replacement before mutation. Lookup preserves conflicts, reservations, and delegations and returns the selected claim, address, and revision together.

All 50 DNS tests passed, including five registry regressions covering rename, idempotence, address loss, revision reuse, atomic rejection, and namespace boundaries. Strict all-target Clippy passed on the host and Linux musl target; formatting passed. Production callers still need to adopt the registry. These checks did not change system DNS, native trust, or installed guest behavior.

## Guest registry integration

Guest DNS and web-service discovery now share the ownership registry. Inspect replaces all claims under the complete container ID; rename uses a fresh inspect, and die/destroy removes that ID. Registration reports invalid names individually. DNS resolves custom exact names and descendant wildcards, returns SERVFAIL for conflicts, and limits short names to exact claims. Web-service snapshots compare the selected owner and address, including when two owners have the same IP.

The full host-executed guest suites passed 99 library and 184 binary tests with the accompanying DNS boundary regression. Strict Linux musl all-target Clippy and formatting passed. This integration does not complete host registration, macOS resolver ownership, reconnect snapshot removal, or revision checks across guest certificate signing. The new behavior has not run inside a VM or browser.

## Consolidate host container registration

Host start, restart, rename, network refresh, reconciliation, and startup recovery now register validated inspect metadata through one Runtime entry. Invalid inspect responses propagate errors without synthesizing identity from a request token. A successful Docker rename followed by invalid inspect returns HTTP 500 and preserves previous host ownership.

Docker library tests passed 127 cases, mock API tests passed eight, and daemon recovery tests passed four. Strict all-target Clippy and formatting passed. All 28 Runtime tests passed serially; parallel runs still failed in unchanged listener tests, including an address-rebind race. These results do not establish VM or browser acceptance.

## Acknowledge guest route cleanup

Container removal first makes its complete-ID registration unavailable. The Docker event task removes that owner only after domain-rule cleanup and published-port cleanup succeed. A failed rule deletion or closed domain task propagates an error and preserves the owner for retry.

All eight Docker event tests passed, including failed cleanup acknowledgment and closed-task regressions. Strict Linux musl all-target Clippy, formatting, and diff checks passed. Native iptables deletion and reconnect snapshot retry remain unverified.

## Project exact host records from the registry

NetworkManager now provides a Registry projection that preserves canonical exact names, omits wildcards and conflicts, and withdraws unavailable owners. Three focused projection tests, strict all-target Clippy, and formatting passed. Runtime does not call this API yet; the host ownership migration remains incomplete. mDNS announcements still cover only the configured suffix.

## Pin guest TLS ownership across asynchronous work

The guest captures the selected owner, registration revision, and installed route identity before observing HTTPS access. The proxy rechecks that route after authorization, certificate configuration, TLS handshake, and upstream connection. A different owner at the same IP, removal and reinsertion of the same owner or route, a changed upstream port, or replacement by native TLS rejects the request. Identical registration and publication keep an active handshake valid. Failed upstream connections invalidate only the pinned route under the DNS and installed-route locks. The redundant IP-to-port map is removed.

Service regressions passed on the host for authorization waits, ownership changes, route replacement, and stale failure invalidation. Strict Linux musl all-target Clippy and formatting passed. VM and browser acceptance remain unverified.

## Exercise the asynchronous TLS relay

A real TCP/TLS regression pauses certificate configuration, the TLS handshake, and the upstream connection. Ownership replacement at the same address stops forwarding at every stage. Reinstalling the same owner at the same address and port also stops forwarding without changing its DNS revision. The valid control forwards HTTP and verifies the response and forwarded scheme. All five relay scenarios, five service tests, and strict Linux musl all-target Clippy passed. These socket tests do not validate the guest NAT path, browser trust, or macOS authorization.

## Validate complete guest reconnect snapshots

The event task validates the entire running-container list and every full-ID inspect before changing ownership. Reconciliation refreshes known IDs, retains unaddressed identities, and disables every absent owner before attempting external cleanup. Empty snapshots remove all absent owners; failed cleanup keeps unavailable identities for retry. All 11 Docker event tests passed, including real Unix-socket cases for incomplete snapshots, identity mismatch, known-owner refresh, empty lists, and cleanup retry. Strict Linux musl all-target Clippy and formatting passed. Acknowledged batch synchronization with the route actor remains the next required boundary; VM recovery is unverified.
