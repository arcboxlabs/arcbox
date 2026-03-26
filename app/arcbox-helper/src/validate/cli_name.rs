use std::str::FromStr;

/// A validated CLI tool name from the allow list (e.g. `docker`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliName(String);

impl CliName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CliName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if arcbox_constants::paths::DOCKER_CLI_TOOLS.contains(&s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "CLI name '{s}' is not in the allow list: {}",
                arcbox_constants::paths::DOCKER_CLI_TOOLS.join(", ")
            ))
        }
    }
}

impl std::fmt::Display for CliName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
