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
