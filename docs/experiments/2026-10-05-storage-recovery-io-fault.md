# 2026-10-05 — Does a real storage I/O failure reach the protection and recovery checks?

- Type: experiment
- Area: guest storage health, offline checks, write verification, Docker verification
- Outcome: all four privileged integration checks passed; the kernel forced Btrfs read-only after a device I/O failure and the production health observer reported `READ_ONLY`
- Probes: [`storage-recovery-linux.sh`](../../guest/arcbox-agent/tests/storage-recovery-linux.sh), [`linux_integration.rs`](../../guest/arcbox-agent/src/agent/linux/storage_check/linux_integration.rs)
- Guest: Lima `arcbox-m2`, aarch64, Linux `6.8.0-137-generic`; device-mapper `linear` and `error` targets; existing local Docker engine
- Tools: the rebuilt recovery rootfs contains Btrfs tools `7.1`, e2fsprogs `1.47.3`, and static BusyBox
- Initial rootfs SHA-256: `e0fe5227096fcbdfb522c3337bac1cdb72c91f39ba25e9aeb541e649b549d19f`; the corrected artifacts are recorded below

The [current status](../logs/2026-10-05-agent-v7-admission.md#current-status--2026-10-07) separates completed gates from remaining work. Earlier sections record the result and limitations of each historical run.

## Question

Can ArcBox observe a kernel-enforced read-only transition caused by failed storage I/O, reject writes afterward, and verify both offline storage and a complete local Docker lifecycle? A successful `mount -o remount,ro` check would not establish the failure behavior.

## Hypotheses

1. The production offline check functions can inspect clean Btrfs and ext4 devices without changing either backing image.
2. The production health and write checks accept writable mounts and reject mounted devices for offline checking.
3. The Docker check can import static BusyBox, run a container without networking, verify durable writes, and remove its container and image.
4. Replacing a disposable Btrfs device mapping with the kernel `error` target causes an I/O error, a kernel-forced read-only transition, and a `READ_ONLY` health observation.

## Method

Build the Linux agent test binary from the repository:

```sh
cargo test -p arcbox-agent --target aarch64-unknown-linux-musl --bin arcbox-agent --no-run
```

Copy the emitted executable, the checked-in shell harness, and the recovery EROFS image to a Linux test guest. Run the harness as root with the executable and rootfs paths:

```sh
sudo bash storage-recovery-linux.sh /absolute/path/to/agent-tests /absolute/path/to/rootfs-storage-recovery.erofs
```

The harness requires `dmsetup`, `losetup`, `mount`, `umount`, `mountpoint`, `truncate`, `blockdev`, and `dmesg`. The guest must support EROFS, Btrfs, ext4, loop devices, and the device-mapper `linear` and `error` targets. The Docker check requires a running local Docker engine. The harness installs no packages.

The harness creates one temporary directory, a 256 MiB Btrfs image, a 64 MiB ext4 image, two loop devices, and one uniquely named device-mapper mapping. It mounts the rootfs read-only to use the shipped filesystem tools and BusyBox. Each test calls the production guest implementation. The tests use explicit fixture paths and do not inspect or attach existing ArcBox data images.

First, the harness checks both unmounted devices and compares SHA256 hashes before and after the checks. Next, it mounts both filesystems and runs the actual health and write probes. The Docker check uses a unique image and container, `NetworkMode=none`, `sync -d /probe-data` to report `fdatasync` errors, and `sync -f /` after removal. Finally, the harness replaces only its own mapper table with `error`, writes and fsyncs an open Btrfs file, and waits for the health observer to report read-only. The test also requires a new kernel `forced readonly` message.

The fault test restores its original mapper table so cleanup can proceed. The shell exit trap unmounts its three mounts, removes its mapper, and detaches its loop devices. A cleanup failure fails the run and preserves the temporary directory. No recovery or repair utility writes to an existing ArcBox disk.

## Results

One complete run passed all four checks:

| Check | Result | Evidence |
|---|---|---|
| Clean offline checks | Passed | Both checker exits were successful; both image hashes stayed unchanged. |
| Mounted health and durable writes | Passed | Both mounts reported writable; create/write/fsync/read/remove/directory-fsync passed; offline checks rejected mounted devices. |
| Local Docker lifecycle | Passed | Import, create, start, file write/sync/read, wait with exit 0, container removal, and image removal all succeeded. |
| Actual I/O failure | Passed | The write/fsync returned `EIO`; Btrfs aborted its transaction and forced read-only; production health reported `READ_ONLY`; the write probe failed. |

The fault check completed in 29.97 seconds in this run. This elapsed time includes the kernel transaction path and is not a performance benchmark. The other three checks completed in 2.17, 0.01, and 0.40 seconds respectively.

The decisive output was:

```text
I/O failure: Err(Os { code: 5, kind: Uncategorized, message: "I/O error" })
Storage health: StorageVolumeHealth { role: DATA, state: READ_ONLY, device: "/dev/mapper/arcbox-storage-test-19005e8f-d834-4805-851a-6fb43d94cd2b", mount_point: "/tmp/arcbox-storage-test.ZNIalbbn/data", filesystem: "btrfs", detail: "The filesystem is mounted read-only; writes are unavailable." }
BTRFS: error (device dm-0) in btrfs_commit_transaction:2553: errno=-5 IO failure (Error while writing out transaction)
BTRFS info (device dm-0: state E): forced readonly
BTRFS warning (device dm-0: state E): Skipping commit of aborted transaction.
BTRFS error (device dm-0: state EA): Transaction aborted (error -5)
BTRFS: error (device dm-0: state EA) in cleanup_transaction:2047: errno=-5 IO failure
```

The harness exited with status 0 after cleanup. A subsequent `dmsetup ls` reported `No devices found`. `losetup -l` returned no loop devices. Docker queries for `arcbox-storage-check-*` returned no containers or images.

## Findings and limits

1. All four hypotheses held in this guest. The forced read-only result came from a kernel I/O failure, not a requested remount.
2. Read-only mount observations and successful write verification remain separate facts. The health observer performs no writes; the explicit recovery checks establish write behavior.
3. A healthy response requires successful Docker cleanup. A failed or timed-out Docker mutation reports failure; the response does not claim that a timed-out server operation left no resources.
4. The initial experiment verified Linux filesystem behavior and the guest check implementations. It did not identify the cause of the original VM fault, prove a hypervisor-specific block path, or complete the daemon/Desktop recovery workflow. Later live results are recorded below.

## Additional regression checks

The complete Linux agent binary unit suite ran as the unprivileged Lima user: 302 tests passed, two failed, and the four privileged integration tests were ignored by that invocation. The four integration tests then had the separate successful root invocation described above.

Both failed tests were repeated using a separate, unmodified source snapshot of baseline commit `1106545` and its own compiled Linux test binary. Each baseline test failed at the same assertion in the same guest:

```sh
baseline-agent-tests --exact machine_export::vfs::sidecar::tests::what_the_mac_writes_lands_as_the_targets_attributes
baseline-agent-tests --exact machine_export::vfs::sidecar::xattrs::tests::a_side_entry_of_a_recreated_file_is_dropped
```

The first assertion expected mode `0644` and received `0664`. The test creates its fixture with the inherited umask; this guest's umask was `0002`. The second assertion expected a removed-and-recreated file to lose its previous side entry, but the lookup still returned its resource fork. That implementation compares the inode number and optional creation time. This experiment did not distinguish inode reuse, unavailable creation time, or timestamp resolution as the cause of the identity collision.

No test assertions or sidecar implementation were changed. The full Linux unit gate remains failed in this environment; the baseline comparison establishes that these two failures were already reproducible at `1106545`.

## Daemon recovery scenario

The disposable [`storage_recovery`](../../tests/e2e/tests/storage_recovery.rs) scenario exercises the host workflow against the staged development rootfs and agent. Run each backend with the signed release daemon:

```sh
ARCBOX_VM_BACKEND=vz cargo test -p arcbox-e2e --test storage_recovery -- --ignored --nocapture
ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test storage_recovery -- --ignored --nocapture
```

The scenario first checks healthy paired disks, restarts the daemon, and verifies that check-only protection survives the restart. Explicit recovery must pass offline checks, durable write checks, and the complete local Docker lifecycle before removing protection.

Next, the scenario stops the daemon and changes only the disposable ext4 superblock's `s_blocks_count_lo` at byte offset 1028. The host's image identity, filesystem magic, and UUID checks must still pass. The changed field also invalidates the ext4 superblock checksum. This fixture establishes a guest mount failure after successful host identity checks; it does not isolate geometry validation from checksum validation.

The failed boot must retain the recovery API, stop the System VM, preserve the original mount error and fresh `UNAVAILABLE` observation, and publish durable protection. A recovery attempt must fail the metadata offline check. After stopping the daemon, the fixture restores and syncs the original four bytes. Another daemon restart must keep protection in place until explicit recovery passes all checks. Finally, the scenario removes the disposable metadata image and verifies that neither startup nor recovery recreates the missing member.

Every daemon shutdown must exit successfully. A failed scenario retains its temporary data directory and captures available live diagnostics. The scenario does not read or change production storage.

## Earlier Guest repeat before test corrections

A fresh static aarch64 test binary from source tree `5e73dc2932b4533e637412909db05df8442d6e8b` repeated all four privileged checks in Lima `arcbox-m2`. The test binary SHA-256 was `373c90d338eb70c9080af1eccb09251a63f6fe5eedcfe0bde85e137842554ace`. The original harness SHA-256 was `80945a665fa19af8aefbe94227a93d38d3d5a0c588c0fa786858ba2725e93f5c`. The recovery rootfs matched the SHA-256 recorded above.

The runner checked all four complete test names before invoking the original harness. Each invocation executed one passing test, and cleanup exited with status 0. Source and input hashes remained unchanged. The harness test-count correction had not been approved or applied for that run.

At that stage, the daemon recovery scenario was blocked by its `data/docker-data.img` fixture path; `Config::docker_img_path` uses `data/docker.img`. The correction was subsequently approved and applied on 2026-10-07. The Guest repeat did not establish the Host recovery workflow or clear the two baseline-reproduced Linux unit failures.

## Earlier application validation

The independent normal VZ/HV matrix passed on application tree `b430c58d3f2990894f220460a008053c8c5ad407`. Each backend completed all nine normal lifecycle phases, and both daemons exited with status 0. Actual binaries, signatures, Guest copies, and startup assets matched the recorded inputs. The matrix used the audited Guest whose production logic is unchanged; it did not rebuild the Guest or execute recovery orchestration. The [protocol and admission log](../logs/2026-10-05-agent-v7-admission.md#continuation--current-application-matrix-and-lima-prerequisites) records the exact evidence.

The Desktop storage regression gate also passed all 39 selected tests. The initial native GNU check in Lima stopped before compilation because its offline Cargo cache lacked `rcgen`. Dependencies, test corrections, and live integration had not yet been completed; the subsequent results are recorded below.

## Continuation — corrected rootfs and live recovery on 2026-10-07

Colima supplied the missing build dependencies. The initial official ARM64 and x86_64 rootfs builds passed their basic tool checks. A later corruption probe showed why clean-image checks alone were insufficient: unpatched e2fsck 1.47.3 reported an invalid group descriptor checksum but returned exit status `0`.

The independent reproduction used a freshly formatted 64 MiB ext4 image with no pending journal recovery. A clean `e2fsck -fn` invocation returned `0`. Flipping one checksum bit at byte 2078 produced `Group descriptor 0 checksum is 0x66b0, should be 0x66b1. IGNORED.` The damaged invocation also returned `0`. Both read-only checks preserved the full image hash. This reproduction did not depend on an unclean shutdown or journal contents. Its record is `/Users/Xuan/Developer/arcboxlabs/ext4-clean-check.VGieWNB2/result.json`, SHA-256 `01b2ce070c8c5bc064bfa797609a285d4235bc71677b89f6c9aa557a39006f0f`.

In `e2fsck/unix.c`, `check_super_block()` cleared filesystem validity after detecting the checksum error. A second unconditional `ext2fs_mark_valid()` discarded that result before the main passes. The boot-assets patch preserves the invalid state when `E2F_OPT_NO` disables repairs. The initial validity reset and repair-mode behavior remain unchanged. Guest recovery continues to use the checker's exit status; no output-text parser was added.

The checked-in boot-assets probe `src/build/scripts/check-e2fsck-readonly.sh` creates a 64 MiB ext4 image with explicit 4096-byte blocks and `metadata_csum`. Its group descriptor checksum starts at byte 4126. The probe requires a clean check to return `0`, flips one checksum bit in a copy, and requires the damaged check to return `4`. Both checks must preserve their whole-image hashes. The same probe failed against the previous rootfs because its damaged check returned `0`. Every official rootfs build now executes this probe with the produced static tools.

Both corrected official builds passed the probe. The tools extracted from each final EROFS artifact passed the probe again. The x86_64 tools ran under Colima's platform emulation:

| Architecture | Rootfs bytes | Rootfs SHA-256 | Clean / damaged exit status |
|---|---:|---|---|
| ARM64 | 7,122,944 | `4fcb3147b574b2d4568af23f16a47f52147e82676905ed884755f1ec37d61046` | `0` / `4` |
| x86_64 | 6,844,416 | `babcceb1122766e769a141ab316bdad91417efd35484a95e9e8e18f8773c4b0d` | `0` / `4` |

The artifacts were built from boot-assets tree `8afaae99fd3402d24ec1385033b232283cac785e`. Before commit, the patch was rewritten without context lines so the vendored patch passed Git's whitespace check. Applying either patch with the official `patch -p1` command produced byte-identical e2fsprogs source trees. The resulting `e2fsck/unix.c` SHA-256 was `731f51b8d0e5a416c4f317e90ec5918d4b8be9f2d5c3172a271ddcde715a5673`. Boot-assets commit `9a240aa77f676ae9913d06930ff16ad32def7324` contains the final patch format. The CLI was rebuilt to verify the embedded patch. The artifacts were retained with their actual build-tree identity; they were not rebuilt from the later patch-format tree. The final record is `/Users/Xuan/Developer/arcboxlabs/boot-assets-e2fsck-fix.8QDfDwV5/validation.json`, SHA-256 `7e47096a7a3a8d56146d6e8ba7d2f0860f80a51bbbf1281d0c299d63ce0e070a`.

An isolated VZ run used the corrected ARM64 artifact and the previously audited signed daemon, CLI, and Guest. `abctl disk check` completed and retained protection. The daemon exited with status `0`. On restart, `WatchSetupStatus` replayed the same operation ID, retained protection, and reported `vmRunning=false`. `abctl disk recover` then completed offline checks, durable writes on both volumes, and the Docker lifecycle. Only that successful recovery removed protection. The second daemon also exited with status `0`.

The Runtime and Desktop offline reports still contain free-block and free-inode count differences and an empty `orphan_present` diagnostic. The patch retains upstream `PR_NO_OK` behavior for these conditions, which can return `0` in read-only mode. A passed check therefore does not mean the report contains no ext4 diagnostics. The separate checksum-damage regression requires exit status `4`.

The Runtime record is `/private/tmp/arcbox-runtime-recovery-corrected.MZzQ5t/audit.json`, SHA-256 `a3ad7892b38ca9ee6743fe8fb32fb307291c4753bfacefc80cf0da21914d4ded`. A separate Desktop run passed both real XCTest actions through the production recovery model and watcher, including protected replay after daemon restart. Its record is `/private/tmp/arcbox-desktop-recovery.9rxelhve/audit.json`, SHA-256 `ca604071b8fa138992b73ecc71404a5b6ef47eee74abe9dfa22ba52c971ff273`. Both runs verified staged assets and signatures before and after execution. Bridge NIC creation, NFS mounting, and host integration were disabled. All temporary daemons exited.

These runs established the dual-architecture checker and healthy Desktop-to-Runtime recovery results. The corruption-and-missing-disk scenario had not run, and its fixture correction was still awaiting approval at that time. The earlier unpatched rootfs's successful lifecycle run did not establish corruption detection. No production pin changed.

## Continuation — healthy HV recovery without vmnet on 2026-10-07

The healthy recovery workflow passed on HV using a daemon built from committed Runtime source `08d1ec15fde7797dc02935f05b4353f89d1d3708`, tree `f1d4bd688e273897af645c16110ba83c5f8e8cf8`. The daemon build used `cargo build --locked --release -p arcbox-daemon --bin arcbox-daemon --no-default-features --features gic`. The feature graph and actual Cargo artifacts confirmed that the VMM had `default` and `gic`, with no `vmnet` feature. The CLI was built separately from the same source. The daemon retained Developer ID signing. Both builds recompiled the selected binaries from the recorded source paths.

The run used new temporary storage, independent sockets, dynamically allocated loopback ports, `--no-mount-nfs`, and the corrected ARM64 rootfs above. The Guest was reused with SHA-256 `32ef7b9235a96e6c11ae148b37f25d6b1d59a511b20fbc8163db95c49d21fe7e`. A source comparison found identical Guest Rust code, local dependency Rust code, and v7 Guest message definitions. The committed Host adds a `SystemService.RecoverStorage` method to its API descriptor; the reused Guest was not rebuilt from that commit and does not use that Host service.

`CHECK_ONLY` completed and retained its durable hold. After daemon exit `0` and restart, `WatchSetupStatus` replayed the same operation ID with protection enabled and `vmRunning=false`. `RECOVER` completed offline checks, durable write verification on both volumes, and the Docker import/create/start/write/sync/read/wait/remove lifecycle. Recovery then removed the hold. The second daemon also exited `0`.

Live device snapshots before the check and after recovery each showed HV vCPU counters, one primary network device, vsock, and no bridge network device. All 34 observed setup statuses reported no installed container route, DNS resolver, or Docker socket link. Observed host bridge interfaces, production container routes, resolver files, hosts file, and user SSH/Docker configuration were unchanged. The 1,466 source entries, host binaries, and staged assets retained their recorded contents.

The record is `/private/tmp/arcbox-hv-recovery-runner.z4xyf5kj/audit.json`, SHA-256 `0ec09fef54f47c9bb653fc4c88cc589a93e5f3847eac2ea6a4454a9ab72b596d`. The runner and exact build inputs are linked from that record. This result establishes healthy HV recovery without `vmnet`; it does not establish default-feature HV or fault recovery. Test corrections had not yet been applied during that run. No production pin changed.

## Current Guest validation on 2026-10-07

After approval, the checked-in harness requires all four complete test names and invokes each with `--exact --ignored`. Each invocation must report exactly one passing test. Guest commit `79bcc1f5ed5045f76160bd2c5aa96a969dd7a6f9` records the integration tests and harness. The validation built fresh static aarch64-musl test and release binaries from a 1,473-entry source snapshot. The snapshot remained unchanged, and all 355 Guest dependency, configuration, and protocol inputs matched the working source after the run. Three parallel API changes were recorded separately.

| Input | SHA-256 |
|---|---|
| Source manifest | `a5eef58e5e8f5819727ecd6d476b3be2f08f3ad1fe1d60ad0e399254ffb55a2c` |
| Test binary | `5c3cd5bde97db76e91d99a19a5d003f5b01647ab1160695afad69499976a41ec` |
| Harness | `adc22d6255587494a68ddf5c90f1d4c11edd5a28d6a8e2b7df9ca49bcd91d429` |
| Release Guest | `dbb6ab708fd700b8cfb12efd7a9633dce4a74f79af93d5730f66b0a1a3285523` |
| Corrected ARM64 rootfs | `4fcb3147b574b2d4568af23f16a47f52147e82676905ed884755f1ec37d61046` |

All four tests passed once in Lima `arcbox-m2`. Offline checks preserved both image hashes. Mounted durable writes and the complete Docker lifecycle passed. The fault test received `EIO`, observed `READ_ONLY`, rejected the write probe, and required the new kernel `forced readonly` message. Cleanup exited with status 0. Before and after inventories contained no test mapper, loop device, fixture directory, container, or image.

Workspace formatting and musl all-target Clippy with `-D warnings` passed. Compiler diagnostics contained zero warnings and errors; Cargo retained the existing `proc-macro-error2 v2.0.1` future-incompatibility notice. The record is `/private/tmp/arcbox-guest-approved-tests.vim94nmm/validation.json`, SHA-256 `7c985432f1036e0a20e68473c972f332312b3dad99e64b39daa635789912c74f`. This selected run does not clear the historical full Guest-suite failures or establish the complete Host fault E2E.

## Complete storage fault E2E on 2026-10-07

The unchanged `paired_storage_check_restart_and_metadata_faults` test passed on VZ and HV. Each invocation executed exactly one ignored test, with zero failures. The E2E library's 18 tests and strict all-target Clippy also passed. Commit `d03dd36f` contains the tested fixture and its dependency.

Both backends passed check-only protection, successful daemon shutdown, protected replay after restart, explicit recovery, durable writes on both volumes, and the Docker lifecycle. The metadata geometry/checksum fault preserved the original mount diagnostic and `UNAVAILABLE` observation. Recovery rejected the damaged metadata with e2fsck exit status `4`. Restoring the injected bytes did not bypass the hold; explicit recovery then passed. Removing metadata left the image missing through startup and recovery. Every scenario daemon shutdown returned success.

The daemon was built with `--no-default-features --features gic` and Developer ID signed. Its SHA-256 was `390e23271b08e845703ae2f725553c187f7cf7bb91fa3f77952aac0cb397a6c0`. The freshly built Guest SHA-256 was `dbb6ab708fd700b8cfb12efd7a9633dce4a74f79af93d5730f66b0a1a3285523`. The rootfs was the corrected ARM64 artifact above. The source snapshot contains 1,473 verified entries. Its manifest SHA-256 is `691fd0d4b92eea8bcf353892406673ec2071fa6321baeb79ba10d2131b60ea64`.

The daemon snapshot predates the separate Connect streaming-input correction in `7594e58d`. That correction has its own full API tests and strict Clippy. The fault scenario uses the SystemService recovery RPCs; its source matches the committed E2E byte for byte.

VZ used `ARCBOX_DIAG_DISABLE_BRIDGE_NIC=1`; HV had no `vmnet` feature. Both runs retained the test's normal NFS behavior under its isolated mount root. All successful test directories were removed. Observed resolver files, hosts file, Docker and SSH configuration, bridge interfaces, and production container routes remained unchanged. The elapsed times were 66.84 seconds for VZ and 56.48 seconds for HV; these are scenario durations, not performance benchmarks.

An initial VZ invocation failed before VM startup because the externally selected temporary root exceeded macOS's Unix socket path limit. Its record is preserved. The successful retry used a shorter temporary root with the same test binary and assertions.

The complete record is `/private/tmp/arcbox-approved-fault-e2e.k140d7vl/audit.json`, SHA-256 `0c8cae15e96ba10743354324f811ca2882a1e22d258c0d4ccc6f6c437f945f82`. The record links the source, build artifacts, commands, logs, and host boundary checks. This closes the corruption-and-missing-disk recovery gate. Default-feature HV, the historical Guest unit failures, and DAX remain outside the passed scope. No production pin, publication, or push changed.
