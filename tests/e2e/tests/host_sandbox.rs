//! Host sandbox e2e: create, exec, pause, resume and remove a sandbox that
//! runs in its own VM on the Mac, on HV and (when the session is unlocked)
//! on VZ. Run:
//!
//! ```console
//! ARCBOX_E2E_IMAGE=public.ecr.aws/docker/library/alpine:latest \
//!     cargo test -p arcbox-e2e --test host_sandbox -- --ignored --nocapture
//! ```

use arcbox_e2e::host_sandbox::run;
use arcbox_e2e::sandbox::SandboxSmokeConfig;

#[test]
#[ignore = "requires boot assets, a signed daemon and arcbox-hv-vmm, and registry access"]
fn host_sandbox() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_test_writer()
        .init();

    run(SandboxSmokeConfig::from_env()).expect("host sandbox scenario failed");
}
