# 2026-10-07 — Storage recovery uses published boot assets

- Type: change
- Area: storage provisioning, recovery, guest metadata, boot assets
- PR: [Runtime #734](https://github.com/arcboxlabs/arcbox/pull/734)
- Related: [I/O fault experiment](../experiments/2026-10-05-storage-recovery-io-fault.md), [side-entry identity](2026-10-07-sidecar-inode-generation.md)

## Review corrections

Interrupted provisioning now retains authority in a durable manifest before publishing the final image paths. A retry resumes only the original staged files with matching identities, UUID headers, and `New` state. A file lock serializes concurrent provisioning. A missing finalized image without its original staging link remains an error.

Recovery can recreate a missing System VM record from verified cached assets and the existing verified pair while retaining the maintenance reservation and durable hold. This operation does not provision disks, download assets, or boot a VM.

Adopted pairs without a manifest enter `LegacyPair`. The guest verifies every ext4 metadata destination before accepting the pair as `Paired`. Missing destinations remain protected: the old format cannot distinguish an interrupted copy from a deleted authoritative destination with stale Btrfs data. Recovery must not silently copy that stale data. The regression includes an empty original ext4 destination that had no migration marker.

Side entries use native inode-generation handles scoped to persistent filesystem identity. The immediate recreation regression has no sleep. Linux, musl, symlink, remount, cross-filesystem, and overlay checks cover the identity behavior.

## Published inputs

Boot-assets [v0.8.9](https://github.com/arcboxlabs/boot-assets/releases/tag/v0.8.9) was built from `e63104bb412a964b5007a139ce96bfad0ae5f57f` with kernel `v0.0.25`. [Release run 37607821140](https://github.com/arcboxlabs/boot-assets/actions/runs/37607821140), attempt 2, completed GitHub and R2 publication. Attempt 1 failed an Alpine pull before publication; published bytes were not rebuilt or replaced.

| Input | SHA-256 |
|---|---|
| Merged manifest, pinned by `assets.lock` | `f2003a5919551afd3f34ebeea6b20acdf0ba2bd55fd855068cc33fd8b7aa037b` |
| ARM64 bundle | `e552dd2bf3cccd56f7998d0e6dfced752398e913a42a8a5e2cdfabf4b08f9bac` |
| Published ARM64 rootfs | `2f3a77e9c8de61f2083bc6e9690c9422268c5b1aa6c77c01f2c51f58f594c9cd` |
| Fresh Runtime v7 guest | `7e8b94d096ba6e2b1ccb68177f96298d537baba999958f90e4f65d10d4037295` |

Both rootfs builds require read-only e2fsck to return `0` for a clean image and `4` for checksum damage, preserving both image hashes. Five GitHub release assets, checksum sidecars, archive contents, kernel digests, and all ten ARM64 runtime objects were verified. CDN consumption passed on [x86_64 with QEMU boot](https://github.com/arcboxlabs/boot-assets/actions/runs/37610196828) and [ARM64 without QEMU boot](https://github.com/arcboxlabs/boot-assets/actions/runs/37610274748). R2 checks returned HTTP 200. The local CDN request returned Cloudflare 1010/403, so direct local byte comparison of GitHub and CDN manifests remains unverified.

## Validation

Runtime source was `4dda96ed78fb46d75dc1ef2fe46e446666198fd9` with the new boot pin. No code changed after the following checks.

| Check | Result |
|---|---|
| Host workspace | `cargo fmt --check`, strict workspace Clippy excluding the Linux guest, `cargo xtask check-layers`, and all workspace tests excluding the guest passed. The layer check covered 68 members and 190 edges, with 2 grandfathered edges. |
| Guest | Strict all-target musl Clippy passed. Native Linux tests passed: 135 library, 313 binary, and 7 DNS tests; 6 existing binary tests remained ignored. |
| Published-rootfs Linux driver | All four privileged storage checks passed. The I/O fault returned `EIO`, the kernel forced Btrfs read-only, and the observer reported `READ_ONLY`. Offline image hashes stayed unchanged. Durable writes and the Docker lifecycle passed. |
| Side-entry driver | 25 ext4 tests, 2 privileged mount tests, and 25 overlay tests passed. Fixture mounts, loop devices, mapper entries, images, and containers were removed. |
| VZ daemon recovery | Passed with the default-feature signed daemon and `ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1`. |
| HV daemon recovery | Passed with `--no-default-features --features gic`. The default HV bridge path remains unverified. |

The daemon scenario checks both disks, restarts under a durable hold, removes `machines/default/config.toml`, and recovers by rebuilding that record. It then rejects metadata corruption, preserves protection through restart, verifies recovery after restoring the fixture, and rejects a missing metadata image without creating a replacement. Successful recovery requires durable writes on both volumes and a Docker container lifecycle. The scenario uses temporary disks and independent sockets; it does not use production storage.

Reproduce the storage fault check with `guest/arcbox-agent/tests/storage-recovery-linux.sh` and the side-entry check with `tests/bench/sidecar-identity/run.sh`. Both drivers accept the compiled Linux agent test binary. The storage driver also accepts the published rootfs. The daemon check is `cargo test --locked -p arcbox-e2e --test storage_recovery -- --ignored --nocapture`, with matching staged development assets and a Developer ID signed release daemon.

For VZ, set `ARCBOX_VM_BACKEND=vz ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1`. For HV, build the daemon separately with `cargo build --locked --release -p arcbox-daemon --bin arcbox-daemon --no-default-features --features gic`, sign the new binary, and set `ARCBOX_VM_BACKEND=hv`. Both runs keep the test's isolated NFS behavior. The signed VZ daemon SHA-256 was `68d60a63933f3f8f396b4efe2a59d581b6443c5ed5211e512747519fbd8a71a5`; the signed HV daemon SHA-256 was `34aae91a52e5b6c9d900fddca64282b8812fe1708c2bb6b9bc16e8f80114cd83`.

Local evidence is in `/private/tmp/arcbox-redwhisk-integration.MHIwSg3Y/734-final-host-gates.log`, `734-linux-integrated-root.log`, `734-vz-recovery.log`, and `734-hv-recovery.log`; release verification is in `/private/tmp/arcbox-boot-0.8.9-verification.h0B50MFb/release-validation.json`; privileged Linux verification is in `/private/tmp/arcbox-published-guest.0hk94rf1/validation.json`.
