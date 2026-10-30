//! The rootfs a distro machine boots: a published image from the local
//! registry under the boot shim the daemon already caches for the System VM.

use arcbox_connect::v1::CreateMachineRequest;
use arcbox_core::Runtime;
use arcbox_core::machine::{BootShim, MachineRootfs};
use arcbox_core::machine_image::{self, ImageSelector, MachineImage};
use connectrpc::{ConnectError, RequestContext};
use tokio_util::sync::CancellationToken;

/// Pulls the requested distro image; an empty architecture selects the host.
pub(super) async fn pull_image(
    runtime: &Runtime,
    context: &RequestContext,
    request: &CreateMachineRequest,
) -> Result<MachineImage, ConnectError> {
    let arch = if request.arch.is_empty() {
        machine_image::host_image_arch().to_string()
    } else {
        machine_image::image_arch(&request.arch).to_string()
    };
    let selector = ImageSelector::Distro {
        distro: request.distro.clone(),
        release: (!request.version.is_empty()).then(|| request.version.clone()),
        arch,
    };
    // Direct library calls have no daemon owner to cancel their token.
    let cancelled = context
        .extensions()
        .get::<CancellationToken>()
        .cloned()
        .unwrap_or_default();
    let machine = request.name.as_str();
    let image = runtime
        .machine_image_manager()
        .pull(&selector, &cancelled, |done, total| {
            tracing::debug!(machine, done, total, "machine image pull");
        })
        .await
        .map_err(|e| match &e {
            arcbox_image::ImageError::Cancelled => {
                ConnectError::unavailable("daemon is shutting down")
            }
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
