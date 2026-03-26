//! arcbox-helper — privileged helper daemon for host mutations.
//!
//! Runs as a root launchd daemon with socket activation. launchd creates the
//! socket at `/var/run/arcbox-helper.sock` and starts this process on-demand
//! when the ArcBox daemon connects.

mod server;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut idle_exit = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                eprintln!(
                    "arcbox-helper {}\nPrivileged helper daemon for host mutations (routes, DNS, sockets)\n\nOptions:\n  --idle-exit  Exit after {}s with no active connections",
                    env!("CARGO_PKG_VERSION"),
                    server::IDLE_TIMEOUT.as_secs(),
                );
                return Ok(());
            }
            "--version" | "-V" => {
                eprintln!("arcbox-helper {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--idle-exit" => idle_exit = true,
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("run with --help for usage");
                std::process::exit(1);
            }
        }
    }

    // Helper runs as root — write logs to /var/log/arcbox/helper.log.
    let log_guard = arcbox_logging::init(arcbox_logging::LogConfig {
        log_dir: std::path::PathBuf::from(arcbox_constants::paths::privileged_log::HELPER_LOG_DIR),
        file_name: arcbox_constants::paths::privileged_log::HELPER_LOG.to_string(),
        default_filter: "arcbox_helper=info".to_string(),
        foreground: true, // Also log to stderr (captured by launchd).
        ..arcbox_logging::LogConfig::default()
    });

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server::run(idle_exit));

    log_guard.flush();
    result
}
