# Contributing to ArcBox

We welcome contributions of all kinds -- bug reports, feature requests, docs
improvements, and code.

## Building from Source

```bash
# Clone
git clone https://github.com/arcboxlabs/arcbox.git
cd arcbox

# Build
cargo build --release -p arcbox-cli -p arcbox-daemon

# Sign both binaries (required for macOS virtualization)
codesign --entitlements tests/resources/entitlements.plist --force -s - \
    target/release/arcbox target/release/arcbox-daemon

# Run
./target/release/arcbox --help
```

### Prerequisites

- Rust 1.85+ (install via [rustup](https://rustup.rs))
- macOS 13+ with Xcode Command Line Tools (`xcode-select --install`)
- ~500 MB disk space

### Guest Agent Cross-Compilation (Optional)

The `arcbox-agent` runs inside the Linux guest VM and must be cross-compiled:

```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add aarch64-unknown-linux-musl
cargo build -p arcbox-agent --target aarch64-unknown-linux-musl --release
```

## Project Structure

```
common/          Shared error types and constants
hypervisor/      Virtualization.framework bindings, VMM, VirtIO devices
services/        VirtioFS, networking (NAT/DHCP/DNS), container state, OCI
comm/            Protobuf definitions, gRPC services, vsock/unix transport
app/             Core orchestration, API server, Docker Engine API, CLI, daemon
guest/           In-VM agent (cross-compiled for Linux)
```

## Code Standards

- Run `cargo clippy -- -D warnings` and `cargo fmt` before committing -- zero warnings required
- All code comments must be in English
- `unsafe` blocks require a `// SAFETY:` comment explaining the invariant
- Use `thiserror` for crate-specific errors, `anyhow` in CLI/API layers
- Prefer `RwLock` over `Arc<Mutex<T>>` on hot paths

## Commit Guidelines

- Format: `type(scope): summary` (e.g. `fix(net): correct checksum on fragmented packets`)
- Keep commits atomic and compilable; target ~200 lines changed (excluding generated files)
- Do not add `Co-Authored-By` lines

## macOS Development Notes

- Virtualization.framework requires entitlement signing after every build:
  ```bash
  codesign --entitlements tests/resources/entitlements.plist --force -s - target/debug/arcbox
  ```
- Without signing, you get "Virtualization not available" errors

### Platform Pitfalls

- **libc `mode_t`**: `u16` on macOS, `u32` on Linux. Always use `u32::from(libc::S_IFMT)`.
- **xattr API**: Parameter order differs between macOS and Linux. Implement separately with `#[cfg(target_os)]`.
- **`fallocate`**: Not available on macOS. Use `ftruncate` as fallback.

## Uninstall

```bash
# Stop the daemon
arcbox daemon stop

# Restore Docker Desktop as default
arcbox docker disable

# Remove ArcBox files
rm -rf ~/.arcbox
rm /usr/local/bin/arcbox
rm /usr/local/bin/arcbox-daemon

# Remove the launchd service (if installed)
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/dev.arcbox.daemon.plist
rm ~/Library/LaunchAgents/dev.arcbox.daemon.plist
```

## License

By contributing, you agree that your contributions will be licensed under
[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
