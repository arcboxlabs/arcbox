//! End-to-end probe: boots the System VM's own kernel and EROFS rootfs under
//! QEMU TCG and serves its `arcbox` virtio-fs share from this process over
//! vhost-user, the way the QEMU backend will.
//!
//! The rootfs is unmodified: its rcS mounts the `arcbox` tag and runs
//! `/arcbox/bin/arcbox-agent init`. The probe puts a shell script there that
//! reboots the guest once (so QEMU stops and restarts the device over the
//! same connection), then reports what the guest sees on the console, reads a
//! large file and a tree of small files through the share, writes a file
//! back, and powers off. A second back-end serves `vhost-user-vsock-pci` with
//! only a config space (the guest CID), to check that QEMU accepts it and the
//! guest driver starts its rings; it moves no vsock traffic.
//!
//! ```text
//! cargo run --release -p arcbox-vhost-user --example qemu_fs_probe -- \
//!     --qemu qemu-system-aarch64 --kernel kernel --rootfs rootfs.erofs \
//!     [--bios-dir <qemu>/pc-bios] [--big-mib 64] [--small-files 2000] [--smp 4]
//! ```
//!
//! QEMU must be built with `--enable-vhost-user --disable-vhost-net` (macOS
//! builds leave vhost-user off by default). Exit status 0 means every check
//! passed.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("qemu_fs_probe runs on macOS only");
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
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
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arcbox_fs::{CacheProfile, FsServer};
    use std::sync::atomic::{AtomicBool, Ordering};

    use arcbox_vhost_user::{Backend, ProtocolFeatures, VhostUserDevice, Vrings};
    use arcbox_virtio_core::{DescChain, GuestMemWriter, VirtioDevice};
    use arcbox_virtio_fs::{FsConfig, FuseResponse, VirtioFs};

    type Error = Box<dyn std::error::Error>;

    const VIRTIO_F_VERSION_1: u64 = 1 << 32;
    const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;
    /// Queue 0 is hiprio (FORGET, INTERRUPT); queue 1 carries requests.
    const NUM_QUEUES: usize = 2;

    /// virtio-fs over vhost-user: the HV backend's `VirtioFs` session and
    /// `FsServer` handler, fed from vhost-user vrings.
    struct FsDevice {
        fs: VirtioFs,
        requests: u64,
    }

    impl VhostUserDevice for FsDevice {
        fn num_queues(&self) -> usize {
            NUM_QUEUES
        }

        fn features(&self) -> u64 {
            VIRTIO_F_VERSION_1 | VIRTIO_RING_F_EVENT_IDX
        }

        fn process_queue(
            &mut self,
            queue: u16,
            vrings: &mut Vrings,
        ) -> arcbox_vhost_user::Result<()> {
            let Some(vring) = vrings.get_mut(queue) else {
                return Ok(());
            };
            let fs = &mut self.fs;
            self.requests += vring.process_avail(|mem, chain| handle_chain(fs, mem, chain)) as u64;
            Ok(())
        }

        fn reset(&mut self) {
            self.fs.reset();
        }
    }

    /// vhost-user-vsock with a config space and nothing else: enough for QEMU
    /// to realize the device and for the guest driver to post RX buffers.
    struct VsockConfigOnly {
        cid: u64,
        rx_posted: Arc<AtomicBool>,
    }

    impl VhostUserDevice for VsockConfigOnly {
        fn num_queues(&self) -> usize {
            2 // RX, TX; QEMU emulates the event queue itself
        }

        fn features(&self) -> u64 {
            VIRTIO_F_VERSION_1
        }

        fn protocol_features(&self) -> ProtocolFeatures {
            ProtocolFeatures::CONFIG
        }

        fn read_config(&self, offset: u32, data: &mut [u8]) -> arcbox_vhost_user::Result<()> {
            // struct virtio_vsock_config { le64 guest_cid; }
            let config = self.cid.to_le_bytes();
            let src = config
                .get(offset as usize..offset as usize + data.len())
                .ok_or_else(|| {
                    arcbox_vhost_user::Error::Device(format!("config read at {offset}"))
                })?;
            data.copy_from_slice(src);
            Ok(())
        }

        fn process_queue(
            &mut self,
            queue: u16,
            vrings: &mut Vrings,
        ) -> arcbox_vhost_user::Result<()> {
            if queue == 0 {
                let posted = vrings
                    .get_mut(0)
                    .and_then(|v| v.queue_mut())
                    .is_some_and(|q| q.has_avail());
                self.rx_posted.fetch_or(posted, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    /// Gathers the request from the readable descriptors, runs it, and
    /// scatters the reply into the writable ones.
    fn handle_chain(fs: &mut VirtioFs, mem: &GuestMemWriter, chain: &DescChain) -> u32 {
        let mut request = Vec::new();
        for d in chain.descriptors.iter().filter(|d| !d.is_write()) {
            if let Some(bytes) = mem.slice(d.addr as usize, d.len as usize) {
                request.extend_from_slice(bytes);
            }
        }
        let reply = fs.process_request(&request).unwrap_or_else(|e| {
            tracing::warn!("FUSE request failed: {e}");
            let unique = request
                .get(8..16)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
            FuseResponse::error(unique, libc::EIO).into_data()
        });
        let mut written = 0;
        for d in chain.descriptors.iter().filter(|d| d.is_write()) {
            let rest = &reply[written..];
            if rest.is_empty() {
                break;
            }
            let n = rest.len().min(d.len as usize);
            // SAFETY: writable descriptors belong to the device until the used
            // ring publishes the chain.
            if let Some(dst) = unsafe { mem.slice_mut(d.addr as usize, n) } {
                dst.copy_from_slice(&rest[..n]);
                written += n;
            }
        }
        written as u32
    }

    struct Args {
        qemu: PathBuf,
        kernel: PathBuf,
        rootfs: PathBuf,
        bios_dir: Option<PathBuf>,
        big_mib: usize,
        small_files: usize,
        smp: u32,
        timeout: Duration,
    }

    fn parse_args() -> Result<Args, Error> {
        let mut args = std::env::args().skip(1);
        let (mut qemu, mut kernel, mut rootfs, mut bios_dir) = (None, None, None, None);
        let (mut big_mib, mut small_files, mut smp, mut timeout) = (64, 2000, 4, 600);
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--qemu" => qemu = Some(PathBuf::from(value()?)),
                "--kernel" => kernel = Some(PathBuf::from(value()?)),
                "--rootfs" => rootfs = Some(PathBuf::from(value()?)),
                "--bios-dir" => bios_dir = Some(PathBuf::from(value()?)),
                "--big-mib" => big_mib = value()?.parse()?,
                "--small-files" => small_files = value()?.parse()?,
                "--smp" => smp = value()?.parse()?,
                "--timeout-secs" => timeout = value()?.parse()?,
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        Ok(Args {
            qemu: qemu.ok_or("--qemu is required")?,
            kernel: kernel.ok_or("--kernel is required")?,
            rootfs: rootfs.ok_or("--rootfs is required")?,
            bios_dir,
            big_mib,
            small_files,
            smp,
            timeout: Duration::from_secs(timeout),
        })
    }

    /// The stand-in agent rcS runs from the share.
    const AGENT: &str = r#"#!/bin/busybox sh
bb=/bin/busybox
say() { $bb printf 'VHOST-PROBE %s\n' "$*" > /dev/console; }
up() { $bb cut -d' ' -f1 /proc/uptime; }
say "agent-exec args=$*"
if [ ! -e /arcbox/probe/rebooted ]; then
  echo 1 > /arcbox/probe/rebooted && $bb sync
  say "rebooting"
  $bb reboot -f
fi
say "mount=$($bb grep ' /arcbox ' /proc/mounts)"
say "vsock=$($bb ls /sys/bus/virtio/drivers/vmw_vsock_virtio_transport 2>/dev/null | $bb grep virtio)"
say "hello=$($bb cat /arcbox/probe/hello.txt)"
echo 3 > /proc/sys/vm/drop_caches
t0=$(up); $bb dd if=/arcbox/probe/big.bin of=/dev/null bs=1M 2>/dev/null; t1=$(up)
say "big-read start=$t0 end=$t1"
echo 3 > /proc/sys/vm/drop_caches
say "sha256=$($bb sha256sum /arcbox/probe/big.bin | $bb cut -d' ' -f1)"
t0=$(up); n=$($bb cat /arcbox/probe/small/* | $bb wc -c); t1=$(up)
say "small-read bytes=$n start=$t0 end=$t1"
t0=$(up); n=$($bb ls -l /arcbox/probe/small | $bb wc -l); t1=$(up)
say "small-stat entries=$n start=$t0 end=$t1"
echo "written-by-guest $($bb uname -r)" > /arcbox/probe/from-guest.txt && $bb sync
say "wrote=$?"
say "done"
$bb poweroff -f
"#;

    fn build_share(root: &Path, big_mib: usize, small_files: usize) -> Result<String, Error> {
        std::fs::create_dir_all(root.join("bin"))?;
        std::fs::create_dir_all(root.join("probe/small"))?;
        let agent = root.join("bin/arcbox-agent");
        std::fs::write(&agent, AGENT)?;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))?;
        std::fs::write(root.join("probe/hello.txt"), "hello from the host")?;
        let mut big = vec![0u8; big_mib << 20];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut big)?;
        std::fs::write(root.join("probe/big.bin"), &big)?;
        for i in 0..small_files {
            std::fs::write(root.join(format!("probe/small/{i:05}")), [b'x'; 1024])?;
        }
        let out = Command::new("shasum")
            .args(["-a", "256"])
            .arg(root.join("probe/big.bin"))
            .output()?;
        let text = String::from_utf8(out.stdout)?;
        Ok(text
            .split_whitespace()
            .next()
            .ok_or("shasum printed nothing")?
            .to_owned())
    }

    /// A socketpair whose second end survives `exec` into QEMU.
    fn chardev_pair() -> Result<(UnixStream, OwnedFd), Error> {
        let mut fds = [0; 2];
        // SAFETY: socketpair writes two descriptors into `fds`.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fresh descriptors, sole owner of each.
        let (ours, theirs) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: F_SETFD on an owned descriptor; only ours is close-on-exec.
        unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok((UnixStream::from(ours), theirs))
    }

    fn field<'a>(console: &'a str, key: &str) -> Option<&'a str> {
        console
            .lines()
            .filter_map(|l| l.split_once("VHOST-PROBE ").map(|(_, rest)| rest))
            .find_map(|rest| rest.strip_prefix(key))
    }

    fn kv(line: &str, key: &str) -> Option<f64> {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(key)?.parse().ok())
    }

    pub fn run() -> Result<(), Error> {
        let args = parse_args()?;
        let work = tempfile::tempdir()?;
        let share = work.path().join("share");
        let expected_sha = build_share(&share, args.big_mib, args.small_files)?;
        let console_path = work.path().join("console.log");
        let qemu_log = work.path().join("qemu.log");

        let mut server = FsServer::new(arcbox_fs::FsConfig {
            tag: "arcbox".into(),
            source: share.to_string_lossy().into_owned(),
            cache_profile: CacheProfile::Dynamic,
            ..arcbox_fs::FsConfig::default()
        });
        server.start()?;
        let fs = VirtioFs::with_handler(
            FsConfig {
                tag: "arcbox".into(),
                num_queues: 1,
                queue_size: 1024,
                shared_dir: share.to_string_lossy().into_owned(),
            },
            Arc::new(server),
        );

        let (ours, theirs) = chardev_pair()?;
        let backend = Backend::new("vhost-fs-arcbox", FsDevice { fs, requests: 0 }).spawn(ours)?;
        let (vsock_ours, vsock_theirs) = chardev_pair()?;
        let rx_posted = Arc::new(AtomicBool::new(false));
        let vsock = Backend::new(
            "vhost-vsock",
            VsockConfigOnly {
                cid: 3,
                rx_posted: Arc::clone(&rx_posted),
            },
        )
        .spawn(vsock_ours)?;

        let mut qemu = Command::new(&args.qemu);
        if let Some(dir) = &args.bios_dir {
            qemu.arg("-L").arg(dir);
        }
        qemu.args(["-accel", "tcg,thread=multi", "-cpu", "max,pauth-impdef=on"])
            .args(["-smp", &args.smp.to_string(), "-m", "2G"])
            .args(["-object", "memory-backend-shm,id=mem,size=2G,share=on"])
            .args(["-machine", "virt,memory-backend=mem"])
            .args(["-display", "none", "-nodefaults"])
            .arg("-kernel")
            .arg(&args.kernel)
            .args([
                "-append",
                // ttyAMA0 keeps the kernel log when hvc0 never comes up;
                // the last console= is /dev/console for the probe script.
                "console=ttyAMA0 console=hvc0 root=/dev/vda ro rootfstype=erofs panic=-1",
            ])
            .arg("-serial")
            .arg(format!("file:{}", work.path().join("serial.log").display()))
            .arg("-drive")
            .arg(format!(
                "file={},format=raw,if=none,id=root,readonly=on",
                args.rootfs.display()
            ))
            .args(["-device", "virtio-blk-pci,drive=root"])
            .args(["-device", "virtio-serial-pci"])
            .arg("-chardev")
            .arg(format!("file,id=hvc,path={}", console_path.display()))
            .args(["-device", "virtconsole,chardev=hvc"])
            .arg("-chardev")
            .arg(format!("socket,id=fs0,fd={}", theirs.as_raw_fd()))
            .args([
                "-device",
                "vhost-user-fs-pci,chardev=fs0,tag=arcbox,queue-size=1024",
            ])
            .arg("-chardev")
            .arg(format!("socket,id=vs0,fd={}", vsock_theirs.as_raw_fd()))
            .args(["-device", "vhost-user-vsock-pci,chardev=vs0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&qemu_log)?);

        let started = Instant::now();
        let mut child = qemu.spawn()?;
        drop(theirs);
        drop(vsock_theirs);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if started.elapsed() > args.timeout {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let elapsed = started.elapsed();
        let qemu_out = std::fs::read_to_string(&qemu_log).unwrap_or_default();
        let device = backend.join()?;
        vsock.join()?;
        let console = std::fs::read_to_string(&console_path).unwrap_or_default();

        println!("--- console (VHOST-PROBE and virtiofs lines) ---");
        for line in console
            .lines()
            .filter(|l| l.contains("VHOST-PROBE") || l.contains("virtio"))
        {
            println!("{line}");
        }
        if !qemu_out.trim().is_empty() {
            println!("--- qemu stderr ---\n{}", qemu_out.trim());
        }
        println!("--- summary ---");
        match status {
            Some(status) => println!(
                "qemu exit: {status}, wall time {:.1}s",
                elapsed.as_secs_f64()
            ),
            None => println!("qemu killed after {:.0}s timeout", elapsed.as_secs_f64()),
        }
        println!("FUSE requests served: {}", device.requests);

        let mut failures = Vec::new();
        let mut check = |ok: bool, what: String| {
            println!("[{}] {what}", if ok { "ok" } else { "FAIL" });
            if !ok {
                failures.push(what);
            }
        };
        check(status.is_some(), "guest powered off".into());
        let boots = console.matches("VHOST-PROBE agent-exec").count();
        check(
            boots == 2,
            format!("rcS ran the agent from the share on {boots}/2 boots (one guest reboot)"),
        );
        check(
            field(&console, "mount=").is_some_and(|m| m.contains("virtiofs")),
            format!("guest mount: {}", field(&console, "mount=").unwrap_or("-")),
        );
        check(
            field(&console, "vsock=").is_some_and(|v| v.starts_with("virtio")),
            format!(
                "guest vsock driver bound: {}",
                field(&console, "vsock=").unwrap_or("-")
            ),
        );
        check(
            rx_posted.load(Ordering::SeqCst),
            "guest posted vsock RX buffers".into(),
        );
        check(
            field(&console, "hello=") == Some("hello from the host"),
            "small file content".into(),
        );
        let sha = field(&console, "sha256=").unwrap_or("-");
        check(
            sha == expected_sha,
            format!(
                "{} MiB sha256 guest={sha} host={expected_sha}",
                args.big_mib
            ),
        );
        let written =
            std::fs::read_to_string(share.join("probe/from-guest.txt")).unwrap_or_default();
        check(
            written.starts_with("written-by-guest"),
            format!("guest write visible on host: {:?}", written.trim()),
        );
        check(
            field(&console, "done").is_some(),
            "probe script finished".into(),
        );

        if let Some(line) = field(&console, "big-read ") {
            if let (Some(t0), Some(t1)) = (kv(line, "start="), kv(line, "end=")) {
                let secs = (t1 - t0).max(0.01);
                println!(
                    "big read: {} MiB in {secs:.2}s = {:.1} MiB/s",
                    args.big_mib,
                    args.big_mib as f64 / secs
                );
            }
        }
        for (key, what) in [("small-read ", "cat"), ("small-stat ", "ls -l")] {
            if let Some(line) = field(&console, key) {
                if let (Some(t0), Some(t1)) = (kv(line, "start="), kv(line, "end=")) {
                    let secs = (t1 - t0).max(0.01);
                    println!(
                        "{what} of {} small files: {secs:.2}s = {:.0} files/s",
                        args.small_files,
                        args.small_files as f64 / secs
                    );
                }
            }
        }
        std::io::stdout().flush()?;
        if failures.is_empty() {
            Ok(())
        } else {
            let keep = work.keep();
            Err(format!(
                "{} check(s) failed; console at {}",
                failures.len(),
                keep.join("console.log").display()
            )
            .into())
        }
    }
}
