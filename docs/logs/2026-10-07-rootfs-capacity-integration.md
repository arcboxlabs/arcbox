# 2026-10-07 — Caller-owned rootfs capacity and publication passed Linux validation

- Type: change
- Area: `arcbox-computer-runtime` rootfs construction
- PR: [#736 — caller-owned rootfs construction and publication](https://github.com/arcboxlabs/arcbox/pull/736)
- Source: [`build_rootfs` and publication](../../computer/arcbox-computer-runtime/src/rootfs/build.rs), [cached template conversion](../../computer/arcbox-computer-runtime/src/rootfs.rs)
- Related: [Runtime README](../../computer/arcbox-computer-runtime/README.md), [kernel-mounted rootfs probes](../../computer/arcbox-computer-runtime/tests/integration/rootfs.rs)

## Problem or trigger

The cached rootfs converter started images at 512 MiB, allowed the formatter to grow images to fit their source, and owned the output path. Node image builders need a sparse image with an exact capacity at a caller-owned path. The caller must also retain the config and manifest digest from the same registry resolution that supplies the image contents.

## What was done

- Added `RootfsSource`, `RootfsSpec`, and `RootfsBuilder::build_rootfs`. The caller supplies a directory or resolved image, the output path, and capacity.
- Required positive capacities in 128 MiB units. Required both the declared ext4 capacity and file length to match the request before injection because the formatter can enlarge the image or declare more blocks than the file contains.
- Converted into a sibling temporary file. Injected the agent before publishing with an atomic rename. Failed conversion, injection, and publication preserve the destination and remove temporary files.
- Exposed `inject_vm_agent` and `VM_AGENT_PATH`. Default kernel arguments select `init=/sbin/vm-agent`; converted images retain the distribution's init.
- Reused publication for cached templates, starting at 512 MiB and allowing growth to fit the source. Preserved the `rootfs-<layer>-<agent>.ext4` name used by template identity.
- Made resolver setup failures return an error. Successful injection requires `/etc/resolv.conf` to link to `../run/resolv.conf`.
- Added the optional `remote-image` feature, an `oci2rootfs` re-export, and a Linux CI feature compilation gate. Kept registry access outside the default feature set.

## Evidence

The Linux checks ran in the existing Lima `arcbox-m2` instance: aarch64 Ubuntu, kernel `6.8.0-137-generic`, root loop-device access, and BusyBox. The registry probe resolved the dependency's default image platform, `linux/amd64`; mounting ext4 does not execute the image's binaries.

| Registry probe field | Result |
| --- | --- |
| Reference | `docker.io/library/debian:12` |
| Resolved manifest | `sha256:bc49dc1918ee1a47a93e65b5e4676e8680fb754b133197b92ca52bfe6731d5f0` |
| Apparent capacity | 34,359,738,368 bytes, 32 GiB |
| Allocated capacity | 139,177,984 bytes, approximately 132.7 MiB |
| Conversion and injection | 591 ms; 852 ms on the default-fixture rerun, excluding registry fetch |
| Complete test | 11.36 s; 7.65 s on the rerun, including registry fetch |
| Retained config | command `["bash"]`, empty entrypoint |
| Kernel mount checks | Agent contents, mode `0755`, and resolver symlink passed |

The probe uses `MetadataExt::blocks() * 512` for allocated capacity and requires allocation below one eighth of apparent capacity. The test mounts the image read-only after injection and releases the mount and loop device after checking the contents. The probe validates image construction and mounting; VM boot requires a separate runtime check.

Reproduce the registry probe on a Linux host with root, BusyBox, loop-device access, and registry access:

```bash
sudo -E env ARCBOX_REQUIRE_BLOCK_TOOLS=1 \
  ARCBOX_TEST_IMAGE=docker.io/library/debian:12 \
  cargo test --locked -p arcbox-computer-runtime --features remote-image \
  --test integration a_registry_image_builds_at_sparse_computer_capacity \
  -- --ignored --nocapture
```

The same revision passed:

- `cargo fmt --check`.
- `cargo check -p arcbox-computer-runtime --features remote-image` on macOS.
- `cargo clippy --locked -p arcbox-computer-runtime --features remote-image --all-targets -- -D warnings` on macOS.
- The same strict Clippy command with `cargo +nightly` on Linux. Lima's older stable Clippy does not recognize the existing workspace lint `clippy::unused_async_trait_impl`.
- 231 library tests and 42 `manager_over_fakes` tests on macOS.
- 231 library tests and 6 non-ignored integration tests on Linux, with `ARCBOX_REQUIRE_BLOCK_TOOLS=1`. The separate ignored registry probe passed as shown above.
- `cargo xtask check-layers`: 68 members and 190 edges checked.

The local-image probes verify 256 MiB caller capacity, 512 MiB capacity for a small cached source and cache hits, destination replacement, repeated agent injection, preservation of distribution init, and cleanup after injection or publication failure. Unit tests reject zero and unaligned capacity, detect over-declared ext4 geometry, and verify cleanup after source-read failure.

A review regression supplies a sparse overlay file of 128 MiB plus 4 KiB with a requested capacity of 128 MiB. The formatter enlarges the image to 256 MiB, so comparing declared geometry only with file length did not reject the oversized output. The regression failed against that implementation because execution reached agent injection. The capacity check now rejects both size mismatches before injection, preserves an existing destination, and removes the temporary image.

After this correction, workspace formatting, strict all-target Clippy with `remote-image` on macOS and Linux, 232 macOS library tests, and 42 macOS manager tests passed. Linux passed all 5 rootfs build unit tests and both exact rootfs integration tests with `ARCBOX_REQUIRE_BLOCK_TOOLS=1`; no selected test was ignored or skipped.

A later review found that sharing this exact-capacity check also imposed a new 512 MiB limit on cached templates. The `cached_templates_can_grow_beyond_the_initial_capacity` regression supplies a sparse source file of 512 MiB plus 4 KiB. Before the fix, the formatter produced matching geometry and file length of 640 MiB, but template conversion rejected that output against 512 MiB. Exact validation now belongs to `build_rootfs`; template conversion accepts formatter growth and retains the same owned worker for injection, cancellation cleanup, and publication.

The regression now passes a Linux kernel mount, verifies the full payload length and tail bytes, checks the distribution init and injected agent, and confirms a cache hit. The same source still fails an explicit 512 MiB `build_rootfs` request, preserves the previous destination, and leaves no temporary image. The template correction passed workspace formatting, strict `remote-image` all-target Clippy on macOS and Linux, 17 macOS rootfs unit tests, 19 Linux rootfs unit tests, and 4 kernel-mounted rootfs tests. The existing registry probe remained ignored in this run; its earlier measurements above were not rerun.

Reproduce the template regression without registry access:

```bash
sudo -E env ARCBOX_REQUIRE_BLOCK_TOOLS=1 BUSYBOX=/bin/busybox \
  cargo test --locked -p arcbox-computer-runtime --features remote-image \
  --test integration rootfs::cached_templates_can_grow_beyond_the_initial_capacity \
  -- --exact --nocapture
```

## Loop-device test isolation

[Linux CI job 112820142140](https://github.com/arcboxlabs/arcbox/actions/runs/37629615821/job/112820142140) checked out merge commit `0a70004ea9ceefcd89fc93dc0f2db6c2f87fc521` for PR head `a2ae15739b35a665f39be8574f81d53a500e6a77`. The template-growth regression passed. Both existing block-tool tests failed because a second detach of `/dev/loop4` succeeded after the tests observed its backing-file entry disappear. The job log did not record the backing identity at the second detach, so the actual next owner cannot be identified from that log.

The [isolated detach probe](../../tests/bench/loop-detach/probe.py) creates a high-numbered loop device and two private backing images. In Lima, both util-linux 2.39.3 and BusyBox 1.36.1 returned exit status 1 for a second detach of an idle device; the raw ioctl returned `ENXIO`. After the probe bound its second image to the same device number, detach returned success. The probe verified backing identity before detach and removed its device and temporary images afterward. This establishes the device-reuse mechanism; it does not identify the process that reused the CI device.

All loop users in the runtime integration binary now share `common::LOOP_DEVICE_TEST`. The lock covers mknod, both block-tool tests, rootfs mount tests, and cancellation cleanup. Synchronous tests use `blocking_lock()` and asynchronous tests use `lock().await`. The existing detach, contents, geometry, and cleanup assertions remain unchanged. The lock isolates this test binary; it does not coordinate other processes.

Before and after the isolation change, one local run with default test concurrency passed all 8 runtime integration tests and all 6 tap-net integration tests. No selected test was skipped or ignored. The CI failure and controlled probe supply the failure evidence; the local baseline did not reproduce the scheduling race. Workspace formatting and Linux nightly strict Clippy for every runtime target with `remote-image` also passed. The final probe passed Ruff lint and format checks with `sdk/python/pyproject.toml`, then completed its Linux run and cleanup.

Reproduce the probe and the integration check on a Linux host with root, Python 3, BusyBox, util-linux, nftables, and loop-device access:

```bash
sudo python3 tests/bench/loop-detach/probe.py
sudo -E env ARCBOX_REQUIRE_BLOCK_TOOLS=1 ARCBOX_REQUIRE_NFT=1 BUSYBOX=/bin/busybox \
  cargo test --locked -p arcbox-computer-runtime -p arcbox-tap-net --test integration
```

## Follow-ups

`containerregistry-registry 0.1.2` rejects the bare numeric-tag reference `debian:12` in practice. Its `find_tag_separator` treats the numeric suffix without a slash as a port, producing the wrong repository name. The registry probe returned `manifest not found` twice, including an unchanged escalated retry. The fully qualified reference above resolves the intended image. The [upstream parser](https://github.com/arcboxlabs/containerregistry-rs/blob/master/crates/registry/src/reference.rs#L224-L246) still contained the same code when checked on 2026-10-07. Track the parser correction upstream; callers must use a fully qualified reference until a corrected dependency is adopted.

Cargo reported existing dependency future-incompatibility advisories for `proc-macro-error2 2.0.1` and, under Linux nightly, `nix 0.29.0`. Strict source lint checks passed.
