use super::{BootAssetProvider, BootAssets};
use crate::error::{ImageError, Result};

impl BootAssetProvider {
    /// Verifies cached boot assets without downloading or updating the cache.
    pub async fn verified_cached_assets(&self) -> Result<BootAssets> {
        let config = self.config();
        let manifest = self.read_cached_manifest_required().await?;
        if manifest.schema_version != arcbox_boot::manifest::schema_version_for(&config.version) {
            return Err(ImageError::config(
                "cached boot manifest has an unsupported schema",
            ));
        }
        let target = manifest.targets.get(&config.arch).ok_or_else(|| {
            ImageError::config(format!(
                "cached boot manifest lacks architecture {}",
                config.arch
            ))
        })?;
        let directory = config.version_cache_dir();
        let kernel = config
            .custom_kernel
            .clone()
            .unwrap_or_else(|| directory.join("kernel"));
        let rootfs_image = directory.join("rootfs.erofs");
        for (path, hash) in [
            (
                &kernel,
                config
                    .custom_kernel
                    .is_none()
                    .then_some(&target.kernel.sha256),
            ),
            (&rootfs_image, Some(&target.rootfs.sha256)),
        ] {
            if !tokio::fs::metadata(path).await?.is_file() {
                return Err(ImageError::config(format!(
                    "boot asset is not a regular file: {}",
                    path.display()
                )));
            }
            if let Some(expected) = hash {
                let actual = arcbox_boot::util::sha256_file_async(path)
                    .await
                    .map_err(|error| {
                        ImageError::config(format!("verify {}: {error}", path.display()))
                    })?;
                if actual != *expected {
                    return Err(ImageError::config(format!(
                        "boot asset SHA256 mismatch: {}",
                        path.display()
                    )));
                }
            }
        }
        Ok(BootAssets {
            kernel,
            rootfs_image,
            cmdline: target.kernel_cmdline.clone(),
            version: config.version.clone(),
            manifest,
        })
    }
}
