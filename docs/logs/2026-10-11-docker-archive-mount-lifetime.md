# 2026-10-11 — Docker archive teardown releases indexed overlay mounts

- Type: investigation
- Area: boot-assets dockerd, container CA injection
- Commits: boot-assets `807f3aa`, `f12d77a`
- Related: [signed candidate acceptance](2026-10-10-https-signed-candidates.md), [local HTTPS](../local-https.md)

## Problem

Native candidate acceptance failed during container CA injection and subsequent start. Docker returned `device or resource busy` while mounting overlayfs with `index=on,nfs_export=on`. The original installed Docker 29.8.2 reproduced the failure without the new CA code.

## Cause and change

Moby's `containerFSView.Close` waits for its worker's completion channel, then lazily unmounts the rootfs. The worker closes the channel before the Go runtime terminates its locked OS thread. Until that thread exits, its private mount namespace, root, and current directory retain references to the overlay mount. A subsequent mount of the same indexed upper directory can fail with `EBUSY`.

The patch restores the original mount namespace and filesystem root before reporting completion, including setup errors. The thread remains locked because `CLONE_FS` remains detached. Completion uses a buffered channel so panic cleanup cannot block on a receiver. The patch preserves the existing archive path validation and mount security checks.

The fix is vendored in boot-assets under `patches/dockerd/`. The release workflow builds Docker from the `docker-v29.8.2` tag, which resolves to the installed revision `8af9fe3a36bab3e039862a2ab1cef1880c9b4d03`. Docker retains its upstream static recipe, Go 1.26.8, CGO, journald, and storage drivers. Both patched daemons are appended to the release manifest. Republish retrieves their original build artifacts. No protocol version changes are required.

## Native evidence

All engine probes ran inside independent mount and network namespaces on the existing ArcBox 6.18.38 kernel. Each engine used fresh Btrfs-backed state and the installed patched containerd 2.3.6. The production App, helper, workloads, and CA trust were not replaced.

| Check | Result |
| --- | --- |
| Original installed engine, 100 immediate HEAD archive requests | 65 failures |
| Unmodified source, same diagnostic compiler and settings as the patch | 58 failures out of 100 |
| Patched diagnostic build | 0 failures out of 100 |
| Patched full static build with Go 1.26.8, CGO, and journald | 100 HEADs and 100 archive/start cycles passed |
| Checked-in regression against unmodified source | Failed on HEAD with HTTP 500; owned processes stopped |
| Actual ArcBox CA reconciliation against the patched native engine | 20 rotations, repeat injection, original-content preservation, and immediate starts passed |

Each archive/start cycle checks a missing path, writes a marker through PUT, reads and verifies the marker through GET, then starts the container immediately and requires exit code 0. No delay or retry separates these operations. The full static binary has SHA-256 `c3aba6a13a72b4a88fc0358b8df0b90cefe8dec7502f5c19332b9831d8578d54`.

Boot-assets formatting, Clippy, 15 unit tests, the local artifact-consumption E2E, Go vet, and actionlint passed. The native CA injection regression additionally builds the host adapter for the guest target; that build exposed macOS-only error conversions in the shared helper library, which now use the matching target guard.

The final native adapter test passed in 6.66 seconds with clean bridge and engine shutdown. The bridge cancels and joins its socket tasks before releasing pooled clients. The macOS Docker/helper checks passed strict Clippy and 151 unit tests; two opt-in tests were skipped by that unit command, and the native opt-in test was run separately as described above. Linux cross-target Clippy remains blocked by existing diagnostics in the hypervisor, VirtIO, and networking dependencies; those unrelated findings were not suppressed or changed.

## Reproduce

The engine driver is checked in at `boot-assets/tests/overlay-archive/`. Follow that directory's README to build and run the isolated engine check. The ignored runtime test `handlers::container::ca_injection::tests::native::native_archive_injection_survives_immediate_start_and_rotation` uses the same fixture through `ARCBOX_CA_TEST_NATIVE_SOCKET` and `ARCBOX_CA_TEST_NATIVE_CONTAINER`.

Build the runtime test binary:

```sh
devenv shell -- cargo test -p arcbox-docker --target aarch64-unknown-linux-musl --lib --no-run
```

Pass that executable to the engine driver after `--`, followed by `--ignored --exact handlers::container::ca_injection::tests::native::native_archive_injection_survives_immediate_start_and_rotation --nocapture`. The fixture remains alive until the test finishes, then removes its container and stops both daemons. The test uses generated public CA data and never changes host Keychain items or trust.

## Remaining acceptance

The updated signed App/runtime pair still needs installation and lifecycle acceptance. Host NFS browsing under the patched engine, browser HTTPS, the full macOS CA publication path, first-use authorization, and x86_64 execution are not established by these isolated checks. The new boot-assets code has not been pushed or published, and release pins remain unchanged.
