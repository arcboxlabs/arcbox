use super::{Confinement, config_in, finalize_startup_cleanup, manager, no_tap, wait_for_event};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, resume_unwind};

use arcbox_computer_runtime::snapshot::SnapshotGeometry;
use arcbox_computer_runtime::{
    ComputerSpec, ComputerState, OutputChunk, RestoreComputerSpec, SandboxManager, SnapshotCatalog,
};
use futures::FutureExt;

/// A restored VM must retain its source paths and geometry for another checkpoint.
#[tokio::test]
#[ignore = "requires FC_BINARY/FC_JAILER/FC_KERNEL/FC_ROOTFS, root, KVM, and vm-agent in rootfs"]
async fn jailed_checkpoint_chain_preserves_source_and_geometry() {
    assert!(super::common::is_root(), "this jailed test requires root");
    let dir = tempfile::Builder::new()
        .prefix("arcbox-checkpoint-chain-")
        .tempdir()
        .unwrap();
    let cfg = config_in(dir.path().to_str().unwrap(), Confinement::Jailed)
        .expect("set FC_BINARY, FC_JAILER, FC_KERNEL, and FC_ROOTFS");
    // Explicit sources differ from defaults, so recording defaults cannot pass.
    let kernel = dir.path().join("source-kernel");
    let rootfs = dir.path().join("source-rootfs.ext4");
    std::fs::copy(&cfg.runtime.defaults.kernel, &kernel).unwrap();
    std::fs::copy(&cfg.runtime.defaults.rootfs, &rootfs).unwrap();
    let kernel = kernel.to_str().unwrap().to_owned();
    let rootfs = rootfs.to_str().unwrap().to_owned();
    let geometry = SnapshotGeometry {
        vcpus: 3,
        memory_mib: 768,
    };
    let catalog = SnapshotCatalog::new(dir.path().to_str().unwrap());
    let mgr = manager(cfg);
    // Async cleanup must finish before unwinding drops the CoW backing files.
    let outcome = AssertUnwindSafe(async {
        finalize_startup_cleanup(&mgr).await;
        let mut events = mgr.subscribe_events();
        let (mut id, ip) = mgr
            .create_sandbox(ComputerSpec {
                id: Some("origin".into()),
                kernel: kernel.clone(),
                rootfs: rootfs.clone(),
                vcpus: geometry.vcpus,
                memory_mib: geometry.memory_mib,
                ..no_tap()
            })
            .await
            .unwrap();
        assert_eq!(ip, "");
        assert!(wait_for_event(&mut events, &id, "ready").await);

        // /run is tmpfs: the marker must survive in checkpointed memory.
        assert_eq!(
            run_script(&mgr, &id, "printf generation-0 > /run/checkpoint-marker").await,
            ""
        );
        let mut snapshots = Vec::new();
        for generation in 1..=2 {
            let checkpoint = mgr
                .checkpoint_sandbox(&id, format!("generation-{generation}"), HashMap::new())
                .await
                .unwrap();
            let meta = catalog.find_by_id(&checkpoint.snapshot_id).unwrap();
            assert_eq!(meta.geometry, Some(geometry));
            assert_eq!(meta.kernel_path.as_deref(), Some(kernel.as_str()));
            assert_eq!(meta.rootfs_path.as_deref(), Some(rootfs.as_str()));
            eprintln!(
                "checkpoint generation={generation} source={} geometry={:?} kernel={} rootfs={}",
                meta.vm_id, meta.geometry, kernel, rootfs
            );

            // Remove the old jail before restore to catch references to staged assets.
            let pid = super::journaled_pid(dir.path(), &id);
            mgr.remove_sandbox(&id, true).await.unwrap();
            assert!(!super::firecracker_alive(pid));
            let (restored, ip) = mgr
                .restore_sandbox(RestoreComputerSpec {
                    id: Some(format!("generation-{generation}")),
                    snapshot_id: checkpoint.snapshot_id.clone(),
                    ..RestoreComputerSpec::default()
                })
                .await
                .unwrap();
            id = restored;
            assert_eq!(ip, "");
            let info = mgr.inspect_sandbox(&id).unwrap();
            assert_eq!(info.state, ComputerState::Ready);
            assert_eq!(info.vcpus, geometry.vcpus);
            assert_eq!(info.memory_mib, geometry.memory_mib);
            assert_eq!(
                run_script(
                    &mgr,
                    &id,
                    "read -r marker < /run/checkpoint-marker; printf '%s\\n' \"$marker\"; \
                 read -r cpus < /sys/devices/system/cpu/online; printf '%s\\n' \"$cpus\"",
                )
                .await,
                format!("generation-{}\n0-2\n", generation - 1)
            );
            assert_eq!(
                run_script(
                    &mgr,
                    &id,
                    &format!("printf generation-{generation} > /run/checkpoint-marker"),
                )
                .await,
                ""
            );
            snapshots.push(checkpoint.snapshot_id);
        }

        let pid = super::journaled_pid(dir.path(), &id);
        mgr.remove_sandbox(&id, true).await.unwrap();
        assert!(!super::firecracker_alive(pid));
        for snapshot in snapshots {
            mgr.delete_checkpoint(&snapshot).await.unwrap();
        }
        assert!(mgr.list_checkpoints(None).unwrap().is_empty());
    })
    .catch_unwind()
    .await;

    let cleanup_errors = cleanup(&mgr).await;
    if !cleanup_errors.is_empty() {
        let path = dir.keep();
        eprintln!(
            "checkpoint cleanup failed; retained {}: {}",
            path.display(),
            cleanup_errors.join("; ")
        );
    }
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
    assert!(cleanup_errors.is_empty(), "checkpoint cleanup failed");
}

async fn cleanup(mgr: &SandboxManager) -> Vec<String> {
    let mut errors = Vec::new();
    match mgr.list_sandboxes(None, &HashMap::new()) {
        Ok(sandboxes) => {
            for sandbox in sandboxes {
                if let Err(error) = mgr.remove_sandbox(&sandbox.id, true).await {
                    errors.push(format!("remove sandbox {}: {error}", sandbox.id));
                }
            }
        }
        Err(error) => errors.push(format!("list sandboxes: {error}")),
    }
    match mgr.list_checkpoints(None) {
        Ok(checkpoints) => {
            for checkpoint in checkpoints {
                if let Err(error) = mgr.delete_checkpoint(&checkpoint.id).await {
                    errors.push(format!("delete checkpoint {}: {error}", checkpoint.id));
                }
            }
        }
        Err(error) => errors.push(format!("list checkpoints: {error}")),
    }
    errors
}

async fn run_script(mgr: &SandboxManager, id: &String, script: &str) -> String {
    let mut output = mgr
        .run_in_sandbox(
            id,
            vec!["/bin/sh".into(), "-c".into(), script.into()],
            HashMap::new(),
            "/".into(),
            "root".into(),
            false,
            None,
            30,
        )
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    while let Some(chunk) = output.recv().await {
        match chunk.unwrap() {
            OutputChunk::Stdout(bytes) => stdout.extend(bytes),
            OutputChunk::Stderr(bytes) => stderr.extend(bytes),
            OutputChunk::Exit(status) => exit = Some(status.conventional_code()),
        }
    }
    assert_eq!(
        exit,
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    String::from_utf8(stdout).unwrap()
}
