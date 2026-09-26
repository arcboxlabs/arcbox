//! Container rules the agent owns in the guest's nat PREROUTING chain.
//!
//! Agent components rewrite the destination of traffic bound for
//! containers. Every such rule carries an iptables comment
//! `<owner><container id>`, which is what lets [`sweep`] find an owner's
//! rules again after an agent restart, when the record of what the previous
//! process installed is gone. The owner's event reconciliation then
//! reinstalls what still applies.

use std::process::Output;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

const IPTABLES: &str = "/sbin/iptables";

/// Deletes every nat PREROUTING rule tagged by `owner`, whoever installed
/// it, and returns how many went.
pub async fn sweep(owner: &str) -> Result<usize> {
    let listing = Command::new(IPTABLES)
        .args(["-t", "nat", "-w", "2", "-S", "PREROUTING"])
        .output()
        .await
        .context("listing nat PREROUTING")?;
    if !listing.status.success() {
        bail!(
            "iptables -t nat -S PREROUTING failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    let mut failures = Vec::new();
    let mut removed = 0usize;
    for args in listing.lines().filter_map(|line| sweep_args(owner, line)) {
        match run(&args).await {
            Ok(()) => removed += 1,
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    if !failures.is_empty() {
        bail!(
            "failed to sweep {} stale {owner} rule(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(removed)
}

/// If `line` (from `iptables -S PREROUTING`) is tagged by `owner`, the argv
/// that deletes it.
fn sweep_args(owner: &str, line: &str) -> Option<Vec<String>> {
    let spec = line.strip_prefix("-A PREROUTING ")?;
    let fields: Vec<String> = spec.split_whitespace().map(unquote).collect();
    let tagged = fields
        .windows(2)
        .any(|pair| pair[0] == "--comment" && pair[1].starts_with(owner));
    tagged.then(|| nat_prerouting("-D", &fields))
}

/// `iptables -S` quotes comments; `-D` wants them bare.
fn unquote(field: &str) -> String {
    field
        .strip_prefix('"')
        .and_then(|f| f.strip_suffix('"'))
        .unwrap_or(field)
        .to_owned()
}

/// The argv applying `verb` (`-C`, `-I`, `-D`) to a nat PREROUTING rule.
pub fn nat_prerouting(verb: &str, spec: &[String]) -> Vec<String> {
    ["-t", "nat", "-w", "2", verb, "PREROUTING"]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(spec.iter().cloned())
        .collect()
}

/// Runs an `iptables -C` argv: whether the rule is installed.
pub async fn is_installed(check_args: &[String]) -> Result<bool> {
    let output = Command::new(IPTABLES)
        .args(check_args)
        .output()
        .await
        .context("failed to run iptables")?;
    classify_check(&output, check_args)
}

fn classify_check(output: &Output, args: &[String]) -> Result<bool> {
    if output.status.success() {
        return Ok(true);
    }
    // Exit 1 is iptables' "no such rule"; anything else is a real failure.
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    bail!(
        "iptables {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Runs an iptables argv that must succeed.
pub async fn run(args: &[String]) -> Result<()> {
    let output = Command::new(IPTABLES)
        .args(args)
        .output()
        .await
        .context("failed to run iptables")?;
    if !output.status.success() {
        bail!(
            "iptables {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_recognises_only_the_owners_rules() {
        let ours = "-A PREROUTING -i eth0 -p tcp -m tcp --dport 32768 -m comment \
                    --comment \"arcbox-publish:abc123\" -j DNAT --to-destination 172.17.0.2:80";
        let args = sweep_args("arcbox-publish:", ours).expect("our rule is swept");
        assert_eq!(&args[..6], ["-t", "nat", "-w", "2", "-D", "PREROUTING"]);
        assert!(
            args.contains(&"arcbox-publish:abc123".to_string()),
            "comment unquoted"
        );

        let docker = "-A PREROUTING -m addrtype --dst-type LOCAL -j DOCKER";
        assert!(sweep_args("arcbox-publish:", docker).is_none());
        let sandbox = "-A PREROUTING -p tcp -m tcp --dport 40000 -m comment \
                       --comment \"arcbox-sbx:gen\" -j DNAT --to-destination 172.20.0.2:80";
        assert!(
            sweep_args("arcbox-publish:", sandbox).is_none(),
            "sandbox rules belong elsewhere"
        );
        assert!(
            sweep_args("arcbox-sbx:", ours).is_none(),
            "one owner never sweeps another's rules"
        );
    }

    #[test]
    fn rule_check_distinguishes_absent_from_broken() {
        use std::os::unix::process::ExitStatusExt as _;
        let exited = |code: i32| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: b"err".to_vec(),
        };
        let args = vec!["-C".to_string()];
        assert!(classify_check(&exited(0), &args).unwrap());
        assert!(!classify_check(&exited(1), &args).unwrap());
        assert!(classify_check(&exited(2), &args).is_err());
    }
}
