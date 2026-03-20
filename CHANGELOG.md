# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/arcboxlabs/arcbox/compare/v0.3.0...v0.2.0) (2026-03-20)


### Features

* add boot assets management and VM lifecycle improvements ([4e2b05d](https://github.com/arcboxlabs/arcbox/commit/4e2b05d75a08a65674b3b279551372970ed8af1d))
* **agent:** add arcbox-agent guest crate ([edafd44](https://github.com/arcboxlabs/arcbox/commit/edafd4460b719c67ad05fb988675539e5082f22e))
* **agent:** add blanket iptables FORWARD rules for sandbox subnet ([599e596](https://github.com/arcboxlabs/arcbox/commit/599e5969ea5256538a7c9e3689421172166cf9a0))
* **agent:** add container logs streaming support ([bf2dc54](https://github.com/arcboxlabs/arcbox/commit/bf2dc544d4c6aa9d55a635bb32c6435a1c99750c))
* **agent:** add guest DNS server and Docker event listener (Phase 1) ([ef9da60](https://github.com/arcboxlabs/arcbox/commit/ef9da603dfa2023c1b514e72309259debd9d0dc1))
* **agent:** add init module for PID 1 system initialization ([af82598](https://github.com/arcboxlabs/arcbox/commit/af82598fcfc25d1d096cad58d1fcc0ff557210df))
* **agent:** add PortForwardManager for iptables DNAT sandbox port forwarding ([98d58df](https://github.com/arcboxlabs/arcbox/commit/98d58df71d6a8b52e233692cc46e7898a6d30f2d))
* **agent:** add supervisor module and wire PID 1 path in main ([246509c](https://github.com/arcboxlabs/arcbox/commit/246509c8aae136f961e1a156a09430a9a035e837))
* **agent:** bootstrap bundled runtime using boot asset version ([7203619](https://github.com/arcboxlabs/arcbox/commit/7203619911d162da60929b53bac687f4b4be04db))
* **agent:** idempotent EnsureRuntime with state machine and per-service status ([5585298](https://github.com/arcboxlabs/arcbox/commit/55852986700e7762eb8c03a4bc2aa09e74419a33))
* **agent:** implement container process shim for TTY log capture ([705465c](https://github.com/arcboxlabs/arcbox/commit/705465ce2bb567e5fcbae8db23ed2aeee68abb5f))
* **agent:** implement container stdout/stderr log capture ([5847a8f](https://github.com/arcboxlabs/arcbox/commit/5847a8fb9b553439080e7eabe2c9071f22fb2d39))
* **agent:** integrate PortForwardManager into sandbox dispatch and cleanup ([be9d3e6](https://github.com/arcboxlabs/arcbox/commit/be9d3e63a7c40108c63b73c99777c71c6a539b50))
* **agent:** register sandbox DNS in /etc/hosts on create/restore ([27fc18d](https://github.com/arcboxlabs/arcbox/commit/27fc18d03e7d82960a1d5b11157d5bae94645436))
* **agent:** RPC V2 wire format with trace ID propagation ([afdddcd](https://github.com/arcboxlabs/arcbox/commit/afdddcdf1d719573f93782ebf9f22f469bd507ec))
* **agent:** switch data volume from ext4 to Btrfs with subvolumes ([bf30528](https://github.com/arcboxlabs/arcbox/commit/bf30528077b7b644fb36d5c0c5542b8b41798928))
* **agent:** sync guest clock from host ping and remove NTP bootstrap ([#25](https://github.com/arcboxlabs/arcbox/issues/25)) ([6a79134](https://github.com/arcboxlabs/arcbox/commit/6a791341ae6300a474b912bf2dfc44107d95e69f))
* **api:** add arcbox-api crate ([584acc3](https://github.com/arcboxlabs/arcbox/commit/584acc3cc574af7481d0f7ff526b5efc97ad8787))
* **api:** implement gRPC service handlers ([6a86835](https://github.com/arcboxlabs/arcbox/commit/6a8683553135c9a763594eb951e15fc20f6a3f8e))
* **api:** implement real image pull with progress streaming ([fbe6e52](https://github.com/arcboxlabs/arcbox/commit/fbe6e52c7628fc1764c413dd3cf96332a96388ec))
* **boot_assets:** squashfs rootfs support, bump to alpha.25 ([2626a09](https://github.com/arcboxlabs/arcbox/commit/2626a09efa3a4cf7f0a2e81b6e9a9dbed68e36cc))
* **boot:** enforce manifest/checksum and align dev boot scripts ([9136ad1](https://github.com/arcboxlabs/arcbox/commit/9136ad1df8455c715ed593190bbf1d0625647ca0))
* **ci:** add E2E tests GitHub Actions workflow and docs ([e34163e](https://github.com/arcboxlabs/arcbox/commit/e34163e63d589c832a5705a929c89ee5cbee2293))
* CLI rename, asset management, Docker tools bundling ([#33](https://github.com/arcboxlabs/arcbox/issues/33)) ([92e4052](https://github.com/arcboxlabs/arcbox/commit/92e40522901d769bd35b1223978ab970f72d3dd7))
* **cli:** add `abctl doctor` diagnostic command ([078f62f](https://github.com/arcboxlabs/arcbox/commit/078f62f4b725856e58f539ea8259ff1b2872ae82))
* **cli:** add `abctl uninstall` command ([16c19f5](https://github.com/arcboxlabs/arcbox/commit/16c19f5c78dda3af9d45a0af1ab9c283f53a5460))
* **cli:** add daemon command for background service ([9561f13](https://github.com/arcboxlabs/arcbox/commit/9561f13cdcdecb8d667c3600ddac203639a4681e))
* **cli:** add daemon status and robust shutdown checks ([8d3fc07](https://github.com/arcboxlabs/arcbox/commit/8d3fc07beaac8dfb111b6da2e52619c6ebcda288))
* **cli:** add diagnose command for system health checks ([3d68277](https://github.com/arcboxlabs/arcbox/commit/3d68277de1fadcf67274cc52fb8ebf2f88d63550))
* **cli:** add JSON output and --offline flag to boot commands ([fc5ee1d](https://github.com/arcboxlabs/arcbox/commit/fc5ee1dfd4d5c9cfc67f5b0687d3d0e133fb4fb3))
* **cli:** CLI improvements ([4da7ffc](https://github.com/arcboxlabs/arcbox/commit/4da7ffc820e60f4c4471e8229fbc360a2e2649b7))
* **cli:** daemonize daemon command and add stop action ([c2aa07a](https://github.com/arcboxlabs/arcbox/commit/c2aa07acb71d2acac2d160443eee8a842003a46e))
* **cli:** implement daemon client and connect container commands ([cabda22](https://github.com/arcboxlabs/arcbox/commit/cabda2252b88044f68292f856aaf47609e4184f0))
* **cli:** implement run, exec, and logs commands ([2ceaebf](https://github.com/arcboxlabs/arcbox/commit/2ceaebfc78e74d32607fb9e86071381bbf9f896b))
* **cli:** require daemon action and add explicit start ([3a252a5](https://github.com/arcboxlabs/arcbox/commit/3a252a548a9b5952464df9596db2c907c73c3ae6))
* **cli:** show machine IP and guest runtime status on status/start ([f8ae38b](https://github.com/arcboxlabs/arcbox/commit/f8ae38b097e1ffa8c8538792fd0cae85b597b361))
* container DNS resolution via *.arcbox.local ([#36](https://github.com/arcboxlabs/arcbox/issues/36)) ([725bf5d](https://github.com/arcboxlabs/arcbox/commit/725bf5ddb846f8f50be9f58400372669e79a3e1f))
* container networking, Docker compat improvements, and release infrastructure ([239cbcd](https://github.com/arcboxlabs/arcbox/commit/239cbcde8d20acda6456c8c629e58edb4046461e))
* **container:** add arcbox-container crate ([dc7a5c8](https://github.com/arcboxlabs/arcbox/commit/dc7a5c83803b78ab518da2890e49a9378ecfd8ae))
* **container:** enhance container manager and OCI support ([8ec833a](https://github.com/arcboxlabs/arcbox/commit/8ec833a88d364abccdcb87ccf610a5ab19753556))
* **container:** fix force remove state machine and exit notification ([e300856](https://github.com/arcboxlabs/arcbox/commit/e30085645214b80976ae552b23055b9483843541))
* **container:** implement complete container rootfs and isolation ([0e72dbc](https://github.com/arcboxlabs/arcbox/commit/0e72dbcf9659ed77d782ccedc5723a94976fba0e))
* **core,vmm:** add vsock connection for Container-Agent communication ([a97edcb](https://github.com/arcboxlabs/arcbox/commit/a97edcbf080c6322d4354fb8fe89e83ca4dea408))
* **core,vmm:** integrate VirtioFS configuration across layers ([cf6d718](https://github.com/arcboxlabs/arcbox/commit/cf6d718a03fd7e3d814c1a040c86be6842b1d020))
* **core:** add AgentClient and AgentPool ([002f81b](https://github.com/arcboxlabs/arcbox/commit/002f81b21d95de7e29ef6030cb6343113b3454ba))
* **core:** add graceful stop path for VM and machine managers ([37922db](https://github.com/arcboxlabs/arcbox/commit/37922db0b003ccb303a4f1471daba83289369dfd))
* **core:** add machine tracking and image pull progress ([1adcec6](https://github.com/arcboxlabs/arcbox/commit/1adcec6dc5f3a805e7ae604db12ee1794d526142))
* **core:** add public port forwarding methods for smart proxy ([8ffc93a](https://github.com/arcboxlabs/arcbox/commit/8ffc93a242ded9913a15c9002551b7f116a20223))
* **core:** add Runtime::get_agent() for macOS vsock connections ([70893b8](https://github.com/arcboxlabs/arcbox/commit/70893b8587524cb92180bc8945589990663d0332))
* **core:** add sandbox_port_forward/remove to AgentClient ([b44a64a](https://github.com/arcboxlabs/arcbox/commit/b44a64a34988f2b233051ad08be4b35fc48388d1))
* **core:** eagerly boot default vm during runtime init ([db67645](https://github.com/arcboxlabs/arcbox/commit/db67645362c0a32c53fed4a56c532d51a9d8b97e))
* **core:** prefer graceful shutdown in runtime lifecycle ([0f95ace](https://github.com/arcboxlabs/arcbox/commit/0f95ace64c90b7fae1e8859f587947ffbfd3933d))
* **core:** replace boot_assets with arcbox-boot thin wrapper ([2f24079](https://github.com/arcboxlabs/arcbox/commit/2f240795f914a1f1c322fa41a69c9ca0ac4bfe6a))
* **core:** schema v6 — EROFS rootfs, remove initramfs and legacy boot paths ([023b7bb](https://github.com/arcboxlabs/arcbox/commit/023b7bbd2d21da3808381879e76cf4450af94c14))
* **core:** trace ID task-local and guest Docker HTTP status propagation ([b2abc23](https://github.com/arcboxlabs/arcbox/commit/b2abc23c2f4dfc7a36f9699bb2cef3d737c2245b))
* **dns:** add arcbox-dns crate for shared DNS packet parsing ([b55ed1f](https://github.com/arcboxlabs/arcbox/commit/b55ed1f6eaebf44ff519b2caa8d5813fe596b72b))
* **dns:** share DNS hosts table between host DnsService and VMM datapath (Phase 2) ([fa4440e](https://github.com/arcboxlabs/arcbox/commit/fa4440e6f9b9622e4105d299b75180caf2d79844))
* **docker:** add arcbox-docker crate ([ce8e899](https://github.com/arcboxlabs/arcbox/commit/ce8e8996613315d33f6f6caf3a4799e51b377d5a))
* **docker:** add context management ([8c0a6f9](https://github.com/arcboxlabs/arcbox/commit/8c0a6f97ea36e35ba1f5690bef46803cb41ea7ae))
* **docker:** add smart proxy infrastructure for guest dockerd ([0666fe3](https://github.com/arcboxlabs/arcbox/commit/0666fe33a9f8747e56aa7e650c797bbc66ef4932))
* **docker:** align wait/exec/resolve semantics and add SSOT proxy ([7dc11f7](https://github.com/arcboxlabs/arcbox/commit/7dc11f769e83fa8b41da1a47cda782c71ce34fed))
* **docker:** auto-install CLI tools from app bundle on Desktop launch ([#34](https://github.com/arcboxlabs/arcbox/issues/34)) ([e4a21e1](https://github.com/arcboxlabs/arcbox/commit/e4a21e1a280acc33207371ad70ec24a4ad6d963d))
* **docker:** implement container pause/unpause ([588f063](https://github.com/arcboxlabs/arcbox/commit/588f0637d88e792f0faefdb940cff20d8d139221))
* **docker:** implement container stats and top commands ([95a94c6](https://github.com/arcboxlabs/arcbox/commit/95a94c6998e9912179247deaca0c6aa28a31368d))
* **docker:** implement Docker API handlers and managers ([6549e35](https://github.com/arcboxlabs/arcbox/commit/6549e359a4ce426d746d0f41a2281e3da89abac9))
* **docker:** implement Docker context management ([a4a23fc](https://github.com/arcboxlabs/arcbox/commit/a4a23fc0e39c17746712b20827fd22573d457c39))
* **docker:** improve cli compatibility and runtime behavior ([9bcec0f](https://github.com/arcboxlabs/arcbox/commit/9bcec0fecd479590dda5420da235d65c6d9ca0d3))
* **docker:** merge guest info into /info response ([a9b2509](https://github.com/arcboxlabs/arcbox/commit/a9b2509b307c4dfd14b2e267d31eefbb7b6f996d))
* **e2e:** add smoke matrix tests for backend x distro coverage ([655b7e0](https://github.com/arcboxlabs/arcbox/commit/655b7e0f51803c94d2f2738b91626ef59b6f0ad9))
* enhance API, transport, and agent components ([24ce1ed](https://github.com/arcboxlabs/arcbox/commit/24ce1edf153f9427c7feda680925faffe8ea78bd))
* **fs:** filesystem improvements ([97b96f3](https://github.com/arcboxlabs/arcbox/commit/97b96f3a3f68bc5100f9cb14aaa929b459d43c17))
* **fs:** implement complete PassthroughFs with comprehensive tests ([423453a](https://github.com/arcboxlabs/arcbox/commit/423453a1864d054f1b66ab77b5cd2220581a22da))
* **grpc:** add arcbox-grpc crate for gRPC client/server ([4e28e20](https://github.com/arcboxlabs/arcbox/commit/4e28e202239533591814f09a30bded7e2918158c))
* **helper:** add privileged helper for utun/route operations ([32d2c23](https://github.com/arcboxlabs/arcbox/commit/32d2c23c68c77018337fa0a67695d7956ec1ce3c))
* **helper:** privileged network helper with fd passing and hello handshake ([746aaea](https://github.com/arcboxlabs/arcbox/commit/746aaea2eff8974ac685339c89248fa22fd24f3e))
* **hypervisor:** add arcbox-vm Firecracker sandbox library ([#11](https://github.com/arcboxlabs/arcbox/issues/11)) ([009c125](https://github.com/arcboxlabs/arcbox/commit/009c125f48f9066e9551b09ca434247d57459323))
* **hypervisor:** add device configuration and balloon support ([44d9244](https://github.com/arcboxlabs/arcbox/commit/44d9244c75c49ae3e5a04c6170a1cd5a5dbd6791))
* **hypervisor:** add interactive console mode to arcbox-boot ([f9dda95](https://github.com/arcboxlabs/arcbox/commit/f9dda9598065b65210e47b30aa6325048d60c5ba))
* **hypervisor:** add performance benchmark example ([8159e8f](https://github.com/arcboxlabs/arcbox/commit/8159e8f2724c71a529e724e4432b3d744c77378d))
* **hypervisor:** add VirtioFS option to boot_vm example ([4462554](https://github.com/arcboxlabs/arcbox/commit/44625547e079f274bb7c92d2675501873827b080))
* **hypervisor:** add vsock connection test to boot_vm example ([9ba900c](https://github.com/arcboxlabs/arcbox/commit/9ba900c79bdad14bbcf660ce97da8889654abd22))
* **hypervisor:** convert examples to bins with clap, add snapshot and dirty tracking tests ([c065ce6](https://github.com/arcboxlabs/arcbox/commit/c065ce683231f192b240246631f0cc8511f9e803))
* **hypervisor:** implement macOS vsock FFI bindings ([da188f9](https://github.com/arcboxlabs/arcbox/commit/da188f923c3eb1836613b7e983ce6916190691e3))
* **hypervisor:** improve serial console and vsock connection ([64fd0aa](https://github.com/arcboxlabs/arcbox/commit/64fd0aa3494a399ba270f86e56010fdbe3a12a39))
* **image:** implement container image pull from OCI registries ([bc91f95](https://github.com/arcboxlabs/arcbox/commit/bc91f9534c1941b75468cc8a071aedf7000e4f0e))
* implement placeholder functionality across multiple crates ([7e4dc47](https://github.com/arcboxlabs/arcbox/commit/7e4dc47dc05f5e4c9ef8879659155dee44acccdd))
* **irq:** implement real IRQ delivery mechanism ([4001c6b](https://github.com/arcboxlabs/arcbox/commit/4001c6bae30badef9fd4f08680b6c1b76acc1fd6))
* **machine:** route machine CLI through daemon gRPC ([1bf7f26](https://github.com/arcboxlabs/arcbox/commit/1bf7f26bfa6ccc1bbbd974413ba7f37824f52dbf))
* migrate CDN base URL to boot.arcboxcdn.com ([1042f5c](https://github.com/arcboxlabs/arcbox/commit/1042f5ca536e0ed0e623784fe9668cef1703fc51))
* migrate from release-plz to release-please ([51472a2](https://github.com/arcboxlabs/arcbox/commit/51472a2158b6998c674adff6ddb782efd63ced7f))
* **net:** add async network datapath event loop ([83580d5](https://github.com/arcboxlabs/arcbox/commit/83580d5f41f58cde13192563c6dfcf3bb2e7c755))
* **net:** add Ethernet frame handling and ARP responder ([95593f4](https://github.com/arcboxlabs/arcbox/commit/95593f4bcfcc2e2d4ca6a78c62a17eb539adef28))
* **net:** add InboundRelay core struct for TCP inbound port forwarding ([8781e21](https://github.com/arcboxlabs/arcbox/commit/8781e2116fdfefdc24d9740483c89beae2e5f589))
* **net:** add L3 tunnel service with bidirectional utun routing (Phase 3) ([13499c6](https://github.com/arcboxlabs/arcbox/commit/13499c664a65e9c274678463de7f6fca390e74a8))
* **net:** add smoltcp device adapter for guest socketpair ([0b45171](https://github.com/arcboxlabs/arcbox/commit/0b4517141037b75391191c9d12104a3e4862342d))
* **net:** add socket proxy for ICMP, UDP, and TCP ([5f32ea1](https://github.com/arcboxlabs/arcbox/commit/5f32ea1ebeed3bddfc14dd267a3f1abef60d5902))
* **net:** add TCP packet builder to ethernet module ([54298d0](https://github.com/arcboxlabs/arcbox/commit/54298d03ef1eafd1957777e27465b2fa179293fb))
* **net:** add UDP inbound and InboundListenerManager ([6ebfbb1](https://github.com/arcboxlabs/arcbox/commit/6ebfbb1ddefec394e643aa569227836ed428bb6d))
* **net:** daemon owns route lifecycle via arcbox-helperctl ([7979aac](https://github.com/arcboxlabs/arcbox/commit/7979aac6b720a9ca6022397ac6aae1c551d4f3bf))
* **net:** daemon uses helper for utun creation via fd passing (Step 2) ([2264fcb](https://github.com/arcboxlabs/arcbox/commit/2264fcbca9240a1bd230a5b034ab7a1166a715d7))
* **net:** implement ARM64 NEON checksum optimization ([06bbeb9](https://github.com/arcboxlabs/arcbox/commit/06bbeb9a7fb8b185fa81d01eff761dc2d7b55223))
* **net:** implement inbound tcp via smoltcp active connect ([018c493](https://github.com/arcboxlabs/arcbox/commit/018c493db085836bd4f6e28756529db2c222219d))
* **net:** implement mDNS responder for container discovery ([c8eea2d](https://github.com/arcboxlabs/arcbox/commit/c8eea2d784188e55e82787d7699a52cc4667bd7a))
* **net:** implement outbound tcp bridge via smoltcp socket pool ([b2ea39a](https://github.com/arcboxlabs/arcbox/commit/b2ea39a9e6798a67d88326eae3fd46e1cd15d4c2))
* **net:** implement vmnet.framework backend for macOS networking ([7a61543](https://github.com/arcboxlabs/arcbox/commit/7a615437b4b832cc65a259dd93fe19be9883e68b))
* **net:** L3 direct routing via vmnet bridge (replaces utun approach) ([1b05e30](https://github.com/arcboxlabs/arcbox/commit/1b05e304b77cefa337cbd8b0f8d9c35accccaee8))
* **net:** proxy ARP on bridge NIC, eliminates gateway IP discovery ([a03d5c8](https://github.com/arcboxlabs/arcbox/commit/a03d5c8653201d0a8db1b36ceac80d3aa991c6a8))
* **net:** sandbox DNS, broader subnet routing, dead code cleanup (Phase 4-6) ([96b7b73](https://github.com/arcboxlabs/arcbox/commit/96b7b7357af0d8cc4db5637c813930bc507b73aa))
* **net:** wire inbound port forwarding through VMM to Docker API ([a88a02e](https://github.com/arcboxlabs/arcbox/commit/a88a02ed0a534c20ea168b41b6d6e3da6b8849cd))
* **oci:** add arcbox-oci crate ([b4d2730](https://github.com/arcboxlabs/arcbox/commit/b4d273017b32293117732064aa39857e106d19b7))
* **pro:** add pro layer crates ([3e25810](https://github.com/arcboxlabs/arcbox/commit/3e2581022f086ff47a635859bdc255d698383109))
* **proto:** add SandboxPortForward request/response messages ([7a6643f](https://github.com/arcboxlabs/arcbox/commit/7a6643fb0d7576ecaeecc7561fdcbf29121da8e4))
* **protocol:** add arcbox-protocol crate ([d3aa72e](https://github.com/arcboxlabs/arcbox/commit/d3aa72e290c24e0699a32b536fb648c97445e895))
* **protocol:** add EnsureRuntime status and ServiceStatus message ([22b7974](https://github.com/arcboxlabs/arcbox/commit/22b797426f7d72a2f412f7738abf9dff1b6a0fad))
* **protocol:** add port binding notification messages ([6408542](https://github.com/arcboxlabs/arcbox/commit/6408542c0fcbda2ba702b22248d2c04ea52ee331))
* **release:** auto-update arcbox-desktop version on release ([500aee5](https://github.com/arcboxlabs/arcbox/commit/500aee506413466e74784f066209e7352263a3fb))
* replace hardcoded boot asset constants with boot-assets.lock ([06e94af](https://github.com/arcboxlabs/arcbox/commit/06e94afc68a4008030b70ecc316fe51b77d094d2))
* **runtime:** add guest-docker proxy backend and runtime readiness RPC ([d4eeb4c](https://github.com/arcboxlabs/arcbox/commit/d4eeb4c322183f29cdd93543d8312f7d684d2a9c))
* **runtime:** implement port forwarding and fix container entrypoint ([eda46e7](https://github.com/arcboxlabs/arcbox/commit/eda46e7641ba4751d39f9b40b48db16cb7ec47f3))
* **runtime:** persist dockerd data on block device ([e270a6b](https://github.com/arcboxlabs/arcbox/commit/e270a6be708bbcc18d69f44709de33d106c6ea37))
* **sandbox:** file I/O and clock sync over vsock ([#35](https://github.com/arcboxlabs/arcbox/issues/35)) ([286b947](https://github.com/arcboxlabs/arcbox/commit/286b9478e7c0741464602ad29f92881c53b35ebd))
* share /Users via VirtioFS and add rootfs/modloop boot assets ([8ca1b09](https://github.com/arcboxlabs/arcbox/commit/8ca1b090755ce993b4aaa05eeb1d83ef1cda97e2))
* **telemetry:** support per-app prefixed Sentry env vars ([b1ccb9b](https://github.com/arcboxlabs/arcbox/commit/b1ccb9bdd1537a6da31a1457b7c2c48709a2786f))
* transparent /Users path mapping for Docker bind mounts ([6d217c3](https://github.com/arcboxlabs/arcbox/commit/6d217c37d1f58205b24c6d761c01b493e797f4a4))
* **transport:** add arcbox-transport crate ([098f89d](https://github.com/arcboxlabs/arcbox/commit/098f89d624c4e8ce90beee81bab9ed21e0e3eabf))
* **transport:** add macOS vsock stream support ([e1a2db4](https://github.com/arcboxlabs/arcbox/commit/e1a2db479aa6ff04b34722198cb134ec38fdf9e9))
* **transport:** replace vsock busy-polling with AsyncFd, add full-duplex split API ([#45](https://github.com/arcboxlabs/arcbox/issues/45)) ([ba9ee1a](https://github.com/arcboxlabs/arcbox/commit/ba9ee1ac305714390a32ddaa8bad96452ac150aa))
* **virtfs:** add home directory VirtioFS share support ([f1cb815](https://github.com/arcboxlabs/arcbox/commit/f1cb8156d2b393d0bb59fd9112c6f5149fadb07a))
* **vmm:** add ACPI graceful stop request API ([8777d77](https://github.com/arcboxlabs/arcbox/commit/8777d77ff8ebb561331fe444dd775403f9c73935))
* **vmm:** integrate FileHandle network device with custom datapath ([c51d0d7](https://github.com/arcboxlabs/arcbox/commit/c51d0d751c518295f925389809a0fa24ba8eaafa))
* **vmm:** integrate L3 tunnel into VMM and runtime (Phase 3) ([1edb1fc](https://github.com/arcboxlabs/arcbox/commit/1edb1fc6a06738439c7313a90996966eea7581ad))
* **vmm:** VMM improvements ([e2c456b](https://github.com/arcboxlabs/arcbox/commit/e2c456b7a8f540601cd115149750945377141f86))
* **vz:** add arcbox-vz crate with VirtioFS and vsock support ([23a9101](https://github.com/arcboxlabs/arcbox/commit/23a91017132e17778242d3b21c77eb04c6e9f210))
* **vz:** add VZFileHandleNetworkDeviceAttachment support ([7867227](https://github.com/arcboxlabs/arcbox/commit/786722779243188077ed2384afd532344df261e2))
* **vz:** enable nested virtualization on macOS 15+ with M3+ ([36d891b](https://github.com/arcboxlabs/arcbox/commit/36d891b78063bb3b873e968c2ba99bafcdf70601))
* **wire:** add SandboxPortForward request/response message types ([35e4517](https://github.com/arcboxlabs/arcbox/commit/35e4517fbed5025cd955117da523b9d63d9942fa))


### Bug Fixes

* address new review comments ([3ed2c3b](https://github.com/arcboxlabs/arcbox/commit/3ed2c3b15d0d92394fad84a84dcaae6e9c78368f))
* address PR review comments ([11116d0](https://github.com/arcboxlabs/arcbox/commit/11116d0b4d38d2c5afa913215a70b734345ee8d0))
* address review comments and cargo fmt issues ([d9233c9](https://github.com/arcboxlabs/arcbox/commit/d9233c99d2f8791bff1b5ddbf569146b1bfe7ae7))
* **agent:** add missing STATUS_STARTED re-export in ensure_runtime ([8e578e0](https://github.com/arcboxlabs/arcbox/commit/8e578e0976e0f8b07a346a734d4da6d7d2287761))
* **agent:** configure guest primary NIC via DHCP in PID1 init ([5d43bef](https://github.com/arcboxlabs/arcbox/commit/5d43befe8b3e30121a158ec0cb46950d63e2e3cd))
* **agent:** configure guest primary NIC via DHCP in PID1 init ([e934115](https://github.com/arcboxlabs/arcbox/commit/e934115fda38340cacddf9e2c105b64b13305d95))
* **agent:** decode stop/remove request once, return 400 on failure ([d118d76](https://github.com/arcboxlabs/arcbox/commit/d118d76021f4f9c8f92b93fdac1cc36bcabaf963))
* **agent:** delete iptables rules before removing allocation entry ([b9446d3](https://github.com/arcboxlabs/arcbox/commit/b9446d3eefa3071befd03f5a571cc61cda588f0e))
* **agent:** disable containerd CRI plugin, bump to alpha.21 ([6fb350e](https://github.com/arcboxlabs/arcbox/commit/6fb350eba36e098684083a76822f17a133da8c42))
* **agent:** disable containerd CRI via config.toml, bump to alpha.22 ([985bf53](https://github.com/arcboxlabs/arcbox/commit/985bf53bd8b090b1cca9be880363a49d60838b4c))
* **agent:** enable iptables for container networking, bump to alpha.28 ([7716e63](https://github.com/arcboxlabs/arcbox/commit/7716e63e5b2e1ec299a2311ee741c7d1c0911385))
* **agent:** ensure mount points exist before tmpfs mount, no panic on SIGCHLD failure ([a89b38f](https://github.com/arcboxlabs/arcbox/commit/a89b38f45374255ba2501365e8e285a6ced93c1b))
* **agent:** exec commands now run inside container rootfs ([4a370d8](https://github.com/arcboxlabs/arcbox/commit/4a370d815ec2dd31177bc83c4fd5c86056efc919))
* **agent:** exec commands now run inside container rootfs ([e7874fa](https://github.com/arcboxlabs/arcbox/commit/e7874fada827aebbbff96e68199fc1963ee9a637))
* **agent:** export dns module from lib.rs for sandbox.rs access ([9bbb4b0](https://github.com/arcboxlabs/arcbox/commit/9bbb4b010f0f887f13b8848122d00b39ef50aed6))
* **agent:** fix borrow conflict and SandboxId type mismatch in port forward ([b7f5031](https://github.com/arcboxlabs/arcbox/commit/b7f5031acb11245cda6e4583e3f42667bb45b64a))
* **agent:** fix cross-compilation errors and harden data mount ([e580b9b](https://github.com/arcboxlabs/arcbox/commit/e580b9bae2d0fcf8ac35cc8f7f8f8a1251e2a0ea))
* **agent:** fix dns marker matching and support IP upsert ([2e45aae](https://github.com/arcboxlabs/arcbox/commit/2e45aaefb6bd904c4bd47e891fa5fbd501fbb0d9))
* **agent:** fix PATH for spawned daemons, enable ip_forward, remove --bridge=none ([4365903](https://github.com/arcboxlabs/arcbox/commit/436590316949647a31d84711c68e3e8c1626ba0b))
* **agent:** poll for containerd socket readiness before spawning dockerd ([7d697a8](https://github.com/arcboxlabs/arcbox/commit/7d697a8e19cbcfd21d3235efa2e30040b993ec4c))
* **agent:** remove legacy home share mount fallback ([778e986](https://github.com/arcboxlabs/arcbox/commit/778e986038109a1a621a4fa5a6eb7aa583afd50d))
* **agent:** resolve PR review comments in runtime status path ([cce508b](https://github.com/arcboxlabs/arcbox/commit/cce508b49954716f6ab79823f515f3cd3355fc58))
* **agent:** resolve PR review comments in runtime status path ([be49c0e](https://github.com/arcboxlabs/arcbox/commit/be49c0e8cc98dee0a66aab65bed4f5b87108b2da))
* **agent:** restore linux cross-build in machine init ([f6d411f](https://github.com/arcboxlabs/arcbox/commit/f6d411fa4f6ee02e076a16a945157cd15479c1d5))
* **agent:** restrict arcbox-agent to Linux-only compilation ([#16](https://github.com/arcboxlabs/arcbox/issues/16)) ([3b0ab6e](https://github.com/arcboxlabs/arcbox/commit/3b0ab6ee71848f452119dc52fb42da84d9025421))
* **agent:** retry vsock bind and load vsock modules on startup ([120185e](https://github.com/arcboxlabs/arcbox/commit/120185ebc3650a2b7abb448894d01ee87f594a76))
* **agent:** sync guest clock via NTP before spawning containerd/dockerd ([ae367ae](https://github.com/arcboxlabs/arcbox/commit/ae367aee9d796ccbf0120523a5f939b75110f12f))
* **agent:** use /bin/busybox for mount calls in ensure_runtime_prerequisites ([e9c51ee](https://github.com/arcboxlabs/arcbox/commit/e9c51ee5059074d452cab7cdbcea83fb8ec07ec0))
* **agent:** use runc instead of youki as default OCI runtime ([68bbe21](https://github.com/arcboxlabs/arcbox/commit/68bbe21c705d55b7e85de5aa1135f25b1517e88c))
* **agent:** use youki as default OCI runtime with runc as built-in fallback ([13a7439](https://github.com/arcboxlabs/arcbox/commit/13a74391e3d9352e3a64cb86e8f8c9a2d54fc110))
* align workspace dependency versions and add release-please markers ([71f11af](https://github.com/arcboxlabs/arcbox/commit/71f11af918f91d125464e83c472521f5d0ba79d5))
* **boot_assets:** load cached manifest even when custom initramfs is set ([bccc9f3](https://github.com/arcboxlabs/arcbox/commit/bccc9f31e1ec7519a734315581fa48dae7f85325))
* **boot-assets:** bump to v0.0.1-alpha.26 ([d731c6f](https://github.com/arcboxlabs/arcbox/commit/d731c6fd941e672c62492c371e09dca6e92ce24e))
* **boot-assets:** bump to v0.0.1-alpha.27 ([634f002](https://github.com/arcboxlabs/arcbox/commit/634f00278cd4703289110ab722445ead641e4a15))
* **boot:** add arcbox.mode=machine to ext4 rootfs kernel cmdline ([0e8628b](https://github.com/arcboxlabs/arcbox/commit/0e8628bafd4053dd5d1140aff3dcaa97e471cc26))
* **boot:** bump boot asset version to alpha.16 ([07119df](https://github.com/arcboxlabs/arcbox/commit/07119df9bd36f18973ce95779b8f22ee08797950))
* **boot:** bump boot asset version to alpha.17 ([3587a4a](https://github.com/arcboxlabs/arcbox/commit/3587a4a397f2bde0874fbbb6f60747dc43d3741a))
* **boot:** guest network, console redirect, serial console, dockerd prereqs ([1a22fa0](https://github.com/arcboxlabs/arcbox/commit/1a22fa04e614a640c5b84c8540af18e8a834f81f))
* **boot:** increase startup timeouts for Alpine OpenRC boot ([069810e](https://github.com/arcboxlabs/arcbox/commit/069810e191e1ca9564a215401b691ff104602b86))
* **boot:** install guest agent binary to data_dir/bin on daemon init ([37fcca6](https://github.com/arcboxlabs/arcbox/commit/37fcca631f5eb4d57b58854d139a444969c24769))
* **build:** remove restricted com.apple.vm.networking entitlement ([63f96d2](https://github.com/arcboxlabs/arcbox/commit/63f96d2d03e8af620363b28d5094098c3f191e48))
* **ci:** ad-hoc sign arcbox-helper before smoke test ([#84](https://github.com/arcboxlabs/arcbox/issues/84)) ([ba6759a](https://github.com/arcboxlabs/arcbox/commit/ba6759a01c7b95138bd7492144d28fbbae0ebf61))
* **ci:** add musl linker shim for cross builds ([6acdf98](https://github.com/arcboxlabs/arcbox/commit/6acdf98ca6aa61a4d77e052b4b71b35ecb0761e9))
* **ci:** enforce smoke matrix outcomes on master ([27fb3e8](https://github.com/arcboxlabs/arcbox/commit/27fb3e8ca922dea1c9671fcb4aa9d0240901c14d))
* **ci:** gate self-hosted e2e job behind manual trigger ([1585a0e](https://github.com/arcboxlabs/arcbox/commit/1585a0e0e02e8d84854d6c8e088e22c311eec721))
* **ci:** harden E2E pipeline gating and add musl-cross caching ([6d9fdf1](https://github.com/arcboxlabs/arcbox/commit/6d9fdf19fe18d64556e1449b331e8ad2099c2f28))
* **ci:** increase macOS build timeout for musl toolchain ([19c7b92](https://github.com/arcboxlabs/arcbox/commit/19c7b9249511632a2704ef78c5501f4da1c39e8e))
* **ci:** install docker cli for api compatibility tests ([17a8c41](https://github.com/arcboxlabs/arcbox/commit/17a8c413bcede0f1a0382647811b445c872827d8))
* **ci:** install protoc in macOS workflows ([e59c1ed](https://github.com/arcboxlabs/arcbox/commit/e59c1ed1b2b7762de8efb24ca59e39ff63932434))
* **ci:** install squashfs tools for initramfs build ([5116ace](https://github.com/arcboxlabs/arcbox/commit/5116aced79c013a95ce5f403806ebfc0585b4f6b))
* **ci:** make docker cli smoke step non-blocking ([7220753](https://github.com/arcboxlabs/arcbox/commit/72207538a9d80d111adb4336b152bffa7e649fc3))
* **ci:** pin docker cli api version for compatibility tests ([54db95c](https://github.com/arcboxlabs/arcbox/commit/54db95cc9f6d43763655d30adbc1ae927d64417a))
* **ci:** remove tracked-but-ignored boot assets from git index ([25dcb64](https://github.com/arcboxlabs/arcbox/commit/25dcb647ea539580f2921e948599c1b6079489ba))
* **ci:** repair docker api e2e workflow ([f147182](https://github.com/arcboxlabs/arcbox/commit/f147182687987f9298ccb80a2377d7d50ed6893b))
* **ci:** replace unavailable rust-action with rust-toolchain ([615f372](https://github.com/arcboxlabs/arcbox/commit/615f372063b14389ed1294dd04671d011e5f4aff))
* **cli:** add login item approval reset as explicit uninstall step ([7a87010](https://github.com/arcboxlabs/arcbox/commit/7a87010d47523d48ac1627ec04f14b6a3d2cd3e5))
* **cli:** clean up daemon sockets on shutdown ([692ff58](https://github.com/arcboxlabs/arcbox/commit/692ff5889a9bdc312767aac2f293ede0b895f297))
* **cli:** extend daemon stop wait timeout to 40s ([5ad5073](https://github.com/arcboxlabs/arcbox/commit/5ad507352c825a67c50e5de1ec6a4e09a1331644))
* **cli:** require daemon for image operations ([ec608ee](https://github.com/arcboxlabs/arcbox/commit/ec608ee9b1417e947276a611c26043e0c0ccd1e6))
* **cli:** route machine exec/ssh through daemon gRPC ([ead07c6](https://github.com/arcboxlabs/arcbox/commit/ead07c6a2a9fc0a2a1acc9b1d36f613c9552f042))
* **core:** bump boot assets to alpha.29 ([5ef755a](https://github.com/arcboxlabs/arcbox/commit/5ef755ab0e72005f9d1825b7dddb6d52f6f00bb4))
* **core:** bump boot assets to alpha.30 ([b4afe72](https://github.com/arcboxlabs/arcbox/commit/b4afe7291f42b7ea8def796454e5f08daf004eeb))
* **core:** extend graceful vm shutdown timeout to 30s ([02db34d](https://github.com/arcboxlabs/arcbox/commit/02db34d0389e5939094fc0d9ffd95ceac295e8b1))
* **core:** gate guest backend readiness on runtime status and endpoint match ([8faee2e](https://github.com/arcboxlabs/arcbox/commit/8faee2e686efa7fd0afb5a70b2a558aa804c7d7d))
* **core:** leak VMM handle after graceful ACPI stop ([5e5a82b](https://github.com/arcboxlabs/arcbox/commit/5e5a82b1dc32bb03aac0a57dc8dd87b507af57a3))
* **core:** remove home virtiofs share fallback ([f67b6a4](https://github.com/arcboxlabs/arcbox/commit/f67b6a4e8d44d14c3555f69957d866a765ac1b19))
* **core:** reset inbound_listener on stop to prevent stale manager ([96dd167](https://github.com/arcboxlabs/arcbox/commit/96dd1673fe04ab46f68d847eb81c539e2fed74a1))
* **core:** show full path in missing binary error messages ([0f598ea](https://github.com/arcboxlabs/arcbox/commit/0f598ea91b71040e8c48fd7455895e5550483a8e))
* **core:** unify guest endpoint validation across readiness paths ([dd1c0a2](https://github.com/arcboxlabs/arcbox/commit/dd1c0a254c3644e5fb7813c0c55b8c951eb345b7))
* **core:** unify guest endpoint validation across readiness paths ([446537f](https://github.com/arcboxlabs/arcbox/commit/446537fa070c4f35a4a953f06bf6474e7be28e04))
* **core:** ZBOOT kernel decompression, VM config propagation, boot cache unification ([0b7bf90](https://github.com/arcboxlabs/arcbox/commit/0b7bf908b489f6c4db2013022b35b920715f8558))
* correct boot-assets path and use gateway DNS instead of 8.8.8.8 ([e8c30de](https://github.com/arcboxlabs/arcbox/commit/e8c30debdb14b5139fffac3bbcbcef41282a082a))
* correct musl linker name for aarch64 target ([dde3ad9](https://github.com/arcboxlabs/arcbox/commit/dde3ad95f55939c3d9246d1aadcd498361c351eb))
* **daemon:** address review findings for stale state cleanup ([66f0afc](https://github.com/arcboxlabs/arcbox/commit/66f0afc4b2478232dd01e366b7904f5e9ac25f32))
* **daemon:** clean up stale state before startup ([3e48003](https://github.com/arcboxlabs/arcbox/commit/3e48003baaca58b37224958e31a7086a3ba258ee))
* **daemon:** graceful shutdown with CancellationToken ([#39](https://github.com/arcboxlabs/arcbox/issues/39)) ([ae34816](https://github.com/arcboxlabs/arcbox/commit/ae3481634e66d8a535130422680c0c1a0360b5b8))
* **docker:** always update context on enable to fix stale socket path ([8a0c45e](https://github.com/arcboxlabs/arcbox/commit/8a0c45e8d98df79f18f9f89969395955bf70620d))
* **docker:** clean up networking on container kill/restart ([#50](https://github.com/arcboxlabs/arcbox/issues/50)) ([0b7fc40](https://github.com/arcboxlabs/arcbox/commit/0b7fc40fa2363b859abcf91ff559a24480fcff65))
* **docker:** enable HTTP upgrades on guest proxy connection ([f2915a2](https://github.com/arcboxlabs/arcbox/commit/f2915a2287005eae98355dc951498f8ffe156eb0))
* **docker:** harden restart networking and canonical resolution ([#52](https://github.com/arcboxlabs/arcbox/issues/52)) ([766062c](https://github.com/arcboxlabs/arcbox/commit/766062c3d767fe722a0090ca4f5b3a3a1c9da566))
* **docker:** stream pass-through proxy bodies ([6b2e97e](https://github.com/arcboxlabs/arcbox/commit/6b2e97e33746bf81a7e6ab44128d5d2f1a3d5c11))
* **docker:** strip API version prefix before routing ([4371e11](https://github.com/arcboxlabs/arcbox/commit/4371e111f01ff2636f2f1d87628e00b8464664f2))
* **docker:** use canonical container ID for port forwarding rules ([aaa2eab](https://github.com/arcboxlabs/arcbox/commit/aaa2eabb1269fd5ad71ac172ce89a75d88476ddc))
* **hypervisor:** improve Darwin FFI safety by removing unsafe unwrap() ([7653fcb](https://github.com/arcboxlabs/arcbox/commit/7653fcbd37fbaccbbacb735cc54ae893c5d52ea7))
* **hypervisor:** use block_in_place for async VM operations ([c7b3d68](https://github.com/arcboxlabs/arcbox/commit/c7b3d68ac7cc90613a14adf32e5a2eb515279dd7))
* **hypervisor:** various improvements and fixes ([4037639](https://github.com/arcboxlabs/arcbox/commit/4037639451daa65a1e6d21b62973b859da27b634))
* **image:** improve layer extraction and error handling ([12484a7](https://github.com/arcboxlabs/arcbox/commit/12484a7b0a9bec4a3d0eabde18354631251a315e))
* install protobuf in build-agent job ([8c904ea](https://github.com/arcboxlabs/arcbox/commit/8c904ea833ce2bd4b6fcf3b82b85f852e52bf20e))
* **lint:** remove blanket clippy suppression, add workspace lint config ([#20](https://github.com/arcboxlabs/arcbox/issues/20)) ([e0a9db4](https://github.com/arcboxlabs/arcbox/commit/e0a9db46424566a00c7d0fbd68268fb5480361ee))
* **logs:** improve log streaming reliability and add Docker-compatible rotation ([9ad7fa9](https://github.com/arcboxlabs/arcbox/commit/9ad7fa92f812f5edeebbedb9594c593a3bb3ab84))
* **machine:** harden guest agent readiness and IP discovery ([06a6474](https://github.com/arcboxlabs/arcbox/commit/06a6474816f84265f99b505670c5c9589766852b))
* **net:** add com.apple.vm.networking entitlement for vmnet bridge ([d46802c](https://github.com/arcboxlabs/arcbox/commit/d46802ce8599541ef3793dfaead21adc8ec7522a))
* **net:** add retry logic to route reconciler ([a23bee1](https://github.com/arcboxlabs/arcbox/commit/a23bee1fd7586efa8a69349688b10b5b145b3e44))
* **net:** add retry logic to route reconciler ([#77](https://github.com/arcboxlabs/arcbox/issues/77)) ([0f8d304](https://github.com/arcboxlabs/arcbox/commit/0f8d304b8791ecd3d48ae8910fd4138bc5ba6da9))
* **net:** add write queue to prevent frame drops on WouldBlock ([c997511](https://github.com/arcboxlabs/arcbox/commit/c997511bb035513d61b9d3907162c0f32b49c170))
* **net:** address PR review — HostIp binding, UDP reply socket, dead code ([ea39d5a](https://github.com/arcboxlabs/arcbox/commit/ea39d5a49bc42db5fd6edc5916c5a5b554ece4fa))
* **net:** apply same TCP ordering fix to inbound relay path ([55b4eb1](https://github.com/arcboxlabs/arcbox/commit/55b4eb1befe719d9be3271c5026c8a95f4c87c65))
* **net:** avoid 198.18.0.0 IP conflict, fix cross-compile and async issues ([84db1df](https://github.com/arcboxlabs/arcbox/commit/84db1dfc9c7c162dd2cb07643aeb725f330c1c9b))
* **net:** change custom network stack subnet from 192.168.64.0/24 to 10.0.2.0/24 ([c1dd477](https://github.com/arcboxlabs/arcbox/commit/c1dd477c2fe5c356bbe6ecaaa0339edb7d5bdbf1))
* **net:** close SYN gate unclosed loops ([70adb37](https://github.com/arcboxlabs/arcbox/commit/70adb37c58f79928dd71cd49898d1ab5b90fb85b))
* **net:** confirmed macOS utun write() does not deliver to local IP stack ([2d49809](https://github.com/arcboxlabs/arcbox/commit/2d49809bddb8bd4e3aac8a149e4c96cc77757a4c))
* **net:** connect to guest Docker host port for inbound forwarding ([222e3b8](https://github.com/arcboxlabs/arcbox/commit/222e3b8599e049df3b8a3f92b68efa0a91070cdb))
* **net:** DHCP uses write queue, DNS returns SERVFAIL, system resolv.conf ([5972601](https://github.com/arcboxlabs/arcbox/commit/5972601459611be328b8965dbf99d8b8f01a5d20))
* **net:** fix partial send data loss, guest FIN signal, and stale port_handles in tcp_bridge ([711aff4](https://github.com/arcboxlabs/arcbox/commit/711aff4970405d7e00848d9bc42725763075f767))
* **net:** fix premature guest EOF, host disconnect handling, and listen panic ([891166c](https://github.com/arcboxlabs/arcbox/commit/891166cee87a505fd73a39651bab51b5936e76f5))
* **net:** handle transient ICMP socket errors and guard datapath spawn ([27a8143](https://github.com/arcboxlabs/arcbox/commit/27a8143c6ab0162173b4f37d916bfeba8595fce2))
* **net:** make DNS forwarding non-blocking to unblock datapath loop ([c6e01fb](https://github.com/arcboxlabs/arcbox/commit/c6e01fb8297e5aa4d9e3502f32c849bcfb507cfe))
* **net:** move smoltcp poll to common tail to prevent timer starvation ([8df62d2](https://github.com/arcboxlabs/arcbox/commit/8df62d244586165a05a7c142cc2085548637bf90))
* **net:** preserve data from handshake probe in tcp_bridge ([003055f](https://github.com/arcboxlabs/arcbox/commit/003055f9e282d338d077c7806f6268e1e6ad55f9))
* **net:** preserve TCP segment ordering and add seq validation ([c57d80f](https://github.com/arcboxlabs/arcbox/commit/c57d80fa4f85650f9e5e4ad2d5edbff1f607e951))
* **net:** probe host channel during handshake for connect failure ([1fa97b3](https://github.com/arcboxlabs/arcbox/commit/1fa97b38aa033f0dda82792eef8a623e50bf32ac))
* **net:** reject invalid HostIp and key listeners by (ip, port, proto) ([2605c29](https://github.com/arcboxlabs/arcbox/commit/2605c2918bd2edeee89db6db38534be7efd659a8))
* **net:** replenish listen sockets immediately after SYN acceptance ([8c49bda](https://github.com/arcboxlabs/arcbox/commit/8c49bdaf44aadf5a9daf1404ddf374154a4da8b0))
* **net:** resolve clippy warnings in test code ([46fcc75](https://github.com/arcboxlabs/arcbox/commit/46fcc750f3d4aa762d4fc84175a9f7e31ba54093))
* **net:** resolve remaining review findings in datapath/runtime/vmm ([c72d505](https://github.com/arcboxlabs/arcbox/commit/c72d505745be273a2047af26ea5ef441c5e5b774))
* **net:** respect write queue backpressure and cap reply drain batch ([b4284e3](https://github.com/arcboxlabs/arcbox/commit/b4284e3e07b58c0adc5a32c4cd46c1bd940d386c))
* **net:** reuse UDP flow socket for subsequent packets ([884d4f8](https://github.com/arcboxlabs/arcbox/commit/884d4f837c861f043a29e324e76151b8a4b4bc4e))
* **net:** robust bridge NIC detection, skip primary interface by name ([0f03c22](https://github.com/arcboxlabs/arcbox/commit/0f03c221fd421f0d6b83e277ceff707c94187603))
* **net:** switch utun read loop to blocking poll+read (AsyncFd unreliable on PF_SYSTEM) ([3c69fa9](https://github.com/arcboxlabs/arcbox/commit/3c69fa94bf4e50bf9bd72941073b69f7360c1681))
* **net:** synchronize TCP seq numbers and add MSS segmentation ([b3cc153](https://github.com/arcboxlabs/arcbox/commit/b3cc153bd325645c4bfd8cb94e2fbc398b6075c8))
* **net:** update route_reconciler to call ArcBoxHelper (single binary) ([985e1cd](https://github.com/arcboxlabs/arcbox/commit/985e1cdfdef41340d4da05ca67f0905ac640c792))
* **net:** use 240.0.0.1 (Class E reserved) for utun address, macOS requires IPv4 for -interface routes ([4164592](https://github.com/arcboxlabs/arcbox/commit/4164592a25d46165c71960dcbe517459b9d64e1e))
* **net:** use actual remote IP as source in TCP/UDP proxy replies ([611595c](https://github.com/arcboxlabs/arcbox/commit/611595cb903f969bd795c6b85f8af8afd1fc66a0))
* **net:** yield to tokio runtime in datapath loop to prevent task starvation ([790e096](https://github.com/arcboxlabs/arcbox/commit/790e096ebd200a768ae3d60dde615971af636ab4))
* **perf:** normalize cpu percentage metrics ([fbf5c04](https://github.com/arcboxlabs/arcbox/commit/fbf5c04b71242eeb7b21421de17b5903bf735b9a))
* **persistence:** persist block_devices in machine config ([d076130](https://github.com/arcboxlabs/arcbox/commit/d0761305057089c88364f2827f43d12d038bc09b))
* prevent lost notification in waiter loop by calling ([5585298](https://github.com/arcboxlabs/arcbox/commit/55852986700e7762eb8c03a4bc2aa09e74419a33))
* **protocol:** rustfmt generated prost output ([80c4728](https://github.com/arcboxlabs/arcbox/commit/80c472808315bdee1f2ac5dcee150c75f332682b))
* **release:** break release-plz v0.1.5 loop ([573367f](https://github.com/arcboxlabs/arcbox/commit/573367fd20910596277a1176d0a37fc603bca278))
* **release:** decouple tag/release creation from release-plz ([5cf5b5c](https://github.com/arcboxlabs/arcbox/commit/5cf5b5c7ccb00a8845168356e124e04e94ee7dd9))
* **release:** enable workspace-wide change detection for version bumps ([103cb78](https://github.com/arcboxlabs/arcbox/commit/103cb782483c822dbb5eef7ee525863129a083a1))
* **release:** include all workspace crates in facade changelog ([c086eec](https://github.com/arcboxlabs/arcbox/commit/c086eecd8a90e3dc07fe9ac958e1b19ed9c37ae3))
* **release:** move facade publish=false to release-plz.toml only ([12682da](https://github.com/arcboxlabs/arcbox/commit/12682dac50c1c6f67965bda9f94566b8a8dd37f3))
* remove --locked from release builds ([ded024e](https://github.com/arcboxlabs/arcbox/commit/ded024e7f339b0b514850ab732831f9b8b5ecb01))
* remove eprintln! debug output from RPC hot path. ([5585298](https://github.com/arcboxlabs/arcbox/commit/55852986700e7762eb8c03a4bc2aa09e74419a33))
* remove youki, validate all 4 runtime binaries, harden init ([c116d77](https://github.com/arcboxlabs/arcbox/commit/c116d77b79b68c2a6c5d83f2299bd70409f7b6c6))
* remove youki, validate all 4 runtime binaries, harden init ([3445c49](https://github.com/arcboxlabs/arcbox/commit/3445c497611b5ab827ec0a589cde99f58517785a))
* resolve clippy warnings and update agent README ([6beae8d](https://github.com/arcboxlabs/arcbox/commit/6beae8dcf277c8652df49f3772b1eb6b085f3772))
* resolve incomplete implementations and improve error handling ([45220c3](https://github.com/arcboxlabs/arcbox/commit/45220c3c9e120579aa3d81a480206e887b869fb1))
* resolve P0 issues and add container operation endpoints ([948a53c](https://github.com/arcboxlabs/arcbox/commit/948a53c5367c469d54aa0412d357513db2f23302))
* resolve remaining PR review comments ([2f4adc4](https://github.com/arcboxlabs/arcbox/commit/2f4adc41e4747663caf7f2f66ff878843b83b88a))
* **runtime:** enforce bundled guest runtime manifest integrity ([5e8857e](https://github.com/arcboxlabs/arcbox/commit/5e8857ecffd52ab683f4bd8cd92a3ccd7a9cd30f))
* **runtime:** enforce docker-ready gate and bundled fallback ([e494348](https://github.com/arcboxlabs/arcbox/commit/e494348d40f681c5bc72cbbcfe74e47c2904d157))
* **runtime:** harden vm startup recovery and guest proxy errors ([749a2e6](https://github.com/arcboxlabs/arcbox/commit/749a2e67adc449c0925b05a254cf91f4062cc6b2))
* **runtime:** propagate guest docker vsock port via kernel cmdline ([33ae472](https://github.com/arcboxlabs/arcbox/commit/33ae472c788de9891e491a192c397293792111eb))
* **runtime:** stabilize dockerd startup and readiness reporting ([34997f3](https://github.com/arcboxlabs/arcbox/commit/34997f34eafa98a4bb1ce6e2854d349ccb00cce2))
* **runtime:** stabilize dockerd startup and readiness reporting ([cb20f8e](https://github.com/arcboxlabs/arcbox/commit/cb20f8e4e4d8a3a923b691b8085de71b41944a07))
* scope app token to arcbox-desktop repo for cross-repo push ([d2dc78a](https://github.com/arcboxlabs/arcbox/commit/d2dc78ae2668e2d753a89311ac3799f55cb7912a))
* set git identity for update-desktop job ([295b896](https://github.com/arcboxlabs/arcbox/commit/295b896a6a832069638ee9a08ee623624d996049))
* update Cargo.lock in release-please PR and restore --locked builds ([3b84b26](https://github.com/arcboxlabs/arcbox/commit/3b84b26233b1e5bd069521750920aa553393375a))
* use arcbox-labs bot for Cargo.lock commits ([657f684](https://github.com/arcboxlabs/arcbox/commit/657f6843dc32ca2a62fba614b8351f9b4fe55e1f))
* use patch bump for pre-1.0 releases ([7d54bd1](https://github.com/arcboxlabs/arcbox/commit/7d54bd1924cfd1f3f6792e460422257bb1b444e7))
* use token-authenticated git clone for arcbox-desktop push ([fc332f2](https://github.com/arcboxlabs/arcbox/commit/fc332f25857769f2efbeb69977a8c80ea76a6610))
* validate vsock endpoint port on ensure_runtime ready path ([edf760e](https://github.com/arcboxlabs/arcbox/commit/edf760e272697d38d31875ef832e0b3ca8811b90))
* validate vsock endpoint port on ensure_runtime ready path ([5e5e1e0](https://github.com/arcboxlabs/arcbox/commit/5e5e1e0601f51ea7b61c32adb03d2b03dfa661db))
* **virtio:** virtio device improvements ([a205ae0](https://github.com/arcboxlabs/arcbox/commit/a205ae0baf10b17ba8f346a3b55bc619e62edd84))
* **vmm:** add missing block_devices in vmm_boot example ([d29d9b7](https://github.com/arcboxlabs/arcbox/commit/d29d9b798436fc26b6a76347b575d13e49beeca5))
* **vmm:** handle poisoned Mutex/RwLock in IRQ chip ([c054849](https://github.com/arcboxlabs/arcbox/commit/c054849ec2a2d9b799a854fe318637112d8f6083))
* **vmm:** update vmm_boot example with new VmmConfig fields ([23811a2](https://github.com/arcboxlabs/arcbox/commit/23811a2add4e62ba98faf5a9eafabff970e117bd))
* **vm:** stabilize machine lifecycle e2e behavior ([1e25b46](https://github.com/arcboxlabs/arcbox/commit/1e25b46a8388acef28ab69a66268fb7b9e310cd6))
* **vsock:** quiet transient connect resets during guest startup ([0aae297](https://github.com/arcboxlabs/arcbox/commit/0aae297407885052527cbe6cc1ccd3c626ad5dad))
* **vz:** dispatch request_stop through VM queue ([c4a6213](https://github.com/arcboxlabs/arcbox/commit/c4a6213c0a433044ad9f3e818a0900e127a942fe))
* **vz:** reset start completion sender for retried VM starts ([e8b53be](https://github.com/arcboxlabs/arcbox/commit/e8b53bec5ec0f20d0eb75d8ad5b3df40725b14af))
* **vz:** use correct initWithFileHandle: selector for network attachment ([c7d53dd](https://github.com/arcboxlabs/arcbox/commit/c7d53dda7fb6b0a5712c697d69d71fac1560578c))


### Code Refactoring

* **agent:** delete machine_init and legacy service management ([7b48f25](https://github.com/arcboxlabs/arcbox/commit/7b48f252d2f2a52331273afe7d59a2665c780225))
* **agent:** improve robustness of init and supervisor ([34b2ec2](https://github.com/arcboxlabs/arcbox/commit/34b2ec23dee466c0638403effe3fc3ee71971abf))
* **agent:** introduce SandboxError for structured decode/internal error handling ([6719722](https://github.com/arcboxlabs/arcbox/commit/6719722cd893009520f4a4f1927b3db44cf02155))
* **agent:** move containerd state to [@containerd](https://github.com/containerd) subvolume ([a3567e2](https://github.com/arcboxlabs/arcbox/commit/a3567e24e4a3d189dce638ce25397a4a80b4480b))
* **agent:** remove [@logs](https://github.com/logs) mount and use /tmp log fallback ([d392d1a](https://github.com/arcboxlabs/arcbox/commit/d392d1a9030711123fdeff7c431540ae4e0db296))
* **agent:** remove iptables DNAT port forwarding module ([9ae1c0c](https://github.com/arcboxlabs/arcbox/commit/9ae1c0cafd261dde3ba494a8be8fa200359db617))
* **agent:** remove legacy guest runtime RPC surface ([4eb560d](https://github.com/arcboxlabs/arcbox/commit/4eb560dddcf140d4b8bc035aa84d3a86304d45aa))
* **app:** decouple from arcbox-image types ([1a3b041](https://github.com/arcboxlabs/arcbox/commit/1a3b0416368c8e8f944783f20894138de595ed77))
* **build:** unify initramfs build to boot-assets canonical script ([85a583d](https://github.com/arcboxlabs/arcbox/commit/85a583d3c20007cd9bd8d9f8ffd75e4f3f9ba93f))
* **cli:** drop tower from grpc unix connector ([e4c6ab6](https://github.com/arcboxlabs/arcbox/commit/e4c6ab6084da575f30b381092e6acf61c9acdbc0))
* **cli:** expose unix connector for intra-crate reuse ([e1043b1](https://github.com/arcboxlabs/arcbox/commit/e1043b1ed089cfc324ec73cbeabbc684f8479fb7))
* **cli:** remove container commands and diagnose ([e80571b](https://github.com/arcboxlabs/arcbox/commit/e80571bbe4f8f7cf1603e685cf5c63b00cbed9a0))
* **cli:** use is_permission_denied() helper in dns commands ([e1b2ec6](https://github.com/arcboxlabs/arcbox/commit/e1b2ec6685e5eab556e3db0a2537604a969105cd))
* **common:** add shared constants crate ([#14](https://github.com/arcboxlabs/arcbox/issues/14)) ([52c69f5](https://github.com/arcboxlabs/arcbox/commit/52c69f547b129f3c4b1ac30a6c46d430546d9068))
* **constants:** add shared path constants, remove duplicates from agent ([774780e](https://github.com/arcboxlabs/arcbox/commit/774780e3ee09f3d331cda11dd86ef199e454fa8c))
* **container:** remove dead manager and volume modules ([03c94c2](https://github.com/arcboxlabs/arcbox/commit/03c94c2b54c382793617823c5444e07a6b7e0ce6))
* **core:** drop dead host container runtime/rpc surface ([2b165aa](https://github.com/arcboxlabs/arcbox/commit/2b165aaba037146fccbc34a1d1c609b8887b0a00))
* **core:** drop unused arcbox-container wiring ([fd11101](https://github.com/arcboxlabs/arcbox/commit/fd111018c736d782c83d65c5f6fe4e17e4e9c547))
* **core:** remove ContainerBackendMode, unify to smart proxy ([e4b3e4d](https://github.com/arcboxlabs/arcbox/commit/e4b3e4db7da93a9262acc9f334be3bd6bc1081c1))
* **core:** remove ContainerProvisionMode and --initrd CLI arg ([cabc76b](https://github.com/arcboxlabs/arcbox/commit/cabc76b3198f0afdc70c08aa688fb366da853bc2))
* **core:** remove initrd field and dead constants ([618f2bc](https://github.com/arcboxlabs/arcbox/commit/618f2bc84843b8ac023cef3cff3655a65eb4d082))
* **daemon:** decouple arcbox-desktop from arcbox-daemon ([#81](https://github.com/arcboxlabs/arcbox/issues/81)) ([afe6593](https://github.com/arcboxlabs/arcbox/commit/afe6593cf7cc826a5640cf719001414300bb6a9f))
* **daemon:** move runtime into arcbox-daemon binary ([c22cc0e](https://github.com/arcboxlabs/arcbox/commit/c22cc0ed06a26681445d425e7708bb76ed06a2d8))
* **daemon:** remove vestigial foreground flag ([34b99dd](https://github.com/arcboxlabs/arcbox/commit/34b99dd852e04876e8351c4e2811408328cf976a))
* **docker:** convert all handlers to smart proxy ([0714d40](https://github.com/arcboxlabs/arcbox/commit/0714d40156ccc8bc937b1a6bf813d5ba7a0757d6))
* **docker:** extract canonical_id_or_fallback helper ([2360056](https://github.com/arcboxlabs/arcbox/commit/23600563dd74d24e2c52947039f236a4b9cc6937))
* **docker:** proxy container lifecycle and events ([7d9edd8](https://github.com/arcboxlabs/arcbox/commit/7d9edd8ee335bf23ad74ebe24cfb541b65812d94))
* **docker:** proxy system endpoints to guest dockerd ([e35bedb](https://github.com/arcboxlabs/arcbox/commit/e35bedb1c78d10fdc5e0ecbbb540a0ba1a328dc3))
* **docker:** split handlers and deduplicate api routing ([e3e80a2](https://github.com/arcboxlabs/arcbox/commit/e3e80a20829493cd6436a073144e56e981024264))
* **error:** create arcbox-error crate and unify error types ([9037fcc](https://github.com/arcboxlabs/arcbox/commit/9037fcc054de5ff6ef358195e1d88b3b19c6e527))
* **grpc:** remove duplicated Container/Image/Network/System services ([df5cf97](https://github.com/arcboxlabs/arcbox/commit/df5cf97667f0ba8a75e89b5508257c94daa8b7e8))
* **hypervisor:** consolidate darwin FFI into arcbox-vz ([be802d3](https://github.com/arcboxlabs/arcbox/commit/be802d3b3a01d10a0e24b23dffd39af98e52f19c))
* **net:** extract DHCP server into standalone arcbox-dhcp crate ([#43](https://github.com/arcboxlabs/arcbox/issues/43)) ([6b7976b](https://github.com/arcboxlabs/arcbox/commit/6b7976bcf2207baa0642b6cffb098bb2a718c04c))
* **net:** integrate InboundRelay into SocketProxy and datapath ([07ed41b](https://github.com/arcboxlabs/arcbox/commit/07ed41bc914c22ac89d7be421a31df99989dc8ba))
* **net:** remove dead docker network CRUD API ([15ab733](https://github.com/arcboxlabs/arcbox/commit/15ab733ba4dd651db4e45c2c5c2635874139bb5b))
* **net:** remove legacy TcpProxy from socket_proxy ([b7da057](https://github.com/arcboxlabs/arcbox/commit/b7da057e1572f25f0f6f9d9043bc623c2fc322a7))
* **net:** remove Rust helper, route management moves to Swift XPC helper ([f4669eb](https://github.com/arcboxlabs/arcbox/commit/f4669eb2a67e3671f413691b19fe61174a4bf9ac))
* **net:** remove unnecessary unsafe from NAT translate functions ([#29](https://github.com/arcboxlabs/arcbox/issues/29)) ([f96e96c](https://github.com/arcboxlabs/arcbox/commit/f96e96cc23e89a653e451efeb48319c34de0974b))
* **net:** remove utun/L3 tunnel code, keep vmnet bridge approach ([90a5908](https://github.com/arcboxlabs/arcbox/commit/90a5908a2068a483b596ba7028a5ac9788b28068))
* **net:** replace text parsing with system APIs for route management ([#76](https://github.com/arcboxlabs/arcbox/issues/76)) ([5b226fa](https://github.com/arcboxlabs/arcbox/commit/5b226fa95fa83abc523009e7f5ce41954b03f204))
* **net:** replace utun + pf NAT with socket proxy datapath ([f99aea9](https://github.com/arcboxlabs/arcbox/commit/f99aea95c0a5627da5e88ddab8db4b9559c4d095))
* **net:** rewrite datapath loop with smoltcp poll model ([123261e](https://github.com/arcboxlabs/arcbox/commit/123261e041b13ba263570a3f317df9d9fa29e35e))
* **net:** unify subnets to 172.16.0.0/12, drop 10.88 and NE plan ([71ca659](https://github.com/arcboxlabs/arcbox/commit/71ca659a0d5973a454e0c576e7f314ef00540a68))
* **post-split:** remove stale api server and cleanup refs ([89749c2](https://github.com/arcboxlabs/arcbox/commit/89749c21b25cfbb3caa452b889d64b9beaf60513))
* **proto:** unify protocol definitions into arcbox-protocol ([8a6cf39](https://github.com/arcboxlabs/arcbox/commit/8a6cf3931a4e8e0d2ce008e162df1d6c82fd0298))
* reorganize ~/.arcbox/ directory layout ([#48](https://github.com/arcboxlabs/arcbox/issues/48)) ([c97b336](https://github.com/arcboxlabs/arcbox/commit/c97b336c04d0dcb840a4009020fc92e7f707f1ef))
* reorganize crates into layered directory structure ([592692a](https://github.com/arcboxlabs/arcbox/commit/592692a9da468dc5ae10dfe32a467c15f9c91a1b))
* reorganize workspace directory layout ([3fb5a5b](https://github.com/arcboxlabs/arcbox/commit/3fb5a5bfd399c36c50e86a37223c50eeec25a39c))
* reorganize workspace directory layout ([#53](https://github.com/arcboxlabs/arcbox/issues/53)) ([8aa9957](https://github.com/arcboxlabs/arcbox/commit/8aa9957120b10115e076ae39a0c2a3ed2a58da5d))
* **rpc:** remove sandbox port forward RPC messages and handlers ([a0e2b9f](https://github.com/arcboxlabs/arcbox/commit/a0e2b9f0c8953395c537ce4f43b85a348ab7a4f4))
* **scripts:** remove initramfs references and legacy build scripts ([8f3eac2](https://github.com/arcboxlabs/arcbox/commit/8f3eac2c080275077ed487497137d2ff007426e9))
* **vmm:** split vmm.rs into platform submodules ([#23](https://github.com/arcboxlabs/arcbox/issues/23)) ([7b73354](https://github.com/arcboxlabs/arcbox/commit/7b733540c3e47b1bc8eae3468dad668113c6507a))
* **workspace:** remove dead arcbox-image crate ([2bf755b](https://github.com/arcboxlabs/arcbox/commit/2bf755bed8a1b24331546485f9472908ff2d45b0))


### Tests

* add integration tests for DNS, shared table, and aliases ([10049e5](https://github.com/arcboxlabs/arcbox/commit/10049e504aac8e076e60cbec2b5569204874d778))
* **agent:** add coverage for runtime supervisor RPC message mapping ([2971c2d](https://github.com/arcboxlabs/arcbox/commit/2971c2df000479480b5b977f85500654fb0c0a6f))
* **cli:** add integration tests for daemon client ([e861a2a](https://github.com/arcboxlabs/arcbox/commit/e861a2ad939a7c0fe3772dafc2fcbd09c519afeb))
* **core,agent:** add regressions for runtime wait wakeups and guest docker HTTP parsing ([5db554b](https://github.com/arcboxlabs/arcbox/commit/5db554bb4fdc1734edff453537e6a5d2686c21ec))
* **core:** validate vsock endpoint parser for guest backend status ([c653134](https://github.com/arcboxlabs/arcbox/commit/c6531348fd1077d3d96b755349b73662237a7eb2))
* **docker:** add Docker API integration tests ([5399bf0](https://github.com/arcboxlabs/arcbox/commit/5399bf0d266af612b0a3e03062492581457d3ab5))
* **docker:** add wait condition regressions for API semantics ([80f8c87](https://github.com/arcboxlabs/arcbox/commit/80f8c8740ceff07761f8482885ec14af52fcdf5d))
* **docker:** run api tests on native backend ([8b8c94d](https://github.com/arcboxlabs/arcbox/commit/8b8c94d5002d29463360f5be3b7aaa2948a9e290))
* **docker:** validate trace id middleware propagation ([2d1b4f5](https://github.com/arcboxlabs/arcbox/commit/2d1b4f5f8f55aab3b6f32080ce2aa32a2518cc46))
* **e2e:** add Docker compatibility E2E tests ([8ae6fb8](https://github.com/arcboxlabs/arcbox/commit/8ae6fb840d712890c396bde6289401a29f587fc9))
* **net:** add datapath loop unit tests ([c211155](https://github.com/arcboxlabs/arcbox/commit/c211155ae01d6ee9004d729b8ff6ef8a95772949))
* **net:** add inbound relay and container handler tests ([8df3232](https://github.com/arcboxlabs/arcbox/commit/8df3232b6f90fbd72b75d7445d3f60f026abf760))
* **net:** add socket proxy and datapath unit tests ([13bbff3](https://github.com/arcboxlabs/arcbox/commit/13bbff3445022275292f6171d293885234fd573f))
* **net:** cover same-port-different-IP coexistence and invalid HostIp rejection ([6c4a528](https://github.com/arcboxlabs/arcbox/commit/6c4a528db917dbe5f518f8322033895c6134566b))


### Documentation

* add CLAUDE.md documentation for all crates ([2ae081b](https://github.com/arcboxlabs/arcbox/commit/2ae081bb924f69d185d25ee3ce98e5abd89ea44f))
* add L3 routing development journey log ([2c3b40e](https://github.com/arcboxlabs/arcbox/commit/2c3b40e2d525f82e134538f5e69a47e667e3bc21))
* add planning guidelines requiring fully resolved plans ([548d53c](https://github.com/arcboxlabs/arcbox/commit/548d53cdb60f79d3daa4c70f6a89ea8b4b17f398))
* add README.md for all crates ([8c5adc8](https://github.com/arcboxlabs/arcbox/commit/8c5adc8deb1d9f751d68f59132c8a06a37a69cb6))
* **architecture:** document arcbox-daemon split ([af1f924](https://github.com/arcboxlabs/arcbox/commit/af1f924304bc706b06aca808d5da939fb1bd5d34))
* clarify commit sizing and frequency guidelines ([398639b](https://github.com/arcboxlabs/arcbox/commit/398639b613b593a653ad3b18bf33e0e9f2dbb628))
* **claude:** note that some tasks require a running daemon ([c25ec81](https://github.com/arcboxlabs/arcbox/commit/c25ec81dbc15054df1f3d84534ca25f5644bcf05))
* **cli:** update daemon start command references ([0d55e63](https://github.com/arcboxlabs/arcbox/commit/0d55e6368692add2eafdb895a24e1d7c8941e1bf))
* **core:** clarify macOS vsock streaming architecture ([3fb88cc](https://github.com/arcboxlabs/arcbox/commit/3fb88ccdf22c0530f49a4780b49b2c773befab94))
* **core:** update stale ext4/first-boot comments to EROFS/Btrfs ([b37b271](https://github.com/arcboxlabs/arcbox/commit/b37b271c0112ec2910690c20894891995845ee26))
* downgrade helper to dev-only, NE is sole production path ([7428074](https://github.com/arcboxlabs/arcbox/commit/7428074efcdc54f4fea7a43a677363b10dc27939))
* format copilot-instructions.md ([29ed0df](https://github.com/arcboxlabs/arcbox/commit/29ed0dfc9bddafef2519d49066f1ce2d7f0d8d7c))
* mark network plan as implemented ([1035666](https://github.com/arcboxlabs/arcbox/commit/1035666df139696e7d251543e6d70be121e752ac))
* note alpha stage, prioritize long-term quality over stability ([1e180d6](https://github.com/arcboxlabs/arcbox/commit/1e180d6bc7912e5e87414669f22e81f033bc1706))
* **readme:** add desktop, discord, telegram, and docs badges ([21a49c3](https://github.com/arcboxlabs/arcbox/commit/21a49c3ae6828e7ec21f6e5c738aa9efdf853796))
* **readme:** realign P0 API docs with current code ([7360b27](https://github.com/arcboxlabs/arcbox/commit/7360b275b111c1118533e495d6a5d56c2dc6b998))
* **readme:** use daemon commands for start and stop ([eb6008f](https://github.com/arcboxlabs/arcbox/commit/eb6008f7274f92a7888d049d1169ae4d5b3d65eb))
* **repo:** correct tests directory description ([d95a0d9](https://github.com/arcboxlabs/arcbox/commit/d95a0d9f4f4500a2d9a7a256748cdb0dea3c3c13))
* **repo:** refresh structural paths and contribution links ([6debd86](https://github.com/arcboxlabs/arcbox/commit/6debd86dd53fcc8255e93383a99386d013bb08eb))
* reposition README around sandbox/Computer Use, extract CONTRIBUTING.md ([e071735](https://github.com/arcboxlabs/arcbox/commit/e071735fcbdf841ecc9ee0c58184a16626a03762))
* require clippy and fmt checks before committing ([cfe1b3b](https://github.com/arcboxlabs/arcbox/commit/cfe1b3bcdbeb8a5d79635bb77543941c57821719))
* require new branch with conventional naming for change sets ([0ea9e3d](https://github.com/arcboxlabs/arcbox/commit/0ea9e3d5790c001fbb1e71c837daafa5072d4aee))
* **runtime:** fix comment drift and docker API wording ([caeb19e](https://github.com/arcboxlabs/arcbox/commit/caeb19edd358d449a6cda9cd135a98a095ef4176))
* update boot-assets docs for schema v7 ([91b1e92](https://github.com/arcboxlabs/arcbox/commit/91b1e920458f6d73ab9980dcfc33c267d7327bdd))
* update README with EROFS boot architecture and project structure ([fd850c1](https://github.com/arcboxlabs/arcbox/commit/fd850c1eeccdccd595d4de27a37a0f8df8f548a7))


### Styles

* **agent:** cargo fmt dns.rs ([407b9af](https://github.com/arcboxlabs/arcbox/commit/407b9afe45d01e699212633f7cb1363b3a7ce8db))
* **agent:** cargo fmt port_forward.rs ([7d10d73](https://github.com/arcboxlabs/arcbox/commit/7d10d7305fa573bd25bc4bca3d00190d00679a35))
* cargo fmt ([4ae09c7](https://github.com/arcboxlabs/arcbox/commit/4ae09c756d7c3d0ca2d38c01870f6c7a1d1cd453))
* cargo fmt ([444d6af](https://github.com/arcboxlabs/arcbox/commit/444d6af84e46a3fce6431a91faa6b18c50c23afb))
* cargo fmt and extract validate_reported_vsock_endpoint helper ([a9dba0e](https://github.com/arcboxlabs/arcbox/commit/a9dba0ebe71dbc352d54585965f6e2a4c6347e76))
* cargo fmt and extract validate_reported_vsock_endpoint helper ([03c31d1](https://github.com/arcboxlabs/arcbox/commit/03c31d10aabc6adfc97f2ae80c0bd8e51b7877c4))
* **docker:** remove blanket lint suppression, fix all warnings ([fd99973](https://github.com/arcboxlabs/arcbox/commit/fd99973d600aae0c01f83d9dd0a52ee1e056412e))
* fix cargo fmt in boot_assets LazyLock ([7460acd](https://github.com/arcboxlabs/arcbox/commit/7460acd1ec8683bf2d8814484a88002b7035a789))
* **fmt:** apply rustfmt in touched files ([0ae2387](https://github.com/arcboxlabs/arcbox/commit/0ae23870f83db9ca349d135c48cb602f58adfcf5))
* **net:** apply cargo fmt to socket proxy and datapath ([6e5f41b](https://github.com/arcboxlabs/arcbox/commit/6e5f41b2643dc15f1793dce4b9dd5b57a177db44))
* **net:** fix formatting in route reconciler and daemon ([f2d3a9d](https://github.com/arcboxlabs/arcbox/commit/f2d3a9ddf37643329e1919e6eac7edf1536fb44e))
* **net:** run rustfmt for inbound relay ([15e40d2](https://github.com/arcboxlabs/arcbox/commit/15e40d2af976fab265cc6023c55a07e1dc0fef0b))


### Build System

* add macOS entitlements for app bundle ([cbafc5b](https://github.com/arcboxlabs/arcbox/commit/cbafc5ba1b53822b49167e98ea5b80412e647e2b))
* **ci:** package and run arcbox-daemon binary ([c4f8f1e](https://github.com/arcboxlabs/arcbox/commit/c4f8f1e75ed602105a6b9f23bf5078798aaac0df))
* **signing:** scope virtualization signing to daemon ([78ff844](https://github.com/arcboxlabs/arcbox/commit/78ff844490b5745580854bae5ce2126195d18c1f))


### Continuous Integration

* add arcbox-helper to release build, tarball, and CI smoke test ([4c20c85](https://github.com/arcboxlabs/arcbox/commit/4c20c85981b7b28ac216f5484f7fcdbd9651d5ec))
* add Docker API E2E test workflow and scripts ([47d129f](https://github.com/arcboxlabs/arcbox/commit/47d129f77de8ae4b1b4dcc6051930d84291e809b))
* build and package arcbox-agent in release workflow ([2f4df86](https://github.com/arcboxlabs/arcbox/commit/2f4df860982ebd81aae0174e14963d403e124869))
* checkout arcbox-labs/boot-assets and set BOOT_ASSETS_DIR env var. ([85a583d](https://github.com/arcboxlabs/arcbox/commit/85a583d3c20007cd9bd8d9f8ffd75e4f3f9ba93f))
* **e2e:** disable automatic triggers on GitHub Actions ([c05d399](https://github.com/arcboxlabs/arcbox/commit/c05d3999062272a4004602e23208db68ce2c9cbf))
* **release:** remove boot asset build from release workflow ([fb356f9](https://github.com/arcboxlabs/arcbox/commit/fb356f9e02662d470f946c459a726033120dbeb9))
* **release:** rename workflow to Release Please ([1cd4003](https://github.com/arcboxlabs/arcbox/commit/1cd400335cac97d0877c979eea30d8bd3a5dbbb1))
* **vm:** merge arcbox-vm and arcbox-agent Linux workflows into test-vm-linux.yml ([abde8f7](https://github.com/arcboxlabs/arcbox/commit/abde8f7d3bc75b228e3b97806f8913f952f866b2))
* **workflows:** add basic ci workflow ([c6f313b](https://github.com/arcboxlabs/arcbox/commit/c6f313ba399716cada6ac33aaffce21b5c8d3462))
* **workflows:** bump macos runner and sign cli smoke test ([28d4341](https://github.com/arcboxlabs/arcbox/commit/28d434192df81bddacb5c031cfd3996d23d5835a))


### Miscellaneous Chores

* add build scripts and config files ([7dba7eb](https://github.com/arcboxlabs/arcbox/commit/7dba7eb21aedc82b4be45e9a2687a89112fd23a4))
* add clippy allow attributes for incomplete modules ([b78a203](https://github.com/arcboxlabs/arcbox/commit/b78a203ed9e08c7acc37c83c94d45856787b3295))
* **boot:** bump default boot asset version to v0.0.1-alpha.10 ([28842f5](https://github.com/arcboxlabs/arcbox/commit/28842f5fe87ee4dc9717749cfb406fa4fe6cbb51))
* **boot:** bump default boot asset version to v0.0.1-alpha.4 ([ef95f35](https://github.com/arcboxlabs/arcbox/commit/ef95f356a9d0cba275ef2b577840ac6c01291399))
* bump boot version to `0.5.1` ([3af1c9b](https://github.com/arcboxlabs/arcbox/commit/3af1c9b801340ee2aa6d791647d4438d22feecb9))
* bump BOOT_ASSET_VERSION to 0.2.3 ([a741cab](https://github.com/arcboxlabs/arcbox/commit/a741cab1377e04e2132483c0a17c7a39c74c12c6))
* bump BOOT_ASSET_VERSION to 0.2.3 ([2ab9dc3](https://github.com/arcboxlabs/arcbox/commit/2ab9dc305e1be9c90d0928a58f090bcc4c480416))
* bump BOOT_ASSET_VERSION to 0.3.0 ([6f10a11](https://github.com/arcboxlabs/arcbox/commit/6f10a11aa682db1961721dc1a46008fc15e1077f))
* bump version to 0.2.0 ([d365921](https://github.com/arcboxlabs/arcbox/commit/d3659210a97e91969b93e2c820d6a1bf230eba34))
* **ci:** add release-plz for automated releases ([#40](https://github.com/arcboxlabs/arcbox/issues/40)) ([8c37dd6](https://github.com/arcboxlabs/arcbox/commit/8c37dd62b3f91b675512d3173f0604fa516c0612))
* **ci:** add release-plz for automated releases and crates.io publishing ([8c37dd6](https://github.com/arcboxlabs/arcbox/commit/8c37dd62b3f91b675512d3173f0604fa516c0612))
* **ci:** remove broken e2e workflow, fix docker-api-e2e paths ([df4c7c9](https://github.com/arcboxlabs/arcbox/commit/df4c7c9e085926fe6472f4c39ee1da062d2b44cb))
* **cli:** clean up Cargo.toml dependency declarations ([094a64b](https://github.com/arcboxlabs/arcbox/commit/094a64b93e0180abe431fecd8d5da4316d9daf23))
* **deps:** bump arcbox-boot to 0.3 to match boot-assets.lock ([f824c39](https://github.com/arcboxlabs/arcbox/commit/f824c39bc15e5ba14b9e5f9d7b293f31694d3120))
* **deps:** remove stale arcbox-container deps ([1525b16](https://github.com/arcboxlabs/arcbox/commit/1525b16458de61a3675f257b5947592334cfc254))
* **deps:** switch arcbox-boot from git to crates.io v0.2 ([40df8a8](https://github.com/arcboxlabs/arcbox/commit/40df8a8f689115aae1bf8376849cb708ec6ca4da))
* **docker:** remove host-only system deps and gate vm tests ([70b159d](https://github.com/arcboxlabs/arcbox/commit/70b159d562d666e235e99973aa664d422d2eb987))
* **docker:** upgrade axum to 0.8 and add port forwarding diagnostics ([0960b69](https://github.com/arcboxlabs/arcbox/commit/0960b6934030715b0ce204b65326a2303b024e15))
* **docs:** add AGENTS.md symlink and self-update rule ([7281f09](https://github.com/arcboxlabs/arcbox/commit/7281f09ead882b3612d6777b11fd6258eb36b4ad))
* **docs:** add testing expectations to CLAUDE.md ([6928172](https://github.com/arcboxlabs/arcbox/commit/6928172df58fc2a6b349b21c53303837bfc17618))
* **docs:** audit and consolidate CLAUDE.md files ([4aca433](https://github.com/arcboxlabs/arcbox/commit/4aca433714642cf68888d69484150fb330fa9765))
* **e2e:** pin backend mode in scripts and API CI workflow ([482b4a2](https://github.com/arcboxlabs/arcbox/commit/482b4a282b9df57f889d9643c55d8a11dffe640a))
* **e2e:** remove broken e2e test suite ([c4a9278](https://github.com/arcboxlabs/arcbox/commit/c4a927897b4e285b75da128c6c0f211e9f2f820f))
* gitignore test resources and macOS files ([6f03ffd](https://github.com/arcboxlabs/arcbox/commit/6f03ffd305de56e134b5b3c47a5e377d38f56988))
* house keeping -- remove outdated license file ([d53643d](https://github.com/arcboxlabs/arcbox/commit/d53643d423ba642d0e0cd326af69abd3e8dc2595))
* include arcbox-agent in release tarball ([37a2af7](https://github.com/arcboxlabs/arcbox/commit/37a2af7d7ce67d067d9ff09e4bd9225af436d35f))
* **lint:** resolve post-e2e fmt and docs issues ([3c83c76](https://github.com/arcboxlabs/arcbox/commit/3c83c76d42a0196bcabed39c48dabbb4a5f62812))
* **master:** release 0.1.10 ([#61](https://github.com/arcboxlabs/arcbox/issues/61)) ([13a8475](https://github.com/arcboxlabs/arcbox/commit/13a84756a11602a74ee76c9e79acabb1553e068f))
* **master:** release 0.1.11 ([#62](https://github.com/arcboxlabs/arcbox/issues/62)) ([231d334](https://github.com/arcboxlabs/arcbox/commit/231d334f83a6af6116835e0731c99e2eda4c3c12))
* **master:** release 0.1.12 ([#64](https://github.com/arcboxlabs/arcbox/issues/64)) ([99f7700](https://github.com/arcboxlabs/arcbox/commit/99f7700640d4bdf616dc348b0349d72bd3d6f7ad))
* **master:** release 0.1.7 ([#58](https://github.com/arcboxlabs/arcbox/issues/58)) ([322ca7d](https://github.com/arcboxlabs/arcbox/commit/322ca7d1b4a9b007bc8841852974f69759d3140d))
* **master:** release 0.1.8 ([#59](https://github.com/arcboxlabs/arcbox/issues/59)) ([eebb5c0](https://github.com/arcboxlabs/arcbox/commit/eebb5c079b60c8bb66b88622984db9b8a50c892d))
* **master:** release 0.1.9 ([#60](https://github.com/arcboxlabs/arcbox/issues/60)) ([b173157](https://github.com/arcboxlabs/arcbox/commit/b17315730548e8322b8c9d56ed98670327c38cd2))
* **master:** release 0.2.0 ([3d955e0](https://github.com/arcboxlabs/arcbox/commit/3d955e0077ee89da4c8a7d17ee15df9346e4e222))
* **master:** release 0.2.1 ([#70](https://github.com/arcboxlabs/arcbox/issues/70)) ([8e1d86c](https://github.com/arcboxlabs/arcbox/commit/8e1d86cb1620289094e982ed576c7dfcee87c2d7))
* **master:** release 0.2.2 ([#71](https://github.com/arcboxlabs/arcbox/issues/71)) ([fe0e1e5](https://github.com/arcboxlabs/arcbox/commit/fe0e1e50d167b92a81a76701310c9ebceadb3c3c))
* **master:** release 0.2.3 ([#72](https://github.com/arcboxlabs/arcbox/issues/72)) ([22252f9](https://github.com/arcboxlabs/arcbox/commit/22252f99c45a736ba72bf371f9a70d3a1d3d2381))
* **master:** release 0.2.4 ([#74](https://github.com/arcboxlabs/arcbox/issues/74)) ([b517477](https://github.com/arcboxlabs/arcbox/commit/b517477396d91e456b24c105ce24d7bd2acaee1c))
* **master:** release 0.2.5 ([#75](https://github.com/arcboxlabs/arcbox/issues/75)) ([a56a8e1](https://github.com/arcboxlabs/arcbox/commit/a56a8e1c01a97160d476a2e3149a97f296240b5b))
* **master:** release 0.2.6 ([#77](https://github.com/arcboxlabs/arcbox/issues/77)) ([0bcb6b9](https://github.com/arcboxlabs/arcbox/commit/0bcb6b9516f0dea22245ff7025295614785fb719))
* **master:** release 0.2.7 ([#78](https://github.com/arcboxlabs/arcbox/issues/78)) ([5325789](https://github.com/arcboxlabs/arcbox/commit/5325789802e2d7bedd1a5992a068cbe7ee4a393a))
* **master:** release 0.3.0 ([d1d570a](https://github.com/arcboxlabs/arcbox/commit/d1d570ac4ea1c04e2ca05840d49c618888913cac))
* **master:** release 0.3.0 ([#82](https://github.com/arcboxlabs/arcbox/issues/82)) ([d1d570a](https://github.com/arcboxlabs/arcbox/commit/d1d570ac4ea1c04e2ca05840d49c618888913cac))
* migrate GitHub org slug from arcbox-labs to arcboxlabs ([112e050](https://github.com/arcboxlabs/arcbox/commit/112e050a57f85a36ecf67f9cb60b6004ba6e6b17))
* move dev logs to internal-docs ([2e09935](https://github.com/arcboxlabs/arcbox/commit/2e099357d7df34014a21401774691cbf8201b33c))
* **net:** downgrade noisy datapath logs to debug level ([be03419](https://github.com/arcboxlabs/arcbox/commit/be03419c60587f018ae25739d039627fc3303228))
* **net:** fix stale tcp bridge cleanup comment ([b8d548d](https://github.com/arcboxlabs/arcbox/commit/b8d548d2b317ce7b04e474a96fbba91d577278f7))
* **net:** remove unused tcp builder and clean up imports ([428e374](https://github.com/arcboxlabs/arcbox/commit/428e37475bf11651d54857736b560437744bfbb9))
* publish crates to crates.io (v0.0.1-alpha.1) ([09e60df](https://github.com/arcboxlabs/arcbox/commit/09e60df2f390b20bf65b6fc40f588e6cfc69f6f4))
* release v0.1.1 — unify workspace version and fix lint warnings ([6f9621a](https://github.com/arcboxlabs/arcbox/commit/6f9621ad1812e5cf83aa398d622fa9466ac5724d))
* release v0.1.2 ([bdff717](https://github.com/arcboxlabs/arcbox/commit/bdff71786f5f86df6a0bf35b09c6a4df0a036d49))
* release v0.1.4 ([083111b](https://github.com/arcboxlabs/arcbox/commit/083111b6d9bf20ca5dd20f7b6381f44ad490b42c))
* release v0.1.5 ([#46](https://github.com/arcboxlabs/arcbox/issues/46)) ([2c1823f](https://github.com/arcboxlabs/arcbox/commit/2c1823f25b1ffcced1c8d19fd57ce2b1186d017c))
* release v0.1.5 ([#49](https://github.com/arcboxlabs/arcbox/issues/49)) ([f45f189](https://github.com/arcboxlabs/arcbox/commit/f45f18927d5e5fdff9a8b8dfc3331a3c45f6957f))
* release v0.1.6 ([#56](https://github.com/arcboxlabs/arcbox/issues/56)) ([a23ba6c](https://github.com/arcboxlabs/arcbox/commit/a23ba6c1861832182d64841cc7c1ad26d3864b4a))
* **release:** disable crates.io publish for now ([ed6cae8](https://github.com/arcboxlabs/arcbox/commit/ed6cae8256d20295d10bc016a1d697eb8fd96abb))
* **release:** enable crates.io publishing for workspace crates ([1a53123](https://github.com/arcboxlabs/arcbox/commit/1a531234a35aefaf2e8831201f9dcb4a87c6cb5e))
* **release:** include all conventional commit types in changelog ([aa81671](https://github.com/arcboxlabs/arcbox/commit/aa8167194ac45011ea70f4cde1273f4c21a9ed7e))
* **release:** limit crates.io publishing to virt stack ([6fc1b1c](https://github.com/arcboxlabs/arcbox/commit/6fc1b1c8d36bcb0bbc68e8ecb74357ad2a619f39))
* remove duplicate NOTICE and add SECURITY.md ([89e83c9](https://github.com/arcboxlabs/arcbox/commit/89e83c98a6719d40cb0436c9740c09c8552b5646))
* remove Homebrew formula ([236bd2e](https://github.com/arcboxlabs/arcbox/commit/236bd2eddf45cd1b52174bb59e221753bbc10513))
* remove plan files accidentally merged to master ([e9267f7](https://github.com/arcboxlabs/arcbox/commit/e9267f7c1336c37f6025525c4dfb041eb253a0e3))
* replace remaining youki references with runc in docs/proto ([e4d732b](https://github.com/arcboxlabs/arcbox/commit/e4d732b82d66838ee65ed3f5dd35cd7f5bd42153))
* replace remaining youki references with runc in docs/proto ([4fe815a](https://github.com/arcboxlabs/arcbox/commit/4fe815a7647f859e359b5a9fcaefcd4566aba5e6))
* **runtime:** warn when bundled boot manifest lacks runtime asset entries ([e38b6d5](https://github.com/arcboxlabs/arcbox/commit/e38b6d571bdf7b4e47f2a42d8239e463289b2501))
* **tests:** remove dead e2e scaffolding ([000ef67](https://github.com/arcboxlabs/arcbox/commit/000ef673542a8b8621e9a75e0decc44b010ea714))
* update Cargo.lock for release ([fa8fbb0](https://github.com/arcboxlabs/arcbox/commit/fa8fbb02ad4f81df6b0c522a2a78f443b379b54f))
* update docs for dynamic schema versioning ([f46c25b](https://github.com/arcboxlabs/arcbox/commit/f46c25b6ed50ceb573169d6b8df78122ea28501b))
* update GitHub org slug in README to arcboxlabs ([6df5e6b](https://github.com/arcboxlabs/arcbox/commit/6df5e6b2df3d048747df82158eedc4d08de8e2f1))
* update LICENSE-MIT copyright holder to ArcBox Labs ([27dae34](https://github.com/arcboxlabs/arcbox/commit/27dae342d58fb3a36a85c27d70b299ee1e0f4fdf))
* update metadata to arcboxlabs org ([46a946f](https://github.com/arcboxlabs/arcbox/commit/46a946f7248852b1c8adc245fc02dca8e86a8a26))
* update workspace configuration ([9409ad3](https://github.com/arcboxlabs/arcbox/commit/9409ad33271d4995b89229e54007a42383e7331b))
* update workspace dependencies ([3e00946](https://github.com/arcboxlabs/arcbox/commit/3e0094603462bd68628e6e4149f65f5ec3856c08))
* **vz:** add SAFETY comments to all unsafe blocks in arcbox-vz ([#26](https://github.com/arcboxlabs/arcbox/issues/26)) ([5a14350](https://github.com/arcboxlabs/arcbox/commit/5a14350ef3dbdcc421c172dd33624109b688f2eb))
* **workspace:** add arcbox-grpc to workspace ([6b205ca](https://github.com/arcboxlabs/arcbox/commit/6b205ca867b35e7827ce9a02b9cccaa98f13d51c))

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
