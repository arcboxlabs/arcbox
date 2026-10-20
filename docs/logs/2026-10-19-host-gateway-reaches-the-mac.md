# 2026-10-19 — `host-gateway` reaches the Mac

- Type: change
- Area: `arcbox-agent` (`docker_config`), `arcbox-e2e` (`smoke`)
- Commits / PRs: this change
- Related: #157 (`host.docker.internal` DNS and gateway-to-loopback translation)

## Problem or trigger

Cloudflare's local container runtime in workerd (used by `wrangler dev` and Miniflare) starts a `cloudflare/proxy-everything` egress sidecar next to each container. workerd creates the sidecar on the default bridge with `ExtraHosts: ["host.docker.internal:host-gateway"]` and DNS servers `1.1.1.1` and `8.8.8.8`. On macOS it listens for the sidecar on `127.0.0.1` (`createSidecarContainer` and `ensureEgressListenerStarted` in `src/workerd/server/container-client.c++`). The sidecar dials `host.docker.internal`. On ArcBox the sidecar could not reach workerd. On Docker Desktop and OrbStack it can.

The guest `daemon.json` did not set a host gateway, so dockerd 29 used the default bridge's gateway (`setHostGatewayIP`, `daemon/daemon_unix.go`). That address is `172.17.0.1`, the guest's own `docker0`. dockerd writes the entry into the container's `/etc/hosts`, and `/etc/hosts` wins over ArcBox's DNS answer (`10.0.2.1`, from `register_host_dns`). So `host.docker.internal` resolved to the VM. Compose files that carry `extra_hosts: ["host.docker.internal:host-gateway"]` for Linux portability had the same failure.

OrbStack on 2026-10-19 (Docker 29.4.0, `alpine:3.21`, `python3 -m http.server --bind 127.0.0.1` on the Mac):

| From a container | Resolves to | Fetch from the Mac's loopback |
|---|---|---|
| `host.docker.internal`, no `--add-host` | `0.250.250.254` | succeeds |
| `--add-host host.docker.internal:host-gateway --dns 1.1.1.1` | `0.250.250.254` | succeeds |
| Default bridge gateway `192.168.215.1` | — | connection refused |

OrbStack sets dockerd's host gateway to its host address. It does not forward the bridge gateway to the Mac.

## What was done

- The guest `daemon.json` sets `host-gateway-ips` to `["10.0.2.1"]`, the uplink gateway that `host.docker.internal` already resolves to. The host datapath splices TCP and UDP for that address into the Mac's `127.0.0.1` on every port (`splicetcp::egress::DefaultEgress`, the `arcbox-proxy` UDP egress). UDP 53 and 67 to the gateway stay DNS and DHCP.
- `host-gateway-ip` and `host-gateway-ips` are owned keys. dockerd refuses to start when `daemon.json` names both spellings (`findConfigurationConflicts`, `daemon/config/config.go`), so an operator value for either in `[docker.engine]` is dropped with a warning.
- `10.0.2.0/24` is a reserved guest network (`RESERVED_GUEST_NETWORKS`), so no container pool can take the address.

dockerd resolves `host-gateway` when it builds a container's network sandbox (`buildSandboxOptions`, `daemon/container_operations.go`). Existing containers get the new address on their next start.

## Evidence

- `cargo test -p arcbox-agent --lib docker_config` passes. `daemon_json_points_host_gateway_at_the_mac` and `operator_host_gateway_is_refused_in_either_spelling` cover the rendered key and both operator spellings.
- The `smoke` e2e has a new `host_gateway` phase. A container started with `--add-host host.docker.internal:host-gateway` fetches a body from an origin bound to the Mac's `127.0.0.1` and must receive every byte. The same container command against OrbStack printed `0.250.250.254` and the full byte count. The phase has not run on ArcBox: no ArcBox VM was available for this change.

## Follow-ups

- Run `smoke` on VZ and HV and record the `host_gateway` timing here.
