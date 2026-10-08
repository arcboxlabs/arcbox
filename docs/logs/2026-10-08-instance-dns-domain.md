# 2026-10-08 — One DNS suffix reaches the host and System VM

- Type: change
- Area: daemon startup, guest DNS, domain routing, HTTPS ownership
- Related: [Local domains and HTTPS](../local-https.md)

## Cause and behavior

The daemon previously changed host DNS after `Runtime::init()`. The System VM always used `arcbox.local`. A configured suffix could therefore resolve on the host while discovery, guest DNS, service URLs, and HTTPS ownership used a different namespace.

`arcbox_constants::dns::Domain` now owns validation shared by the CLI, daemon, helper, and guest. The daemon applies the suffix before runtime initialization. The engine replaces all `arcbox.dns_domain=` tokens with the configured value. The guest reads that value once at startup. Invalid or duplicate guest parameters fail startup. Changing the suffix changes the desired boot command line and enters the existing machine drift check. Protocol and minimum protocol remain 7.

Managed HTTPS remains limited to the current CA's `arcbox.local` namespace. Other suffixes retain HTTP and native TLS, without managed routes or a misleading CA authorization request. This change does not provide arbitrary custom `.local` certificates or wildcards.

## Verification

The following checks passed through `devenv shell`:

- `cargo test -p arcbox-constants --features std`: 32 tests, including validation and kernel parameter parsing.
- `cargo check -p arcbox-constants --no-default-features`: the shared crate retains its `no_std` boundary.
- `cargo test -p arcbox-helper -p arcbox-engine -p arcbox-core -p arcbox-daemon -p arcbox-cli --lib --bins`: 618 passed, one native helper test ignored.
- `cargo test -p arcbox-agent --lib`: 97 host-executed tests, including DNS responses for a configured suffix.
- `cargo test -p arcbox-agent --bin arcbox-agent domains::`: 45 host-executed tests, including real HTTP/TLS probes, ownership, routing policy, and streaming relay behavior.
- `cargo clippy -p arcbox-constants -p arcbox-helper -p arcbox-engine -p arcbox-core -p arcbox-daemon -p arcbox-cli -p arcbox-e2e --all-targets -- -D warnings`: passed.
- `cargo clippy -p arcbox-agent --target aarch64-unknown-linux-musl --all-targets`: no code warnings. Cargo retains the existing future-incompatibility notice for `proc-macro-error2`.
- `cargo test -p arcbox-agent --target aarch64-unknown-linux-musl --bin arcbox-agent --no-run`: Linux test binary compiled; not executed.
- `cargo test -p arcbox-e2e --test local_https --no-run`: compiled; not executed.
- `cargo fmt --all --check`: passed.

The HTTPS E2E scenario accepts `ARCBOX_DNS_DOMAIN=run.dev.arcbox.local`. It verifies the public service directory and rejects the old default-suffix SNI, in addition to its existing container DNS, CA injection, and HTTPS checks.

## Remaining acceptance

The separate CPU-test daemon at `/private/tmp/arcbox-cpu-repro-w0v7qz49` remained live during these checks. No competing VM was started. Current macOS system DNS, browser authorization, same-profile restart after a suffix change, and foreign-suffix routing still require live acceptance. Host tests and Linux compilation do not establish those behaviors.

## Signed development artifact

`make build-runnable` succeeded with runtime `910d7844ff17d4a6d3fa0eca2d756f776add5a11` and Desktop `7f9601431fd4a76354532e9a1b1d1bd8c8fc0fe9`. The output is `arcbox-desktop/.build/DerivedData-Runnable/Build/Products/Debug/ArcBox.app`. The build used `ARCBOX_DIR` and `ARCBOX_HOST_TARGET_DIR` in this worktree and Developer ID identity `34B370D02E5D7D81DFD7739FA0BC0C25AE948ACC`. The build log is `/private/tmp/arcbox-https-domain-runnable-build-20261008.log`.

`codesign --verify --deep --strict` passed. The embedded CLI, helper, and daemon have no `/nix/store/` dynamic dependencies. The CLI and daemon report `0.8.2`; the resource release pin remains `v0.8.0`, so the source pair above identifies this development build. Its LaunchAgent contains `--profile development` and omits `--docker-integration`.

The embedded agents match the freshly built source binaries:

- `arcbox-agent`: SHA-256 `e1e5fd83fd119c6ecd8136819cc706a7858c5adb3dc590ae1e9dfcd92a2e63f7`.
- `vm-agent`: SHA-256 `8307bb403cd7bcc17b783c75c98499e9f46840e8217eda32f92c6f9fdfbe3fbd`.

The bundle was not installed or launched. Packaging still reports the optional `pstramp` terminal helper as absent; embedded terminal behavior was outside these HTTPS checks.
