use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::appledouble::{AppleDouble, Attr};
use super::super::xattrs::{Mode, Store};
use super::Identity;

const STORE: Store = Store::new(64);

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{command:?}: {output:?}");
}

struct Mount(PathBuf);

impl Mount {
    fn image(image: &Path, target: &Path, filesystem: &str) -> Self {
        run(Command::new("mount")
            .args(["-t", filesystem, "-o", "loop"])
            .arg(image)
            .arg(target));
        Self(target.to_owned())
    }

    fn overlay(root: &Path) -> Self {
        let target = root.join("merged");
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            root.join("lower").display(),
            root.join("upper").display(),
            root.join("work").display()
        );
        run(Command::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &options])
            .arg(&target));
        Self(target)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        run(Command::new("umount").arg(&self.0));
    }
}

fn content() -> AppleDouble {
    AppleDouble {
        resource_fork: Some(vec![9u8; 100]),
        ..AppleDouble::default()
    }
}

#[test]
#[ignore = "requires root, mount, mkfs.ext4, and mkfs.btrfs"]
fn mounted_roots_preserve_remounts_and_distinguish_filesystems() {
    for filesystem in ["ext4", "btrfs"] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mounted");
        fs::create_dir(&target).unwrap();
        let images = [dir.path().join("first.img"), dir.path().join("second.img")];
        for image in &images {
            fs::File::create(image)
                .unwrap()
                .set_len(128 * 1024 * 1024)
                .unwrap();
            let force = if filesystem == "ext4" { "-F" } else { "-f" };
            run(Command::new(format!("mkfs.{filesystem}"))
                .args([force, "-q"])
                .arg(image));
        }
        let mount = Mount::image(&images[0], &target, filesystem);
        let first = Identity::of(&target).unwrap();
        STORE.store(&target, &content(), Mode::Replace).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let link_identity = Identity::of(&link).unwrap();
        STORE.store(&link, &content(), Mode::Replace).unwrap();
        drop(mount);

        let mount = Mount::image(&images[0], &target, filesystem);
        assert_eq!(
            Identity::of(&target).unwrap(),
            first,
            "{filesystem} remount"
        );
        assert_eq!(STORE.load(&target).unwrap(), Some(content()));
        drop(mount);

        let _mount = Mount::image(&images[1], &target, filesystem);
        let second = Identity::of(&target).unwrap();
        assert_ne!(
            first, second,
            "{filesystem} roots belong to different filesystems"
        );
        assert_eq!(STORE.load(&target).unwrap(), None);
        assert_eq!(Identity::of(&link).unwrap(), link_identity);
        assert_eq!(STORE.load(&link).unwrap(), Some(content()));
    }
}

#[test]
#[ignore = "requires root and overlayfs mount support"]
fn overlay_copy_up_and_remount_preserve_the_side_entry() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["lower", "upper", "work", "merged"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    fs::write(dir.path().join("lower/file"), b"lower content").unwrap();
    let target = dir.path().join("merged/file");
    let mount = Mount::overlay(dir.path());
    let first = Identity::of(&target).unwrap();
    let content = AppleDouble {
        attrs: vec![Attr {
            name: b"note".to_vec(),
            value: b"copy up".to_vec(),
        }],
        ..content()
    };
    STORE.store(&target, &content, Mode::Replace).unwrap();
    assert_eq!(Identity::of(&target).unwrap(), first);
    assert_eq!(STORE.load(&target).unwrap(), Some(content.clone()));
    drop(mount);

    let _mount = Mount::overlay(dir.path());
    assert_eq!(Identity::of(&target).unwrap(), first);
    assert_eq!(STORE.load(&target).unwrap(), Some(content));
}
