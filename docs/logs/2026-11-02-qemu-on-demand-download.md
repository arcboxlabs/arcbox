# 2026-11-02 — The daemon downloads the pinned QEMU build on demand

- Type: change
- Area: `arcbox-asset` (resumable download), `arcbox-image` (`qemu`), `arcbox-daemon` (System startup), `arcbox-api`, `abctl`, `api.proto`, `assets.lock`
- Commits / PRs: this change
- Related: [no-virtualization host](2026-11-02-no-virtualization-host.md), [daemon lifecycle](../daemon-lifecycle.md) "Independent readiness", [data directories](../data-directories.md) §1.5

## Problem or trigger

A Mac without hardware virtualization can only run the System VM under
QEMU TCG. QEMU is not part of the app bundle: the daemon installs one
pinned build, built by `arcboxlabs/qemu` and published as
`https://boot.arcboxcdn.com/qemu/v{version}/qemu-darwin-arm64-v{version}.tar.gz`,
and only on the Macs that need it. A Mac that cannot reach the CDN needs
another way to install the same archive, and the wait for a 6 MB
download has to be visible to the user.

## What was done

- `assets.lock` gains `[qemu]` (`version`, `cdn`, `arch.<arch>.sha256`),
  compiled into the daemon like the boot pin. An empty SHA-256 pins no
  build: nothing is downloaded, and the daemon behaves as before. The
  development profile honours `ARCBOX_QEMU_VERSION`, `ARCBOX_QEMU_SHA256`
  and `ARCBOX_QEMU_CDN`; a production daemon ignores them with a warning.
- `arcbox_asset::download_resumable` keeps the partial file, asks for
  `Range: bytes=N-`, appends on `206` and starts over on `200`. A `416`
  keeps a file that is already complete and empties one longer than the
  entity. It does not hash; the caller checks the finished file.
- `arcbox_image::qemu::QemuRuntime` installs into
  `<data_dir>/runtime/qemu/<version>/` under an exclusive `.lock` shared by
  the daemon and `abctl`. The archive is checked against the pin, extracted
  into staging (regular files and directories only), checked against
  `BUILD-INFO.json` with `licenses/` kept, cleared of quarantine, checked
  with `codesign --verify --strict` per binary, marked with its SHA-256 and
  renamed into place; other versions are removed. A pinned build needs
  ArcBox's Developer ID (Team `422ACSY6Y5`) on every Mach-O, plus the
  identifier `com.arcboxlabs.qemu.system-aarch64` on the executable. The
  development override accepts ad-hoc signatures but still requires that
  identifier.
- `QemuRuntime::ensure` retries transient failures (network errors, 5xx,
  408, 429) from 1 s doubling to 30 s until shutdown, resuming each time,
  and rechecks the install before each attempt, so an `abctl qemu import`
  ends the wait. A checksum mismatch of a resumed file retries from zero
  once; of a fresh file it fails.
- System startup on a host without virtualization runs `ensure` before it
  reports the host reason. `SystemRuntimeStatus` gains `DOWNLOADING` and a
  `ComponentDownload` (component, version, stage, downloaded and total
  bytes), set exactly while the state is `DOWNLOADING`. Transfer updates
  are folded to one per 250 ms.
- `abctl qemu import <archive> [--data-dir]` runs the same install from a
  local file.

## Evidence

Taken on an Amp Mac satellite (macOS 26, no hardware virtualization) with
`tests/bench/qemu-download/run.sh`: an isolated development daemon, the
dev override pointed at `cdn.py` on `127.0.0.1:18080` serving the CI
archive `qemu-darwin-arm64-v0.0.0-dev.0169f2b.tar.gz` (5,930,315 bytes,
SHA-256 `53657969…092aa`, QEMU 10.2.4, ad-hoc signed) at 1.5 MB/s, and
`watch.py` printing every `WatchSetupStatus` update.

| Scenario | CDN requests | System status | Installed |
|---|---|---|---|
| full | `200`, 5,930,315 B | 19 `TRANSFERRING` updates 0.28 s apart, 8,056 → 5,930,315 B; `VERIFYING` 5.09 s, `INSTALLING` 5.10 s, `STARTING` 5.20 s, then the host-reason `FAILED` | yes, `licenses/` with 6 projects, 0 quarantined files, `--version` 10.2.4 |
| resume | `200` cut at 2,000,000 B; `Range: bytes=2000000-` → `206`, 3,930,315 B | `WAITING_TO_RETRY 2000000/5930315` "QEMU download interrupted: … end of file before message length reached. Retrying in 1 s; …", then as above | yes |
| restart | `200` (2,162,688 B sent) until SIGTERM; new daemon `bytes=2097152-` → `206` | 24 transfer updates across both daemons, then as above | yes |
| checksum | one `200`, no retry | `FAILED` "Failed to install QEMU …: sha256 mismatch … expected 2d71…, got 5365…" | no, `.downloads/` empty |
| import | `503` | `WAITING_TO_RETRY 0/0` "… HTTP 503 … install it with `abctl qemu import <archive>`"; `abctl qemu import` of a copy carrying `com.apple.quarantine` installs it; the next attempt finds it without a request: `STARTING` 0.93 s | yes, 0 quarantined files |
| signature | one `200` | binaries built with the CI archive pinned in `assets.lock`: `FAILED` "… rejected QEMU build: lib/libglib-2.0.0.dylib fails its code signature check: test-requirement: code failed to satisfy specified code requirement(s)"; `abctl qemu import` fails the same way | no |

The Developer ID requirement is the one `codesign` parses: the
`arcbox-image` test checks it against `/bin/ls` (Apple-signed: rejected),
and with Chrome's team and identifier substituted it accepted Chrome's
`chrome_crashpad_handler` and rejected it with ArcBox's.

## Follow-ups

- `assets.lock` names no build until `arcboxlabs/qemu` v0.1.0 is signed,
  notarized and published; fill in `version` and `arch.arm64.sha256` then.
- The QEMU backend selects the executable; a user-configured QEMU path
  must take precedence over the installed build.
- `SetSystemVmBackend(QEMU)` does not download yet: only a host without
  virtualization does.
- Boot-asset downloads do not publish `ComponentDownload`.
- `abctl qemu import` waits for the root's lock while a daemon transfer
  holds it.
