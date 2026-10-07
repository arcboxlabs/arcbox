//! Read injected images through the kernel's ext4 driver.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use arcbox_computer_runtime::snapshot_cow::{BlockTools, BusyboxBlockTools};
use arcbox_computer_runtime::{
    ROOTFS_CAPACITY_GRANULARITY, RootfsBuilder, RootfsPaths, RootfsSource, RootfsSpec,
    VM_AGENT_PATH,
};
use arcbox_ext4::constants::file_mode;
use arcbox_ext4::{FormatOptions, Formatter};

fn block_tools() -> Option<Arc<dyn BlockTools>> {
    let busybox = std::env::var("BUSYBOX").unwrap_or_else(|_| "/bin/busybox".into());
    if super::common::is_root() && Path::new(&busybox).exists() {
        Some(Arc::new(BusyboxBlockTools::new(busybox)))
    } else {
        assert!(
            std::env::var_os("ARCBOX_REQUIRE_BLOCK_TOOLS").is_none(),
            "rootfs integration requires root and busybox"
        );
        eprintln!("SKIP rootfs integration: requires root and busybox");
        None
    }
}

fn builder(agent: &Path, tools: Arc<dyn BlockTools>) -> RootfsBuilder {
    RootfsBuilder::new(
        RootfsPaths {
            vm_agent: agent.to_path_buf(),
            cache_dir: agent.parent().unwrap().join("cache"),
            busybox: "/nonexistent/busybox".into(),
        },
        tools,
    )
}

fn with_mounted(
    tools: &dyn BlockTools,
    image: &Path,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    use nix::mount::{MsFlags, mount, umount};

    let mount_dir = tempfile::tempdir()?;
    let device = tools.attach_loop(image, true)?;
    let mounted = mount(
        Some(device.as_str()),
        mount_dir.path(),
        Some("ext4"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    );
    let result = match mounted {
        Ok(()) => {
            let checked = check(mount_dir.path());
            umount(mount_dir.path())
                .context("unmount test image")
                .and(checked)
        }
        Err(error) => Err(error).context("mount test image"),
    };
    tools
        .detach_loop(&device)
        .context("detach test image")
        .and(result)
}

fn verify_boot_files(root: &Path, expected_agent: &[u8]) -> Result<()> {
    let agent = root.join(VM_AGENT_PATH.trim_start_matches('/'));
    ensure!(
        std::fs::read(&agent)? == expected_agent,
        "agent bytes differ"
    );
    ensure!(
        std::fs::metadata(&agent)?.permissions().mode() & 0o777 == 0o755,
        "agent must be executable"
    );
    ensure!(
        std::fs::read_link(root.join("etc/resolv.conf"))? == Path::new("../run/resolv.conf"),
        "resolver must point into the run tmpfs"
    );
    ensure!(
        std::fs::read(root.join("sbin/init"))? == b"distribution-init",
        "injection must preserve the distribution init"
    );
    Ok(())
}

#[tokio::test]
async fn injection_preserves_distribution_files_and_can_replace_the_agent() {
    let Some(tools) = block_tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("existing.ext4");
    let mut formatter =
        Formatter::with_options(&image, FormatOptions::new(128 * 1024 * 1024)).unwrap();
    for path in ["/sbin", "/etc", "/run"] {
        formatter
            .create(
                path,
                file_mode::S_IFDIR | 0o755,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
    }
    formatter
        .create(
            "/sbin/init",
            file_mode::S_IFREG | 0o755,
            None,
            None,
            Some(&mut b"distribution-init".as_slice()),
            None,
            None,
            None,
        )
        .unwrap();
    formatter.close().unwrap();

    let agent = dir.path().join("vm-agent");
    for contents in [b"first-agent".as_slice(), b"replacement-agent".as_slice()] {
        std::fs::write(&agent, contents).unwrap();
        builder(&agent, Arc::clone(&tools))
            .inject_vm_agent(&image)
            .await
            .unwrap();
        with_mounted(&*tools, &image, |root| verify_boot_files(root, contents)).unwrap();
    }
}

#[tokio::test]
async fn a_caller_sized_build_is_mountable_and_replaces_the_destination() {
    let Some(tools) = block_tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let layer = dir.path().join("ABCDEF");
    std::fs::create_dir_all(layer.join("diff/sbin")).unwrap();
    std::fs::write(layer.join("diff/sbin/init"), b"distribution-init").unwrap();
    std::fs::write(layer.join("link"), "ABCDEF").unwrap();
    let image = dir.path().join("images/digest/rootfs.ext4");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"previous-image").unwrap();
    let agent = dir.path().join("vm-agent");
    std::fs::write(&agent, b"staged-agent").unwrap();
    let size = 2 * ROOTFS_CAPACITY_GRANULARITY;

    builder(&agent, Arc::clone(&tools))
        .build_rootfs(RootfsSpec {
            source: RootfsSource::Directory(layer.clone()),
            out: image.clone(),
            size,
        })
        .await
        .unwrap();

    assert_eq!(std::fs::metadata(&image).unwrap().len(), size);
    assert_eq!(
        std::fs::read_dir(image.parent().unwrap()).unwrap().count(),
        1
    );
    assert!(!dir.path().join("cache").exists());
    with_mounted(&*tools, &image, |root| {
        verify_boot_files(root, b"staged-agent")
    })
    .unwrap();

    let builder = builder(&agent, Arc::clone(&tools));
    let cached = builder
        .convert_layer_to_rootfs(layer.to_str().unwrap(), &Default::default())
        .await
        .unwrap();
    assert_eq!(std::fs::metadata(&cached).unwrap().len(), 512 * 1024 * 1024);
    let stem = Path::new(&cached).file_stem().unwrap().to_str().unwrap();
    let keys = stem.strip_prefix("rootfs-").unwrap();
    let (layer_key, agent_key) = keys.split_once('-').unwrap();
    assert_eq!((layer_key.len(), agent_key.len()), (16, 16));
    assert_eq!(
        builder
            .convert_layer_to_rootfs(layer.to_str().unwrap(), &Default::default())
            .await
            .unwrap(),
        cached
    );
    with_mounted(&*tools, Path::new(&cached), |root| {
        verify_boot_files(root, b"staged-agent")
    })
    .unwrap();

    let blocked = image.parent().unwrap().join("directory");
    std::fs::create_dir(&blocked).unwrap();
    let error = builder
        .build_rootfs(RootfsSpec {
            source: RootfsSource::Directory(layer.clone()),
            out: blocked.clone(),
            size,
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("publish rootfs image"),
        "{error}"
    );
    assert!(blocked.is_dir());
    assert_eq!(
        std::fs::read_dir(image.parent().unwrap()).unwrap().count(),
        2
    );

    std::fs::create_dir_all(layer.join("diff/etc/resolv.conf")).unwrap();
    let error = builder
        .build_rootfs(RootfsSpec {
            source: RootfsSource::Directory(layer),
            out: image.clone(),
            size,
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("replace /etc/resolv.conf"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_dir(image.parent().unwrap()).unwrap().count(),
        2
    );
    with_mounted(&*tools, &image, |root| {
        verify_boot_files(root, b"staged-agent")
    })
    .unwrap();
}
