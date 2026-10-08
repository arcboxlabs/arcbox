# Local domains and HTTPS

ArcBox gives Docker containers local DNS names and routes their web services through ports 80 and 443. The daemon keeps the root private key in the user's macOS Keychain. The guest keeps leaf private keys and requests signatures from the daemon. The privileged helper handles public certificates only.

## User operations

Containers and HTTP services can run without local CA trust. Enable local HTTPS from Desktop settings or run `abctl tls trust` in the login session. macOS controls authorization for changes to system trust. Existing correct trust does not require another authorization.

Run `abctl tls status --json` to inspect the current root's fingerprint, validity, publication, and trust. Run `abctl tls untrust` to remove system trust without stopping containers. Trust status does not prove that DNS, a route, or an upstream service works.

The `certificate_state` field distinguishes `missing`, `valid`, `renewal_due`, `expired`, and `not_yet_valid` independently of trust. Renewal is due when a valid root expires within 30 days. A root in that renewal window can remain trusted. Check the Mac's date and time when a certificate is not yet valid. Desktop does not request trust authorization for a missing or inactive certificate. Browser observation also requires a valid certificate; restoring validity starts a new observation cycle for the same root.

Run `abctl tls repair` to restore a missing or damaged `tls/ca.pem` from the profile's existing Keychain CA. Repair does not create a key or change trust. The optional `--expected-fingerprint` pins the current root. Repair rejects an inactive root, multiple matching Keychain items, and a publication containing a recoverable different certificate or multiple PEM objects. A repaired file does not establish that trust in an unknown previous root was removed.

Run `abctl tls rotate` to replace the root while the daemon is running, including in `--no-linux-vm` mode. The daemon creates the replacement private key. The login-session CLI authorizes the replacement, verifies native trust, and revokes the exact previous root. The daemon then publishes the replacement, switches its signer, and deletes the retired identity. The optional `--expected-fingerprint` pins the current root before authorization. Repeated preparation reuses the pending candidate. Desktop exposes the same operation as **Settings → System → Local HTTPS → Renew Certificate** for valid or expired roots, including interrupted rotation retries.

Canceling replacement authorization preserves the previous publication. If an interruption occurs after revocation, HTTPS can remain unavailable until `abctl tls rotate` completes the pending operation. The journal retains both public certificates for recovery. Rotation does not update the trust stores of already-running containers; their client trust requires a separate refresh.

Trust, repair, rotation, and revocation are exclusive per profile through native authorization. A concurrent command fails until the active command completes or the user cancels authorization. Status remains read-only and does not take that lock.

The [authorization decision](adr/0007-local-https-independent-of-startup.md) separates HTTPS from Docker readiness. The [installed validation log](logs/2026-10-08-helper-ca-trust.md#installed-desktop-authorization-and-restart) covers the earlier startup-authorization flow. That evidence does not validate the new on-demand flow.

## Service discovery contract

The daemon's `--dns-domain` option takes precedence over `ARCBOX_DNS_DOMAIN`; the default is `arcbox.local`. Host DNS receives the validated suffix before VM startup. The System VM receives the same suffix through `arcbox.dns_domain=` on the kernel command line. DNS replies, HTTP Host and TLS SNI probes, service URLs, and HTTPS ownership checks use that suffix. A changed suffix participates in the existing VM boot-configuration drift check.

The guest reports installed routes through `ListContainerWebServices`. The snapshot includes only names that resolve to the selected container address. The host exposes those routes through `SystemService.ListWebServices` and `WatchWebServices`.

Clients use the supplied URLs. Upstream port fields describe the application listener, not the public URL port. A managed HTTPS endpoint requires ArcBox CA trust. A passthrough endpoint uses the application's own TLS and must not request ArcBox CA authorization. A directory error ends the stream; clients must clear stale entries.

Managed HTTPS requires `arcbox.local` or a subdomain such as `one.dev.arcbox.local` as the configured suffix. Other suffixes support HTTP and native TLS, but do not receive managed HTTPS routes or trigger ArcBox CA authorization. The CA name constraints remain unchanged.

The host verifies each CSR's signature and single DNS name before signing. The name must belong to a current host container DNS registration and resolve to that registration's address. The host rechecks the registration after signing, so deletion, ownership transfer, or registration replacement discards an in-flight result. The guest separately checks the connection's original destination against its current DNS ownership.

Managed HTTPS replaces `X-Forwarded-Proto` with `https` on every HTTP/1.1 request. The relay preserves Host, path/query, Location, and repeated Set-Cookie values. Bodies remain streaming; trailer capability is negotiated per hop. WebSocket upgrades preserve bytes already read with the handshake. Native TLS passes through unchanged. Managed HTTP/2 and gRPC translation are not implemented.

## Completion requirements

The feature objective includes the complete local-domain experience across Docker, Compose, machines, and Kubernetes. A unit test or a published protocol field does not establish product completion. Each row needs evidence at its stated scope.

| Requirement | Required evidence | Audit state on 2026-10-08 |
|---|---|---|
| Container and Compose names | Access fresh container and service/project names from macOS without resolver overrides or published ports. | Basic macOS HTTPS path verified; current dynamic changes need another VM run. |
| Instance DNS suffix | Start with a custom suffix; verify host and guest DNS, probe Host/SNI, service URLs, TLS ownership, and restart after changing it. | Shared validation and host/guest propagation are implemented. VM and browser acceptance remain pending. |
| Custom `.local` names and implicit/default/custom wildcards | Resolve and serve exact and nested names; reject collisions or ambiguous ownership; verify certificate names. | Missing. |
| HTTP and HTTPS discovery | Use actual protocol responses; exercise multiple listeners, explicit overrides, late listeners, same-port replacement, and network changes. | Selection tests exist; current installed NAT path needs validation. |
| Native HTTPS | Preserve the application's certificate on 443, 8443, and explicit ports; cover HTTP/2-only and mTLS listeners. | HTTP/1.1 and HTTP/2 discovery pass real TLS socket tests. Explicit native ports bypass discovery for mTLS; installed NAT and mTLS validation remain pending. |
| First HTTPS access | A browser request causes one login-session authorization; concurrent requests and reconnects do not repeat the prompt. | Guest observation, daemon relay, and Desktop Watch/acknowledgement are connected. Host tests cover observation leases, cancellation, and stale trust cycles. Real browser validation is pending. |
| Cancellation and existing trust | Cancel authorization while Docker stays usable; retry explicitly; restart with existing trust without another mutation. | Independent state tests exist; new combined native flow needs validation. |
| Desktop service entry points | Open the same authoritative URL from the list, details, and menu bar; handle deletion during authorization and directory disconnects. | List, details, and menu bar share the opener. Retry, dismissal, and reload actions are implemented. The Swift app builds and its focused tests pass; the paired runtime and browser flow remain unverified. |
| Browser index | Serve an index at `arcbox.local` with current service links and useful failure states. | Missing. |
| Container client trust | Verify container-to-container HTTPS without custom CA flags; verify opt-out and preservation of unrelated roots. | Create/start injection preserves unrelated roots and honors opt-out; a running restart reports a skip. Real container validation is missing. |
| Unsupported trust stores | State behavior for read-only roots, mounted stores, Java, Node, image builds, and containers created inside the VM. | Partial boundaries documented; full coverage missing. |
| Machine integration | Verify machine names, HTTP/HTTPS port configuration, managed/native TLS, and client trust from inside a machine. | Machine DNS exists; web-service and trust integration missing. |
| Kubernetes integration | Verify `cluster.local`, Service reachability, LoadBalancer/Ingress wildcard names, and intended HTTPS behavior from macOS. | LoadBalancer port forwarding exists; domain and HTTPS integration missing. |
| Domain ownership | Reject a TLS SNI name that does not belong to the actual destination; validate the same ownership at signing and routing boundaries. | Guest routing checks SNI ownership before and after authorization. The host signer independently verifies current container DNS ownership before and after signing. Real wrong-SNI rejection remains pending. |
| Private-key boundary | Verify signed creator access and rejection of unrelated identities; confirm no private root bytes reach disk exports, helper, or guest. | Native baseline exists; complete identity-negative matrix missing. |
| Root and leaf lifecycle | Verify actual leaf expiry, concurrent issuance, root renewal, rotation, repair, revocation, migration, and uninstall without removing unrelated roots. | Leaf validation, publication repair, certificate lifetime status, and journal-based rotation are implemented. CLI and Desktop renewal use the daemon's serialized mutation worker. Host tests and four isolated rotation tests pass. The Admin trust rotation test was canceled during revocation and did not pass; cleanup of its three trusted test roots awaits explicit authorization. Signed app, browser, and full native lifecycle acceptance remain pending. |
| Application protocols | Verify forwarded scheme, redirects, secure cookies, WebSocket, SSE, HTTP/2, and gRPC on supported modes. | Five real TCP tests pass for managed HTTP/1.1 scheme, headers, streaming/trailers, keep-alive, early responses, WebSocket read-ahead, and disconnect/cancellation. Native HTTP/2 discovery passes. Installed protocol and gRPC evidence remain pending. |
| Diagnostics and recovery | Distinguish DNS, route, upstream, proxy, and trust failures; recover after daemon/guest restart and event-stream gaps. | CA metadata exists; combined diagnostics and recovery evidence missing. |
| Performance | Measure first request, cached requests, concurrent handshakes, route changes, and idle cost at increasing service counts. | No comparable HTTPS measurements. |
| Release integration | Validate the paired signed app/runtime, then publish the runtime before changing the Desktop release pin. | Development branches only; release not authorized by this document. |

## OrbStack comparison sources

The comparison uses public product behavior. Do not infer undocumented implementation details or claim coverage for an unsupported protocol or trust store.

- [Container domains](https://docs.orbstack.dev/docker/domains): naming, custom domains, wildcards, index, and protocol discovery.
- [HTTPS](https://docs.orbstack.dev/features/https): first-use installation, container CA injection, private-key protection, and native HTTPS forwarding.
- [Forwarded scheme clarification](https://github.com/orbstack/orbstack/issues/1895#issuecomment-2803371612): OrbStack sets `X-Forwarded-Proto` for proxied HTTPS; direct HTTP does not receive that header.
- [Linux networking](https://docs.orbstack.dev/machines/network): machine names, port configuration, and macOS trust integration.
- [Kubernetes](https://docs.orbstack.dev/kubernetes/): Service addresses, `cluster.local`, and LoadBalancer/Ingress domains.
- [Menu bar](https://docs.orbstack.dev/menu-bar): service discovery and opening.
