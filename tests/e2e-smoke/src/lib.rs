//! E2E test harness for ArcBox.
//!
//! Manages the full daemon + helper lifecycle so smoke tests can run
//! against a real ArcBox environment. The harness:
//!
//! - Kills any stale `arcbox-daemon` / `arcbox-helper` processes
//! - Cleans residual socket files
//! - Verifies port 5553 (DNS) is free
//! - Starts `arcbox-helper` under `sudo`
//! - Starts the signed `arcbox-daemon`
//! - Waits for the "ArcBox daemon started" stdout marker (up to 120s)
//! - Provides a [`TestEnvironment::docker`] helper to run Docker CLI commands
//! - Tears down everything on drop (SIGTERM -> wait -> SIGKILL)
//!
//! # Prerequisites
//!
//! Binaries must be pre-built and the daemon must be code-signed with a
//! Developer ID certificate **before** running tests:
//!
//! ```bash
//! make sign-daemon
//! make build-helper
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo test -p arcbox-e2e-smoke --test smoke -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is required because all tests share a single daemon
//! instance and port allocations (DNS on 5553, docker socket).

mod harness;

pub use harness::TestEnvironment;
