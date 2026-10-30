//! The driver contract against a real Virtualization.framework VM.
//!
//! Every check the port's `driver_contract!` runs against the fake runs here
//! against `VzDriver`, one `#[ignore]`d test each. The list below mirrors
//! `driver_contract!`; a check added to the port is added here. Checks for
//! capabilities VZ does not claim (checkpoint, adopt, prepare, staging) skip
//! themselves.
//!
//! ## Prerequisites
//!
//! | Variable        | Description                                              |
//! |-----------------|----------------------------------------------------------|
//! | `VZ_KERNEL`     | Path to an arm64 Linux kernel image                      |
//! | `VZ_INITRD`     | Optional initrd                                          |
//! | `VZ_CMDLINE`    | Kernel command line; default `console=hvc0`              |
//! | `VZ_DIAL_PORT`  | Optional guest vsock port that accepts once booted       |
//!
//! Tests without `VZ_KERNEL` return early. The guest only has to print on
//! `hvc0` and keep running until the test stops it. The test binary needs
//! the `com.apple.security.virtualization` entitlement.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p arcbox-vz-driver --test contract --no-run
//! codesign -f -s - --entitlements bundle/arcbox.dev.entitlements target/debug/deps/contract-<hash>
//! VZ_KERNEL=... target/debug/deps/contract-<hash> --include-ignored --test-threads=1
//! ```

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arcbox_vm_driver::testkit::ContractHarness;
use arcbox_vm_driver::{
    BootSpec, ConsoleSpec, IsolationSpec, MacAddr, NicAttachment, NicSpec, VmDriver, VmHandle,
    VmId, VmSpec, VsockSpec,
};
use arcbox_vz_driver::VzDriver;
use async_trait::async_trait;

/// How long `ready` waits for the guest's first console output.
const READY_DEADLINE: Duration = Duration::from_secs(60);

/// `VzDriver` over temporary directories.
struct VzHarness {
    driver: Arc<VzDriver>,
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    cmdline: String,
    dial_port: Option<u32>,
    root: tempfile::TempDir,
    dirs: AtomicUsize,
}

impl VzHarness {
    fn from_env() -> Option<Self> {
        Some(Self {
            driver: Arc::new(VzDriver::new(None, None)),
            kernel: std::env::var_os("VZ_KERNEL")?.into(),
            initrd: std::env::var_os("VZ_INITRD").map(Into::into),
            cmdline: std::env::var("VZ_CMDLINE").unwrap_or_else(|_| "console=hvc0".to_owned()),
            dial_port: std::env::var("VZ_DIAL_PORT")
                .ok()
                .map(|port| port.parse().expect("VZ_DIAL_PORT is a port number")),
            root: tempfile::tempdir().expect("tempdir"),
            dirs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ContractHarness for VzHarness {
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
                cmdline: self.cmdline.clone(),
                initrd: self.initrd.clone(),
            },
            disks: vec![],
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
        let dir = self.root.path().join(format!("vm-{n}"));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        dir
    }

    fn dial_port(&self) -> Option<u32> {
        self.dial_port
    }

    fn guest_dial_out_port(&self) -> Option<u32> {
        None
    }

    async fn ready(&self, handle: &dyn VmHandle) {
        let console = handle.console().expect("the spec asks for a console");
        let deadline = tokio::time::Instant::now() + READY_DEADLINE;
        loop {
            if !console.read_output(4096).await.expect("console").is_empty() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no console output within {READY_DEADLINE:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

macro_rules! contract {
    ($($check:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread")]
            #[ignore = "needs VZ_KERNEL and the virtualization entitlement"]
            async fn $check() {
                let Some(harness) = VzHarness::from_env() else {
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
