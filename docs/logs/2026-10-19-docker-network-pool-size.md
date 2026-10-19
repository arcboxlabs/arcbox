# 2026-10-19 — Docker allocates /24 networks from the container pool

- Type: change
- Area: `arcbox-constants` (`container_network`), `arcbox-agent` (`docker_config`), `arcbox-e2e` (`smoke`)
- Commits / PRs: this change
- Related: none

## Problem or trigger

A developer machine running the production engine failed `docker compose up` with `all predefined address pools have been fully subnetted`. The guest `daemon.json` set `default-address-pools` to `[{"base": "172.16.0.0/12", "size": 16}]`, because `ContainerNetwork::docker_network_prefix` returned `prefix + 4`. A /12 pool split into /16 networks holds 16 networks. The default bridge takes one, so only 15 user-defined networks fit. On the affected machine, 16 networks occupied all of `172.16.0.0/16` through `172.31.0.0/16`, and none of them had a running container.

Upstream Docker 29 has about 30 local networks by default: `172.17–172.31` as /16 and `192.168.0.0/16` as /20 (`daemon/libnetwork/ipamutils/utils.go`). Its global pool and the configuration OrbStack recommends both use /24 networks.

## What was done

- `docker_network_prefix` now returns `max(24, prefix + 4)`. The production pool gives 4096 /24 networks. A pool narrower than /20 still splits into sixteenths, so a /24 pool keeps its 16 /28 networks.
- The default bridge has its own prefix, `docker_bridge_prefix` (`prefix + 4`). `bip` stays `172.17.0.1/16` on the production pool, so existing containers on `bridge` keep their addresses.
- Docker's predefined-pool allocator skips every prefix already allocated, the bridge included (`allocatePredefinedPool` in `daemon/libnetwork/ipams/defaultipam/address_space.go`). New networks therefore start at `172.16.0.0/24` and skip `172.17.0.0/16`.
- Host routing and the guest `DOCKER-USER` rule use the whole pool CIDR. Neither depends on the network size, so neither changed.

Existing networks keep their /16 subnet until they are recreated (`docker compose down && docker compose up`).

## Evidence

- `cargo test -p arcbox-constants container_network` and `cargo test -p arcbox-agent --lib docker_config` pass. `docker_networks_are_slash_24_unless_the_pool_is_narrower` covers /12, /16, /20, /22 and /24 pools.
- The `smoke` e2e has a new `network_pool` phase. The phase creates 20 networks on the default pool, more than the old limit of 16. It asserts that each network is a /24 inside `172.16.0.0/12` and outside `172.17.0.0/16`. It also asserts that a container reaches a sibling by name on one of the networks. VZ run on 2026-10-19 with the pinned 0.8.9 boot bundle: the suite passed, `network_pool` took 3.4 s, and daemon ready took 4.5 s.

## Follow-ups

None.
