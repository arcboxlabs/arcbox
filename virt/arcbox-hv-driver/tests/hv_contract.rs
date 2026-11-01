//! The driver contract against real `arcbox-hv-vmm` processes.
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use arcbox_hv_driver::HvDriver;
use arcbox_vm_driver::testkit::ContractHarness;
use arcbox_vm_driver::{
    BootSpec, CacheMode, ConsoleSpec, DiskSpec, IsolationSpec, MacAddr, NicAttachment, NicSpec,
    VmDriver, VmHandle, VmId, VmSpec, VsockSpec,
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
