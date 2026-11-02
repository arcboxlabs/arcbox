//! End-to-end probe: boots the System VM under QEMU TCG with the real
//! `arcbox-agent` and runtime, its `arcbox`, `users` and `private` shares
//! served by `arcbox_vhost_user::fs` the way the QEMU backend will.
//!
//! Kernel and EROFS rootfs are the boot-assets release, unmodified; the
//! runtime binaries are the ones its manifest names, laid out as the daemon
//! stages them (`boot/<gen>/manifest.json`, `runtime/<gen>/`). The disks and
//! cmdline are the daemon's: vda the rootfs, vdb/vdc the data and metadata
//! images `arcbox_storage::prepare_pair` creates, and the cmdline tokens
//! `desired_boot` appends.
//!
//! The agent is staged behind `guest/wrapper.sh`, which runs it from a
//! devtmpfs copy (the reason is in the script) and starts
//! `guest/driver.sh`. The driver reaches the agent over vsock loopback
//! (`guest/agent_rpc.rs`), asks it for the runtime (EnsureRuntime:
//! materialize to Btrfs, start containerd and dockerd), runs `sync` and
//! `syncfs` on every share, reads a tree of small files from `/Users` with
//! one reader and from `/private` with four, then powers off. A
//! vhost-user-vsock back-end with only a config space gives the guest its
//! vsock transport; no NIC is attached.
//!
//! ```text
//! cargo build --release -p arcbox-agent --target aarch64-unknown-linux-musl
//! cargo run --release -p arcbox-vhost-user --example qemu_system_vm_probe -- \
//!     --qemu qemu-system-aarch64 --kernel kernel --rootfs rootfs.erofs \
//!     --agent target/aarch64-unknown-linux-musl/release/arcbox-agent \
//!     --assets <data dir holding boot/<gen> and runtime/<gen>> --generation 0.8.12 \
//!     [--smp 4] [--memory-mib 4096] [--small-files 4000]
//! ```
//!
//! The guest helpers are built with `rustc` for `aarch64-unknown-linux-musl`
//! and link with `aarch64-linux-musl-gcc`, as the agent does. Exit status 0
//! means every check passed.

#[cfg(target_os = "macos")]
#[path = "../support/mod.rs"]
mod support;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("qemu_system_vm_probe runs on macOS only");
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    match probe::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe FAILED: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
mod probe {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use arcbox_constants::virtiofs::{
        MOUNT_PRIVATE, MOUNT_USERS, TAG_ARCBOX, TAG_PRIVATE, TAG_USERS,
    };
    use arcbox_vhost_user::Backend;
    use arcbox_vhost_user::fs::{FsShare, FsStats, QUEUE_SIZE, REQUEST_QUEUES};

    use crate::support::{VsockConfigOnly, chardev_pair};

    pub type Error = Box<dyn std::error::Error>;

    /// Routine files `guest/exec_coherence.rs` maps from the share.
    pub const EXEC_ROUNDS: u32 = 16;

    pub struct Args {
        qemu: PathBuf,
        kernel: PathBuf,
        rootfs: PathBuf,
        agent: PathBuf,
        assets: PathBuf,
        pub generation: String,
        smp: u32,
        memory_mib: u64,
        pub small_files: usize,
        timeout: Duration,
    }

    fn parse_args() -> Result<Args, Error> {
        let mut flags = std::collections::HashMap::new();
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let value = argv.next().ok_or_else(|| format!("{flag} needs a value"))?;
            flags.insert(flag, value);
        }
        let mut take = |name: &str, default: Option<&str>| {
            flags
                .remove(name)
                .or_else(|| default.map(str::to_owned))
                .ok_or_else(|| format!("{name} is required"))
        };
        let args = Args {
            qemu: take("--qemu", None)?.into(),
            kernel: take("--kernel", None)?.into(),
            rootfs: take("--rootfs", None)?.into(),
            agent: take("--agent", None)?.into(),
            assets: take("--assets", None)?.into(),
            generation: take("--generation", None)?,
            smp: take("--smp", Some("4"))?.parse()?,
            memory_mib: take("--memory-mib", Some("4096"))?.parse()?,
            small_files: take("--small-files", Some("4000"))?.parse()?,
            timeout: Duration::from_secs(take("--timeout-secs", Some("1800"))?.parse()?),
        };
        match flags.into_keys().next() {
            Some(flag) => Err(format!("unknown argument {flag}").into()),
            None => Ok(args),
        }
    }

    fn install(path: &Path, contents: &[u8]) -> Result<(), Error> {
        std::fs::write(path, contents)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// The data directory the `arcbox` share serves: agent, guest helpers,
    /// driver, boot manifest, runtime, and the paired disk images.
    fn stage_share(args: &Args, root: &Path, users: &Path, private: &Path) -> Result<(), Error> {
        for dir in ["bin", "probe", "data"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        install(
            &root.join("bin/arcbox-agent.real"),
            &std::fs::read(&args.agent)?,
        )?;
        install(
            &root.join("bin/arcbox-agent"),
            include_bytes!("guest/wrapper.sh"),
        )?;
        install(
            &root.join("probe/driver.sh"),
            include_bytes!("guest/driver.sh"),
        )?;
        let env = format!(
            "G={}\nU='{}'\nP='{}'\nEXEC_ROUNDS={EXEC_ROUNDS}\n",
            args.generation,
            users.display(),
            private.display()
        );
        std::fs::write(root.join("probe/env"), env)?;
        let guest =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/qemu_system_vm_probe/guest");
        for (source, name) in [
            ("agent_rpc.rs", "agent-rpc"),
            ("exec_coherence.rs", "exec-coherence"),
        ] {
            let status = Command::new("rustc")
                .args([
                    "--edition",
                    "2024",
                    "--target",
                    "aarch64-unknown-linux-musl",
                ])
                .args(["-C", "linker=aarch64-linux-musl-gcc", "-C", "opt-level=2"])
                .arg(guest.join(source))
                .arg("-o")
                .arg(root.join("bin").join(name))
                .status()?;
            if !status.success() {
                return Err(format!("building the guest helper {name} failed").into());
            }
        }
        // `mov w0, #2; ret`, padded to a page for `O_DIRECT`.
        let mut routine = vec![0; 4096];
        routine[..8].copy_from_slice(&[0x40, 0, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6]);
        std::fs::write(root.join("probe/ret2.bin"), &routine)?;
        std::fs::create_dir_all(root.join("probe/exec"))?;
        for round in 0..EXEC_ROUNDS {
            std::fs::write(root.join(format!("probe/exec/{round}.bin")), &routine)?;
        }
        for dir in ["boot", "runtime"] {
            std::fs::create_dir_all(root.join(dir))?;
            let sub = Path::new(dir).join(&args.generation);
            let copy = Command::new("cp")
                .arg("-cR")
                .arg(args.assets.join(&sub))
                .arg(root.join(&sub))
                .status()?;
            if !copy.success() {
                return Err(format!("copying {} failed", sub.display()).into());
            }
        }
        arcbox_storage::prepare_pair(
            &root.join("data/docker.img"),
            &root.join("data/docker-meta.img"),
            16 << 30,
            1 << 30,
        )?;
        Ok(())
    }

    /// `files` 4 KiB files in four top-level directories of 50 each.
    fn small_tree(root: &Path, files: usize) -> Result<(), Error> {
        for i in 0..files {
            let dir = root.join(format!("d{}/s{:03}", i % 4, i / 4 / 50));
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(format!("f{i:05}")), [b'x'; 4096])?;
        }
        Ok(())
    }

    /// The kernel cmdline the daemon boots the System VM with: the
    /// manifest's, plus the tokens `desired_boot` appends.
    fn cmdline(args: &Args) -> Result<String, Error> {
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(
            args.assets
                .join("boot")
                .join(&args.generation)
                .join("manifest.json"),
        )?)?;
        let base = manifest["targets"]["arm64"]["kernel_cmdline"]
            .as_str()
            .ok_or("manifest has no arm64 kernel_cmdline")?;
        Ok(format!(
            "{base} arcbox.container_network=10.80.0.0/20 arcbox.dns_domain=arcbox.local \
             arcbox.storage_manifest=/arcbox/data/docker.storage.json \
             arcbox.docker_metadata_device=/dev/vdc arcbox.runtime_generation={}",
            args.generation
        ))
    }

    /// QEMU with the System VM's block, console and entropy devices; the
    /// vhost-user devices are added by the caller.
    fn qemu_command(args: &Args, work: &Path, share: &Path) -> Result<Command, Error> {
        let mem = format!("{}M", args.memory_mib);
        let mut qemu = Command::new(&args.qemu);
        qemu.args(["-accel", "tcg,thread=multi", "-cpu", "max,pauth-impdef=on"])
            .args(["-smp", &args.smp.to_string(), "-m", &mem])
            .arg("-object")
            .arg(format!("memory-backend-shm,id=mem,size={mem},share=on"))
            .args(["-machine", "virt,memory-backend=mem"])
            .args(["-display", "none", "-nodefaults"])
            .arg("-kernel")
            .arg(&args.kernel)
            .args(["-append", &cmdline(args)?])
            .arg("-serial")
            .arg(format!("file:{}/serial.log", work.display()));
        let disks = [
            (args.rootfs.clone(), ",readonly=on"),
            (share.join("data/docker.img"), ",discard=unmap"),
            (share.join("data/docker-meta.img"), ",discard=unmap"),
        ];
        for (i, (path, opts)) in disks.iter().enumerate() {
            qemu.arg("-drive")
                .arg(format!(
                    "file={},format=raw,if=none,id=vd{i}{opts}",
                    path.display()
                ))
                .arg("-device")
                .arg(format!("virtio-blk-pci,drive=vd{i},addr={:#x}", 2 + i));
        }
        qemu.args(["-device", "virtio-serial-pci", "-device", "virtio-rng-pci"]);
        for hvc in ["console", "agent-console"] {
            qemu.arg("-chardev")
                .arg(format!("file,id={hvc},path={}/{hvc}.log", work.display()))
                .args(["-device", &format!("virtconsole,chardev={hvc}")]);
        }
        Ok(qemu)
    }

    pub fn run() -> Result<(), Error> {
        let args = parse_args()?;
        let work = tempfile::tempdir()?;
        let work_path = work.path().canonicalize()?;
        let log = std::fs::File::create(work_path.join("backend.log"))?;
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(std::sync::Mutex::new(log))
            .with_thread_names(true)
            .with_ansi(false)
            .init();

        let users_tree = tempfile::Builder::new()
            .prefix("vhost-probe-")
            .tempdir_in(std::env::var("HOME")?)?;
        let private_tree = work_path.join("tree");
        small_tree(users_tree.path(), args.small_files)?;
        small_tree(&private_tree, args.small_files)?;
        let share = work_path.join("share");
        stage_share(&args, &share, users_tree.path(), &private_tree)?;

        let mut qemu = qemu_command(&args, &work_path, &share)?;
        let mut theirs = Vec::new();
        let mut shares = Vec::new();
        let sources = [
            (TAG_ARCBOX, share.as_path()),
            (TAG_USERS, Path::new(MOUNT_USERS)),
            (TAG_PRIVATE, Path::new(MOUNT_PRIVATE)),
        ];
        for (i, (tag, source)) in sources.into_iter().enumerate() {
            let (ours, fd) = chardev_pair()?;
            shares.push(FsShare::new(tag, source).spawn(ours)?);
            qemu.arg("-chardev")
                .arg(format!("socket,id=fs{i},fd={}", fd.as_raw_fd()))
                .arg("-device")
                .arg(format!(
                    "vhost-user-fs-pci,chardev=fs{i},tag={tag},queue-size={QUEUE_SIZE},num-request-queues={REQUEST_QUEUES}"
                ));
            theirs.push(fd);
        }
        let (vsock_ours, fd) = chardev_pair()?;
        let device = VsockConfigOnly {
            cid: 3,
            rx_posted: Arc::new(AtomicBool::new(false)),
        };
        let vsock = Backend::new("vhost-vsock", device).spawn(vsock_ours)?;
        qemu.arg("-chardev")
            .arg(format!("socket,id=vs0,fd={}", fd.as_raw_fd()))
            .args(["-device", "vhost-user-vsock-pci,chardev=vs0"]);
        theirs.push(fd);

        qemu.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(work_path.join("qemu.log"))?);
        let started = Instant::now();
        let mut child = qemu.spawn()?;
        drop(theirs);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if started.elapsed() > args.timeout {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let elapsed = started.elapsed();
        let stats: Vec<(String, FsStats)> = shares
            .into_iter()
            .map(|b| Ok::<_, Error>((b.tag().to_owned(), b.join()?)))
            .collect::<Result<_, _>>()?;
        vsock.join()?;
        let console = std::fs::read_to_string(work_path.join("console.log")).unwrap_or_default();
        for line in console.lines().filter(|l| l.contains("PROBE ")) {
            println!("{line}");
        }
        for (tag, s) in &stats {
            println!(
                "{tag}: {} requests, busy {:.2}s of {:.1}s, deepest batch {}",
                s.requests,
                s.busy.as_secs_f64(),
                elapsed.as_secs_f64(),
                s.max_batch
            );
        }
        if status.is_some_and(|s| s.success()) && console.contains("PROBE done") {
            Ok(())
        } else {
            let keep = work.keep();
            Err(format!("the guest did not finish; logs in {}", keep.display()).into())
        }
    }
}
