//! Staging for a driver whose VMs read host paths as they are.
//!
//! The port's rule for such a driver is that staging is the identity on
//! what a VM only reads, and that is what this does for the kernel, a
//! device and a checkpoint image. A disk the VM *writes* is different:
//! `DiskSource::Image` promises the caller's original never sees a guest
//! write, and `DiskSource::Handover` that the file has left its source.
//! Both keep their meaning here by landing in an area of the VM's own,
//! `{runtime_dir}/staged`, which is also what lets `unstage_disk` find a
//! disk again — the pause release takes a copy-mode rootfs back out of it.
//!
//! The area lives in the runtime directory, so a removal takes it with the
//! rest of the computer's files; a discard leaves it, as the port allows.

use std::path::{Component, Path, PathBuf};

use arcbox_vm_driver::{CheckpointImage, DiskSource, Error, Result, Staging};
use async_trait::async_trait;

/// One VM's host-path staging, rooted at `{runtime_dir}/staged`.
#[derive(Debug, Clone)]
pub struct HostPathStaging {
    root: PathBuf,
}

impl HostPathStaging {
    /// The staging of the VM whose scratch space is `runtime_dir`.
    pub fn new(runtime_dir: &Path) -> Self {
        Self {
            root: runtime_dir.join("staged"),
        }
    }

    /// Where the disk `id` lands. Refuses an id that is not a plain name:
    /// staging replaces and unstaging moves the file at this path.
    fn disk(&self, id: &str) -> Result<PathBuf> {
        let mut components = Path::new(id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(Error::InvalidSpec(format!(
                "disk id `{id}` must be a plain name"
            )));
        }
        Ok(self.root.join(format!("{id}.ext4")))
    }
}

/// Move `src` to `dst`, copying when they are on different filesystems.
async fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match tokio::fs::rename(src, dst).await {
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            tokio::fs::copy(src, dst).await?;
            tokio::fs::remove_file(src).await
        }
        other => other,
    }
}

#[async_trait]
impl Staging for HostPathStaging {
    async fn stage_kernel(&self, src: &Path) -> Result<PathBuf> {
        Ok(src.to_path_buf())
    }

    async fn stage_disk(&self, id: &str, source: DiskSource<'_>) -> Result<PathBuf> {
        let into = self.disk(id)?;
        let src = match source {
            DiskSource::Device(path) => return Ok(path.to_path_buf()),
            DiskSource::Image(path) | DiskSource::Handover(path) => path,
        };
        if src == into {
            return Ok(into);
        }
        tokio::fs::create_dir_all(&self.root).await?;
        if matches!(source, DiskSource::Handover(_)) {
            move_file(src, &into).await?;
        } else {
            // On APFS `copy` clones, so a private copy costs no blocks
            // until the guest writes.
            tokio::fs::copy(src, &into).await?;
        }
        Ok(into)
    }

    async fn unstage_disk(&self, id: &str, dst: &Path) -> Result<bool> {
        let staged = self.disk(id)?;
        if !tokio::fs::try_exists(&staged).await? {
            return Ok(false);
        }
        move_file(&staged, dst).await?;
        Ok(true)
    }

    async fn stage_checkpoint(&self, image: &CheckpointImage) -> Result<CheckpointImage> {
        Ok(image.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk the guest writes is never the caller's file: an image is
    /// copied, a handover moved, and both come back out by id. A device and
    /// the read-only files are named where they are.
    #[tokio::test]
    async fn written_disks_land_in_the_area_and_read_only_files_stay_put() {
        let dir = tempfile::tempdir().unwrap();
        let staging = HostPathStaging::new(dir.path());
        let template = dir.path().join("template.ext4");
        std::fs::write(&template, b"template").unwrap();

        let image = staging
            .stage_disk("rootfs", DiskSource::Image(&template))
            .await
            .unwrap();
        assert_ne!(image, template);
        std::fs::write(&image, b"guest wrote").unwrap();
        assert_eq!(std::fs::read(&template).unwrap(), b"template");

        let parked = dir.path().join("parked.ext4");
        assert!(staging.unstage_disk("rootfs", &parked).await.unwrap());
        assert!(!image.exists());
        assert!(!staging.unstage_disk("rootfs", &parked).await.unwrap());

        let back = staging
            .stage_disk("rootfs", DiskSource::Handover(&parked))
            .await
            .unwrap();
        assert!(!parked.exists(), "a handover leaves its source");
        assert_eq!(std::fs::read(&back).unwrap(), b"guest wrote");

        let device = Path::new("/dev/disk9");
        assert_eq!(
            staging
                .stage_disk("rootfs", DiskSource::Device(device))
                .await
                .unwrap(),
            device
        );
        assert_eq!(staging.stage_kernel(&template).await.unwrap(), template);
        assert!(
            staging
                .stage_disk("../escape", DiskSource::Image(&template))
                .await
                .is_err()
        );
    }
}
