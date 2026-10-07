# 2026-10-07 — Caller-owned rootfs capacity and publication passed Linux validation

- Type: change
- Area: `arcbox-computer-runtime` rootfs construction
- Commits / PRs: `4ac4d025`, `db573604`, `15cdc217`, `6336c042`; integrates the effective behavior from `f3680d93`
- Related: [Runtime README](../../computer/arcbox-computer-runtime/README.md), [kernel-mounted rootfs probes](../../computer/arcbox-computer-runtime/tests/integration/rootfs.rs)

## Problem or trigger

The cached rootfs converter fixed capacity at 512 MiB and owned the output path. Node image builders need a larger sparse image at a caller-owned path. The caller must also retain the config and manifest digest from the same registry resolution that supplies the image contents.

## What was done

- Added `RootfsSource`, `RootfsSpec`, and `RootfsBuilder::build_rootfs`. The caller supplies a directory or resolved image, the output path, and capacity.
- Required positive capacities in 128 MiB units. Required both the declared ext4 capacity and file length to match the request before injection because the formatter can enlarge the image or declare more blocks than the file contains.
- Converted into a sibling temporary file. Injected the agent before publishing with an atomic rename. Failed conversion, injection, and publication preserve the destination and remove temporary files.
- Exposed `inject_vm_agent` and `VM_AGENT_PATH`. Default kernel arguments select `init=/sbin/vm-agent`; converted images retain the distribution's init.
- Reused publication for cached 512 MiB templates. Preserved the `rootfs-<layer>-<agent>.ext4` name used by template identity.
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

The local-image probes verify 256 MiB caller capacity, unchanged 512 MiB cache capacity and cache hits, destination replacement, repeated agent injection, preservation of distribution init, and cleanup after injection or publication failure. Unit tests reject zero and unaligned capacity, detect over-declared ext4 geometry, and verify cleanup after source-read failure.

A review regression supplies a sparse overlay file of 128 MiB plus 4 KiB with a requested capacity of 128 MiB. The formatter enlarges the image to 256 MiB, so comparing declared geometry only with file length did not reject the oversized output. The regression failed against that implementation because execution reached agent injection. The capacity check now rejects both size mismatches before injection, preserves an existing destination, and removes the temporary image.

After this correction, workspace formatting, strict all-target Clippy with `remote-image` on macOS and Linux, 232 macOS library tests, and 42 macOS manager tests passed. Linux passed all 5 rootfs build unit tests and both exact rootfs integration tests with `ARCBOX_REQUIRE_BLOCK_TOOLS=1`; no selected test was ignored or skipped.

## Follow-ups

`containerregistry-registry 0.1.2` rejects the bare numeric-tag reference `debian:12` in practice. Its `find_tag_separator` treats the numeric suffix without a slash as a port, producing the wrong repository name. The registry probe returned `manifest not found` twice, including an unchanged escalated retry. The fully qualified reference above resolves the intended image. The [upstream parser](https://github.com/arcboxlabs/containerregistry-rs/blob/master/crates/registry/src/reference.rs#L224-L246) still contained the same code when checked on 2026-10-07. Track the parser correction upstream; callers must use a fully qualified reference until a corrected dependency is adopted.

Cargo reported existing dependency future-incompatibility advisories for `proc-macro-error2 2.0.1` and, under Linux nightly, `nix 0.29.0`. Strict source lint checks passed.
