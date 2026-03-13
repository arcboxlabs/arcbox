# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
