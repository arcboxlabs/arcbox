# Local domains and HTTPS

ArcBox gives Docker containers local DNS names and routes their web services through ports 80 and 443. The daemon keeps the root private key in the user's macOS Keychain. The guest keeps leaf private keys and requests signatures from the daemon. The privileged helper handles public certificates only.

## User operations

Containers and HTTP services can run without local CA trust. Enable local HTTPS from Desktop settings or run `abctl tls trust` in the login session. macOS controls authorization for changes to system trust. Existing correct trust does not require another authorization.

Run `abctl tls status --json` to inspect the current root's fingerprint, validity, publication, and trust. Run `abctl tls untrust` to remove system trust without stopping containers. Trust status does not prove that DNS, a route, or an upstream service works.

The [authorization decision](adr/0007-local-https-independent-of-startup.md) separates HTTPS from Docker readiness. The [installed validation log](logs/2026-10-08-helper-ca-trust.md#installed-desktop-authorization-and-restart) covers the earlier startup-authorization flow. That evidence does not validate the new on-demand flow.

## Service discovery contract

The guest reports installed routes through `ListContainerWebServices`. The snapshot includes only names that resolve to the selected container address. The host exposes those routes through `SystemService.ListWebServices` and `WatchWebServices`.

Clients use the supplied URLs. Upstream port fields describe the application listener, not the public URL port. A managed HTTPS endpoint requires ArcBox CA trust. A passthrough endpoint uses the application's own TLS and must not request ArcBox CA authorization. A directory error ends the stream; clients must clear stale entries.

## Completion requirements

The feature objective includes the complete local-domain experience across Docker, Compose, machines, and Kubernetes. A unit test or a published protocol field does not establish product completion. Each row needs evidence at its stated scope.

| Requirement | Required evidence | Audit state on 2026-10-08 |
|---|---|---|
| Container and Compose names | Access fresh container and service/project names from macOS without resolver overrides or published ports. | Basic macOS HTTPS path verified; current dynamic changes need another VM run. |
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
| Domain ownership | Reject a TLS SNI name that does not belong to the actual destination; validate the same ownership at signing and routing boundaries. | Guest routing checks SNI ownership before and after authorization. Independent host signing-boundary validation and real wrong-SNI rejection remain pending. |
| Private-key boundary | Verify signed creator access and rejection of unrelated identities; confirm no private root bytes reach disk exports, helper, or guest. | Native baseline exists; complete identity-negative matrix missing. |
| Root and leaf lifecycle | Verify actual leaf expiry, concurrent issuance, root renewal, rotation, repair, revocation, migration, and uninstall without removing unrelated roots. | Leaves are verified against the published root and cached with its DER. CA preparation retries after failure; the signer follows the current published identity. Explicit root renewal, rotation, corrupt-publication repair, and native lifecycle evidence remain pending. |
| Application protocols | Verify forwarded scheme, redirects, secure cookies, WebSocket, SSE, HTTP/2, and gRPC on supported modes. | Managed TLS currently advertises HTTP/1.1 only; protocol matrix missing. |
| Diagnostics and recovery | Distinguish DNS, route, upstream, proxy, and trust failures; recover after daemon/guest restart and event-stream gaps. | CA metadata exists; combined diagnostics and recovery evidence missing. |
| Performance | Measure first request, cached requests, concurrent handshakes, route changes, and idle cost at increasing service counts. | No comparable HTTPS measurements. |
| Release integration | Validate the paired signed app/runtime, then publish the runtime before changing the Desktop release pin. | Development branches only; release not authorized by this document. |

## OrbStack comparison sources

The comparison uses public product behavior. Do not infer undocumented implementation details or claim coverage for an unsupported protocol or trust store.

- [Container domains](https://docs.orbstack.dev/docker/domains): naming, custom domains, wildcards, index, and protocol discovery.
- [HTTPS](https://docs.orbstack.dev/features/https): first-use installation, container CA injection, private-key protection, and native HTTPS forwarding.
- [Linux networking](https://docs.orbstack.dev/machines/network): machine names, port configuration, and macOS trust integration.
- [Kubernetes](https://docs.orbstack.dev/kubernetes/): Service addresses, `cluster.local`, and LoadBalancer/Ingress domains.
- [Menu bar](https://docs.orbstack.dev/menu-bar): service discovery and opening.
