//! `abctl logs` — view daemon and component log files.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

/// Arguments for the logs command.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Component to show logs for.
    #[arg(default_value = "daemon")]
    pub component: LogComponent,

    /// Follow log output (like tail -f).
    #[arg(long, short = 'f')]
    pub follow: bool,

    /// Number of lines to show from end of file.
    #[arg(long, short = 'n', default_value = "100")]
    pub lines: usize,

    /// Data directory for ArcBox.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// Log components.
#[derive(Debug, Clone, ValueEnum)]
pub enum LogComponent {
    /// Daemon log (JSON).
    Daemon,
    /// Privileged helper log (JSON, requires root).
    Helper,
    /// Guest agent log (plain text, via VirtioFS).
    Agent,
    /// Guest dockerd log (plain text, via VirtioFS).
    Dockerd,
    /// Guest containerd log (plain text, via VirtioFS).
    Containerd,
}

/// Execute the logs command.
pub async fn execute(args: LogsArgs) -> Result<()> {
    let log_path = resolve_log_path(&args)?;

    if !log_path.exists() {
        bail!(
            "Log file not found: {}\nIs the {} running?",
            log_path.display(),
            args.component.label()
        );
    }

    if args.follow {
        tail_follow(&log_path, args.lines).await
    } else {
        tail_lines(&log_path, args.lines)
    }
}

impl LogComponent {
    fn label(&self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Helper => "helper",
            Self::Agent => "agent",
            Self::Dockerd => "dockerd",
            Self::Containerd => "containerd",
        }
    }
}

fn resolve_log_path(args: &LogsArgs) -> Result<PathBuf> {
    match args.component {
        LogComponent::Helper => {
            // Helper logs are in /var/log/arcbox/ (root-owned).
            Ok(PathBuf::from(arcbox_constants::paths::privileged_log::HELPER_LOG_DIR)
                .join(arcbox_constants::paths::privileged_log::HELPER_LOG))
        }
        _ => {
            let data_dir = resolve_data_dir(args.data_dir.as_ref());
            let log_dir = data_dir.join(arcbox_constants::paths::host::LOG);
            let file_name = match args.component {
                LogComponent::Daemon => arcbox_constants::paths::host::DAEMON_LOG,
                LogComponent::Agent => arcbox_constants::paths::host::AGENT_LOG,
                LogComponent::Dockerd => "dockerd.log",
                LogComponent::Containerd => "containerd.log",
                LogComponent::Helper => unreachable!(),
            };
            Ok(log_dir.join(file_name))
        }
    }
}

/// Print the last `n` lines of a file.
fn tail_lines(path: &Path, n: usize) -> Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}

/// Print the last `n` lines then follow new output.
async fn tail_follow(path: &Path, n: usize) -> Result<()> {
    // Print initial tail.
    tail_lines(path, n)?;

    // Seek to end and poll for new content.
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    file.seek(SeekFrom::End(0))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // No new data — poll again after a short sleep.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(_) => {
                print!("{line}");
            }
            Err(e) => {
                bail!("Error reading log file: {e}");
            }
        }
    }
}

fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
    data_dir.cloned().unwrap_or_else(|| {
        dirs::home_dir().map_or_else(
            || PathBuf::from("/var/lib/arcbox"),
            |home| home.join(".arcbox"),
        )
    })
}
