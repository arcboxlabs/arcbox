//! Parser for the `[[tools]]` section of `assets.lock`.

use std::collections::HashMap;

use serde::Deserialize;

/// Top-level lockfile structure (only the parts we care about).
#[derive(Debug, Deserialize)]
pub struct AssetsLock {
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
}

/// Per-architecture metadata for a tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchEntry {
    pub sha256: String,
}

/// A single `[[tools]]` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub arch: HashMap<String, ArchEntry>,
}

impl ToolEntry {
    /// Returns the SHA-256 checksum for the given architecture, if present.
    #[must_use]
    pub fn sha256_for_arch(&self, arch: &str) -> Option<&str> {
        // Accept common aliases.
        let key = match arch {
            "aarch64" => "arm64",
            "amd64" => "x86_64",
            _ => arch,
        };
        self.arch.get(key).map(|e| e.sha256.as_str())
    }
}

/// Parse the `[[tools]]` entries from `assets.lock` TOML content.
pub fn parse_tools(lock_toml: &str) -> Result<Vec<ToolEntry>, toml::de::Error> {
    let lock: AssetsLock = toml::from_str(lock_toml)?;
    Ok(lock.tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[boot]
version = "0.5.2"
cdn = "https://boot.arcboxcdn.com"

[[tools]]
name = "docker"
version = "27.5.1"
arch.arm64.sha256 = "aaa"
arch.x86_64.sha256 = "bbb"

[[tools]]
name = "docker-buildx"
version = "0.21.1"
arch.arm64.sha256 = "ccc"
"#;

    #[test]
    fn parse_tool_entries() {
        let tools = parse_tools(SAMPLE).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "docker");
        assert_eq!(tools[0].version, "27.5.1");
        assert_eq!(tools[0].sha256_for_arch("arm64"), Some("aaa"));
        assert_eq!(tools[0].sha256_for_arch("x86_64"), Some("bbb"));
        assert_eq!(tools[1].sha256_for_arch("x86_64"), None);
    }

    #[test]
    fn arch_aliases() {
        let tools = parse_tools(SAMPLE).unwrap();
        // "aarch64" should resolve to "arm64"
        assert_eq!(tools[0].sha256_for_arch("aarch64"), Some("aaa"));
        // "amd64" should resolve to "x86_64"
        assert_eq!(tools[0].sha256_for_arch("amd64"), Some("bbb"));
    }
}
