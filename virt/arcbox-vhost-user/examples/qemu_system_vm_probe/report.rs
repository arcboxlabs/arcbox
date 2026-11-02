//! What `qemu_system_vm_probe` prints after the guest stops: the driver's
//! `PROBE` lines, the agent's mount and runtime lines, per-share back-end
//! load, timings, and the checks.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use arcbox_vhost_user::fs::FsStats;

use crate::probe::{Args, EXEC_ROUNDS};

pub struct Outcome {
    /// QEMU's exit status; `None` when the probe killed it at the timeout.
    pub status: Option<ExitStatus>,
    pub elapsed: Duration,
    pub stats: Vec<(String, FsStats)>,
}

/// The rest of the first `PROBE <key>...` line.
fn field<'a>(console: &'a str, key: &str) -> Option<&'a str> {
    console
        .lines()
        .filter_map(|l| l.split_once("PROBE ").map(|(_, rest)| rest))
        .find_map(|rest| rest.strip_prefix(key))
}

fn kv(line: &str, key: &str) -> Option<f64> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.parse().ok())
}

fn span(console: &str, key: &str) -> Option<f64> {
    let line = field(console, key)?;
    Some(kv(line, "end=")? - kv(line, "start=")?)
}

fn summary(console: &str, outcome: &Outcome) -> String {
    let mut out = String::new();
    match outcome.status {
        Some(status) => {
            let _ = write!(out, "qemu exit: {status}");
        }
        None => out.push_str("qemu killed at the timeout"),
    }
    let _ = writeln!(out, ", wall time {:.1}s", outcome.elapsed.as_secs_f64());
    let staged = field(console, "staging ").and_then(|l| kv(l, "up="));
    let published = field(console, "published ").and_then(|l| kv(l, "up="));
    if let (Some(s), Some(p)) = (staged, published) {
        let _ = writeln!(
            out,
            "runtime materialization (staging to publish): {:.1}s",
            p - s
        );
    }
    for (key, what) in [
        ("ensure ", "EnsureRuntime until ready"),
        ("runtime-read ", "cold read of the runtime through virtiofs"),
        ("users-tar1 ", "tar of the /Users tree, 1 reader"),
        ("private-tar4 ", "tar of the /private tree, 4 readers"),
    ] {
        if let Some(secs) = span(console, key) {
            let _ = writeln!(out, "{what}: {secs:.2}s");
        }
    }
    if let Some(line) = field(console, "runtime-read ") {
        if let (Some(bytes), Some(secs)) = (kv(line, "bytes="), span(console, "runtime-read ")) {
            let _ = writeln!(
                out,
                "runtime read rate: {:.0} MiB/s",
                bytes / secs / 1048576.0
            );
        }
    }
    out
}

/// Prints the report and returns the number of failed checks.
pub fn report(args: &Args, work: &Path, outcome: &Outcome) -> usize {
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let console = read(&work.join("console.log"));
    let agent_log = read(&work.join("share/log/agent.log"));
    let backend_log = read(&work.join("backend.log"));
    println!("--- guest driver ---");
    for line in console.lines().filter(|l| l.contains("PROBE ")) {
        println!("{line}");
    }
    println!("--- agent.log (mounts, runtime) ---");
    let keys = [
        "dax=always",
        "Btrfs identity",
        "guest-local runtime",
        "listening on vsock",
        "spawned bundled",
    ];
    for line in agent_log
        .lines()
        .filter(|l| keys.iter().any(|k| l.contains(k)))
    {
        println!("{line}");
    }
    println!("--- back-ends ---");
    let secs = outcome.elapsed.as_secs_f64();
    for (tag, s) in &outcome.stats {
        let syncfs = backend_log
            .lines()
            .filter(|l| l.contains(&format!("vhost-fs-{tag}-vq")) && l.contains("opcode 50"))
            .count();
        println!(
            "{tag}: {} requests, busy {:.2}s of {secs:.1}s, deepest batch {}, FUSE_SYNCFS answered ENOSYS {syncfs}x",
            s.requests,
            s.busy.as_secs_f64(),
            s.max_batch
        );
    }
    println!("--- summary ---");
    print!("{}", summary(&console, outcome));

    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("[{}] {what}", if ok { "ok" } else { "FAIL" });
        failures += usize::from(!ok);
    };
    check(
        outcome.status.is_some_and(|s| s.success()),
        "guest powered off",
    );
    check(
        field(&console, "ping ").is_some_and(|l| l.contains("type=0x1001")),
        "agent answered Ping over vsock",
    );
    let mount = |m: &str| {
        field(
            &console,
            &format!("mount {} {m} virtiofs ", &m[1..].to_lowercase()),
        )
    };
    for m in ["/arcbox", "/Users", "/private"] {
        check(mount(m).is_some(), &format!("{m} mounted virtiofs"));
    }
    // The agent remounts /arcbox with dax=always. virtiofs has no reconfigure
    // operation, so the kernel accepts the option and ignores it.
    let dax_attempt = agent_log
        .lines()
        .position(|l| l.contains("dax=always") && l.contains("/arcbox"));
    let serving = agent_log
        .lines()
        .position(|l| l.contains("Agent listening on vsock port 1024"));
    check(
        dax_attempt.zip(serving).is_some_and(|(d, s)| d < s)
            && mount("/arcbox").is_some_and(|opts| !opts.contains("dax")),
        "agent carried on past the dax=always remount; /arcbox has no DAX",
    );
    check(
        field(&console, "ensure ").is_some_and(|l| l.contains(" 1=1")),
        "EnsureRuntime: runtime ready",
    );
    check(
        agent_log
            .lines()
            .any(|l| l.contains("guest-local runtime ready") && l.contains("materialized")),
        &format!("runtime {} materialized to Btrfs", args.generation),
    );
    check(field(&console, "sync rc=0").is_some(), "sync exits 0");
    let syncfs_ok = console
        .lines()
        .filter(|l| l.contains("PROBE syncfs ") && l.ends_with("rc=0"))
        .count();
    check(
        syncfs_ok == 4,
        &format!("syncfs exits 0 on every share ({syncfs_ok}/4)"),
    );
    let expected = (args.small_files * 4096) as f64;
    for key in ["users-tar1 ", "private-tar4 "] {
        let bytes = field(&console, key).and_then(|l| kv(l, "bytes="));
        check(
            bytes.is_some_and(|b| b > expected),
            &format!("{key}read every small file"),
        );
    }
    check(field(&console, "done").is_some(), "driver finished");
    // QEMU TCG drops a page's translations when the guest CPU or a QEMU
    // device writes it, but not when a vhost-user back-end does, and its
    // IC IVAU is a no-op; until arcboxlabs/qemu fixes that, this fails, and
    // the agent and helpers run from devtmpfs copies (`guest/wrapper.sh`).
    let coherence = field(&console, "exec-coherence ").unwrap_or("missing");
    check(
        coherence == format!("buffered=2 direct=2 share-stale=0/{EXEC_ROUNDS}"),
        &format!(
            "code the back-end writes runs as written, also mapped straight from the share ({coherence}; fails on unpatched QEMU)"
        ),
    );
    failures
}
