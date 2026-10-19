# 2026-10-19 — Align Clippy configuration with Rust 1.96

The workspace declares Rust 1.96 and devenv supplies Rust 1.96.0. The workspace lint table also allowed `unused_async_trait_impl`, a lint introduced in Clippy 1.98. Clippy 1.96 reported an unknown lint for each workspace crate.

Remove the unsupported lint entry. Pin the macOS and Linux validation workflows to Rust 1.96.0 so validation uses the same compiler and lint definitions as devenv. Keep `unused_async`, which is available in the configured toolchain.

Cargo can resolve an external subcommand from `CARGO_HOME/bin` before the devenv tool on `PATH`. On the development Mac, `cargo clippy --version` reported 1.99 while `cargo-clippy --version` reported 1.96. Use the pinned executable directly for local static checks:

```sh
devenv shell -- cargo-clippy clippy -p splicetcp -p arcbox-vmm --all-targets --features splicetcp/tokio-frame-sink -- -D warnings
devenv shell -- cargo-fmt --all -- --check
```

The Clippy command completed without warnings on 2026-10-19. This result is a local static check. ArcTap acceptance requires execution on the dedicated test Mac; the toolchain change does not establish runtime acceptance.
