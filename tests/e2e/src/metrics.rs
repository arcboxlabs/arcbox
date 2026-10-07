//! Machine-readable per-run metrics.
//!
//! Every performance target is a hard number (cold boot <1.5 s, …), and
//! the easiest casualty of a correctness-fix campaign is a silent
//! performance regression. Each e2e run records its phase timings as
//! JSON: into the run's data dir, and — when `ARCBOX_E2E_METRICS_DIR`
//! is set (the `cargo xtask e2e` runner does this) — into the archive
//! directory as `<label>.metrics.json`, so passing runs leave numbers
//! behind too.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;

/// The boot inputs selected by the harness.
#[derive(Debug, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum BootAssets {
    /// The resolved version passed to the daemon, including a config override.
    Bundle {
        /// Boot bundle version.
        version: String,
    },
    /// Direct VMM probes can select files outside a versioned bundle.
    Files {
        /// Selected kernel path.
        kernel: PathBuf,
        /// Selected rootfs path.
        rootfs: PathBuf,
    },
}

/// Run inputs and checkout state. These fields do not identify the binaries' build revisions.
#[derive(Debug, Serialize)]
pub struct Provenance {
    /// Checkout inspected when the metrics record was created.
    pub checkout_root: PathBuf,
    /// Full commit at the inspected checkout's HEAD.
    pub checkout_commit: String,
    /// Whether tracked or untracked files differ from HEAD.
    pub checkout_dirty: bool,
    /// Host operating system, kernel release, and architecture from `uname -srm`.
    pub host: String,
    /// Process arguments with their original boundaries.
    pub argv: Vec<String>,
    /// Boot inputs supplied by the harness after resolution.
    pub boot_assets: BootAssets,
}

impl Provenance {
    fn capture(root: &Path, boot_assets: BootAssets, argv: Vec<String>) -> Result<Self> {
        Ok(Self {
            checkout_root: root.to_owned(),
            checkout_commit: command_stdout(
                Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["rev-parse", "HEAD"]),
            )?,
            checkout_dirty: !command_stdout(Command::new("git").arg("-C").arg(root).args([
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ]))?
            .is_empty(),
            host: command_stdout(Command::new("uname").arg("-srm"))?,
            argv,
            boot_assets,
        })
    }
}

fn command_stdout(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("capturing metrics provenance with {command:?}"))?;
    ensure!(
        output.status.success(),
        "metrics provenance command {command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)
        .context("metrics provenance command returned non-UTF-8 output")?
        .trim()
        .to_owned())
}

/// One timed phase of an e2e run.
#[derive(Debug, Serialize)]
pub struct Phase {
    pub name: String,
    pub seconds: f64,
}

/// Ordered measurements and summary statistics for one explicit unit.
#[derive(Debug, Serialize)]
pub struct Distribution {
    pub name: String,
    pub unit: String,
    pub count: usize,
    pub samples: Vec<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub max: Option<f64>,
}

/// Nearest-rank percentile over an unsorted sample set.
pub fn percentile(values: &[f64], quantile: f64) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (quantile * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted.get(rank - 1).copied()
}

/// Machine-readable record of one e2e run.
#[derive(Debug, Serialize)]
pub struct RunMetrics {
    /// Test target name (e.g. `boot_assets`, `hv_vmm`).
    pub test: String,
    /// System VM backend label, when the run pinned one.
    pub backend: Option<String>,
    /// Whether the run passed. Set by the caller before writing.
    pub passed: bool,
    /// Unix time the run started.
    pub unix_time: u64,
    /// Run label, matching the archived filename.
    pub label: String,
    /// Inputs and checkout state captured before the measured phases.
    pub provenance: Provenance,
    /// Timed phases, in execution order.
    pub phases: Vec<Phase>,
    /// Sample summaries retain their units and sample counts.
    pub distributions: Vec<Distribution>,
}

impl RunMetrics {
    /// Captures provenance. A failed probe returns an error instead of reporting a verified checkout.
    pub fn new(test: &str, backend: Option<&str>, boot_assets: BootAssets) -> Result<Self> {
        let argv = std::env::args_os()
            .map(|arg| {
                arg.into_string()
                    .map_err(|_| anyhow!("metrics argv contains a non-UTF-8 argument"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            test: test.to_owned(),
            backend: backend.map(str::to_owned),
            passed: false,
            unix_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
            label: std::env::var("ARCBOX_E2E_RUN_LABEL")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("{test}-{}", std::process::id())),
            provenance: Provenance::capture(&crate::repo_root(), boot_assets, argv)?,
            phases: Vec::new(),
            distributions: Vec::new(),
        })
    }

    /// Runs `f`, recording its wall-clock duration under `name` (also
    /// when it fails — a slow failure is still a data point).
    pub fn time<T, E>(&mut self, name: &str, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let started = Instant::now();
        let result = f();
        self.record(name, started.elapsed().as_secs_f64());
        result
    }

    /// Records an externally measured phase duration.
    pub fn record(&mut self, name: &str, seconds: f64) {
        self.phases.push(Phase {
            name: name.to_owned(),
            seconds,
        });
    }

    /// Records sample statistics. Empty sets have zero samples and no percentile estimates.
    pub fn record_distribution(&mut self, name: &str, unit: &str, samples: &[f64]) {
        self.distributions.push(Distribution {
            name: name.to_owned(),
            unit: unit.to_owned(),
            count: samples.len(),
            samples: samples.to_vec(),
            p50: percentile(samples, 0.50),
            p95: percentile(samples, 0.95),
            max: samples.iter().copied().reduce(f64::max),
        });
    }

    /// Writes `metrics.json` into `run_dir` (when given) and
    /// `$ARCBOX_E2E_METRICS_DIR/<label>.metrics.json` (when the variable
    /// is set). The label is captured at construction. Returns the written paths.
    pub fn write(&self, run_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
        let archive_dir = std::env::var_os("ARCBOX_E2E_METRICS_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        self.write_to(run_dir, archive_dir.as_deref())
    }

    fn write_to(&self, run_dir: Option<&Path>, archive_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
        let json = serde_json::to_vec_pretty(self).context("serializing run metrics")?;
        let mut written = Vec::new();

        if let Some(dir) = run_dir {
            let path = dir.join("metrics.json");
            std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
            written.push(path);
        }

        if let Some(dir) = archive_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let path = dir.join(format!("{}.metrics.json", self.label));
            std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
            written.push(path);
        }

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot_assets() -> BootAssets {
        BootAssets::Bundle {
            version: "resolved-bundle".into(),
        }
    }

    #[test]
    fn time_records_duration_for_failures_too() {
        let mut metrics = RunMetrics::new("t", Some("hv"), boot_assets()).expect("provenance");
        let ok: Result<(), &str> = metrics.time("good", || Ok(()));
        let err: Result<(), &str> = metrics.time("bad", || Err("boom"));
        assert!(ok.is_ok());
        assert!(err.is_err());
        assert_eq!(metrics.phases.len(), 2);
        assert_eq!(metrics.phases[0].name, "good");
        assert_eq!(metrics.phases[1].name, "bad");
    }

    #[test]
    fn distributions_keep_units_counts_and_empty_sample_sets() {
        let mut metrics = RunMetrics::new("t", None, boot_assets()).expect("provenance");
        let samples: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        metrics.record_distribution("latency", "seconds", &samples);
        metrics.record_distribution("no-steady-samples", "seconds", &[]);
        metrics.record_distribution("throughput", "per_second", &[2.0, 4.0]);
        let record = serde_json::to_value(&metrics).expect("JSON");
        assert_eq!(record["distributions"][0]["count"], 20);
        assert_eq!(
            record["distributions"][0]["samples"],
            serde_json::json!(samples)
        );
        assert_eq!(record["distributions"][0]["p50"], 10.0);
        assert_eq!(record["distributions"][0]["p95"], 19.0);
        assert_eq!(record["distributions"][0]["max"], 20.0);
        assert_eq!(record["distributions"][1]["count"], 0);
        assert!(record["distributions"][1]["p95"].is_null());
        assert_eq!(record["distributions"][2]["unit"], "per_second");
        assert!(metrics.phases.is_empty());
    }

    #[test]
    fn write_lands_in_run_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut metrics = RunMetrics::new("t", None, boot_assets()).expect("provenance");
        metrics.record("phase", 1.5);
        metrics.passed = true;
        let written = metrics.write(Some(dir.path())).expect("write");
        assert!(written.iter().any(|p| p.ends_with("metrics.json")));
        let text = std::fs::read_to_string(dir.path().join("metrics.json")).expect("read");
        assert!(text.contains("\"phase\""));
        assert!(text.contains("\"passed\": true"));
        let record: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(
            record["provenance"]["boot_assets"]["version"],
            "resolved-bundle"
        );
    }

    #[test]
    fn provenance_distinguishes_clean_dirty_and_unavailable_checkouts() {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            command_stdout(
                Command::new("git")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .arg("-C")
                    .arg(repo.path())
                    .args(args),
            )
            .expect("fixture git command")
        };
        git(&["init", "--quiet"]);
        git(&[
            "-c",
            "user.name=Metrics Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let clean = Provenance::capture(repo.path(), boot_assets(), vec![]).expect("clean repo");
        assert_eq!(clean.checkout_commit, git(&["rev-parse", "HEAD"]));
        assert!(!clean.checkout_dirty);
        std::fs::write(repo.path().join("untracked"), "changed").expect("write");
        let dirty = Provenance::capture(repo.path(), boot_assets(), vec![]).expect("dirty repo");
        assert!(dirty.checkout_dirty);
        let no_repo = tempfile::tempdir().expect("tempdir");
        assert!(Provenance::capture(no_repo.path(), boot_assets(), vec![]).is_err());
    }

    #[test]
    fn archive_preserves_the_label_arguments_and_selected_boot_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut metrics = RunMetrics::new("t", None, boot_assets()).expect("provenance");
        metrics.label = "pinned-label".into();
        metrics.provenance = Provenance::capture(
            &crate::repo_root(),
            BootAssets::Files {
                kernel: "custom kernel".into(),
                rootfs: "custom rootfs".into(),
            },
            vec![
                "probe".into(),
                "two words".into(),
                String::new(),
                "\"quoted\"".into(),
            ],
        )
        .expect("provenance");
        let paths = metrics.write_to(None, Some(dir.path())).expect("archive");
        let path = dir.path().join("pinned-label.metrics.json");
        assert_eq!(paths.as_slice(), std::slice::from_ref(&path));
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read")).expect("JSON");
        assert_eq!(record["label"], "pinned-label");
        assert_eq!(
            record["provenance"]["argv"],
            serde_json::json!(["probe", "two words", "", "\"quoted\""])
        );
        assert_eq!(
            record["provenance"]["boot_assets"],
            serde_json::json!({"source":"files", "kernel":"custom kernel", "rootfs":"custom rootfs"})
        );
    }
}
