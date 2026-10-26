# Native TCP ownership through bridge handoffs

The TCP bridge carries `NativeTcp` from egress resolution through handshake, promotion, uploads, and cloned readers. Descriptor conversion no longer discards the owner lease. Inbound and promotion APIs accept managed or unmanaged sockets through `Into<NativeTcp<_>>`.

The Tokio frame sink retains the managed socket. The VMM inline adapter cannot retain that lease and rejects the clone; the primary socket remains on the bridge's polled path. This change preserves ownership. Bridge and reader cancellation remain a separate integration step.

The dedicated test Mac `xuan-work-mbp` passed 155 splicetcp library tests with all features, with no failures or ignored tests. The new cases cover passive handshake and cloned-reader retention, upload after rejected handoff, and inbound handshake teardown. Existing tests cover native conversion, close-before-lease-release ordering, and Tokio reader behavior.

Formatting and strict splicetcp Clippy passed. All-target checks passed for splicetcp, arcbox-proxy, arcbox-net, and arcbox-vmm. Strict arcbox-vmm Clippy also passed. These commands ran through devenv with Rust 1.96.0 and used a fresh target directory for the candidate source.

The source manifest remained unchanged during validation. Its SHA-256 is `2eb6071d01213eb261889c8827d94244ade2ff04a69c903e9d2a156db78a1925`. Results are in `/private/tmp/arctap-native-ownership-results-oct22-v2` and `/private/tmp/arctap-native-ownership-vmm-clippy-oct22-v2`. The source bundle is `/private/tmp/arctap-native-ownership-oct22-v2`. The runner records commands, logs, discovered tests, process cleanup, and original native bindings. DNS and interface snapshots were unchanged during the main run.

No VM, TUN, or native App/NE acceptance ran in this batch. Those checks still require recovery of the test Mac's existing native environment. No throughput claim is made for the ownership change.
