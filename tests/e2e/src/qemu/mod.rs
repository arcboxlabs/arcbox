//! The System VM under QEMU TCG, for probes of the vhost-user devices.
//!
//! Shaped the way the daemon shapes it: EROFS rootfs on vda, the Docker
//! data and metadata images on vdb and vdc, the `arcbox` share served over
//! vhost-user-fs and vsock over vhost-user-vsock, both from the calling
//! process. A harness until the QEMU backend boots the System VM itself;
//! `qemu_vsock_e2e` is the first user.

pub mod fs;

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arcbox_constants::cmdline::{
    CONTAINER_NETWORK_KEY, DEBUG_CONSOLE_KEY, DNS_DOMAIN_KEY, DOCKER_METADATA_DEVICE_KEY,
    HV_EARLYCON_DIRECTIVE, RUNTIME_GENERATION_KEY,
};
use arcbox_constants::container_network::ContainerNetwork;
use arcbox_constants::devices::DOCKER_METADATA_BLOCK_DEVICE;
use arcbox_vhost_user::{Backend, BackendHandle};
use arcbox_virtio_vsock::vhost_user::VhostUserVsock;

use fs::FsDevice;

/// The daemon's Docker image sizes (`vm_lifecycle/mod.rs`); both sparse.
const DATA_IMAGE_BYTES: u64 = 8 << 40;
const METADATA_IMAGE_BYTES: u64 = 2 << 30;

pub struct VmOptions {
    pub qemu: PathBuf,
    pub data_dir: PathBuf,
    pub version: String,
    pub agent: PathBuf,
    pub cid: u64,
    pub accel: String,
    pub cpu: String,
    pub smp: u32,
    pub memory_mib: u64,
    /// gvproxy binary serving the NIC (`-listen-qemu`); no NIC without it.
    pub gvproxy: Option<PathBuf>,
    /// Execute the agent straight from the vhost-user-fs share instead of
    /// from a copy the guest wrote (see `AGENT_LAUNCHER`). Reproduces the
    /// stale-translation crash under TCG.
    pub exec_from_share: bool,
    pub log_dir: PathBuf,
}

pub struct Vm {
    pub qemu: Child,
    pub vsock: Arc<VhostUserVsock>,
    fs: BackendHandle<FsDevice>,
    gvproxy: Option<Reaped>,
    log_dir: PathBuf,
}

impl Vm {
    pub fn boot(options: &VmOptions) -> Result<Self> {
        let boot = options.data_dir.join("boot").join(&options.version);
        let cmdline = kernel_cmdline(&boot, &options.version)?;
        stage_share(options)?;
        let data = options.data_dir.join("data");
        arcbox_storage::prepare_pair(
            &data.join("docker.img"),
            &data.join("docker-meta.img"),
            DATA_IMAGE_BYTES,
            METADATA_IMAGE_BYTES,
        )
        .context("prepare Docker images")?;

        // Before the chardev pairs: QEMU's ends are inheritable, and a gvproxy
        // holding them would keep the back-ends from seeing QEMU go away.
        let gvproxy = options
            .gvproxy
            .as_deref()
            .map(|bin| spawn_gvproxy(bin, &options.log_dir))
            .transpose()?;
        let (fs_ours, fs_theirs) = chardev_pair()?;
        let fs = Backend::new(
            "vhost-fs-arcbox",
            FsDevice::new("arcbox", &options.data_dir)?,
        )
        .spawn(fs_ours)?;
        let (vsock_ours, vsock_theirs) = chardev_pair()?;
        let vsock = Arc::new(VhostUserVsock::spawn(options.cid, vsock_ours)?);

        let ram = format!("{}M", options.memory_mib);
        let mut qemu = Command::new(&options.qemu);
        qemu.args(["-accel", &options.accel, "-cpu", &options.cpu])
            .args(["-smp", &options.smp.to_string(), "-m", &ram])
            .arg("-object")
            .arg(format!("memory-backend-shm,id=mem,size={ram},share=on"))
            .args(["-machine", "virt,memory-backend=mem"])
            .args(["-display", "none", "-nodefaults", "-no-user-config"])
            .arg("-kernel")
            .arg(boot.join("kernel"))
            .arg("-append")
            .arg(&cmdline)
            .arg("-serial")
            .arg(format!(
                "file:{}",
                options.log_dir.join("serial.log").display()
            ));
        // vda/vdb/vdc follow PCI slot order, so pin the slots.
        for (slot, id, path, extra) in [
            (2, "vda", boot.join("rootfs.erofs"), ",readonly=on"),
            (3, "vdb", data.join("docker.img"), ",discard=unmap"),
            (4, "vdc", data.join("docker-meta.img"), ",discard=unmap"),
        ] {
            qemu.arg("-drive")
                .arg(format!(
                    "file={},format=raw,if=none,id={id}{extra}",
                    path.display()
                ))
                .arg("-device")
                .arg(format!("virtio-blk-pci,drive={id},addr={slot:#x}"));
        }
        qemu.args(["-device", "virtio-serial-pci,addr=0x5"])
            // hvc0 is also the rcS debug shell (`arcbox.debug_console`):
            // `nc -U <log dir>/console.sock` reaches it.
            .arg("-chardev")
            .arg(format!(
                "socket,id=hvc0,path={},server=on,wait=off,logfile={}",
                options.log_dir.join("console.sock").display(),
                options.log_dir.join("console.log").display()
            ))
            .args(["-device", "virtconsole,chardev=hvc0"])
            .arg("-chardev")
            .arg(format!("socket,id=fs0,fd={}", fs_theirs.as_raw_fd()))
            .args([
                "-device",
                "vhost-user-fs-pci,chardev=fs0,tag=arcbox,queue-size=1024,addr=0x6",
            ])
            .arg("-chardev")
            .arg(format!("socket,id=vsock0,fd={}", vsock_theirs.as_raw_fd()))
            .args(["-device", "vhost-user-vsock-pci,chardev=vsock0,addr=0x7"]);
        if gvproxy.is_some() {
            qemu.arg("-netdev")
                .arg(format!(
                    "stream,id=net0,server=off,addr.type=unix,addr.path={}",
                    options.log_dir.join(GVPROXY_SOCKET).display()
                ))
                .args([
                    "-device",
                    "virtio-net-pci,netdev=net0,romfile=,mac=5a:94:ef:e4:0c:ee,addr=0x8",
                ]);
        }
        let stderr = std::fs::File::create(options.log_dir.join("qemu.log"))?;
        let child = qemu
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .context("spawn QEMU")?;
        drop((fs_theirs, vsock_theirs));
        println!("[boot] qemu pid {} cmdline: {cmdline}", child.id());
        Ok(Self {
            qemu: child,
            vsock,
            fs,
            gvproxy,
            log_dir: options.log_dir.clone(),
        })
    }

    /// Runs `command` in the guest's debug shell on hvc0. Output lands in
    /// `console.log`; the shell reports nothing back.
    pub fn console(&self, command: &str) -> Result<()> {
        let mut shell = UnixStream::connect(self.log_dir.join("console.sock"))?;
        std::io::Write::write_all(&mut shell, format!("{command}\n").as_bytes())?;
        // QEMU drops input still queued when the client goes away.
        std::thread::sleep(std::time::Duration::from_millis(500));
        Ok(())
    }

    /// Points the guest's DNS upstream at gvproxy. The agent's resolver
    /// forwards to the ArcBox gateway, `10.0.2.1:53`; gvproxy serves
    /// 192.168.127.0/24 with DNS on .1. Lasts until the guest reboots.
    pub fn route_guest_dns_to_gvproxy(&self) -> Result<()> {
        for proto in ["udp", "tcp"] {
            self.console(&format!(
                "iptables -t nat -A OUTPUT -d 10.0.2.1/32 -p {proto} --dport 53 \
                 -j DNAT --to-destination 192.168.127.1:53"
            ))?;
        }
        Ok(())
    }

    /// Stops QEMU and both back-ends.
    pub fn shutdown(mut self) -> Result<()> {
        let _ = self.qemu.kill();
        let status = self.qemu.wait()?;
        println!("[shutdown] qemu exited: {status}");
        drop(self.gvproxy.take());
        match Arc::try_unwrap(self.vsock) {
            Ok(vsock) => vsock.join().context("vsock back-end")?,
            Err(_) => bail!("vsock handle still shared at shutdown"),
        }
        self.fs.join().context("fs back-end")?;
        Ok(())
    }
}

/// The boot manifest's command line with what the daemon changes for every
/// System VM (`vm_lifecycle/boot.rs` `desired_boot`): no `quiet`, the
/// container network, the storage manifest, the metadata disk and the
/// runtime generation. The HV PL011 `earlycon` is dropped; `ttyAMA0` keeps
/// the kernel log on QEMU's serial port.
fn kernel_cmdline(boot: &Path, version: &str) -> Result<String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(boot.join("manifest.json"))?)?;
    let base = manifest["targets"]["arm64"]["kernel_cmdline"]
        .as_str()
        .context("manifest has no arm64 kernel_cmdline")?;
    let mut tokens: Vec<String> = vec!["console=ttyAMA0".into()];
    tokens.extend(
        base.split_whitespace()
            .filter(|t| *t != "quiet" && *t != HV_EARLYCON_DIRECTIVE)
            .map(str::to_owned),
    );
    tokens.push(format!(
        "{CONTAINER_NETWORK_KEY}{}",
        ContainerNetwork::default()
    ));
    tokens.push(format!("{DNS_DOMAIN_KEY}arcbox.local"));
    tokens.push(format!(
        "{}/arcbox/data/docker.storage.json",
        arcbox_storage::MANIFEST_CMDLINE_KEY
    ));
    tokens.push(format!(
        "{DOCKER_METADATA_DEVICE_KEY}{DOCKER_METADATA_BLOCK_DEVICE}"
    ));
    tokens.push(format!("{RUNTIME_GENERATION_KEY}{version}"));
    tokens.push(format!("{DEBUG_CONSOLE_KEY}hvc0"));
    Ok(tokens.join(" "))
}

/// Puts the agent where the guest's rcS execs it from (`/arcbox/bin`).
fn stage_share(options: &VmOptions) -> Result<()> {
    let boot = options.data_dir.join("boot").join(&options.version);
    for asset in ["kernel", "rootfs.erofs", "manifest.json"] {
        if !boot.join(asset).is_file() {
            bail!(
                "{} missing; run `abctl boot prefetch --data-dir {}` first",
                boot.join(asset).display(),
                options.data_dir.display()
            );
        }
    }
    let bin = options.data_dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let _ = std::fs::remove_file(bin.join(AGENT_IMAGE));
    let _ = std::fs::remove_file(bin.join("arcbox-agent"));
    if options.exec_from_share {
        std::fs::copy(&options.agent, bin.join("arcbox-agent")).context("stage the agent")?;
    } else {
        std::fs::copy(&options.agent, bin.join(AGENT_IMAGE)).context("stage the agent")?;
        std::fs::write(bin.join("arcbox-agent"), AGENT_LAUNCHER)?;
        std::fs::set_permissions(
            bin.join("arcbox-agent"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
    }
    std::fs::create_dir_all(options.data_dir.join("data"))?;
    Ok(())
}

/// The agent binary the launcher copies; see [`AGENT_LAUNCHER`].
const AGENT_IMAGE: &str = "arcbox-agent.image";

/// Runs the agent (`init` from rcS and the respawned server) from a copy the
/// guest CPU wrote, on a tmpfs over the rootfs's empty `/mnt`.
///
/// TCG keys translated code by guest physical page and drops it only when
/// its own softmmu sees a store to that page (`IC IVAU` is a no-op in system
/// mode, and `-cpu max` reports DIC). The vhost-user-fs back-end fills
/// page-cache pages through shared memory, which TCG never sees, so code
/// executed straight from the share runs translations of whatever those pages
/// held before, a guest reboot included. `VmOptions::exec_from_share`
/// reproduces it.
const AGENT_LAUNCHER: &str = "#!/bin/busybox sh
/bin/busybox grep -q ' /mnt tmpfs ' /proc/mounts ||
  /bin/busybox mount -t tmpfs -o size=64m,mode=0700 tmpfs /mnt || exit 1
/bin/busybox cp /arcbox/bin/arcbox-agent.image /mnt/arcbox-agent.new || exit 1
/bin/busybox mv -f /mnt/arcbox-agent.new /mnt/arcbox-agent || exit 1
exec /mnt/arcbox-agent \"$@\"
";

const GVPROXY_SOCKET: &str = "gvproxy-qemu.sock";

/// A helper process: killed and reaped when dropped, so a boot that fails
/// after starting it leaves nothing running.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts gvproxy (gvisor-tap-vsock) as the NIC's peer: DHCP hands the
/// guest 192.168.127.2 for the MAC below and NATs it out through the host.
fn spawn_gvproxy(bin: &Path, log_dir: &Path) -> Result<Reaped> {
    let socket = log_dir.join(GVPROXY_SOCKET);
    let _ = std::fs::remove_file(&socket);
    let child = Command::new(bin)
        .arg("-listen-qemu")
        .arg(format!("unix://{}", socket.display()))
        .args(["-ssh-port", "-1", "-log-file"])
        .arg(log_dir.join("gvproxy.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Reaped)
        .context("spawn gvproxy")?;
    for _ in 0..100 {
        if socket.exists() {
            return Ok(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    bail!("gvproxy did not create {}", socket.display())
}

/// A socketpair whose second end survives `exec` into QEMU.
fn chardev_pair() -> Result<(UnixStream, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair writes two descriptors into `fds`.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fresh descriptors, sole owner of each.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: F_SETFD on an owned descriptor; only ours is close-on-exec.
    unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok((UnixStream::from(ours), theirs))
}
