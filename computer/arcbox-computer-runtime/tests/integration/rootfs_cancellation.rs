//! Cancel an injection after the kernel attaches a loop device but before the
//! builder receives it. A build attaches no loop device: it writes the image
//! while converting it, and its cancellation is covered by the unit tests in
//! `src/rootfs/build.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use arcbox_computer_runtime::ROOTFS_CAPACITY_GRANULARITY;
use arcbox_computer_runtime::snapshot_cow::BlockTools;
use arcbox_ext4::{FormatOptions, Formatter};
use arcbox_snapshot::error::{Result, SnapshotError};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::{LOOP_DEVICE_TEST, block_tools, builder, verify_agent_files, with_mounted};

struct PausedAttach {
    inner: Arc<dyn BlockTools>,
    attached: UnboundedSender<(String, PathBuf)>,
    resume: Mutex<mpsc::Receiver<()>>,
    detached: UnboundedSender<String>,
}

impl BlockTools for PausedAttach {
    fn attach_loop(&self, backing: &Path, read_only: bool) -> Result<String> {
        let device = self.inner.attach_loop(backing, read_only)?;
        if self
            .attached
            .send((device.clone(), backing.to_path_buf()))
            .is_err()
            || self.resume.lock().unwrap().recv().is_err()
        {
            self.inner.detach_loop(&device)?;
            return Err(SnapshotError::Snapshot("test attach barrier closed".into()));
        }
        Ok(device)
    }

    fn detach_loop(&self, device: &str) -> Result<()> {
        self.inner.detach_loop(device)?;
        self.detached
            .send(device.to_owned())
            .map_err(|_| SnapshotError::Snapshot("test detach receiver closed".into()))?;
        Ok(())
    }

    fn device_sectors(&self, device: &str) -> Result<u64> {
        self.inner.device_sectors(device)
    }
}

struct AttachedLoop {
    tools: Arc<dyn BlockTools>,
    device: String,
    backing_file: PathBuf,
    expected_backing: PathBuf,
    armed: bool,
}

impl Drop for AttachedLoop {
    fn drop(&mut self) {
        // A failing regression must not strand the real loop device.
        if self.armed
            && std::fs::read_to_string(&self.backing_file).is_ok_and(|path| {
                path.trim_end().trim_end_matches(" (deleted)")
                    == self.expected_backing.to_str().unwrap()
            })
            && let Err(error) = self.tools.detach_loop(&self.device)
        {
            eprintln!("test loop cleanup failed: {error}");
        }
    }
}

fn injection_directories() -> BTreeSet<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("arcbox-inject-")
        })
        .collect()
}

#[tokio::test]
async fn cancellation_before_attach_returns_releases_injection_resources() {
    let Some(tools) = block_tools() else { return };
    let _serial = LOOP_DEVICE_TEST.lock().await;
    let directories_before = injection_directories();
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("vm-agent");
    std::fs::write(&agent, b"staged-agent").unwrap();
    let image = dir.path().join("images/rootfs.ext4");
    std::fs::create_dir(image.parent().unwrap()).unwrap();
    Formatter::with_options(&image, FormatOptions::new(ROOTFS_CAPACITY_GRANULARITY))
        .unwrap()
        .close()
        .unwrap();
    let (attached, mut attaching) = unbounded_channel();
    let (release, resume) = mpsc::channel();
    let (detached, mut detaching) = unbounded_channel();
    let builder = builder(
        &agent,
        Arc::new(PausedAttach {
            inner: Arc::clone(&tools),
            attached,
            resume: Mutex::new(resume),
            detached,
        }),
    );
    let destination = image.clone();
    let caller = tokio::spawn(async move { builder.inject_vm_agent(&destination).await });
    let (device, backing) = tokio::time::timeout(Duration::from_secs(5), attaching.recv())
        .await
        .expect("the real loop attach must reach the barrier")
        .unwrap();
    let mut attached = AttachedLoop {
        tools: Arc::clone(&tools),
        backing_file: Path::new("/sys/block")
            .join(Path::new(&device).file_name().unwrap())
            .join("loop/backing_file"),
        device,
        expected_backing: backing.clone(),
        armed: true,
    };
    assert!(attached.backing_file.exists());
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();

    let detached = tokio::time::timeout(Duration::from_secs(5), detaching.recv())
        .await
        .expect("cancellation must still detach the loop device")
        .expect("the builder must detach before dropping its tools");
    assert_eq!(detached, attached.device);
    tokio::time::timeout(Duration::from_secs(5), async {
        while attached.backing_file.exists()
            || !injection_directories().is_subset(&directories_before)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation must release the device and mount directory");
    attached.armed = false;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        !mounts
            .split_whitespace()
            .any(|field| field == attached.device)
    );
    assert_eq!(
        std::fs::read_dir(image.parent().unwrap()).unwrap().count(),
        1
    );
    with_mounted(&*tools, &image, |root| {
        verify_agent_files(root, b"staged-agent")
    })
    .unwrap();
}
