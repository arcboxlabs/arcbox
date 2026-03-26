//! ArcBox-managed host tool download, installation, and management.
//!
//! Reads tool versions and checksums from `assets.lock`, downloads managed host
//! binaries (Docker CLI tools, `kubectl`, and future ArcBox-owned helpers),
//! verifies SHA-256 checksums, and installs them to `~/.arcbox/runtime/bin/`.

pub mod lockfile;
pub mod manager;
pub mod registry;

pub use lockfile::{ToolEntry, ToolGroup, parse_tools, parse_tools_for_group};
pub use manager::{HostToolError, HostToolManager};
