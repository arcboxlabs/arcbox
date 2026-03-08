//! Parser for the `[[tools]]` section of `assets.lock`.

use serde::Deserialize;

/// Top-level lockfile structure (only the parts we care about).
#[derive(Debug, Deserialize)]
pub struct AssetsLock {
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
}

/// Tool installation group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroup {
    Docker,
    Kubernetes,
}

/// A single `[[tools]]` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub group: ToolGroup,
    pub version: String,
    #[serde(default)]
    pub sha256_arm64: Option<String>,
    #[serde(default)]
    pub sha256_x86_64: Option<String>,
}

impl ToolEntry {
    /// Returns the SHA-256 checksum for the given architecture, if present.
    #[must_use]
    pub fn sha256_for_arch(&self, arch: &str) -> Option<&str> {
        match arch {
            "arm64" | "aarch64" => self.sha256_arm64.as_deref(),
            "x86_64" | "amd64" => self.sha256_x86_64.as_deref(),
            _ => None,
        }
    }
}

/// Parse the `[[tools]]` entries from `assets.lock` TOML content.
pub fn parse_tools(lock_toml: &str) -> Result<Vec<ToolEntry>, toml::de::Error> {
    let lock: AssetsLock = toml::from_str(lock_toml)?;
    Ok(lock.tools)
}

/// Parse the `[[tools]]` entries for a specific tool group.
pub fn parse_tools_for_group(
    lock_toml: &str,
    group: ToolGroup,
) -> Result<Vec<ToolEntry>, toml::de::Error> {
    Ok(parse_tools(lock_toml)?
        .into_iter()
        .filter(|tool| tool.group == group)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[boot]
version = "0.3.0"
cdn = "https://boot.arcboxcdn.com"

[[tools]]
name = "docker"
group = "docker"
version = "27.5.1"
sha256_arm64 = "aaa"
sha256_x86_64 = "bbb"

[[tools]]
name = "docker-buildx"
group = "docker"
version = "0.21.1"
sha256_arm64 = "ccc"

[[tools]]
name = "kubectl"
group = "kubernetes"
version = "1.34.3"
sha256_arm64 = "ddd"
"#;

    #[test]
    fn parse_tool_entries() {
        let tools = parse_tools(SAMPLE).unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "docker");
        assert_eq!(tools[0].group, ToolGroup::Docker);
        assert_eq!(tools[0].version, "27.5.1");
        assert_eq!(tools[0].sha256_for_arch("arm64"), Some("aaa"));
        assert_eq!(tools[0].sha256_for_arch("x86_64"), Some("bbb"));
        assert_eq!(tools[1].sha256_for_arch("x86_64"), None);
    }

    #[test]
    fn parse_tools_for_specific_group() {
        let docker_tools = parse_tools_for_group(SAMPLE, ToolGroup::Docker).unwrap();
        assert_eq!(docker_tools.len(), 2);
        assert!(
            docker_tools
                .iter()
                .all(|tool| tool.group == ToolGroup::Docker)
        );

        let kubernetes_tools = parse_tools_for_group(SAMPLE, ToolGroup::Kubernetes).unwrap();
        assert_eq!(kubernetes_tools.len(), 1);
        assert_eq!(kubernetes_tools[0].name, "kubectl");
    }
}
