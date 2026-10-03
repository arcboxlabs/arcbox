//! The rootfs a distro machine boots: a published image from the local
//! registry under the boot shim the daemon already caches for the System VM.

use arcbox_core::Runtime;
use arcbox_core::machine::{BootShim, MachineRootfs};
use arcbox_core::machine_image::{self, ImageSelector, MachineImage};
use connectrpc::ConnectError;

/// Resolves and pulls the image for `distro`/`version` on `arch` (the
/// host's when empty); a cached image is a no-op.
pub(super) async fn pull_image(
    runtime: &Runtime,
    machine: &str,
    distro: &str,
    version: &str,
    arch: &str,
) -> Result<MachineImage, ConnectError> {
    let arch = if arch.is_empty() {
        machine_image::host_image_arch().to_string()
    } else {
        machine_image::image_arch(arch).to_string()
    };
    let selector = ImageSelector::Distro {
        distro: distro.to_owned(),
        release: (!version.is_empty()).then(|| version.to_owned()),
        arch,
    };
    let image = runtime
        .machine_image_manager()
        .pull(&selector, |done, total| {
            tracing::debug!(machine, done, total, "machine image pull");
        })
        .await
        .map_err(|e| match &e {
            arcbox_image::ImageError::Common(c) if c.is_not_found() => {
                ConnectError::not_found(e.to_string())
            }
            _ => ConnectError::internal(e.to_string()),
        })?;
    tracing::info!(
        machine,
        image = %format!("{}@{}", image.manifest.name, image.manifest.version),
        "machine image ready"
    );
    Ok(image)
}

/// `image` behind the boot shim (kernel + EROFS with
/// `/sbin/arcbox-machine-init`) from the same boot-assets cache the daemon
/// populates for the System VM; a warm cache is a no-op.
pub(super) async fn rootfs_for(
    runtime: &Runtime,
    image: &MachineImage,
) -> Result<MachineRootfs, ConnectError> {
    let shim = async {
        let provider = arcbox_core::boot_assets::BootAssetProvider::new(
            runtime.config().data_dir.join("boot"),
        )?;
        let assets = provider.get_assets().await?;
        Ok::<_, arcbox_core::error::CoreError>(BootShim {
            kernel: assets.kernel,
            rootfs: assets.rootfs_image,
        })
    }
    .await
    .map_err(|e| ConnectError::internal(format!("resolve boot shim: {e}")))?;
    Ok(MachineRootfs {
        path: image.rootfs_path(),
        format: image.manifest.rootfs.format.clone(),
        shim: Some(shim),
    })
}
