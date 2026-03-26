# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.12](https://github.com/arcboxlabs/arcbox/compare/v0.3.11...v0.3.12) (2026-03-26)


### Bug Fixes

* address PR review comments ([866a0cc](https://github.com/arcboxlabs/arcbox/commit/866a0cc9828f26a9a0f0861cd582af6a8cf85c08))
* address second round of review comments ([0b1b5b2](https://github.com/arcboxlabs/arcbox/commit/0b1b5b2c31d7c19bed154415be2a4d1cbd884200))
* **core:** close serial FDs on VM stop instead of leaking via mem::forget ([33f3d5f](https://github.com/arcboxlabs/arcbox/commit/33f3d5f9765a677bc183d25564f9ff2ec324e25e))
* **core:** propagate skip_stop_on_drop to DarwinVm and preserve network cleanup ([f1e84f0](https://github.com/arcboxlabs/arcbox/commit/f1e84f0202e5b93bb7bb24d8505a122d1d35eb36))
* **docker:** forward all non-hop-by-hop headers to guest dockerd ([8b65f2d](https://github.com/arcboxlabs/arcbox/commit/8b65f2de2407872e73836ade0b78ab1325e92155))
* **fs:** replace unaligned pointer casts with read_unaligned in FUSE dispatcher ([8cd1e45](https://github.com/arcboxlabs/arcbox/commit/8cd1e454067b24ef224976cf0355a9d01bfd0837))
* **hypervisor:** use read_unaligned for KVM IO data access ([d20c484](https://github.com/arcboxlabs/arcbox/commit/d20c4840d598a20ea15e4df572499c2efaeb4b48))
* **net:** correct ConnTrack expiry timer using process-wide epoch ([3c9edcb](https://github.com/arcboxlabs/arcbox/commit/3c9edcbcfa0564253ce9f073d7b20574fa3c1978))
* **net:** correct MPMC ring CAS ordering to prevent data race ([8efd12f](https://github.com/arcboxlabs/arcbox/commit/8efd12f4df350e49d53f8b31912f849891d926ea))
* **net:** replace ConnTrack fast cache raw pointers with Arc ([d5d52d6](https://github.com/arcboxlabs/arcbox/commit/d5d52d6313d344bbcf7236f1e00d1dcf37d6c4dc))
* **net:** rewrite MPMC ring as Vyukov bounded queue with per-slot sequences ([2a65966](https://github.com/arcboxlabs/arcbox/commit/2a659661478750ae112376356f7fd02ad48222c6))
* **vm-agent:** close master_fd on fork failure to prevent leak ([9f63bef](https://github.com/arcboxlabs/arcbox/commit/9f63befa7e832739c42bab5cb3eef915529c9ca9))
* **vm-agent:** fix PTY master_fd double-close via ownership transfer ([f7a79b0](https://github.com/arcboxlabs/arcbox/commit/f7a79b055e991d8d5df7a6152b81d5511b2de76e))
* **vm-agent:** import IntoRawFd trait for into_raw_fd() call ([971fdcb](https://github.com/arcboxlabs/arcbox/commit/971fdcbc3ed57fd44ed87c1ed20161e69098943a))
* **vmm:** drop trigger_callback lock before invoking IRQ callback ([b35b14d](https://github.com/arcboxlabs/arcbox/commit/b35b14d4faac78797ae678a1b5701fc457d084e3))

## [0.3.11](https://github.com/arcboxlabs/arcbox/compare/v0.3.10...v0.3.11) (2026-03-26)


### Bug Fixes

* **cli:** remove sfltool resetbtm, fix plist leak, rename app to ArcBox ([#123](https://github.com/arcboxlabs/arcbox/issues/123)) ([e22d3f8](https://github.com/arcboxlabs/arcbox/commit/e22d3f81c1a3eb03f78d00d4d8be427562d0a282))
* **net:** add Copy bound to MpmcRing to prevent soundness hole ([#124](https://github.com/arcboxlabs/arcbox/issues/124)) ([4e18408](https://github.com/arcboxlabs/arcbox/commit/4e18408f0bf922c7166967a1126f274e8a1bd566))


### Code Refactoring

* replace test section dividers with mod blocks ([#121](https://github.com/arcboxlabs/arcbox/issues/121)) ([bfb7a5b](https://github.com/arcboxlabs/arcbox/commit/bfb7a5b38e33021f9e89942d22734d287b391406))

## [0.3.10](https://github.com/arcboxlabs/arcbox/compare/v0.3.9...v0.3.10) (2026-03-26)


### Features

* **helper:** default to persistent mode, add --idle-exit flag ([#117](https://github.com/arcboxlabs/arcbox/issues/117)) ([5ec0b33](https://github.com/arcboxlabs/arcbox/commit/5ec0b333faf3d1ecbb892e8777dc79fd1e6851a2))


### Bug Fixes

* **guest-agent:** raise inherited nofile limits ([#120](https://github.com/arcboxlabs/arcbox/issues/120)) ([d836dbe](https://github.com/arcboxlabs/arcbox/commit/d836dbe83a4da02495d06a9484762b1ed6b2e3b8))
* vmnet relay thread leak, MAC mismatch, and idle backoff ([#115](https://github.com/arcboxlabs/arcbox/issues/115)) ([c91d7fa](https://github.com/arcboxlabs/arcbox/commit/c91d7fabd456ff6cfa2d5254c825eb5b2faffdc8))


### Miscellaneous Chores

* remove visual section dividers from test/small files ([#118](https://github.com/arcboxlabs/arcbox/issues/118)) ([b01b547](https://github.com/arcboxlabs/arcbox/commit/b01b54713299cd0c72f50c3ca2f97b473173ab48))

## [0.3.9](https://github.com/arcboxlabs/arcbox/compare/v0.3.8...v0.3.9) (2026-03-25)


### Features

* **hypervisor:** derive default VM memory from host physical RAM ([af2ae7f](https://github.com/arcboxlabs/arcbox/commit/af2ae7fa08a044f4883f04dee25ccb6a9f922a23))


### Bug Fixes

* address PR review comments (8 items) ([8ab1786](https://github.com/arcboxlabs/arcbox/commit/8ab178658d27341738568d1dc330c30fe652bfe1))
* **fs:** wire negative_cache_ttl from FsConfig through to PassthroughFs ([c11042e](https://github.com/arcboxlabs/arcbox/commit/c11042e505127b8d74c74be0d0c5e1a628beacfd))
* **hypervisor:** address review comments on memory defaults ([930a0bb](https://github.com/arcboxlabs/arcbox/commit/930a0bb2d297750b74bb00f1793848c178f2b166))
* **hypervisor:** resolve cgroup path and align memory to 1 MiB ([68cea43](https://github.com/arcboxlabs/arcbox/commit/68cea436dc7f2b6da3e1b35a5473b52ac88a40c8))
* **net:** integrate timer wheel into datapath loop select! ([b0fa321](https://github.com/arcboxlabs/arcbox/commit/b0fa3210617667c9c543b93290af53b6641808fd))
* resolve all clippy warnings, enforce -D warnings ([#111](https://github.com/arcboxlabs/arcbox/issues/111)) ([da27253](https://github.com/arcboxlabs/arcbox/commit/da272533ae14234bbaaec143cad462b0f977a418))
* update stale test assertion, comments, and config example ([e640cf0](https://github.com/arcboxlabs/arcbox/commit/e640cf04fa30b0baff8aa289172c81184053c26b))
* **virtio:** include virtio-net header in inject_rx_batch, fix warning ([053dd25](https://github.com/arcboxlabs/arcbox/commit/053dd25d16ba36fa8b04ef195eae434fe6d7073a))
* **virtio:** integrate EVENT_IDX into device feature negotiation ([281881d](https://github.com/arcboxlabs/arcbox/commit/281881d9fccd44ea45180bbad8b03f7d965e0e45))
* **virtio:** partition INIT/DESTROY from parallel FUSE dispatch ([48308dd](https://github.com/arcboxlabs/arcbox/commit/48308ddb6eab86b3de43c06b6af6fc951df415bd))
* **virtio:** preserve avail ring order in parallel FUSE dispatch ([6e57fb2](https://github.com/arcboxlabs/arcbox/commit/6e57fb2ec208d7d77eb860d75961cd41622702d6))
* **vmm:** fix coalescing/bitmap ordering, add end-to-end tests ([5dffa3e](https://github.com/arcboxlabs/arcbox/commit/5dffa3ea8e5006ae8d76bc09348735c967b58fd6))
* **vmm:** unify remaining hardcoded memory defaults ([7d9cb27](https://github.com/arcboxlabs/arcbox/commit/7d9cb2703dc0062a80989cc58b28c1a7644d99bf))
* **vmm:** wire CoalescingState into IrqChip.trigger_irq() ([ac569f2](https://github.com/arcboxlabs/arcbox/commit/ac569f2fe3d735c73597717246512f1751febc53))


### Performance Improvements

* **core:** replace serial port 200ms polling with adaptive backoff ([a77ebb9](https://github.com/arcboxlabs/arcbox/commit/a77ebb93dde984a128471fcc79b70fdd8da45bf0))
* **fs:** increase FUSE cache TTL from 1s to 10s ([a81316c](https://github.com/arcboxlabs/arcbox/commit/a81316c80d1315101431d1728aef7f6f08ffe152))
* **net:** add unified timer wheel for flow timeout management ([02c82cd](https://github.com/arcboxlabs/arcbox/commit/02c82cd603ef653cadf67ef8c3b0e824f6a0ecc9))
* **net:** increase smoltcp poll interval from 100ms to 250ms ([acda2f4](https://github.com/arcboxlabs/arcbox/commit/acda2f4b9ce35aa77818dbd22c54b6d9ce241e18))
* **virtio:** enable concurrent FUSE request processing with rayon ([fcd6c3c](https://github.com/arcboxlabs/arcbox/commit/fcd6c3cae40e4decb091570ab99b908470b08eed))
* **virtio:** implement EVENT_IDX and interrupt suppression in VirtQueue ([feb7d51](https://github.com/arcboxlabs/arcbox/commit/feb7d51cb4abc984e9e94ae4a1b9bf2e15d5895b))
* **virtio:** implement TX/RX batch coalescing in virtio-net ([a5f3aa7](https://github.com/arcboxlabs/arcbox/commit/a5f3aa7a6cae260ddc523fdc0c1a5a618e881d4c))
* **vmm:** add timer-based interrupt coalescing to IRQ manager ([a21201d](https://github.com/arcboxlabs/arcbox/commit/a21201dedc93b8f2312440f4ec05bdcdafb5d45c))

## [0.3.8](https://github.com/arcboxlabs/arcbox/compare/v0.3.7...v0.3.8) (2026-03-25)


### Features

* **docker:** GuestConnector trait + proxy integration tests ([#109](https://github.com/arcboxlabs/arcbox/issues/109)) ([1e2730a](https://github.com/arcboxlabs/arcbox/commit/1e2730a7327e03920ec55282d12f52e426399cf0))
* overhaul logging system with unified paths, rotation, and structured output ([cef06d0](https://github.com/arcboxlabs/arcbox/commit/cef06d0dd5af5992a638cdc9f4a1a82ba79cb383))


### Bug Fixes

* **agent:** handle log directory creation failure gracefully ([e86166d](https://github.com/arcboxlabs/arcbox/commit/e86166d96c38cd5b6da4380ab58e4866b42db6c8))
* **cli:** correct tail_lines offset calculation and update docs ([ad00ae9](https://github.com/arcboxlabs/arcbox/commit/ad00ae96c761d300630a928823438d0f576fb4bf))
* **cli:** improve logs command reliability ([9eea7c1](https://github.com/arcboxlabs/arcbox/commit/9eea7c107c74e214e1f051ee25c2319261e670a1))
* **daemon:** start gRPC before stale-state cleanup to prevent desktop timeout ([#107](https://github.com/arcboxlabs/arcbox/issues/107)) ([06edbc0](https://github.com/arcboxlabs/arcbox/commit/06edbc011e7cf22de69474a9308b1e455358279e))
* **helper:** graceful shutdown on idle timeout for log flush ([9ab07ae](https://github.com/arcboxlabs/arcbox/commit/9ab07ae58ffedd08b249c135ee0b77e62d16d09d))
* **logging:** validate config, improve rotation error handling ([388f519](https://github.com/arcboxlabs/arcbox/commit/388f5190cee791ed356b6decd43150db3e1aac08))
* quote YAML description values in skill frontmatters ([7dab741](https://github.com/arcboxlabs/arcbox/commit/7dab741c5882372bfb6dbdbe95684a934d638b54))


### Code Refactoring

* **docker:** split proxy.rs into module directory ([#106](https://github.com/arcboxlabs/arcbox/issues/106)) ([ad4010a](https://github.com/arcboxlabs/arcbox/commit/ad4010a4df3597eda29c832e6f7a007f30c4e2ba))


### Documentation

* add Claude Code skills setup to CONTRIBUTING.md ([d638464](https://github.com/arcboxlabs/arcbox/commit/d638464ac04dc2ef8b1facd8c56978b91706725b))
* add code signing guide to CONTRIBUTING.md ([e04dd8b](https://github.com/arcboxlabs/arcbox/commit/e04dd8be45e71f487cecdc505a1d33312173212a))
* **CLAUDE.md:** update signing instructions and add architecture principles ([931db50](https://github.com/arcboxlabs/arcbox/commit/931db5051f44502831bf7e729298f34e63da7a3f))
* fix log rotation claims and legacy path descriptions ([a6a6919](https://github.com/arcboxlabs/arcbox/commit/a6a69196d9771d148d63fe881b3e3dcb8bf36cc0))
* **helper:** add local development guide and Makefile shortcuts ([848391a](https://github.com/arcboxlabs/arcbox/commit/848391ad0bcb752133479da354423e377ed84a06))


### Miscellaneous Chores

* add pre-commit config, fix clippy warnings in arcbox-logging ([a05bf8b](https://github.com/arcboxlabs/arcbox/commit/a05bf8b8f066cf2e6651adc7177c5a9964ad0e63))
* move agent skills to .agents/skills/ for git sharing ([e39f612](https://github.com/arcboxlabs/arcbox/commit/e39f61290760549b6ce4cc67f1fdb639e629e5df))

## [0.3.7](https://github.com/arcboxlabs/arcbox/compare/v0.3.6...v0.3.7) (2026-03-25)


### Features

* implement sandbox exec with bidirectional streaming ([#80](https://github.com/arcboxlabs/arcbox/issues/80)) ([ad1f616](https://github.com/arcboxlabs/arcbox/commit/ad1f616d67354fc7af054e7942f0df4fdc62ebcc))


### Bug Fixes

* **docker:** repair HTTP upgrade proxy for BuildKit and attach ([#105](https://github.com/arcboxlabs/arcbox/issues/105)) ([bf3768d](https://github.com/arcboxlabs/arcbox/commit/bf3768d317e6471b81364f7855db93ebd607ee4a))

## [0.3.6](https://github.com/arcboxlabs/arcbox/compare/v0.3.5...v0.3.6) (2026-03-24)


### Features

* **api:** enable `devicon` for `IconService` ([#102](https://github.com/arcboxlabs/arcbox/issues/102)) ([315df2a](https://github.com/arcboxlabs/arcbox/commit/315df2aea5fdfbc1ba5d7085ef05a9872fb69ece))


### Bug Fixes

* **daemon:** address review feedback on stale cleanup ([4e9f48c](https://github.com/arcboxlabs/arcbox/commit/4e9f48c8ce660ca739bf9bcf359dfffb23dbf807))
* **daemon:** clean up stale state before startup ([#73](https://github.com/arcboxlabs/arcbox/issues/73)) ([4e9f48c](https://github.com/arcboxlabs/arcbox/commit/4e9f48c8ce660ca739bf9bcf359dfffb23dbf807))
* **log:** clean up guest console log formatting ([#67](https://github.com/arcboxlabs/arcbox/issues/67)) ([6f79688](https://github.com/arcboxlabs/arcbox/commit/6f79688b8a02fe22f20161d480e061b04adb726e))

## [0.3.5](https://github.com/arcboxlabs/arcbox/compare/v0.3.4...v0.3.5) (2026-03-23)


### Bug Fixes

* **agent:** remove blocking ntpd sync from runtime prerequisites ([#95](https://github.com/arcboxlabs/arcbox/issues/95)) ([c5969ff](https://github.com/arcboxlabs/arcbox/commit/c5969fff4c8b37b45ca56cb808edccb0150f048e))

## [0.3.4](https://github.com/arcboxlabs/arcbox/compare/v0.3.3...v0.3.4) (2026-03-23)


### Bug Fixes

* **agent:** disable jailer and update sandbox paths for virtiofs mount ([c4a8355](https://github.com/arcboxlabs/arcbox/commit/c4a8355b1f471997cd6d868fb605a8ee5e712e69))
* **net:** create per-SYN listen sockets for concurrent connections ([#97](https://github.com/arcboxlabs/arcbox/issues/97)) ([65d6a53](https://github.com/arcboxlabs/arcbox/commit/65d6a536adb567339004e22505ceed11482c9bfc))
* **net:** harden outbound network stack (P0 + P1) ([#98](https://github.com/arcboxlabs/arcbox/issues/98)) ([aa0f8dc](https://github.com/arcboxlabs/arcbox/commit/aa0f8dc7c586bd424636d05832d0062370be7bcf))


### Code Refactoring

* **api:** split grpc.rs into per-service modules ([#93](https://github.com/arcboxlabs/arcbox/issues/93)) ([fa597ba](https://github.com/arcboxlabs/arcbox/commit/fa597bacf7269da4f237babe9edda315553976af))

## [0.3.3](https://github.com/arcboxlabs/arcbox/compare/v0.3.2...v0.3.3) (2026-03-22)


### Features

* **api:** add IconService gRPC for container image icon lookups ([6f7abf5](https://github.com/arcboxlabs/arcbox/commit/6f7abf53c6fd2a75f3d92f35aacc86233a865814))


### Bug Fixes

* **api:** bump dimicon to 0.1.0 for stable API ([19aa521](https://github.com/arcboxlabs/arcbox/commit/19aa52152e16c107cbd62bb5b319c9b875bb2723))
* **helper:** add missing cli_link/cli_unlink stubs, rename misleading test ([01c65e4](https://github.com/arcboxlabs/arcbox/commit/01c65e4326cb4e10d617634c4ae8ce23c1313e6b))
* **helper:** harden input validation in validate.rs ([ff57feb](https://github.com/arcboxlabs/arcbox/commit/ff57feb6e5141a525af6a1ad4119e32036d4034f))


### Code Refactoring

* **api:** extract icon-to-response conversion via From&lt;ResolvedIcon&gt; ([4def9a4](https://github.com/arcboxlabs/arcbox/commit/4def9a471d30f5ba4139bb1d338b5effd40c4ea7))
* **api:** rename IconService field `reference` to `fqin` ([34188ad](https://github.com/arcboxlabs/arcbox/commit/34188ad468bc54999df7cbd13e237dc4dafe5529))
* **helper:** apply newtype pattern to validation types ([de9261e](https://github.com/arcboxlabs/arcbox/commit/de9261e03cefcc718243f42a9470268f6e7cf8cb))
* **helper:** push validation to RPC boundary, mutations accept strong types ([1015bc1](https://github.com/arcboxlabs/arcbox/commit/1015bc11f71d38038d2c58ed60ce9d28e35be7dd))
* **helper:** remove separator comments, split rpc_test.rs ([9268410](https://github.com/arcboxlabs/arcbox/commit/92684109022956be62395a1e02569a457e735389))
* **helper:** split validate.rs into per-type modules ([6dc9c29](https://github.com/arcboxlabs/arcbox/commit/6dc9c29b49914d38ac46a8f224493994f32e6a6d))


### Tests

* **helper:** align mock servers with newtype parse pattern ([a856b52](https://github.com/arcboxlabs/arcbox/commit/a856b52f6dca6cb1c18be2d4c12cd6a30942dd25))


### Styles

* cargo fmt arcbox-helper ([340553f](https://github.com/arcboxlabs/arcbox/commit/340553f5d9623cd47b50ea14defddd161f3048de))
* cargo fmt validate/mod.rs ([6ca69db](https://github.com/arcboxlabs/arcbox/commit/6ca69db21c49b987d1c7250471b1d26f2167316e))
* fix import ordering in icon_test ([ce20b5c](https://github.com/arcboxlabs/arcbox/commit/ce20b5c0361f99ede05c6f114aa69497f312bc37))

## [0.3.2](https://github.com/arcboxlabs/arcbox/compare/v0.3.1...v0.3.2) (2026-03-21)


### Features

* daemon self-provisioning — desktop becomes pure display layer ([#89](https://github.com/arcboxlabs/arcbox/issues/89)) ([eedcdeb](https://github.com/arcboxlabs/arcbox/commit/eedcdebe4376fe9030d51479c6ac2c7407f9b19d))

## [0.3.1](https://github.com/arcboxlabs/arcbox/compare/v0.3.0...v0.3.1) (2026-03-20)


### Bug Fixes

* **helper:** move clippy allow to function level for CI compatibility ([e4fca5d](https://github.com/arcboxlabs/arcbox/commit/e4fca5df96c7e9eabb54d9580de5f86a1c35c0dd))
* **helper:** use isize::try_from instead of function-level allow for cast ([945b2bb](https://github.com/arcboxlabs/arcbox/commit/945b2bb83f8b70ab63d276f37ce2025962d68cc7))


### Styles

* cargo fmt peer_auth.rs ([bd6c8ab](https://github.com/arcboxlabs/arcbox/commit/bd6c8abbf4a1619718f7cb94a30f3012c772458b))

## [0.3.0](https://github.com/arcboxlabs/arcbox/compare/v0.2.7...v0.3.0) (2026-03-20)


### Bug Fixes

* **ci:** ad-hoc sign arcbox-helper before smoke test ([#84](https://github.com/arcboxlabs/arcbox/issues/84)) ([ba6759a](https://github.com/arcboxlabs/arcbox/commit/ba6759a01c7b95138bd7492144d28fbbae0ebf61))


### Code Refactoring

* **daemon:** decouple arcbox-desktop from arcbox-daemon ([#81](https://github.com/arcboxlabs/arcbox/issues/81)) ([afe6593](https://github.com/arcboxlabs/arcbox/commit/afe6593cf7cc826a5640cf719001414300bb6a9f))

## [0.2.7](https://github.com/arcboxlabs/arcbox/compare/v0.2.6...v0.2.7) (2026-03-18)


### Bug Fixes

* **net:** add retry logic to route reconciler ([a23bee1](https://github.com/arcboxlabs/arcbox/commit/a23bee1fd7586efa8a69349688b10b5b145b3e44))
* **net:** add retry logic to route reconciler ([#77](https://github.com/arcboxlabs/arcbox/issues/77)) ([0f8d304](https://github.com/arcboxlabs/arcbox/commit/0f8d304b8791ecd3d48ae8910fd4138bc5ba6da9))


### Styles

* **net:** fix formatting in route reconciler and daemon ([f2d3a9d](https://github.com/arcboxlabs/arcbox/commit/f2d3a9ddf37643329e1919e6eac7edf1536fb44e))

## [0.2.6](https://github.com/arcboxlabs/arcbox/compare/v0.2.5...v0.2.6) (2026-03-17)


### Code Refactoring

* **net:** replace text parsing with system APIs for route management ([#76](https://github.com/arcboxlabs/arcbox/issues/76)) ([5b226fa](https://github.com/arcboxlabs/arcbox/commit/5b226fa95fa83abc523009e7f5ce41954b03f204))

## [0.2.5](https://github.com/arcboxlabs/arcbox/compare/v0.2.4...v0.2.5) (2026-03-17)


### Features

* **cli:** add `abctl doctor` diagnostic command ([078f62f](https://github.com/arcboxlabs/arcbox/commit/078f62f4b725856e58f539ea8259ff1b2872ae82))
* **cli:** add `abctl uninstall` command ([16c19f5](https://github.com/arcboxlabs/arcbox/commit/16c19f5c78dda3af9d45a0af1ab9c283f53a5460))


### Bug Fixes

* **cli:** add login item approval reset as explicit uninstall step ([7a87010](https://github.com/arcboxlabs/arcbox/commit/7a87010d47523d48ac1627ec04f14b6a3d2cd3e5))
* **daemon:** address review findings for stale state cleanup ([66f0afc](https://github.com/arcboxlabs/arcbox/commit/66f0afc4b2478232dd01e366b7904f5e9ac25f32))


### Documentation

* **readme:** add desktop, discord, telegram, and docs badges ([21a49c3](https://github.com/arcboxlabs/arcbox/commit/21a49c3ae6828e7ec21f6e5c738aa9efdf853796))


### Styles

* cargo fmt ([4ae09c7](https://github.com/arcboxlabs/arcbox/commit/4ae09c756d7c3d0ca2d38c01870f6c7a1d1cd453))

## [0.2.4](https://github.com/arcboxlabs/arcbox/compare/v0.2.3...v0.2.4) (2026-03-17)


### Bug Fixes

* **daemon:** clean up stale state before startup ([3e48003](https://github.com/arcboxlabs/arcbox/commit/3e48003baaca58b37224958e31a7086a3ba258ee))
* **net:** change custom network stack subnet from 192.168.64.0/24 to 10.0.2.0/24 ([c1dd477](https://github.com/arcboxlabs/arcbox/commit/c1dd477c2fe5c356bbe6ecaaa0339edb7d5bdbf1))


### Miscellaneous Chores

* **release:** include all conventional commit types in changelog ([aa81671](https://github.com/arcboxlabs/arcbox/commit/aa8167194ac45011ea70f4cde1273f4c21a9ed7e))

## [0.2.3](https://github.com/arcboxlabs/arcbox/compare/v0.2.2...v0.2.3) (2026-03-16)


### Bug Fixes

* **build:** remove restricted com.apple.vm.networking entitlement ([63f96d2](https://github.com/arcboxlabs/arcbox/commit/63f96d2d03e8af620363b28d5094098c3f191e48))

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
