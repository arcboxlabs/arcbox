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

## Fix DNS configuration at construction

Runtime now supplies its validated suffix through NetConfig when constructing NetworkManager. The live domain setter and outer forwarder lock are removed, eliminating the forwarder write-lock path before Registry integration. Network tests passed 77 cases with four existing ignored tests; seven web-service tests and the configured machine-DNS test passed. Strict net/core/daemon all-target Clippy and formatting passed. Host Registry integration remains incomplete.

## Serialize host network mutations

Network connect and disconnect resolve the container before taking Runtime's full-ID lifecycle lock. The lock covers the guest mutation and host DNS refresh. Canonicalization replaces only the request's `Container` field. A forced disconnect after an explicit container 404 forwards the original payload to remove an orphan endpoint and skips host publication; other errors propagate.

All 146 focused Docker tests passed: 124 library tests, 13 mock handler tests, five request-forwarding tests, and four routing tests. Strict all-target Docker Clippy, formatting, and diff checks passed. These results cover canonical lock ordering, request options and framing, orphan endpoint handling, and unchanged host ownership after failure. Global host commit ordering and installed VM validation remain pending.

## Publish complete DNS ownership changes

NetworkManager now exposes the latest Registry epoch through a watch channel. Wildcards, unavailable owners, and names outside the configured suffix advance this feed even when the concrete mDNS projection does not change. Identical replacements do not notify subscribers. Subscribers read the full Registry snapshot after subscribing and must release watch borrows before taking the Registry lock.

Four projection tests and strict all-target net Clippy passed. The new regression covers wildcard address loss, custom exact names, unchanged replacements, and subscription after a change with no active receiver. Resolver lease integration remains pending.

## Retain listener managers during cancellation

Runtime now retains each machine's current and retiring listener managers in one slot. The next VMM manager stays at its source while Runtime acquires locks or completes prior retirement. Replacement clears previous ownership before awaiting cleanup. Single-owner cleanup and shutdown hold the rules lock before the listener lock through removal; System VM retirement keeps the slot until both managers finish cleanup.

Two current-thread regressions cancel operations at a single pending poll. They verify source ownership while locks are blocked, retained retirement across cancellation, delayed transfer of the next manager, resumed cleanup, and actual TCP delivery through the fresh command channel. All 72 Runtime tests passed serially, as did strict all-target core Clippy and workspace formatting. Existing assertions remain unchanged apart from slot access and fixture construction. Global host commit ordering, VM incarnation fencing, parallel socket-test stability, and installed VM validation remain pending.

## Isolate secure resolver storage

The helper now shares a resolver storage boundary for directory validation, ACL inspection, no-follow file reads, and complete-content ownership checks. Existing install, uninstall, status, and mutation-lock behavior remain unchanged. All eight existing helper DNS tests passed, including foreign contents, writable files, links, and ACL grants. Strict all-target helper Clippy, formatting, and diff checks passed. Independent review confirmed the extracted implementation is identical apart from module visibility. Resolver leases and EOF cleanup remain pending; these checks used temporary directories and did not change system resolver files.

All helper connections now share one `ResolverStore` containing the resolver directory and mutation lock. The existing DNS operations use this store, and tests inject temporary directories. Secure reads evaluate each operation's ownership predicate only after the unchanged file safety checks. Eight DNS unit tests, four RPC tests, and two real-helper DNS filesystem tests passed. Strict all-target helper Clippy, formatting, and diff checks passed. This slice does not add a lease RPC or change system resolver files.

## Resolve concrete wildcard names before opening

List RPCs now accept a concrete hostname and return at most one current installed route. Desktop commit `888a3fd` uses this query before opening and after authorization, with stale-reply fencing across reconnects. Runtime checks passed: 10 host directory tests, 36 API tests, five daemon control-plane tests, six guest service tests, and one framed guest RPC test covering 11 request states. Guest tests ran in Colima; they are not installed ArcBox evidence. Desktop passed 25 focused tests and strict lint. Host/guest strict Clippy and workspace formatting passed. Protocol remains 7/7; no compatibility alias was added. Resource-boundary retention and installed browser validation remain pending.

## Pin listener transfer to a physical VM run

Engine now advances an opaque `VmIncarnation` on each start or reboot. Capture and manager transfer check the current physical run and VMM liveness under the VM registry lock. A failed reboot cannot restore the previous identity. Runtime stores the identity with its listener manager and rejects reuse or binding through a replacement slot. Socket fixtures use an internal forwarding seam; production rejects absent or stopped VMs.

Six Engine VM tests and 73 serial Runtime tests passed, including stale transfer, failed reboot, and canceled manager retirement. Strict all-target Engine/Core Clippy and formatting passed for both intermediate commits. This change establishes manager provenance; final checks after listener awaits, snapshot publication, retirement ordering, and installed VM acceptance remain unfinished.

Forwarding cleanup now checks the expected slot identity before withdrawing old rules, including empty refreshes and bind failures. A real TCP regression proves that a stale operation cannot remove or bind through its replacement slot. All 74 serial Runtime tests, strict all-target Engine/Core Clippy, scoped formatting, and diff checks passed. This slot check does not establish global ordering for owner movement between machines or physical VM changes during an await.

## Share builtin reservations with the guest

Host and guest now construct the same exact host/index reservations. The guest forwards unresolved builtin names to its existing gateway; hostile container or sandbox entries cannot capture them. Descendants and ordinary unavailable claims retain their existing behavior. Invalid suffix construction fails before DNS serving.

All 50 shared DNS tests, three host reservation tests, five guest DNS tests, 12 guest event tests, and the framed guest RPC regression passed. Guest tests ran in networkless Colima containers. Host and guest strict all-target Clippy, workspace formatting, and diff checks passed. Gateway socket failure and installed VM/browser behavior remain unverified.

## Lease resolver roots to a verified daemon connection

The helper now accepts complete resolver-root leases bound to the verified daemon profile, kernel UID, startup nonce, and increasing revision. Exact roots remain exclusive; failed updates invalidate the connection, and cleanup requires the recorded file contents. Nine helper DNS tests, four mock RPC tests, two temporary-root filesystem tests, strict all-target Clippy, and formatting passed. The daemon does not consume leases yet; system resolver files were not changed.

Startup recovery removes only canonical orphan lease files after file-safety checks. Unleased files, foreign contents, writable files, and links remain untouched. Eight lease regressions cover stale nonce/revision, exact and nested roots, foreign preflight, partial rename failure, retained cleanup evidence, and startup recovery. The helper binary suite passed 35 tests with one native root-only test ignored; strict all-target Clippy and formatting passed.

The connection finalizer now reports request task failures after draining tasks and attempting lease cleanup. A deterministic pending poll and publication acknowledgment verify cleanup order for a held mutation. Real Unix-socket coverage verifies kernel UID stamping and EOF removal; handler tests reject unsigned or wrong-profile lease requests. The final binary suite passed 39 tests with one native root-only test ignored, alongside four DNS RPC and two temporary-root filesystem tests. Debug all-target and release binary strict Clippy, formatting, and diff checks passed. Signed production handshakes and native resolver files were not exercised.

## Admit container mutations against a captured physical VM

Docker lifecycle operations now capture the ready physical VM before canonical inspect and validate that identity after taking the full-ID lifecycle lock. A synchronous activity lease records admission and completion, including cancellation. Guest I/O does not hold the global host commit lock. Offline tests use an explicit non-default identity fixture; skipping readiness alone does not bypass Engine liveness.

Two Core admission tests, nine Docker CA/lifecycle tests, and thirteen mock API handler tests passed. The held-stop regression proves another canonical container can complete guest work and host alias publication. Strict all-target Core/Docker Clippy, scoped formatting, and diff checks passed. Guarded final publication, snapshot ordering, and installed VM validation remain pending.

## Select instance authority before resource ownership

The shared Registry now resolves the longest inclusive instance root before resource claims. Current roots retain ordinary resource matching, including a current grandchild beneath a foreign child. Unavailable roots return SERVFAIL before sandbox or public fallback. Effective authority changes invalidate pinned DNS/TLS revisions; identical projections preserve them. Host Other results currently terminate with SERVFAIL until explicit loopback forwarding lands. Guest Other results use the existing host gateway.

Common DNS passed 52 tests, net 115, Core web services 10, Linux guest DNS 6, and Linux guest service/TLS routes 7. Host and guest strict Clippy, formatting, and diff checks passed. The guest binaries ran in the existing networkless Colima environment. Daemon delivery, guest authority transport, mDNS grant consumption, and installed ArcBox validation remain pending.

## Guard request-driven host publication

Container and network mutations now publish ports, aliases, and DNS under the host commit lock. The final synchronous Engine guard prevents physical VM replacement while publishing aliases/DNS; stale cleanup only targets the expected listener slot. Failed cleanup retains the original operation error as well. Engine passed 6 tests, serial Runtime 77, Docker CA/lifecycle 9, and mock API handlers 13. Strict Engine/Core/Docker Clippy and formatting passed. Snapshot, shared-wrapper, retirement, and shutdown fencing remain pending.

## Watch helper resolver authority

The helper now returns complete sorted root snapshots with a global epoch and connection-relative ownership. Only fully accepted live leases expose a port and revision. Failed or retiring connections retain unavailable roots. Watches wait at most 30 seconds; the client allows 35 seconds. The transport poll observes EOF before draining pending watches, preserving tarpc cancellation and synchronous mutation completion. The core slice passed 39 binary tests with one native CA test ignored, strict all-target Clippy, and formatting. Daemon consumption remains pending.

Five authority regressions cover acquisition/replacement/release notifications, rejected revisions and preflight, uncertain publication, bounded waits/cancellation, and a real Unix EOF with an unchanged pending watch. The final helper binary suite passed 44 tests with one native CA test ignored; DNS unit, mock RPC, and temporary-root filesystem suites passed. Debug all-target and release binary strict Clippy, formatting, and diff checks passed. Native resolver files and installed helper authorization were not exercised.

## Forward other-instance DNS to the accepted endpoint

Host and VMM DNS now use the selected instance's explicit loopback port. This path bypasses ordinary upstream caching and rechecks authority after receiving the reply. Real UDP fixtures verify cache isolation, stale-authority rejection, and no default-upstream packet after a bad transaction ID. Net passed 117 tests with five ignored, plus strict Clippy, formatting, and diff checks. Daemon authority delivery and installed multi-instance validation remain pending.

## Retain resolver cleanup through startup cancellation

The daemon now retains the resolver publication task in a shared slot across startup and shutdown. A cancelled waiter leaves the task owned, and the next waiter receives cleanup errors. Normal shutdown, interrupted startup, and failed startup join publication before stopping Runtime. Recovery separates container snapshot replay from Docker tool installation without changing startup order. The cancellation regression, seven mDNS tests, four shutdown tests, strict daemon Clippy, and formatting passed. The task currently owns Bonjour only; helper lease integration and native teardown validation remain pending.

## Serialize shared port writers

Container, sandbox, and shared listener cleanup now enter the host commit boundary. Listener work rechecks physical VM identity after asynchronous operations and cleans only the expected slot on failure. A held listener-stop regression replaces the VM before binding and proves shared cleanup waits for the commit boundary. Serial Runtime passed 78 tests, the focused inbound suite passed four, and strict Core/Docker Clippy and formatting passed. Linux fallback source was updated but not compiled in this slice. Complete snapshots, retirement identity, and shutdown admission remain pending.

## Retain the daemon resolver lease and watch

ResolverService now owns the helper connection, complete resolver lease, authority watch, and mDNS future. Authority withdrawal and mirror cleanup precede transport closure. Required initial setup propagates failure; background reconnection uses a fresh transport and the same service nonce. Four resolver tests, seven mDNS tests, strict daemon all-target Clippy, and formatting passed. Native resolver cutover and installed validation remain pending.

## Keep guest Registry operations synchronous

ContainerRegistry now uses the standard RwLock. DNS lookup, route validation, and service snapshots release guards before asynchronous authorization or I/O. The final Linux musl binary passed 29 DNS, service, TLS, event, and RPC regressions in networkless Colima containers. Strict all-target musl Clippy and formatting passed. Protocol and minimum remain 7/7; guest authority transport and installed VM acceptance remain pending.

## Commit complete host container snapshots

Cold recovery captures an idle host generation and physical VM before querying every running container. Publication rejects intervening mutations or VM replacement. Port work holds the shared commit lock; Registry and aliases publish together under the final Engine guard. An ordinary bind failure preserves healthy untouched listeners, but successful earlier listener work may remain. Serial Runtime passed 80 tests, mock Docker API passed 13, and scoped strict all-target Clippy and formatting passed. Shared writer generation and retirement/shutdown fencing remain pending.

Periodic reconciliation now uses the same complete snapshot boundary and refreshes already-known containers. VM return keeps the short retry cadence until a snapshot actually commits. Six reconciler tests passed, including a held query invalidated by a newer mutation and unchanged host state after query failure. Strict scoped Clippy and formatting passed. Query cancellation and retained reconciler task ownership remain pending.

## Publish only granted Bonjour records

The mDNS projection contains only selected exact `.local` names with Current instance authority, including custom roots and apexes outside the configured suffix. Wildcards do not synthesize concrete records. The mirror reads the latest complete projection from the Registry watch instead of replaying stale record deltas. The net suite passed 118 tests with five existing ignored cases; six mDNS tests and strict net/Core/daemon Clippy passed. Core snapshot tests retain their address assertions against the Registry. Native custom-domain resolution and paired installation remain unverified.

## Admit shared DNS and port writers

Shared DNS and port writers now advance the host generation under the same commit lock. DNS publication returns admission failures through SandboxHost, API, and daemon callers. Cleanup remains available after admission closes. Serial Runtime passed 81 tests and daemon machine-DNS passed two; strict Core/computer/API/Docker/daemon all-target Clippy passed. The regression holds the commit lock and verifies stale-snapshot rejection and explicit closed-admission errors. Shutdown closure and physical-generation retirement remain pending.

Resolver snapshot regressions reject mismatched ports, revisions, and roots before publication. They also cover metadata-only updates, external authority withdrawal, late acknowledgment, and retention of known, attempted, and current roots after failure. Eight resolver tests and strict daemon Clippy passed. The disconnected legacy DNS installer module is removed; no runtime fallback remains. Real-socket lifetime tests and installed cutover remain pending.

## Define the exclusive guest authority channel

The paired wire schema adds an empty Open, a complete root snapshot with its source authority revision, and an acknowledgment of that revision. Protocol and minimum remain 7/7. Generated prost and Connect descriptors were refreshed. Eleven constants/protocol tests, strict constants/protocol/Connect Clippy, and formatting passed. Guest dispatch and host delivery are not implemented by this schema slice.

The obsolete DNS record-delta channel is removed after its final production consumer moved to the Registry watch. Projection tests now assert the complete accepted snapshot and ownership notifications, preserving exact address, conflict, withdrawal, and unchanged-update checks. The complete net suite again passed 118 tests with five ignored cases; strict all-target net Clippy and formatting passed.

The Docker server now retains its reconciler task. Pending guest snapshot queries are canceled by shutdown or VM-state changes; server-scope cancellation also drops the query. Seven reconciler tests passed, including a permanently pending query canceled through both owner paths without publishing stale state. Strict Docker Clippy and formatting passed. Physical identity convergence during a coalesced lifecycle notification remains pending.

## Verify resolver connection lifetime

Real temporary Unix sockets now cover cancellation before the first lease acknowledgment, helper EOF, authority-watch updates, mirror cleanup before client closure, primary and cleanup errors together, and fresh-transport retries with one service nonce. The initial-failure fixture rejects any mirror start. Eleven resolver tests, strict all-target daemon Clippy, formatting, and diff checks passed. Native Keychain, resolver files, installed helper, and VM state were not changed.

## Share the bounded guest RPC frame reader

Guest decoding now uses the existing transport reader's 16 MiB allocation bound and validates the complete V2 header, trace length, and UTF-8 within one frame. Malformed lengths cannot consume the next frame; short-frame compatibility and silent trace replacement are removed. Host transport passed 26 tests and the final Linux musl guest passed 13 RPC tests in networkless Colima. Both strict all-target Clippy gates and formatting passed. Authority-channel dispatch and installed transport validation remain pending.

Readiness streaming now resides in `agent/linux/rpc/readiness.rs`, preserving the storage-recovery restriction and error responses. Five Linux guest dispatcher tests, the musl build, strict all-target musl Clippy, formatting, and diff checks passed. This extraction makes room for the exclusive authority receiver without changing the readiness protocol.

## Retire the previous physical VM before publication

The host commit state now retains the VM that owns published container networking. A replacement VM retires that state before its first publication; canceled cleanup retains ownership for retry. The Docker reconciler checks physical identity on each lifecycle notification and timer tick, even while stopped. Complete guest queries use the existing 30-second interval as their deadline, and failed retirement keeps the short retry cadence. These changes do not start a VM or change installed resolver state.

Graceful and forced shutdown now close host-network admission before slow cleanup, then clear shared host state under the existing lock order. New regressions cover partial publication from a replacement VM, late writers during shutdown, and canceled retirement that preserves a newer shared TCP listener. Serial Runtime passed 83 tests before the final socket regression; that regression passed separately. Strict all-target Core/Docker Clippy and formatting passed. Installed restart and teardown acceptance remain pending.

Eleven Docker reconciler tests passed. Four new paused-time regressions cover coalesced lifecycle notifications, physical VM loss without a notification, a pending query released by its whole-query deadline, and periodic checks while stopped. Strict all-target Core/Docker Clippy and formatting passed after replacing an unchecked duration subtraction in the test with a checked subtraction. Native VM lifecycle behavior remains unverified.

## Receive guest authority through one owned connection

System VM agent services now pass the existing DNS Registry to the exclusive host authority receiver. Complete roots are validated before replacement and acknowledgment. A unique guard withdraws known roots synchronously before releasing the connection slot; each subsequent source revision invalidates pinned route lifetimes, including identical snapshots after a coalesced authority change. A real Unix-socket dispatcher test covers apply, acknowledgment, and EOF withdrawal. Seven shared Registry tests and 22 Linux guest dispatcher, DNS, service, TLS, and storage-watch tests passed. The musl build, strict guest/DNS all-target Clippy, formatting, and diff checks passed. Protocol and minimum remain 7/7; host delivery and installed acceptance remain pending.

The expanded guest dispatcher matrix passed 12 tests in a networkless Linux container. Coverage includes host/System VM admission, exclusive handoff, revision ordering, invalid complete snapshots, identical and empty lifetime barriers, malformed frames, and replacement without Open. The musl build, strict guest all-target Clippy, formatting, and diff checks passed. These fixtures do not establish installed host delivery.

## Split the blocking transport without an idle deadline

The blocking transport now provides independent typed sender and receiver halves. The receiver keeps listening while the writer is idle; the existing shutdown handle interrupts both workers. All 29 transport tests passed, including real socket coverage for idle EOF, a saturated writer, shutdown, oversized frames, and truncated frames. Strict all-target transport Clippy, scoped formatting, and diff checks passed. A workspace formatting check identified a separate pending Engine test edit; that edit is outside this transport slice. The host authority adapter and installed HV behavior remain unverified.

## Name helper methods on the wire

The helper and its clients now use length-delimited JSON with named RPC and error variants. The Bincode dependency and decoder are removed; the helper package remains 1.2.0. A real framed socket test rejects a recorded Bincode mutation and malformed JSON before dispatch, with no resolver file created. The helper passed 73 tests with one native Keychain test ignored, and the daemon passed nine resolver-service tests. Strict debug all-target and release-binary helper Clippy, scoped formatting, and diff checks passed. The installed helper remains unchanged; the paired installer cutover is not yet connected.

## Open agent connections for the captured physical VM

Runtime now exposes an expected-incarnation connection through MachineManager. VmManager checks the live incarnation, selects the backend, and obtains its connected socket under the same VM read lock. The socket fixture passed for both HV and VZ; replacement cannot acquire the write lock during connection, stale or dead incarnations never call the connector, and dropping the client closes the socket. Strict all-target Core/Engine Clippy, scoped formatting, and diff checks passed. The Linux connected-fd constructor was updated but not compiled by this macOS gate. Daemon authority delivery and installed VM validation remain pending.

The shared host response parser now requires complete V2 trace fields and valid UTF-8. Short-frame and out-of-frame trace fallbacks are removed. All four wire tests passed, including malformed headers, trace bounds, and binary payload separation. Strict Engine all-target Clippy, formatting, and diff checks passed. Guest protocol and minimum remain 7/7.

Guest cancellation tests now interrupt partial request reads and partially written acknowledgments, then verify synchronous withdrawal before a new authority connection opens. TLS regressions use the real leaf cache and reject a second signing attempt. Resource replacement, identical-root authority changes, and authority EOF prevent forwarding at certificate configuration, handshake, and upstream connection boundaries. Seventeen focused authority, TLS, and cache tests passed in networkless Linux; the musl build, strict guest all-target Clippy, formatting, and diff checks passed. These fixtures do not establish installed host delivery or browser behavior.

Resolver storage safety tests now exercise connection leases, release, and restart recovery. Modified managed-looking bytes, unsafe modes, symbolic links, hard links, nonregular slots, unsafe directories, and ACL grants remain preserved. The helper binary passed 42 tests with one native Keychain test ignored; strict helper all-target Clippy, formatting, and diff checks passed. Obsolete persistent DNS mutation RPC removal follows this test migration.

The helper now separates watch metadata changes from authority lifetime changes. Ownership uses private connection identity and the effective DNS endpoint; accepted lease revision alone does not renew DNS/TLS authority. The daemon applies a lifetime barrier when the helper revision changes, including a coalesced release and re-acquisition with identical final roots. Real temporary-store and host-projection regressions passed in the 42-test helper binary suite and ten daemon resolver-service tests. Strict helper/daemon all-target Clippy, scoped formatting, and diff checks passed. Helper remains 1.2.0; native installation is unchanged.

## Own host authority transport workers

The Engine authority session consumes its connected client, admits the peer, opens exclusive authority, and accepts one acknowledged replacement at a time. A continuous reader detects idle EOF without a heartbeat. Dropping the session cancels its retained coordinator; the coordinator closes queues, interrupts socket I/O, and joins both workers. The real framed-peer fixture passed for async and blocking transports, including revision-zero publication, later replacement, idle peer EOF, and caller cancellation. Strict Engine all-target Clippy, formatting, and diff checks passed. Daemon delivery and installed HV/VZ acceptance remain pending.

The expanded Engine matrix passed five tests on both transports. Thirty-six protocol-wait cases cover cancellation, timeout, and EOF during Ping, Open, and Replace, with complete or partial frames. Other cases reject malformed or unsolicited acknowledgments, wrong revisions, and failed admission. A saturated socket test joins both workers before permitting the peer to drain bytes. Strict all-target Clippy, formatting, and diff checks passed. The timeout fixture explicitly advances past Tokio's millisecond deadline rounding; no production timeout changed.

## Reserve machine and sandbox namespaces

The shared Registry permanently reserves the machine and sandbox trees before resources register. Container aliases and broader wildcards cannot acquire these trees after resource removal or address loss. ResourceScope generates canonical names and validates the suffix budget for a 63-character resource label. All 53 shared DNS tests, strict all-target Clippy, formatting, and diff checks passed. Host and guest callers move to these names in the next slice; installed names remain unverified.

Host machine and sandbox publication now uses `<name>.machine.<suffix>` and `<id>.sandbox.<suffix>`. The guest forwards the machine tree to the host and resolves sandbox IDs only inside the reserved sandbox tree. Flat and bare resource aliases are removed. Sandbox Create and Restore preflight DNS names before guest access. The shared DNS suite passed 53 tests, the API preflight regression passed, Core Runtime passed 31 serial tests, and seven guest DNS tests passed in networkless Linux. Strict host and musl guest Clippy, the guest test build, formatting, and diff checks passed. Protocol and minimum remain 7/7. The guest sandbox map will move into the shared Registry next; installed resource resolution remains pending.

The obsolete persistent DNS RPC tests are retired after their ownership and storage checks moved to connection-lease fixtures. The final helper suite passed 91 tests with one native Keychain case ignored. Strict debug all-target and release-binary Clippy, scoped formatting, and diff checks passed. The persistent RPC implementation is removed in the next slice.
