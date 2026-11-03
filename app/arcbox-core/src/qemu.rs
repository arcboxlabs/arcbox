//! Where the QEMU backend's `qemu-system-aarch64` comes from.

use crate::config::Config;
use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[qemu]` in `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QemuConfig {
    /// The `qemu-system-aarch64` to run. `ARCBOX_QEMU_PATH` sets it through
    /// the config's environment layer.
    pub path: Option<PathBuf>,
}

/// Resolves the `qemu-system-aarch64` the QEMU backend runs.
///
/// # Errors
///
/// Returns [`CoreError`] `InvalidState` naming the setting when no usable
/// binary is configured.
pub async fn binary(config: &Config) -> Result<PathBuf> {
    let Some(path) = &config.qemu.path else {
        return Err(CoreError::invalid_state(
            "the QEMU backend needs a qemu-system-aarch64; \
             set [qemu] path in config.toml or ARCBOX_QEMU_PATH to one",
        ));
    };
    if !path.is_file() {
        return Err(CoreError::invalid_state(format!(
            "QEMU binary {} ([qemu] path / ARCBOX_QEMU_PATH) is not a file",
            path.display()
        )));
    }
    Ok(path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Figment;
    use figment::providers::{Env, Serialized};

    #[tokio::test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    async fn the_path_comes_from_the_environment_layer_and_must_be_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(dir.path());
        let error = binary(&config).await.unwrap_err().to_string();
        assert!(error.contains("ARCBOX_QEMU_PATH"), "{error}");

        config.qemu.path = Some(dir.path().to_path_buf());
        let error = binary(&config).await.unwrap_err().to_string();
        assert!(error.contains("is not a file"), "{error}");

        let qemu = dir.path().join("qemu-system-aarch64");
        std::fs::write(&qemu, "").unwrap();
        figment::Jail::expect_with(|jail| {
            jail.set_env("ARCBOX_QEMU_PATH", qemu.to_str().unwrap());
            config = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Env::prefixed("ARCBOX_").split("_"))
                .extract()?;
            Ok(())
        });
        assert_eq!(binary(&config).await.unwrap(), qemu);
    }
}
