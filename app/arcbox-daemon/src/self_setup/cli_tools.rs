//! Creates `/usr/local/bin/` symlinks for Docker CLI tools bundled in the app.

use std::path::PathBuf;

use arcbox_helper::client::{Client, ClientError};

use super::SetupTask;

/// Docker CLI tool names to symlink.
const CLI_TOOLS: &[&str] = &[
    "docker",
    "docker-buildx",
    "docker-compose",
    "docker-credential-osxkeychain",
];

pub struct CliTools {
    /// Path to `Contents/MacOS/xbin/` inside the app bundle.
    pub xbin_dir: PathBuf,
}

#[async_trait::async_trait]
impl SetupTask for CliTools {
    fn name(&self) -> &'static str {
        "CLI tools"
    }

    fn is_satisfied(&self) -> bool {
        CLI_TOOLS.iter().all(|name| {
            let link = PathBuf::from(format!("/usr/local/bin/{name}"));
            std::fs::read_link(&link)
                .is_ok_and(|target| target.to_string_lossy().contains("ArcBox"))
        })
    }

    async fn apply(&self, client: &Client) -> Result<(), ClientError> {
        for name in CLI_TOOLS {
            let target = self.xbin_dir.join(name);
            if target.exists() {
                client.cli_link(name, &target.to_string_lossy()).await?;
            }
        }
        Ok(())
    }
}
