# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.2](https://github.com/arcboxlabs/arcbox/compare/v0.2.1...v0.2.2) (2026-03-16)


### Features

* **net:** daemon owns route lifecycle via arcbox-helperctl ([7979aac](https://github.com/arcboxlabs/arcbox/commit/7979aac6b720a9ca6022397ac6aae1c551d4f3bf))


### Bug Fixes

* **net:** robust bridge NIC detection, skip primary interface by name ([0f03c22](https://github.com/arcboxlabs/arcbox/commit/0f03c221fd421f0d6b83e277ceff707c94187603))
* **net:** update route_reconciler to call ArcBoxHelper (single binary) ([985e1cd](https://github.com/arcboxlabs/arcbox/commit/985e1cdfdef41340d4da05ca67f0905ac640c792))

## [0.2.1](https://github.com/arcboxlabs/arcbox/compare/v0.2.0...v0.2.1) (2026-03-16)


### Bug Fixes

* **net:** add com.apple.vm.networking entitlement for vmnet bridge ([d46802c](https://github.com/arcboxlabs/arcbox/commit/d46802ce8599541ef3793dfaead21adc8ec7522a))

## [0.2.0](https://github.com/arcboxlabs/arcbox/compare/v0.1.12...v0.2.0) (2026-03-15)


### Features

* **agent:** add guest DNS server and Docker event listener (Phase 1) ([ef9da60](https://github.com/arcboxlabs/arcbox/commit/ef9da603dfa2023c1b514e72309259debd9d0dc1))
* **dns:** add arcbox-dns crate for shared DNS packet parsing ([b55ed1f](https://github.com/arcboxlabs/arcbox/commit/b55ed1f6eaebf44ff519b2caa8d5813fe596b72b))
* **dns:** share DNS hosts table between host DnsService and VMM datapath (Phase 2) ([fa4440e](https://github.com/arcboxlabs/arcbox/commit/fa4440e6f9b9622e4105d299b75180caf2d79844))
* **helper:** add privileged helper for utun/route operations ([32d2c23](https://github.com/arcboxlabs/arcbox/commit/32d2c23c68c77018337fa0a67695d7956ec1ce3c))
* **helper:** privileged network helper with fd passing and hello handshake ([746aaea](https://github.com/arcboxlabs/arcbox/commit/746aaea2eff8974ac685339c89248fa22fd24f3e))
* **net:** add L3 tunnel service with bidirectional utun routing (Phase 3) ([13499c6](https://github.com/arcboxlabs/arcbox/commit/13499c664a65e9c274678463de7f6fca390e74a8))
* **net:** daemon uses helper for utun creation via fd passing (Step 2) ([2264fcb](https://github.com/arcboxlabs/arcbox/commit/2264fcbca9240a1bd230a5b034ab7a1166a715d7))
* **net:** L3 direct routing via vmnet bridge (replaces utun approach) ([1b05e30](https://github.com/arcboxlabs/arcbox/commit/1b05e304b77cefa337cbd8b0f8d9c35accccaee8))
* **net:** proxy ARP on bridge NIC, eliminates gateway IP discovery ([a03d5c8](https://github.com/arcboxlabs/arcbox/commit/a03d5c8653201d0a8db1b36ceac80d3aa991c6a8))
* **net:** sandbox DNS, broader subnet routing, dead code cleanup (Phase 4-6) ([96b7b73](https://github.com/arcboxlabs/arcbox/commit/96b7b7357af0d8cc4db5637c813930bc507b73aa))
* **vmm:** integrate L3 tunnel into VMM and runtime (Phase 3) ([1edb1fc](https://github.com/arcboxlabs/arcbox/commit/1edb1fc6a06738439c7313a90996966eea7581ad))


### Bug Fixes

* address new review comments ([3ed2c3b](https://github.com/arcboxlabs/arcbox/commit/3ed2c3b15d0d92394fad84a84dcaae6e9c78368f))
* address PR review comments ([11116d0](https://github.com/arcboxlabs/arcbox/commit/11116d0b4d38d2c5afa913215a70b734345ee8d0))
* **net:** avoid 198.18.0.0 IP conflict, fix cross-compile and async issues ([84db1df](https://github.com/arcboxlabs/arcbox/commit/84db1dfc9c7c162dd2cb07643aeb725f330c1c9b))
* **net:** confirmed macOS utun write() does not deliver to local IP stack ([2d49809](https://github.com/arcboxlabs/arcbox/commit/2d49809bddb8bd4e3aac8a149e4c96cc77757a4c))
* **net:** switch utun read loop to blocking poll+read (AsyncFd unreliable on PF_SYSTEM) ([3c69fa9](https://github.com/arcboxlabs/arcbox/commit/3c69fa94bf4e50bf9bd72941073b69f7360c1681))
* **net:** use 240.0.0.1 (Class E reserved) for utun address, macOS requires IPv4 for -interface routes ([4164592](https://github.com/arcboxlabs/arcbox/commit/4164592a25d46165c71960dcbe517459b9d64e1e))
* resolve remaining PR review comments ([2f4adc4](https://github.com/arcboxlabs/arcbox/commit/2f4adc41e4747663caf7f2f66ff878843b83b88a))


### Miscellaneous Chores

* bump version to 0.2.0 ([d365921](https://github.com/arcboxlabs/arcbox/commit/d3659210a97e91969b93e2c820d6a1bf230eba34))

## [0.1.12](https://github.com/arcboxlabs/arcbox/compare/v0.1.11...v0.1.12) (2026-03-14)


### Features

* **agent:** add blanket iptables FORWARD rules for sandbox subnet ([599e596](https://github.com/arcboxlabs/arcbox/commit/599e5969ea5256538a7c9e3689421172166cf9a0))
* **agent:** add PortForwardManager for iptables DNAT sandbox port forwarding ([98d58df](https://github.com/arcboxlabs/arcbox/commit/98d58df71d6a8b52e233692cc46e7898a6d30f2d))
* **agent:** integrate PortForwardManager into sandbox dispatch and cleanup ([be9d3e6](https://github.com/arcboxlabs/arcbox/commit/be9d3e63a7c40108c63b73c99777c71c6a539b50))
* **agent:** register sandbox DNS in /etc/hosts on create/restore ([27fc18d](https://github.com/arcboxlabs/arcbox/commit/27fc18d03e7d82960a1d5b11157d5bae94645436))
* **core:** add sandbox_port_forward/remove to AgentClient ([b44a64a](https://github.com/arcboxlabs/arcbox/commit/b44a64a34988f2b233051ad08be4b35fc48388d1))
* **proto:** add SandboxPortForward request/response messages ([7a6643f](https://github.com/arcboxlabs/arcbox/commit/7a6643fb0d7576ecaeecc7561fdcbf29121da8e4))
* **wire:** add SandboxPortForward request/response message types ([35e4517](https://github.com/arcboxlabs/arcbox/commit/35e4517fbed5025cd955117da523b9d63d9942fa))


### Bug Fixes

* **agent:** decode stop/remove request once, return 400 on failure ([d118d76](https://github.com/arcboxlabs/arcbox/commit/d118d76021f4f9c8f92b93fdac1cc36bcabaf963))
* **agent:** delete iptables rules before removing allocation entry ([b9446d3](https://github.com/arcboxlabs/arcbox/commit/b9446d3eefa3071befd03f5a571cc61cda588f0e))
* **agent:** export dns module from lib.rs for sandbox.rs access ([9bbb4b0](https://github.com/arcboxlabs/arcbox/commit/9bbb4b010f0f887f13b8848122d00b39ef50aed6))
* **agent:** fix borrow conflict and SandboxId type mismatch in port forward ([b7f5031](https://github.com/arcboxlabs/arcbox/commit/b7f5031acb11245cda6e4583e3f42667bb45b64a))
* **agent:** fix dns marker matching and support IP upsert ([2e45aae](https://github.com/arcboxlabs/arcbox/commit/2e45aaefb6bd904c4bd47e891fa5fbd501fbb0d9))
* **docker:** always update context on enable to fix stale socket path ([8a0c45e](https://github.com/arcboxlabs/arcbox/commit/8a0c45e8d98df79f18f9f89969395955bf70620d))
* scope app token to arcbox-desktop repo for cross-repo push ([d2dc78a](https://github.com/arcboxlabs/arcbox/commit/d2dc78ae2668e2d753a89311ac3799f55cb7912a))

## [0.1.11](https://github.com/arcboxlabs/arcbox/compare/v0.1.10...v0.1.11) (2026-03-13)


### Bug Fixes

* correct musl linker name for aarch64 target ([dde3ad9](https://github.com/arcboxlabs/arcbox/commit/dde3ad95f55939c3d9246d1aadcd498361c351eb))

## [0.1.10](https://github.com/arcboxlabs/arcbox/compare/v0.1.9...v0.1.10) (2026-03-13)


### Bug Fixes

* use token-authenticated git clone for arcbox-desktop push ([fc332f2](https://github.com/arcboxlabs/arcbox/commit/fc332f25857769f2efbeb69977a8c80ea76a6610))

## [0.1.9](https://github.com/arcboxlabs/arcbox/compare/v0.1.8...v0.1.9) (2026-03-13)


### Bug Fixes

* install protobuf in build-agent job ([8c904ea](https://github.com/arcboxlabs/arcbox/commit/8c904ea833ce2bd4b6fcf3b82b85f852e52bf20e))
* set git identity for update-desktop job ([295b896](https://github.com/arcboxlabs/arcbox/commit/295b896a6a832069638ee9a08ee623624d996049))

## [0.1.8](https://github.com/arcboxlabs/arcbox/compare/v0.1.7...v0.1.8) (2026-03-13)


### Bug Fixes

* remove --locked from release builds ([ded024e](https://github.com/arcboxlabs/arcbox/commit/ded024e7f339b0b514850ab732831f9b8b5ecb01))
* update Cargo.lock in release-please PR and restore --locked builds ([3b84b26](https://github.com/arcboxlabs/arcbox/commit/3b84b26233b1e5bd069521750920aa553393375a))
* use arcbox-labs bot for Cargo.lock commits ([657f684](https://github.com/arcboxlabs/arcbox/commit/657f6843dc32ca2a62fba614b8351f9b4fe55e1f))

## [0.1.7](https://github.com/arcboxlabs/arcbox/compare/v0.1.6...v0.1.7) (2026-03-13)


### Features

* migrate from release-plz to release-please ([51472a2](https://github.com/arcboxlabs/arcbox/commit/51472a2158b6998c674adff6ddb782efd63ced7f))
* **release:** auto-update arcbox-desktop version on release ([500aee5](https://github.com/arcboxlabs/arcbox/commit/500aee506413466e74784f066209e7352263a3fb))


### Bug Fixes

* align workspace dependency versions and add release-please markers ([71f11af](https://github.com/arcboxlabs/arcbox/commit/71f11af918f91d125464e83c472521f5d0ba79d5))
* **core:** show full path in missing binary error messages ([0f598ea](https://github.com/arcboxlabs/arcbox/commit/0f598ea91b71040e8c48fd7455895e5550483a8e))
* **release:** decouple tag/release creation from release-plz ([5cf5b5c](https://github.com/arcboxlabs/arcbox/commit/5cf5b5c7ccb00a8845168356e124e04e94ee7dd9))
* use patch bump for pre-1.0 releases ([7d54bd1](https://github.com/arcboxlabs/arcbox/commit/7d54bd1924cfd1f3f6792e460422257bb1b444e7))

## [Unreleased]

## [0.1.5] - 2026-03-09

### Features
- Auto-install CLI tools from app bundle on Desktop launch ([#34](https://github.com/arcboxlabs/arcbox/pull/34))
- Replace vsock busy-polling with AsyncFd, add full-duplex split API ([#45](https://github.com/arcboxlabs/arcbox/pull/45))

### Bug Fixes
- Graceful shutdown with CancellationToken ([#39](https://github.com/arcboxlabs/arcbox/pull/39))
- Remove tracked-but-ignored boot assets from git index

### Refactor
- Extract DHCP server into standalone arcbox-dhcp crate ([#43](https://github.com/arcboxlabs/arcbox/pull/43))
- Remove unnecessary unsafe from NAT translate functions ([#29](https://github.com/arcboxlabs/arcbox/pull/29))
- Reorganize ~/.arcbox/ directory layout ([#48](https://github.com/arcboxlabs/arcbox/pull/48))

### Miscellaneous
- Add SAFETY comments to all unsafe blocks in arcbox-vz ([#26](https://github.com/arcboxlabs/arcbox/pull/26))
- Add release-plz for automated releases ([#40](https://github.com/arcboxlabs/arcbox/pull/40))
- Disable crates.io publish for now
- Clean up Cargo.toml dependency declarations
