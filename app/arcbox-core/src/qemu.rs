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
/// That is `[qemu] path` when set, else the build `assets.lock` pins once it
/// is installed. System startup on QEMU installs that build first;
/// `abctl qemu import` installs it from a local archive.
///
/// # Errors
///
/// Returns [`CoreError`] `InvalidState` saying what to do when neither is
/// usable: a configured path that is not a file, a pinned build that is not
/// installed, or a daemon that pins no build.
pub async fn binary(config: &Config) -> Result<PathBuf> {
    if let Some(path) = &config.qemu.path {
        if !path.is_file() {
            return Err(CoreError::invalid_state(format!(
                "QEMU binary {} ([qemu] path / ARCBOX_QEMU_PATH) is not a file",
                path.display()
            )));
        }
        return Ok(path.clone());
    }
    let Some(runtime) = config.qemu_runtime()? else {
        return Err(CoreError::invalid_state(
            "this ArcBox build pins no QEMU build for this host; set [qemu] path in config.toml or \
             ARCBOX_QEMU_PATH to a qemu-system-aarch64 from arcboxlabs/qemu",
        ));
    };
    runtime.installed_executable().ok_or_else(|| {
        CoreError::invalid_state(format!(
            "QEMU {} is not installed; System startup on QEMU downloads it, or install it \
             with `abctl qemu import <archive>`",
            runtime.artifact().version
        ))
    })
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
        // With no path: what to set where this host's pin selects no build,
        // and how to install the build it does select.
        let error = binary(&config).await.unwrap_err().to_string();
        let advice = if config.qemu_runtime().unwrap().is_some() {
            "abctl qemu import"
        } else {
            "ARCBOX_QEMU_PATH"
        };
        assert!(error.contains(advice), "{error}");

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

    /// Without `[qemu] path` the installed pinned build runs, and a
    /// configured path still wins over it. A development override names the
    /// build, so the test does not depend on what `assets.lock` pins.
    #[test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    fn the_installed_build_runs_unless_a_path_is_configured() {
        use arcbox_constants::env;
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(dir.path());
        config.profile = arcbox_constants::paths::ArcboxProfile::Development;
        let build = dir.path().join("runtime/qemu/0.1.0");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        let installed = build.join("bin/qemu-system-aarch64");
        std::fs::write(&installed, "").unwrap();
        let own = dir.path().join("own-qemu");
        std::fs::write(&own, "").unwrap();
        let sha256 = "ab".repeat(32);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        figment::Jail::expect_with(|jail| {
            jail.set_env(env::QEMU_VERSION, "0.1.0");
            jail.set_env(env::QEMU_SHA256, &sha256);
            jail.set_env(env::QEMU_CDN, "https://example.invalid");
            let error = runtime.block_on(binary(&config)).unwrap_err().to_string();
            assert!(error.contains("abctl qemu import"), "{error}");
            // What `arcbox_image::qemu` writes once a build is installed.
            std::fs::write(build.join(".archive-sha256"), &sha256).unwrap();
            assert_eq!(runtime.block_on(binary(&config)).unwrap(), installed);
            config.qemu.path = Some(own.clone());
            assert_eq!(runtime.block_on(binary(&config)).unwrap(), own);
            Ok(())
        });
    }
}
