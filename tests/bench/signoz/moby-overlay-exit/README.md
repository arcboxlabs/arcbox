# Moby mount-namespace exit experiment

These experimental patches target Moby Docker v29.8.2 and Linux 6.9 or later. They are retained research inputs, not part of the ArcBox runtime build. The measured guest uses Linux 6.18.38 arm64.

## Thread exit barrier

Apply [01-thread-exit.patch](01-thread-exit.patch) to a separate, clean Moby source tree:

```sh
git apply --check /absolute/path/to/01-thread-exit.patch
git apply /absolute/path/to/01-thread-exit.patch
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go test -mod=vendor -c \
  -o /absolute/output/unshare.test ./daemon/internal/unshare
```

Run the test binary as root inside the isolated guest. Set `TMPDIR` to a new owned directory on a filesystem that supports OverlayFS NFS exports. Do not use tmpfs or a host share as the upper/work backing filesystem.

```sh
TMPDIR=/run/arcbox/data/diagnostics/OWNED/tmp ./unshare.test \
  -test.v -test.count=20 -test.timeout=120s
```

The tests exercise normal completion, setup failure after mounting and pinning the working directory, and a real `pidfd_open` failure. The live-worker control must return `EBUSY`; immediate remount after the exit barrier must succeed. Missing kernel or filesystem support must fail the tests, not skip them.

Use [probe-overlay-lifecycle.sh](../probe-overlay-lifecycle.sh) around one repetition to retain the cleanup order. Run additional repetitions without tracing. See the [experiment](../../../../docs/experiments/2026-10-08-overlay-copy-lifetime.md) for the recorded method and limits.

## Docker integration

Apply [02-filesystem-view.patch](02-filesystem-view.patch) after the helper patch. It waits before shared-mount teardown on normal close and setup failure. It propagates archive close errors and retains shared mounts if the exit barrier fails. It does not change `archive.go`, path isolation, or the reversible namespace caller.

[build-check.sh](build-check.sh) records the exact Go 1.27.1 and musl GCC 14.2 cross-build used for the patched daemon. Run it with the patched Moby source directory and a new absolute output directory. The script retains the official static-build tags and CGO features, builds `dockerd` and `daemon.test`, and runs `go vet`. Run the selected daemon tests shown at the end of the script inside the guest. The control must use the same build settings, with only its source and `GitCommit` identity changed. Keep the original installed binary intact for restoration.

For the A/B/A comparison, use a fresh never-started ClickHouse-image container in each phase, with network `none` and restart policy `no`. Copy the same verified configuration into `/etc/clickhouse-server/config-0-0.yaml` ten times with tracing and then one hundred times without tracing. Stop at the first failure; do not retry. If all copies succeed, copy the file out and require the source and recovered SHA256 to match. Require `EBUSY → pass → EBUSY` for control, patched, and control phases. Preserve all prior containers and restore the verified original daemon after the experiment.

The experiment artifact retains the exact runners, binary identities, source snapshots, and raw results. Those runners contain session-specific PID/start-time guards and must not be reused against another VM without refreshing their identities. The patch sequence was applied independently to clean source; all five resulting files match the successful A/B/A source byte for byte.

This evidence covers the observed stopped-container copy race on Linux 6.18.38. It does not validate older kernels, every archive/mount topology, or the original ClickHouse signal-lock stall. The patches are not enabled in a production ArcBox runtime.
