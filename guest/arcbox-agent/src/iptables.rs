//! Container rules the agent owns in the guest's nat PREROUTING chain.
//!
//! Agent components rewrite the destination of traffic bound for containers:
//! `publish_mirror` for publishes pinned to a host address, `domains` for
//! container domains served without a port. Every such rule carries an
//! iptables comment `<owner><container id>`, which is what lets
//!
//! - [`TaggedRules`] replace or remove one container's rules as a set, with
//!   `-C` before every insert or delete so that a rule is never doubled and
//!   a missing one is not an error, and
//! - [`sweep`] find an owner's rules again after an agent restart, when the
//!   record of what the previous process installed is gone. The owner's
//!   event reconciliation then reinstalls what still applies.

use std::collections::HashMap;
use std::process::Output;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

const IPTABLES: &str = "/sbin/iptables";

/// One agent-owned nat PREROUTING rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatRule {
    /// Everything after the chain name: matches, the comment tag, the target.
    spec: Vec<String>,
}

impl NatRule {
    /// A rule applying `target` to traffic matching `matches`, tagged as
    /// `owner`'s rule for `container_id`.
    #[must_use]
    pub fn new(owner: &str, container_id: &str, matches: &[&str], target: &[&str]) -> Self {
        let tag = format!("{owner}{container_id}");
        let spec = matches
            .iter()
            .chain(&["-m", "comment", "--comment", tag.as_str()])
            .chain(target)
            .map(|arg| (*arg).to_owned())
            .collect();
        Self { spec }
    }

    /// The rule as `iptables -S` would print it after `-A PREROUTING`.
    #[cfg(test)]
    pub fn spec(&self) -> String {
        self.spec.join(" ")
    }

    /// Inserts the rule at the top of the chain unless it is there already.
    async fn ensure(&self) -> Result<()> {
        if !is_installed(&nat_prerouting("-C", &self.spec)).await? {
            run(&nat_prerouting("-I", &self.spec)).await?;
        }
        Ok(())
    }

    /// Deletes the rule if it is present.
    async fn delete(&self) -> Result<()> {
        if is_installed(&nat_prerouting("-C", &self.spec)).await? {
            run(&nat_prerouting("-D", &self.spec)).await?;
        }
        Ok(())
    }
}

/// The rules one owner holds in the kernel, per container ID.
pub struct TaggedRules {
    owner: &'static str,
    held: HashMap<String, Vec<NatRule>>,
}

impl TaggedRules {
    /// An empty record for rules tagged with `owner`.
    #[must_use]
    pub fn new(owner: &'static str) -> Self {
        Self {
            owner,
            held: HashMap::new(),
        }
    }

    /// Makes `rules` the container's whole set.
    ///
    /// New rules go in before stale ones come out, so a container whose rule
    /// changes is never briefly without one. A stale rule that fails to come
    /// out stays recorded for the next call to retry.
    pub async fn replace(&mut self, container_id: &str, rules: Vec<NatRule>) -> Result<()> {
        let previous = self.held.remove(container_id).unwrap_or_default();
        let mut held = Vec::with_capacity(rules.len());
        let mut failures = Vec::new();
        for rule in &rules {
            match rule.ensure().await {
                Ok(()) => held.push(rule.clone()),
                Err(e) => failures.push(format!("{e:#}")),
            }
        }
        for stale in previous.into_iter().filter(|rule| !rules.contains(rule)) {
            if let Err(e) = stale.delete().await {
                failures.push(format!("{e:#}"));
                held.push(stale);
            }
        }
        if !held.is_empty() {
            self.held.insert(container_id.to_owned(), held);
        }
        if !failures.is_empty() {
            bail!(
                "{} rule change(s) failed for {container_id}: {}",
                self.owner,
                failures.join("; ")
            );
        }
        Ok(())
    }

    /// Removes every rule held for the container. Missing rules are fine.
    pub async fn remove(&mut self, container_id: &str) -> Result<()> {
        self.replace(container_id, Vec::new()).await
    }
}

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

fn nat_prerouting(verb: &str, spec: &[String]) -> Vec<String> {
    ["-t", "nat", "-w", "2", verb, "PREROUTING"]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(spec.iter().cloned())
        .collect()
}

async fn is_installed(check_args: &[String]) -> Result<bool> {
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

async fn run(args: &[String]) -> Result<()> {
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
    fn a_rule_carries_its_owner_tag_between_matches_and_target() {
        let rule = NatRule::new(
            "arcbox-test:",
            "abc123",
            &["-p", "tcp", "--dport", "80"],
            &["-j", "DNAT", "--to-destination", "172.17.0.2:3000"],
        );
        assert_eq!(
            rule.spec(),
            "-p tcp --dport 80 -m comment --comment arcbox-test:abc123 \
             -j DNAT --to-destination 172.17.0.2:3000"
        );
        assert_eq!(
            nat_prerouting("-I", &rule.spec)[..6],
            ["-t", "nat", "-w", "2", "-I", "PREROUTING"]
        );
    }

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
