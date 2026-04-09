//! Test harness that manages the ArcBox daemon and helper lifecycle.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

/// Shared test environment, initialized once across all tests.
static ENV: OnceCell<TestEnvironment> = OnceCell::const_new();

/// Full ArcBox environment for E2E tests.
///
/// Owns the daemon and helper child processes and tears them down on drop.
pub struct TestEnvironment {
    daemon_process: Option<Child>,
    helper_process: Option<Child>,
    docker_host: String,
    /// Captured daemon stdout/stderr log path for debugging.
    log_file: PathBuf,
    /// Project root (repository root) for locating binaries.
    #[allow(dead_code)]
    project_root: PathBuf,
}

impl TestEnvironment {
    /// Get or start the shared test environment.
    ///
    /// The first caller initializes the environment; subsequent callers
    /// reuse it. This is safe because tests run with `--test-threads=1`.
    pub async fn get_or_start() -> &'static Self {
        ENV.get_or_init(|| async {
            Self::start()
                .await
                .expect("Failed to start ArcBox test environment")
        })
        .await
    }

    /// Start a fresh ArcBox environment from scratch.
    async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let project_root = find_project_root()?;
        let home = dirs::home_dir().ok_or("cannot determine home directory")?;
        let arcbox_run = home.join(".arcbox/run");
        let docker_socket = arcbox_run.join("docker.sock");
        let docker_host = format!("unix://{}", docker_socket.display());

        let log_file = std::env::temp_dir().join(format!("arcbox-e2e-{}.log", std::process::id()));

        eprintln!("[e2e] project root: {}", project_root.display());
        eprintln!("[e2e] daemon log:   {}", log_file.display());

        // Step 1: Kill stale processes.
        kill_stale_processes();

        // Step 2: Clean socket files.
        clean_sockets(&arcbox_run);

        // Step 3: Verify port 5553 is free.
        verify_port_free(5553)?;

        // Step 4: Start helper (requires sudo).
        let helper_bin = project_root.join("target/debug/arcbox-helper");
        if !helper_bin.exists() {
            return Err(format!(
                "arcbox-helper not found at {}. Run `make build-helper` first.",
                helper_bin.display()
            )
            .into());
        }
        let helper_process = start_helper(&helper_bin)?;
        eprintln!("[e2e] helper started (pid={})", helper_process.id());

        // Give the helper a moment to bind its socket.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Step 5: Start signed daemon.
        let daemon_bin = project_root.join("target/debug/arcbox-daemon");
        if !daemon_bin.exists() {
            return Err(format!(
                "arcbox-daemon not found at {}. Run `make sign-daemon` first.",
                daemon_bin.display()
            )
            .into());
        }

        let log_file_stderr = std::fs::File::create(&log_file)?;

        let daemon_process = Command::new(&daemon_bin)
            .env("ARCBOX_HELPER_SOCKET", "/tmp/arcbox-helper.sock")
            .stdout(Stdio::piped())
            .stderr(log_file_stderr)
            .spawn()
            .map_err(|e| format!("failed to start arcbox-daemon: {e}"))?;

        eprintln!("[e2e] daemon started (pid={})", daemon_process.id());

        let mut env = Self {
            daemon_process: Some(daemon_process),
            helper_process: Some(helper_process),
            docker_host,
            log_file,
            project_root,
        };

        // Step 6: Wait for "ArcBox daemon started" on stdout.
        env.wait_for_daemon_ready(Duration::from_secs(120))?;

        eprintln!("[e2e] daemon is ready");
        Ok(env)
    }

    /// Block until the daemon prints "ArcBox daemon started" on stdout,
    /// or return an error if the timeout expires or the process exits.
    fn wait_for_daemon_ready(
        &mut self,
        timeout: Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let daemon = self
            .daemon_process
            .as_mut()
            .ok_or("daemon process not started")?;

        let stdout = daemon.stdout.take().ok_or("daemon stdout not captured")?;

        let reader = std::io::BufReader::new(stdout);
        let start = Instant::now();

        // Spawn a thread to read stdout line-by-line so we can enforce
        // the timeout from this thread. Child::stdout is blocking I/O,
        // so we cannot use async reads directly.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let log_path = self.log_file.clone();

        std::thread::spawn(move || {
            // Also append stdout lines to the log file for post-mortem.
            let mut log_out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .ok();

            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if let Some(ref mut f) = log_out {
                            let _ =
                                std::io::Write::write_all(f, format!("[stdout] {l}\n").as_bytes());
                        }
                        let is_ready = l.contains("ArcBox daemon started");
                        let _ = tx.send(l);
                        if is_ready {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        loop {
            if start.elapsed() > timeout {
                eprintln!(
                    "[e2e] TIMEOUT waiting for daemon. Logs:\n{}",
                    self.daemon_logs()
                );
                return Err("daemon did not become ready within timeout".into());
            }

            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(line) => {
                    eprintln!("[e2e] daemon: {line}");
                    if line.contains("ArcBox daemon started") {
                        return Ok(());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Check if daemon exited early.
                    if let Some(ref mut d) = self.daemon_process {
                        if let Ok(Some(status)) = d.try_wait() {
                            eprintln!(
                                "[e2e] daemon exited with {status}. Logs:\n{}",
                                self.daemon_logs()
                            );
                            return Err(format!(
                                "daemon exited with {status} before becoming ready"
                            )
                            .into());
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    eprintln!("[e2e] daemon stdout closed. Logs:\n{}", self.daemon_logs());
                    return Err("daemon stdout closed before ready marker".into());
                }
            }
        }
    }

    /// Build a `tokio::process::Command` for async Docker CLI calls.
    ///
    /// Sets `DOCKER_HOST` to point at the ArcBox daemon's Docker socket.
    pub fn docker(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.env("DOCKER_HOST", &self.docker_host);
        cmd.args(args);
        cmd
    }

    /// Read the combined daemon log file.
    pub fn daemon_logs(&self) -> String {
        std::fs::read_to_string(&self.log_file).unwrap_or_else(|_| "<no log file>".to_string())
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        eprintln!("[e2e] tearing down test environment...");

        // Clean up leftover containers.
        if let Ok(output) = std::process::Command::new("docker")
            .env("DOCKER_HOST", &self.docker_host)
            .args(["ps", "-aq"])
            .output()
        {
            let ids = String::from_utf8_lossy(&output.stdout);
            let ids: Vec<&str> = ids.split_whitespace().collect();
            if !ids.is_empty() {
                eprintln!("[e2e] removing {} leftover container(s)", ids.len());
                let mut rm_args = vec!["rm", "-f"];
                rm_args.extend(ids);
                let _ = std::process::Command::new("docker")
                    .env("DOCKER_HOST", &self.docker_host)
                    .args(&rm_args)
                    .output();
            }
        }

        // SIGTERM daemon, wait up to 5s, then SIGKILL.
        if let Some(ref mut daemon) = self.daemon_process {
            let pid = daemon.id();
            eprintln!("[e2e] sending SIGTERM to daemon (pid={pid})");
            send_signal(pid, libc::SIGTERM);

            let start = Instant::now();
            loop {
                match daemon.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!("[e2e] daemon exited with {status}");
                        break;
                    }
                    Ok(None) if start.elapsed() > Duration::from_secs(5) => {
                        eprintln!("[e2e] daemon did not exit in 5s, sending SIGKILL");
                        let _ = daemon.kill();
                        let _ = daemon.wait();
                        break;
                    }
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        eprintln!("[e2e] error waiting for daemon: {e}");
                        break;
                    }
                }
            }
        }

        // SIGTERM helper. It was started via `sudo`, so SIGTERM to the
        // sudo process forwards the signal to the actual helper.
        if let Some(ref mut helper) = self.helper_process {
            let pid = helper.id();
            eprintln!("[e2e] sending SIGTERM to helper (pid={pid})");
            send_signal(pid, libc::SIGTERM);

            let start = Instant::now();
            loop {
                match helper.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if start.elapsed() > Duration::from_secs(3) => {
                        let _ = helper.kill();
                        let _ = helper.wait();
                        break;
                    }
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        }

        // Clean up sockets.
        let _ = std::fs::remove_file("/tmp/arcbox-helper.sock");
        if let Some(home) = dirs::home_dir() {
            clean_sockets(&home.join(".arcbox/run"));
        }

        eprintln!("[e2e] teardown complete");
    }
}

/// Locate the project root by looking for the workspace Cargo.toml.
fn find_project_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // CARGO_MANIFEST_DIR points to the crate's directory during `cargo test`.
    // Walk up from there to find the workspace root.
    if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let mut path = PathBuf::from(dir);
        loop {
            let cargo_toml = path.join("Cargo.toml");
            if cargo_toml.exists() {
                let contents = std::fs::read_to_string(&cargo_toml)?;
                if contents.contains("[workspace]") {
                    return Ok(path);
                }
            }
            if !path.pop() {
                break;
            }
        }
    }

    // Fallback: walk up from current dir.
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("app").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("cannot find project root".into());
        }
    }
}

/// Kill stale arcbox-daemon and arcbox-helper processes.
fn kill_stale_processes() {
    eprintln!("[e2e] killing stale arcbox processes...");

    let _ = Command::new("pkill")
        .args(["-9", "-f", "arcbox-daemon"])
        .output();

    // Desktop daemon can steal port 5553.
    let _ = Command::new("pkill")
        .args(["-9", "-f", "com.arcboxlabs.desktop.daemon"])
        .output();

    // Helper runs as root.
    let _ = Command::new("sudo")
        .args(["pkill", "-9", "-f", "arcbox-helper"])
        .output();

    // Wait for processes to die and release ports/sockets.
    std::thread::sleep(Duration::from_secs(2));
}

/// Remove `.sock` files from a directory.
fn clean_sockets(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sock") {
                eprintln!("[e2e] removing socket: {}", path.display());
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let _ = std::fs::remove_file("/tmp/arcbox-helper.sock");
}

/// Check that a TCP port is not in use.
fn verify_port_free(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("lsof")
        .args(["-i", &format!(":{port}"), "-t"])
        .output()?;

    let pids = String::from_utf8_lossy(&output.stdout);
    let pids = pids.trim();
    if !pids.is_empty() {
        return Err(format!(
            "port {port} is in use by PID(s): {pids}. \
             Kill them before running E2E tests."
        )
        .into());
    }
    Ok(())
}

/// Start the privileged helper under sudo.
fn start_helper(helper_bin: &std::path::Path) -> Result<Child, Box<dyn std::error::Error>> {
    let child = Command::new("sudo")
        .arg("-E")
        .env("ARCBOX_HELPER_SOCKET", "/tmp/arcbox-helper.sock")
        .arg(helper_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start arcbox-helper via sudo: {e}"))?;
    Ok(child)
}

/// Send a signal to a child process by pid.
///
/// `Child::id()` returns `u32` but `libc::kill` takes `pid_t` (`i32`).
/// PIDs are always positive and fit in i32 on all supported platforms.
#[allow(clippy::cast_possible_wrap)]
fn send_signal(pid: u32, signal: libc::c_int) {
    // SAFETY: pid comes from a Child we own, and signal is a valid constant.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}
