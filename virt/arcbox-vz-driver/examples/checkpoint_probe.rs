//! VZ checkpoint through `VzDriver`: save a guest that keeps running, then
//! restore that one image into several VMs at once.
//!
//! ```text
//! checkpoint_probe clones <dir> <n>   # boot, checkpoint (Resume), restore n clones
//! checkpoint_probe locked <dir>       # checkpoint while locked: must refuse, guest keeps running
//! checkpoint_probe restore <dir> <n>  # restore n clones of <dir>/image (e.g. while locked)
//! ```
//!
//! The guest is the VZ checkpoint spike's: the HV spike 3 initramfs
//! (`arcbox-vmm/examples/checkpoint_linux_spike/guest/build.sh`), whose
//! four threads verify every O_DIRECT read of `/dev/vda` against the seq
//! numbers they keep in RAM and print a tick line a second on hvc0. The
//! source keeps writing its disk for two seconds after the checkpoint, so
//! a restore that paired the saved RAM with the live disk instead of the
//! image's clone reports `FAIL verify`. Each clone boots on its own
//! `clonefile` of the image's disk clone and passes when its first tick
//! follows the saved one and no thread fails.
//!
//! Environment: `VZ_KERNEL` (default `~/.arcbox/boot/<newest>/kernel`),
//! `VZ_INITRD` (default `<dir>/../initramfs.cpio.gz`). Sign the binary:
//! `codesign --force --entitlements bundle/arcbox.dev.entitlements -s - <bin>`.
//! Results: `docs/experiments/2026-10-31-vz-checkpoint-spike.md`, follow-up.

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    if let Err(e) = probe::main().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod probe {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arcbox_vm_driver::{
        AfterCheckpoint, BootSpec, CacheMode, CheckpointFormat, CheckpointImage, CheckpointKind,
        CheckpointOptions, ConsoleSpec, DiskSpec, IsolationSpec, MacAddr, NicAttachment, NicSpec,
        RestoreSpec, ShutdownMode, VmDriver, VmHandle, VmId, VmSpec,
    };
    use arcbox_vz_driver::{CHECKPOINT_FORMAT, VzDriver, disk_clone};

    type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

    const DISK_BYTES: u64 = 256 << 20;

    pub async fn main() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let driver = VzDriver::new(None, None);
        match args.as_slice() {
            [_, m, d, n] if m == "clones" => {
                let dir = PathBuf::from(d);
                save(&driver, &dir, AfterCheckpoint::Resume).await?;
                restore_clones(driver, &dir, n.parse()?).await
            }
            [_, m, d] if m == "locked" => locked(&driver, Path::new(d)).await,
            [_, m, d, n] if m == "restore" => {
                restore_clones(driver, Path::new(d), n.parse()?).await
            }
            _ => Err("usage: checkpoint_probe clones|restore <dir> <n> | locked <dir>".into()),
        }
    }

    fn spec(dir: &Path, id: &str, disk: PathBuf, mac: u8) -> Result<VmSpec> {
        let kernel = match std::env::var_os("VZ_KERNEL") {
            Some(k) => PathBuf::from(k),
            None => newest_kernel()?,
        };
        let initrd = std::env::var_os("VZ_INITRD")
            .map_or_else(|| dir.join("..").join("initramfs.cpio.gz"), PathBuf::from);
        Ok(VmSpec {
            id: VmId::new(id)?,
            cpus: 4,
            memory_mib: 1024,
            boot: BootSpec::Kernel {
                image: kernel,
                cmdline: "console=hvc0 rdinit=/init loglevel=4 panic=-1".into(),
                initrd: Some(initrd),
            },
            disks: vec![DiskSpec {
                id: "data".into(),
                path: disk,
                read_only: false,
                root: false,
                cache: CacheMode::Unsafe,
            }],
            nics: vec![nic(mac)],
            vsock: None,
            shares: vec![],
            console: ConsoleSpec::Pipe,
            balloon: false,
            entropy: true,
            dirty_tracking: false,
            isolation: IsolationSpec::None,
            rosetta: false,
            nested_virt: false,
            debug_console: None,
        })
    }

    fn nic(last: u8) -> NicSpec {
        NicSpec {
            id: "eth0".into(),
            mac: MacAddr::new([0x02, 0xab, 0, 0, 0, last]),
            attachment: NicAttachment::UserNat,
        }
    }

    /// Boots the source and waits for its first tick.
    async fn boot(driver: &VzDriver, dir: &Path) -> Result<(Box<dyn VmHandle>, Lines)> {
        std::fs::create_dir_all(dir)?;
        let disk = dir.join("source.img");
        std::fs::File::create(&disk)?.set_len(DISK_BYTES)?;
        let runtime = dir.join("run-source");
        std::fs::create_dir_all(&runtime)?;
        let vm = driver.boot(spec(dir, "source", disk, 1)?, &runtime).await?;
        let mut lines = Lines::default();
        lines.wait_tick(&*vm, Duration::from_secs(30)).await?;
        Ok((vm, lines))
    }

    async fn save(driver: &VzDriver, dir: &Path, after: AfterCheckpoint) -> Result<()> {
        let (vm, mut lines) = boot(driver, dir).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        lines.drain(&*vm).await?;
        let cp = vm.checkpoint().ok_or("no checkpoint capability")?;
        let t = Instant::now();
        cp.checkpoint(
            &dir.join("image"),
            CheckpointOptions {
                after,
                kind: CheckpointKind::Full,
            },
        )
        .await?;
        let took = t.elapsed();
        let saved = lines.last_tick().ok_or("no tick before the checkpoint")?;
        std::fs::write(dir.join("saved-tick"), &saved)?;
        let state = std::fs::metadata(dir.join("image/state.vzvmsave"))?.len();
        println!(
            "[save] checkpoint {:.0} ms (pause, save, disk clone, manifest), state file {} MiB, after {after:?}",
            ms(took),
            state >> 20
        );
        println!("[save] last tick before: {saved}");
        // The source keeps writing its own disk; the image's clone must not
        // see any of it.
        tokio::time::sleep(Duration::from_secs(2)).await;
        lines.drain(&*vm).await?;
        println!(
            "[save] source after: {}",
            lines.last_tick().unwrap_or_default()
        );
        vm.shutdown(ShutdownMode::Kill).await?;
        lines.no_fails("source")
    }

    async fn locked(driver: &VzDriver, dir: &Path) -> Result<()> {
        let session = driver.checkpoint_blocker();
        println!("[locked] checkpoint_blocker: {session:?}");
        let (vm, mut lines) = boot(driver, dir).await?;
        let cp = vm.checkpoint().ok_or("no checkpoint capability")?;
        let t = Instant::now();
        let result = cp
            .checkpoint(&dir.join("image"), CheckpointOptions::default())
            .await;
        println!(
            "[locked] checkpoint after {:.1} ms: {result:?}; state {}",
            ms(t.elapsed()),
            vm.state()
        );
        let before = lines.last_tick();
        tokio::time::sleep(Duration::from_secs(3)).await;
        lines.drain(&*vm).await?;
        println!("[locked] ticks {before:?} -> {:?}", lines.last_tick());
        vm.shutdown(ShutdownMode::Kill).await?;
        match result {
            Err(arcbox_vm_driver::Error::Unavailable(_)) if lines.last_tick() != before => Ok(()),
            other => Err(format!("expected Unavailable and a running guest, got {other:?}").into()),
        }
    }

    async fn restore_clones(driver: VzDriver, dir: &Path, n: usize) -> Result<()> {
        let saved = std::fs::read_to_string(dir.join("saved-tick"))?;
        let saved_n = tick_number(&saved).ok_or("bad saved tick")?;
        let image = CheckpointImage {
            dir: dir.join("image"),
            format: CheckpointFormat::new(CHECKPOINT_FORMAT),
            kind: CheckpointKind::Full,
        };
        let driver = Arc::new(driver);
        let mut clones = tokio::task::JoinSet::new();
        for i in 0..n {
            let (driver, image, dir) = (Arc::clone(&driver), image.clone(), dir.to_path_buf());
            clones.spawn(async move { restore_clone(&driver, &image, &dir, i, saved_n).await });
        }
        let mut failed = Vec::new();
        while let Some(result) = clones.join_next().await {
            if let Err(e) = result? {
                let mut chain = e.to_string();
                let mut source = e.source();
                while let Some(s) = source {
                    chain = format!("{chain}: {s}");
                    source = s.source();
                }
                failed.push(chain);
            }
        }
        if failed.is_empty() {
            println!("[restore] {n} clones OK");
            Ok(())
        } else {
            Err(failed.join("; ").into())
        }
    }

    /// Restores clone `i` onto its own copy of the image's disk clone.
    async fn restore_clone(
        driver: &VzDriver,
        image: &CheckpointImage,
        dir: &Path,
        i: usize,
        saved_n: u64,
    ) -> Result<()> {
        let run = dir.join(format!("run-clone-{i}-{}", std::process::id()));
        std::fs::create_dir_all(&run)?;
        let disk = run.join("data.img");
        std::fs::copy(disk_clone(&image.dir, "data"), &disk)?;
        let restore = RestoreSpec {
            id: VmId::new(format!("clone-{i}"))?,
            // The saved MAC; `PROBE_NEW_MAC=1` gives each clone its own, which
            // the driver refuses.
            nics: vec![nic(if std::env::var_os("PROBE_NEW_MAC").is_some() {
                u8::try_from(10 + i)?
            } else {
                1
            })],
            disks: vec![DiskSpec {
                id: "data".into(),
                path: disk,
                read_only: false,
                root: false,
                cache: CacheMode::Unsafe,
            }],
            isolation: IsolationSpec::None,
        };
        let t = Instant::now();
        let vm = driver.restore(image, restore, &run).await?;
        let restored = t.elapsed();
        let mut lines = Lines::default();
        let first = lines.wait_tick(&*vm, Duration::from_secs(5)).await?;
        tokio::time::sleep(Duration::from_secs(4)).await;
        lines.drain(&*vm).await?;
        vm.shutdown(ShutdownMode::Kill).await?;
        let _ = std::fs::remove_dir_all(&run);
        println!(
            "[clone {i}] restore {:.0} ms; first {first}; last {}",
            ms(restored),
            lines.last_tick().unwrap_or_default()
        );
        lines.no_fails(&format!("clone {i}"))?;
        if tick_number(&first) != Some(saved_n + 1) {
            return Err(format!("clone {i}: tick jumped from {saved_n} to {first}").into());
        }
        Ok(())
    }

    /// The guest's console, split into lines.
    #[derive(Default)]
    struct Lines {
        partial: String,
        lines: Vec<String>,
    }

    impl Lines {
        async fn drain(&mut self, vm: &dyn VmHandle) -> Result<()> {
            let console = vm.console().ok_or("no console")?;
            loop {
                let bytes = console.read_output(64 << 10).await?;
                if bytes.is_empty() {
                    return Ok(());
                }
                self.partial.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(at) = self.partial.find('\n') {
                    let line = self.partial[..at].trim_end_matches('\r').to_owned();
                    self.partial.drain(..=at);
                    self.lines.push(line);
                }
            }
        }

        fn last_tick(&self) -> Option<String> {
            self.lines
                .iter()
                .rev()
                .find(|l| l.starts_with("tick "))
                .cloned()
        }

        async fn wait_tick(&mut self, vm: &dyn VmHandle, timeout: Duration) -> Result<String> {
            let deadline = Instant::now() + timeout;
            loop {
                self.drain(vm).await?;
                if let Some(tick) = self.last_tick() {
                    return Ok(tick);
                }
                if Instant::now() > deadline {
                    return Err("no tick from the guest".into());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        fn no_fails(&self, who: &str) -> Result<()> {
            let fails: Vec<&String> = self
                .lines
                .iter()
                .filter(|l| l.starts_with("FAIL"))
                .collect();
            match fails.first() {
                None => Ok(()),
                Some(first) => {
                    Err(format!("{who}: {} FAIL lines, first: {first}", fails.len()).into())
                }
            }
        }
    }

    fn tick_number(line: &str) -> Option<u64> {
        line.split_whitespace().nth(1)?.parse().ok()
    }

    fn newest_kernel() -> Result<PathBuf> {
        let boot =
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME unset")?).join(".arcbox/boot");
        let mut kernels: Vec<PathBuf> = std::fs::read_dir(&boot)?
            .filter_map(|e| e.ok().map(|e| e.path().join("kernel")))
            .filter(|p| p.exists())
            .collect();
        kernels.sort();
        kernels
            .pop()
            .ok_or_else(|| format!("no kernel under {}", boot.display()).into())
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }
}
