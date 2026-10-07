//! Build an image at a caller-owned path and capacity.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{RootfsBuilder, is_oci_layout, rootfs_err};
use crate::error::VmmError;

/// One ext4 block group with the formatter's 4 KiB block size.
/// Capacities must contain whole groups because the formatter rounds up.
pub const ROOTFS_CAPACITY_GRANULARITY: u64 = 128 * 1024 * 1024;

pub enum RootfsSource {
    /// An OCI image layout or a Docker overlay2 chain-id directory.
    Directory(PathBuf),
    /// An image resolved by the caller, which can retain its config and digest.
    Image(oci2rootfs::ImageSource),
}

pub struct RootfsSpec {
    pub source: RootfsSource,
    /// Destination replaced atomically after conversion and agent injection.
    pub out: PathBuf,
    /// Sparse ext4 capacity in bytes, in whole [`ROOTFS_CAPACITY_GRANULARITY`] units.
    pub size: u64,
}

impl RootfsBuilder {
    /// Build an ext4 image with the configured agent at the caller's path.
    /// The caller owns the output's lifetime; no cache entry is created.
    /// Failed builds remove their temporary files. Process termination can
    /// leave a sibling `.<uuid>.ext4.tmp` for the caller to remove.
    pub async fn build_rootfs(&self, spec: RootfsSpec) -> crate::error::Result<()> {
        if spec.size == 0 || !spec.size.is_multiple_of(ROOTFS_CAPACITY_GRANULARITY) {
            return Err(VmmError::Config(format!(
                "rootfs capacity {} must be a positive multiple of {ROOTFS_CAPACITY_GRANULARITY} bytes (one ext4 block group)",
                spec.size
            )));
        }
        self.write_and_publish(spec.source, &spec.out, spec.size)
            .await
            .map_err(rootfs_err)
    }

    pub(super) async fn write_and_publish(
        &self,
        source: RootfsSource,
        out: &Path,
        size: u64,
    ) -> Result<()> {
        let parent = out.parent().context("rootfs output path has no parent")?;
        tokio::fs::create_dir_all(parent)
            .await
            .context("create rootfs output directory")?;
        let temporary = parent.join(format!(".{}.ext4.tmp", uuid::Uuid::new_v4()));
        let built = async {
            let path = temporary.clone();
            tokio::task::spawn_blocking(move || {
                write_image(source, &path, size).and_then(|()| verify_geometry(&path))
            })
            .await
            .context("rootfs conversion task panicked")??;
            self.inject_agent(&temporary).await?;
            tokio::fs::rename(&temporary, out)
                .await
                .context("publish rootfs image")
        }
        .await;
        if let Err(error) = built {
            return match tokio::fs::remove_file(&temporary).await {
                Ok(()) => Err(error),
                // The converter removes partial outputs on its own failures.
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "failed to remove temporary image {}: {cleanup}",
                    temporary.display()
                ))),
            };
        }
        Ok(())
    }
}

fn write_image(source: RootfsSource, out: &Path, size: u64) -> Result<()> {
    let converter = oci2rootfs::Converter::new(out).size(size);
    match source {
        RootfsSource::Image(image) => converter.convert(image).context("convert image to ext4")?,
        RootfsSource::Directory(dir) if is_oci_layout(&dir) => {
            let source =
                oci2rootfs::OciLayoutSource::open(&dir).context("open OCI image layout")?;
            converter
                .convert(source)
                .context("convert OCI layout to ext4")?;
        }
        RootfsSource::Directory(dir) => {
            let source = oci2rootfs::Overlay2Source::open(&dir).context("open overlay2 layer")?;
            converter
                .convert(source)
                .context("convert overlay2 layer to ext4")?;
        }
    }
    Ok(())
}

/// Inode pressure can add block groups even with an aligned capacity.
/// Reject an over-declared image before the kernel reports a mount EINVAL.
fn verify_geometry(image: &Path) -> Result<()> {
    let reader = arcbox_ext4::Reader::new(image).context("read converted ext4 image")?;
    let block = reader.superblock();
    let block_size = 1024u64 << block.log_block_size;
    let declared =
        ((u64::from(block.blocks_count_hi) << 32) | u64::from(block.blocks_count_lo)) * block_size;
    let actual = std::fs::metadata(image)?.len();
    if declared > actual {
        bail!(
            "ext4 image declares {declared} bytes but its file holds {actual}; increase the rootfs capacity"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arcbox_snapshot::snapshot_cow::BusyboxBlockTools;

    use super::*;
    use crate::rootfs::RootfsPaths;

    fn overlay2_layer(root: &Path) -> PathBuf {
        let layer = root.join("ABCDEF");
        std::fs::create_dir_all(layer.join("diff/etc")).unwrap();
        std::fs::write(layer.join("diff/etc/hostname"), b"computer\n").unwrap();
        std::fs::write(layer.join("link"), "ABCDEF").unwrap();
        layer
    }

    fn builder(root: &Path) -> RootfsBuilder {
        RootfsBuilder::new(
            RootfsPaths {
                vm_agent: root.join("absent-vm-agent"),
                cache_dir: root.join("cache"),
                busybox: root.join("absent-busybox"),
            },
            Arc::new(BusyboxBlockTools::default()),
        )
    }

    #[tokio::test]
    async fn invalid_capacity_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("images/rootfs.ext4");
        for size in [0, ROOTFS_CAPACITY_GRANULARITY - 1, 30_000_000_000] {
            let error = builder(dir.path())
                .build_rootfs(RootfsSpec {
                    source: RootfsSource::Directory(overlay2_layer(dir.path())),
                    out: out.clone(),
                    size,
                })
                .await
                .unwrap_err();
            assert!(matches!(error, VmmError::Config(_)), "{error}");
            assert!(!out.parent().unwrap().exists());
        }
    }

    #[test]
    fn conversion_honors_capacity_and_rejects_overdeclared_geometry() {
        let dir = tempfile::tempdir().unwrap();
        for size in [
            ROOTFS_CAPACITY_GRANULARITY / 2,
            2 * ROOTFS_CAPACITY_GRANULARITY,
        ] {
            let out = dir.path().join(format!("{size}.ext4"));
            write_image(
                RootfsSource::Directory(overlay2_layer(dir.path())),
                &out,
                size,
            )
            .unwrap();
            assert_eq!(std::fs::metadata(&out).unwrap().len(), size);
            let geometry = verify_geometry(&out);
            if size < ROOTFS_CAPACITY_GRANULARITY {
                assert!(geometry.unwrap_err().to_string().contains("declares"));
            } else {
                geometry.unwrap();
                let reader = arcbox_ext4::Reader::new(&out).unwrap();
                assert!(reader.tree().lookup(Path::new("/etc/hostname")).is_some());
            }
        }
    }

    #[tokio::test]
    async fn a_failed_source_read_leaves_no_temporary_image() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("layout");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("oci-layout"), b"{}").unwrap();
        let out = dir.path().join("images/rootfs.ext4");
        let error = builder(dir.path())
            .build_rootfs(RootfsSpec {
                source: RootfsSource::Directory(source),
                out: out.clone(),
                size: ROOTFS_CAPACITY_GRANULARITY,
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("open OCI image layout"),
            "{error}"
        );
        assert!(!out.exists());
        assert_eq!(std::fs::read_dir(out.parent().unwrap()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn injection_failure_preserves_the_destination_and_removes_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("images/rootfs.ext4");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, b"existing-image").unwrap();
        let error = builder(dir.path())
            .build_rootfs(RootfsSpec {
                source: RootfsSource::Directory(overlay2_layer(dir.path())),
                out: out.clone(),
                size: ROOTFS_CAPACITY_GRANULARITY,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("vm-agent"), "{error}");
        assert_eq!(std::fs::read(&out).unwrap(), b"existing-image");
        assert_eq!(std::fs::read_dir(out.parent().unwrap()).unwrap().count(), 1);
    }
}
