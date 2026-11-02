//! Generic asset download, verification, and caching primitives.
//!
//! This crate provides the building blocks used by `arcbox-docker-tools`
//! (Docker CLI binaries) and `arcbox-image` (the on-demand QEMU runtime) for
//! downloading files from the network, resuming interrupted transfers,
//! verifying SHA-256 checksums, and writing them atomically to a local cache.
//!
//! # Feature flags
//!
//! - **`download`** — enables the async download functions (pulls in `reqwest`,
//!   `sha2`, `tokio`, `futures-util`).  Without this feature only the type
//!   definitions (`progress`, `error`, `arch`) are available.

pub mod arch;
pub mod error;
pub mod progress;

#[cfg(feature = "download")]
pub mod download;

pub use arch::current_arch;
pub use error::{AssetError, Result};
pub use progress::{PreparePhase, PrepareProgress, ProgressCallback};
