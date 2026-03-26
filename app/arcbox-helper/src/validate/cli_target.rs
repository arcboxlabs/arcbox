use std::path::Component;
use std::str::FromStr;

/// A validated CLI symlink target path inside an app bundle.
///
/// Guarantees:
/// - Absolute path under `/Applications/` or `/Users/`
/// - Contains `.app/Contents/MacOS/xbin/` structure
/// - No `..` path traversal
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliTarget(String);

impl CliTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CliTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = std::path::Path::new(s);

        if !path.is_absolute() {
            return Err(format!("CLI target '{s}' must be an absolute path"));
        }

        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("CLI target '{s}' contains '..' path traversal"));
        }

        let components: Vec<_> = path.components().collect();
        let has_valid_app_structure = components.windows(4).any(|w| {
            matches!(&w[0], Component::Normal(name) if name.to_string_lossy().ends_with(".app"))
                && w[1] == Component::Normal("Contents".as_ref())
                && w[2] == Component::Normal("MacOS".as_ref())
                && w[3] == Component::Normal("xbin".as_ref())
        });

        if !has_valid_app_structure {
            return Err(format!(
                "CLI target '{s}' must be inside an .app bundle's Contents/MacOS/xbin/"
            ));
        }

        if !s.starts_with("/Applications/") && !s.starts_with("/Users/") {
            return Err(format!(
                "CLI target '{s}' must be under /Applications/ or /Users/"
            ));
        }

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for CliTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
