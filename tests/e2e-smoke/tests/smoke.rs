//! ArcBox E2E smoke tests.
//!
//! These tests exercise the full ArcBox stack: helper, daemon, VM, guest
//! agent, and Docker API. They require pre-built and signed binaries.
//!
//! # Prerequisites
//!
//! 1. Build and sign the daemon: `make sign-daemon`
//! 2. Build the helper: `make build-helper`
//! 3. Ensure `sudo` is available (helper runs as root)
//! 4. Ensure port 5553 is free (no desktop daemon running)
//!
//! # Running
//!
//! ```bash
//! cargo test -p arcbox-e2e-smoke --test smoke -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is **mandatory**: all tests share a single daemon
//! instance. Parallel execution will cause port and socket conflicts.
//!
//! The harness automatically manages the daemon and helper lifecycle.
//! Stale processes from previous runs are killed during setup.

#[cfg(target_os = "macos")]
mod tests {
    use e2e::TestEnvironment;

    /// Basic container run: `docker run --rm alpine echo hello`
    ///
    /// Validates the full stack: VM boot, agent connectivity, Docker API
    /// proxy, image pull, container creation, execution, and cleanup.
    #[tokio::test]
    #[ignore = "requires full ArcBox environment"]
    async fn container_run_and_exit() {
        let env = TestEnvironment::get_or_start().await;

        let output = env
            .docker(&["run", "--rm", "alpine", "echo", "hello-arcbox"])
            .output()
            .await
            .expect("failed to execute docker CLI");

        assert!(
            output.status.success(),
            "docker run failed (exit={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello-arcbox"),
            "expected 'hello-arcbox' in stdout, got: {stdout}",
        );
    }

    /// Verify `docker ps` works against the ArcBox daemon.
    ///
    /// Lighter-weight than a full container run — only requires the
    /// Docker API proxy to be functional.
    #[tokio::test]
    #[ignore = "requires full ArcBox environment"]
    async fn docker_ps() {
        let env = TestEnvironment::get_or_start().await;

        let output = env
            .docker(&["ps", "--format", "{{.ID}}"])
            .output()
            .await
            .expect("failed to execute docker CLI");

        assert!(
            output.status.success(),
            "docker ps failed (exit={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Verify `docker info` returns valid ArcBox server info.
    ///
    /// This checks that the Docker API proxy correctly returns system
    /// information from the guest dockerd.
    #[tokio::test]
    #[ignore = "requires full ArcBox environment"]
    async fn docker_info() {
        let env = TestEnvironment::get_or_start().await;

        let output = env
            .docker(&["info", "--format", "{{.ServerVersion}}"])
            .output()
            .await
            .expect("failed to execute docker CLI");

        assert!(
            output.status.success(),
            "docker info failed (exit={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = stdout.trim();
        assert!(
            !version.is_empty(),
            "docker info returned empty server version",
        );
    }
}
