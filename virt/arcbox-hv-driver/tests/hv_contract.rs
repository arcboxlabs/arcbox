//! The driver contract, and the checks the host sandboxes plan asks of
//! this driver, against real `arcbox-hv-vmm` processes.
//!
//! The guest is the HV checkpoint spike's Linux initramfs
//! (`virt/arcbox-vmm/examples/checkpoint_linux_spike/guest/`): it hammers
//! `/dev/vda`, serves a vsock echo on port 5000 and prints one `tick` line
//! a second on hvc0, which the console pipe carries here.
//!
//! | Variable     | Description                                         |
//! |--------------|-----------------------------------------------------|
//! | `HV_KERNEL`  | arm64 Linux kernel, e.g. `~/.arcbox/boot/<v>/kernel` |
//! | `HV_INITRD`  | the spike guest's `initramfs.cpio.gz`               |
//!
//! Every test is `#[ignore]`d and returns early without both. The
//! harness signs `arcbox-hv-vmm` ad hoc with `bundle/arcbox.dev.entitlements`.
//!
//! ```bash
//! virt/arcbox-vmm/examples/checkpoint_linux_spike/guest/build.sh /tmp/hv/initramfs.cpio.gz
//! HV_KERNEL=~/.arcbox/boot/0.8.12/kernel HV_INITRD=/tmp/hv/initramfs.cpio.gz \
//!   cargo test -p arcbox-hv-driver --test hv_contract -- --ignored
//! ```

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use arcbox_hv_driver::HvDriver;
use arcbox_vm_driver::testkit::ContractHarness;
use arcbox_vm_driver::{
    AfterCheckpoint, BootSpec, CacheMode, CheckpointKind, CheckpointOptions, ConsoleSpec, DiskSpec,
    ForwardProtocol, ForwardRule, IsolationSpec, MacAddr, NicAttachment, NicSpec, RestoreSpec,
    ShutdownMode, VmDriver, VmHandle, VmId, VmSpec, VmState, VsockSpec,
};
use async_trait::async_trait;

/// How long a guest gets to print a tick.
const TICK_DEADLINE: Duration = Duration::from_secs(60);
/// The spike guest's vsock echo port.
const ECHO_PORT: u32 = 5000;

struct HvHarness {
    driver: Arc<HvDriver>,
    kernel: PathBuf,
    initrd: PathBuf,
    root: tempfile::TempDir,
    dirs: AtomicUsize,
}

impl HvHarness {
    fn from_env() -> Option<Self> {
        let kernel = std::env::var_os("HV_KERNEL")?.into();
        let initrd = std::env::var_os("HV_INITRD")?.into();
        Some(Self {
            driver: Arc::new(HvDriver::new(signed_vmm(), false)),
            kernel,
            initrd,
            // Short: the control socket's path must fit in `sun_path`.
            root: tempfile::Builder::new()
                .prefix("hvc")
                .tempdir_in("/tmp")
                .expect("tempdir"),
            dirs: AtomicUsize::new(0),
        })
    }

    /// A 64 MiB disk of its own for `id`.
    fn disk(&self, id: &VmId) -> PathBuf {
        let path = self.root.path().join(format!("{id}.img"));
        if !path.exists() {
            let file = std::fs::File::create(&path).expect("disk");
            file.set_len(64 << 20).expect("disk size");
        }
        path
    }
}

/// The VMM binary, signed with the hypervisor entitlement unless a build
/// or an earlier run already did.
fn signed_vmm() -> PathBuf {
    static SIGN: Once = Once::new();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_arcbox-hv-vmm"));
    SIGN.call_once(|| {
        let shown = std::process::Command::new("codesign")
            .args(["-d", "--entitlements", "-"])
            .arg(&binary)
            .output()
            .expect("codesign -d");
        if String::from_utf8_lossy(&shown.stdout).contains("com.apple.security.hypervisor") {
            return;
        }
        let entitlements =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bundle/arcbox.dev.entitlements");
        let status = std::process::Command::new("codesign")
            .args(["--force", "--entitlements"])
            .arg(&entitlements)
            .args(["-s", "-"])
            .arg(&binary)
            .status()
            .expect("codesign");
        assert!(status.success(), "codesign {}", binary.display());
    });
    binary
}

#[async_trait]
impl ContractHarness for HvHarness {
    fn driver(&self) -> Arc<dyn VmDriver> {
        self.driver.clone()
    }

    fn spec(&self, id: &VmId) -> VmSpec {
        VmSpec {
            id: id.clone(),
            cpus: 2,
            memory_mib: 512,
            boot: BootSpec::Kernel {
                image: self.kernel.clone(),
                // spike_net brings up eth0 and the vsock echo; its host
                // ports answer nothing here, so its clients just retry.
                cmdline: "console=hvc0 spike_net=1,2,3".into(),
                initrd: Some(self.initrd.clone()),
            },
            disks: vec![DiskSpec {
                id: "rootfs".into(),
                path: self.disk(id),
                read_only: false,
                root: true,
                cache: CacheMode::Unsafe,
            }],
            nics: vec![NicSpec {
                id: "eth0".into(),
                mac: MacAddr::new([0x02, 0xab, 0x00, 0x00, 0x00, 0x01]),
                attachment: NicAttachment::UserNat,
            }],
            vsock: Some(VsockSpec { guest_cid: 3 }),
            shares: vec![],
            console: ConsoleSpec::Pipe,
            balloon: true,
            entropy: true,
            dirty_tracking: false,
            isolation: IsolationSpec::None,
            rosetta: false,
            nested_virt: false,
            debug_console: None,
        }
    }

    fn runtime_dir(&self) -> PathBuf {
        let n = self.dirs.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.path().join(format!("v{n}"));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        dir
    }

    fn dial_port(&self) -> Option<u32> {
        Some(ECHO_PORT)
    }

    fn guest_dial_out_port(&self) -> Option<u32> {
        None
    }

    async fn ready(&self, handle: &dyn VmHandle) {
        next_tick(handle).await;
    }
}

/// The guest's next `tick` line on the console.
async fn next_tick(handle: &dyn VmHandle) -> String {
    let console = handle.console().expect("the harness asks for a console");
    let deadline = tokio::time::Instant::now() + TICK_DEADLINE;
    let mut text = String::new();
    loop {
        let out = console.read_output(64 << 10).await.expect("console");
        text.push_str(&String::from_utf8_lossy(&out));
        if let Some(line) = text.lines().rfind(|l| l.starts_with("tick "))
            && text.ends_with('\n')
        {
            return line.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no tick within {TICK_DEADLINE:?}; console: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `field` of a tick line: `tick 3 ... ops 1 2 3 4 fails 0` → `ops` → 1.
fn tick_field(line: &str, field: &str) -> u64 {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == field).expect(field);
    words[at + 1].parse().expect("a number")
}

/// One round trip through the guest's vsock echo.
///
/// HV answers a dial before the guest has seen it, so a guest that resets
/// the connection reads as EOF here. Under host load a guest resets the
/// first connection or two, also seconds after a restore (seen with six
/// restores at once); a few retries 200 ms apart ride that out.
async fn echo(handle: &dyn VmHandle) {
    let mut failures = Vec::new();
    for _ in 0..5 {
        let conn = handle
            .vsock()
            .expect("vsock")
            .dial(ECHO_PORT)
            .await
            .expect("dial");
        let echoed = tokio::task::spawn_blocking(move || {
            let mut sock = UnixStream::from(conn.fd);
            sock.set_read_timeout(Some(Duration::from_secs(10)))?;
            sock.write_all(&0x5eed_u64.to_le_bytes())?;
            let mut back = [0u8; 8];
            sock.read_exact(&mut back)?;
            Ok::<_, std::io::Error>(u64::from_le_bytes(back))
        })
        .await
        .unwrap();
        match echoed {
            Ok(word) => {
                assert_eq!(word, 0x5eed);
                return;
            }
            Err(e) => failures.push(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("no vsock echo from {}: {failures:?}", handle.id());
}

fn id(name: &str) -> VmId {
    VmId::new(name).unwrap()
}

macro_rules! contract {
    ($($check:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread")]
            #[ignore = "needs HV_KERNEL, HV_INITRD and Hypervisor.framework"]
            async fn $check() {
                let Some(harness) = HvHarness::from_env() else {
                    return;
                };
                arcbox_vm_driver::testkit::contract::$check(&harness).await;
            }
        )*
    };
}

contract! {
    boot_reports_running_then_exits_on_kill,
    events_deliver_exit_exactly_once,
    capabilities_agree_with_accessors,
    checkpoint_resume_keeps_running,
    checkpoint_hold_quiesces,
    restore_yields_a_live_vm,
    detach_then_adopt_round_trip,
    vsock_dial_reaches_the_guest,
    foreign_checkpoint_is_refused,
    record_is_stable,
    prepare_then_boot_matches_boot,
    prepared_listener_is_live_before_boot,
    discard_kills_a_prepared_vm,
    restore_reattaches_disks,
    a_staged_spec_boots,
    a_staged_disk_can_be_taken_back_out,
    a_staged_checkpoint_restores,
    a_dead_vms_area_is_discarded,
}

/// Two VMs run at once, each in its own process, and both answer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs HV_KERNEL, HV_INITRD and Hypervisor.framework"]
async fn two_vms_run_side_by_side() {
    let Some(h) = HvHarness::from_env() else {
        return;
    };
    let a = h
        .driver
        .boot(h.spec(&id("pair-a")), &h.runtime_dir())
        .await
        .expect("boot a");
    let b = h
        .driver
        .boot(h.spec(&id("pair-b")), &h.runtime_dir())
        .await
        .expect("boot b");
    let (pa, pb) = (pid(&*a), pid(&*b));
    assert_ne!(pa, pb, "one process per VM");
    assert_ne!(pa, std::process::id());
    for vm in [&a, &b] {
        next_tick(&**vm).await;
        echo(&**vm).await;
    }
    // Both keep running while the other does.
    let (ta, tb) = (next_tick(&*a).await, next_tick(&*b).await);
    assert_eq!(tick_field(&ta, "fails"), 0, "{ta}");
    assert_eq!(tick_field(&tb, "fails"), 0, "{tb}");
    a.shutdown(ShutdownMode::Kill).await.expect("kill a");
    assert_eq!(b.state(), VmState::Running, "killing one VM left the other");
    echo(&*b).await;
    b.shutdown(ShutdownMode::Kill).await.expect("kill b");
}

/// A guest checkpointed mid-I/O resumes in a new process, on a clone of
/// its disk, and goes on where it stopped: ticks continue, every block
/// still matches what the guest wrote, vsock answers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs HV_KERNEL, HV_INITRD and Hypervisor.framework"]
async fn a_checkpoint_resumes_in_a_new_process() {
    let Some(h) = HvHarness::from_env() else {
        return;
    };
    let source = h
        .driver
        .boot(h.spec(&id("ckpt-src")), &h.runtime_dir())
        .await
        .expect("boot");
    let mut before = next_tick(&*source).await;
    while tick_field(&before, "tick") < 3 {
        before = next_tick(&*source).await;
    }
    let image = source
        .checkpoint()
        .expect("checkpoint")
        .checkpoint(
            &h.runtime_dir().join("image"),
            CheckpointOptions {
                after: AfterCheckpoint::HoldQuiesced,
                kind: CheckpointKind::Full,
            },
        )
        .await
        .expect("checkpoint");
    assert_eq!(source.state(), VmState::Quiesced);
    let source_pid = pid(&*source);
    source.shutdown(ShutdownMode::Kill).await.expect("kill");

    let dir = h.runtime_dir();
    let disk = dir.join("rootfs.img");
    clone(&image.dir.join("disk0.img"), &disk);
    let template = h.spec(&id("ckpt-dst"));
    let restore = RestoreSpec {
        id: id("ckpt-dst"),
        nics: template.nics,
        disks: vec![DiskSpec {
            path: disk,
            ..template.disks[0].clone()
        }],
        isolation: IsolationSpec::None,
    };
    let restored = h
        .driver
        .restore(&image, restore, &dir)
        .await
        .expect("restore");
    assert_ne!(pid(&*restored), source_pid);
    let mut after = next_tick(&*restored).await;
    while tick_field(&after, "tick") < tick_field(&before, "tick") + 3 {
        after = next_tick(&*restored).await;
    }
    eprintln!("source: {before}\nrestored: {after}");
    assert_eq!(tick_field(&after, "fails"), 0, "{after}");
    assert!(
        tick_field(&after, "ops") > tick_field(&before, "ops"),
        "{before} → {after}"
    );
    echo(&*restored).await;
    restored.shutdown(ShutdownMode::Kill).await.expect("kill");
}

/// A checkpoint that resumes keeps the guest going in the same process,
/// and a port forward added before it still listens after it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs HV_KERNEL, HV_INITRD and Hypervisor.framework"]
async fn a_resumed_checkpoint_keeps_the_guest_and_its_forwards() {
    let Some(h) = HvHarness::from_env() else {
        return;
    };
    let vm = h
        .driver
        .boot(h.spec(&id("resume")), &h.runtime_dir())
        .await
        .expect("boot");
    let before = next_tick(&*vm).await;
    let mut forward = vm.take_port_forward().expect("port forwarding");
    let rule = ForwardRule {
        protocol: ForwardProtocol::Tcp,
        host_ip: Ipv4Addr::LOCALHOST,
        host_port: 0,
        guest_port: 80,
    };
    let port = forward.add(rule).await.expect("forward");
    assert_ne!(port, 0);
    let pid_before = pid(&*vm);

    vm.checkpoint()
        .expect("checkpoint")
        .checkpoint(&h.runtime_dir().join("image"), CheckpointOptions::default())
        .await
        .expect("checkpoint");
    assert_eq!(vm.state(), VmState::Running);
    assert_eq!(pid(&*vm), pid_before);
    let mut after = next_tick(&*vm).await;
    while tick_field(&after, "tick") < tick_field(&before, "tick") + 2 {
        after = next_tick(&*vm).await;
    }
    assert_eq!(tick_field(&after, "fails"), 0, "{after}");
    echo(&*vm).await;
    std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .expect("the forward listens after the resume");

    forward.remove(rule).await;
    assert!(
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
        "the forward still listens after its removal"
    );
    vm.shutdown(ShutdownMode::Kill).await.expect("kill");
}

fn pid(handle: &dyn VmHandle) -> u32 {
    handle.record().process.expect("a process record").pid
}

fn clone(src: &Path, dst: &Path) {
    let status = std::process::Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .status()
        .expect("cp");
    assert!(status.success());
}
