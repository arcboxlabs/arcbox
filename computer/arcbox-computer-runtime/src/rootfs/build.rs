//! Build an image at a caller-owned path and capacity.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::sync::oneshot;

use super::{RootfsBuilder, is_oci_layout, rootfs_err};
use crate::error::ComputerError;

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
    /// Cancellation leaves the destination unchanged. The owned worker finishes
    /// conversion and injection before removing its temporary image.
    /// Runtime shutdown or process termination can leave a sibling
    /// `.<uuid>.ext4.tmp` for the caller to remove.
    pub async fn build_rootfs(&self, spec: RootfsSpec) -> crate::error::Result<()> {
        if spec.size == 0 || !spec.size.is_multiple_of(ROOTFS_CAPACITY_GRANULARITY) {
            return Err(ComputerError::Config(format!(
                "rootfs capacity {} must be a positive multiple of {ROOTFS_CAPACITY_GRANULARITY} bytes (one ext4 block group)",
                spec.size
            )));
        }
        self.write_and_publish_with(&spec.out, move |path| {
            write_image(spec.source, path, spec.size)
                .and_then(|()| verify_geometry(path, spec.size))
        })
        .await
        .map_err(rootfs_err)
    }

    pub(super) async fn write_and_publish(
        &self,
        source: RootfsSource,
        out: &Path,
        initial_size: u64,
    ) -> Result<()> {
        self.write_and_publish_with(out, move |path| write_image(source, path, initial_size))
            .await
    }

    async fn write_and_publish_with(
        &self,
        out: &Path,
        write: impl FnOnce(&Path) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let parent = out
            .parent()
            .context("rootfs output path has no parent")?
            .to_owned();
        let builder = Self::new(self.paths.clone(), self.block_tools.clone());
        let (reply, prepared) = oneshot::channel();
        // This task owns every blocking step until injection releases its mount.
        // Dropping the receiver cancels publication, not resource cleanup.
        tokio::spawn(async move {
            let built = async {
                tokio::fs::create_dir_all(&parent)
                    .await
                    .context("create rootfs output directory")?;
                let temporary = TemporaryImage {
                    path: parent.join(format!(".{}.ext4.tmp", uuid::Uuid::new_v4())),
                    armed: true,
                };
                let path = temporary.path.clone();
                let built = async {
                    tokio::task::spawn_blocking(move || write(&path))
                        .await
                        .context("rootfs conversion task panicked")??;
                    builder.inject_agent(&temporary.path).await
                }
                .await;
                match built {
                    Ok(()) => Ok(temporary),
                    Err(error) => Err(temporary.fail(error)),
                }
            }
            .await;
            if let Err(Err(error)) = reply.send(built) {
                tracing::error!(?error, "cancelled rootfs build failed");
            }
        });
        let temporary = prepared.await.context("rootfs build task stopped")??;
        // No await may separate receipt from publication: a cancelled caller
        // must never leave a detached rename that can replace the destination.
        temporary.publish(out)
    }
}

#[derive(Debug)]
struct TemporaryImage {
    path: PathBuf,
    armed: bool,
}

impl TemporaryImage {
    fn publish(mut self, out: &Path) -> Result<()> {
        match std::fs::rename(&self.path, out).context("publish rootfs image") {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(error) => Err(self.fail(error)),
        }
    }

    fn fail(mut self, error: anyhow::Error) -> anyhow::Error {
        match self.cleanup() {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("{cleanup:#}")),
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if !std::mem::take(&mut self.armed) {
            return Ok(());
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            // The converter can remove its partial output before returning an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                let error = anyhow::Error::from(error).context(format!(
                    "failed to remove temporary image {}",
                    self.path.display()
                ));
                tracing::error!(?error, "rootfs build cleanup failed");
                Err(error)
            }
        }
    }
}

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        // The receiver may disappear before taking an already prepared image.
        // Cleanup records failures before returning, including during Drop.
        let _ = self.cleanup();
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

/// File data and inode pressure can make the formatter add block groups.
/// Require both the declared geometry and file length to match the requested capacity.
fn verify_geometry(image: &Path, requested_size: u64) -> Result<()> {
    let reader = arcbox_ext4::Reader::new(image).context("read converted ext4 image")?;
    let block = reader.superblock();
    let block_size = 1024u64 << block.log_block_size;
    let declared =
        ((u64::from(block.blocks_count_hi) << 32) | u64::from(block.blocks_count_lo)) * block_size;
    let actual = std::fs::metadata(image)?.len();
    if declared != requested_size || actual != requested_size {
        bail!(
            "ext4 image declares {declared} bytes and its file holds {actual}; requested rootfs capacity is {requested_size} bytes"
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
    async fn cancellation_waits_for_the_writer_before_removing_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("images/rootfs.ext4");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, b"existing-image").unwrap();
        let builder = builder(dir.path());
        let destination = out.clone();
        let (started, writing) = tokio::sync::oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let (finished, written) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(async move {
            builder
                .write_and_publish_with(&destination, move |path| {
                    std::fs::write(path, b"partial-image")?;
                    started.send(path.to_path_buf()).unwrap();
                    resume.recv().unwrap();
                    assert!(path.exists(), "cleanup must wait for the writer");
                    std::fs::write(path, b"complete-image")?;
                    finished.send(()).unwrap();
                    Ok(())
                })
                .await
        });
        let temporary = writing.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(temporary.exists());
        release.send(()).unwrap();
        written.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while temporary.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the cancelled build must remove the completed temporary image");
        assert_eq!(std::fs::read(&out).unwrap(), b"existing-image");
        assert_eq!(std::fs::read_dir(out.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn dropping_a_delivered_image_preserves_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rootfs.ext4");
        let path = dir.path().join(".prepared.ext4.tmp");
        std::fs::write(&out, b"existing-image").unwrap();
        std::fs::write(&path, b"prepared-image").unwrap();
        let (reply, prepared) = oneshot::channel();
        reply
            .send(TemporaryImage {
                path: path.clone(),
                armed: true,
            })
            .unwrap();

        drop(prepared);

        assert!(!path.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"existing-image");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn publication_reports_both_rename_and_cleanup_failures() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rootfs.ext4");
        let path = dir.path().join(".invalid.ext4.tmp");
        std::fs::write(&out, b"existing-image").unwrap();
        std::fs::create_dir(&path).unwrap();

        let error = TemporaryImage { path, armed: true }
            .publish(&out)
            .unwrap_err();
        let details = format!("{error:#}");
        assert!(details.contains("publish rootfs image"), "{details}");
        assert!(
            details.contains("failed to remove temporary image"),
            "{details}"
        );
        assert_eq!(std::fs::read(out).unwrap(), b"existing-image");
    }

    #[test]
    fn cleanup_failures_are_logged_once_even_when_the_result_is_dropped() {
        if !super::super::tests::isolated_log_test(
            "rootfs::build::tests::cleanup_failures_are_logged_once_even_when_the_result_is_dropped",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = tempfile::NamedTempFile::new().unwrap();
        let writer = log.reopen().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.try_clone().unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for failed_before_delivery in [true, false] {
                let path = dir
                    .path()
                    .join(format!("{failed_before_delivery}.ext4.tmp"));
                std::fs::create_dir(&path).unwrap();
                let image = TemporaryImage { path, armed: true };
                let result = if failed_before_delivery {
                    Err(image.fail(anyhow::anyhow!("conversion failed")))
                } else {
                    Ok(image)
                };
                let (reply, prepared) = oneshot::channel();
                reply.send(result).unwrap();
                drop(prepared);
            }
        });
        let logged = std::fs::read_to_string(log.path()).unwrap();
        assert!(logged.contains("true.ext4.tmp"), "{logged}");
        assert!(logged.contains("false.ext4.tmp"), "{logged}");
        assert_eq!(
            logged.matches("rootfs build cleanup failed").count(),
            2,
            "{logged}"
        );
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
            assert!(matches!(error, ComputerError::Config(_)), "{error}");
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
            let geometry = verify_geometry(&out, size);
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
    async fn oversized_source_is_rejected_before_injection_and_preserves_destination() {
        let dir = tempfile::tempdir().unwrap();
        let layer = overlay2_layer(dir.path());
        std::fs::File::create(layer.join("diff/payload"))
            .unwrap()
            .set_len(ROOTFS_CAPACITY_GRANULARITY + 4096)
            .unwrap();
        let out = dir.path().join("images/rootfs.ext4");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, b"existing-image").unwrap();

        let error = builder(dir.path())
            .build_rootfs(RootfsSpec {
                source: RootfsSource::Directory(layer),
                out: out.clone(),
                size: ROOTFS_CAPACITY_GRANULARITY,
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("requested rootfs capacity"),
            "capacity must be rejected before agent injection: {error}"
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"existing-image");
        assert_eq!(std::fs::read_dir(out.parent().unwrap()).unwrap().count(), 1);
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
