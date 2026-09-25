//! Upstream resolver selection.
//!
//! A forwarder either pins an explicit upstream list or follows the host's
//! resolver file. macOS rewrites `/etc/resolv.conf` (via `/var/run`) on every
//! DNS configuration change — a Wi-Fi switch, a VPN connecting or dropping —
//! so its modification time is a cheap, dependency-free change signal. The
//! previous design read the file once at VM boot and kept forwarding to the
//! old network's resolvers forever (arcboxlabs/arcbox#714).

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use super::{DNS_PORT, DnsConfig};

/// The host's resolver configuration.
pub const SYSTEM_RESOLV_CONF: &str = "/etc/resolv.conf";

/// How often a following forwarder re-`stat`s the resolver file. Bounds the
/// per-query cost to one `stat` per second, so a network change is picked up
/// within a second of the next guest query.
pub const DEFAULT_RESOLVER_RECHECK: Duration = Duration::from_secs(1);

/// The upstream servers a forwarder sends to, and where they come from.
#[derive(Debug)]
pub(super) struct Upstreams {
    /// The pinned list, or the fallback while a followed file yields no
    /// usable server.
    fallback: Vec<SocketAddr>,
    followed: Option<Followed>,
}

/// A resolver file followed for changes.
#[derive(Debug)]
struct Followed {
    path: PathBuf,
    recheck: Duration,
    state: RwLock<State>,
}

#[derive(Debug)]
struct State {
    /// Servers currently forwarded to.
    servers: Vec<SocketAddr>,
    /// Modification time of the file when `servers` was loaded; `None` when
    /// the file was unreadable at that point.
    modified: Option<SystemTime>,
    /// When the file was last `stat`ed.
    checked_at: Instant,
}

impl Upstreams {
    pub(super) fn new(config: &DnsConfig) -> Self {
        let followed = config.system_resolver.clone().map(|path| {
            let state = State {
                servers: load_resolver(&path, &config.upstream),
                modified: resolver_modified(&path),
                checked_at: Instant::now(),
            };
            Followed {
                path,
                recheck: config.system_resolver_recheck,
                state: RwLock::new(state),
            }
        });
        Self {
            fallback: config.upstream.clone(),
            followed,
        }
    }

    /// The servers currently in effect.
    pub(super) fn current(&self) -> Vec<SocketAddr> {
        match &self.followed {
            Some(followed) => followed.state.read().expect(LOCK_POISONED).servers.clone(),
            None => self.fallback.clone(),
        }
    }

    /// Re-reads a followed file whose modification time changed since the
    /// last load, at most once per recheck interval.
    ///
    /// Returns `true` when the server list changed, so the caller can drop
    /// cached answers: a VPN resolver's answers must not outlive the VPN.
    pub(super) fn refresh(&self) -> bool {
        let Some(followed) = &self.followed else {
            return false;
        };
        let due = |state: &State| state.checked_at.elapsed() >= followed.recheck;
        if !due(&followed.state.read().expect(LOCK_POISONED)) {
            return false;
        }

        let mut state = followed.state.write().expect(LOCK_POISONED);
        // Another query may have refreshed between the read and write locks.
        if !due(&state) {
            return false;
        }
        state.checked_at = Instant::now();

        let modified = resolver_modified(&followed.path);
        if modified == state.modified {
            return false;
        }
        state.modified = modified;

        let servers = load_resolver(&followed.path, &self.fallback);
        if servers == state.servers {
            return false;
        }
        tracing::info!(
            resolver = %followed.path.display(),
            from = ?state.servers,
            to = ?servers,
            "system resolver changed; switching DNS upstream"
        );
        state.servers = servers;
        true
    }
}

const LOCK_POISONED: &str = "dns upstream lock poisoned";

/// Modification time of the resolver file, `None` when it cannot be read.
///
/// `/etc/resolv.conf` is a symlink on macOS; `metadata` follows it, so this
/// is the mtime of the generated target, which is what changes.
fn resolver_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Reads the usable `nameserver` entries of `path`, falling back to
/// `fallback` when the file is unreadable or lists no usable server.
fn load_resolver(path: &Path, fallback: &[SocketAddr]) -> Vec<SocketAddr> {
    let servers = fs::read_to_string(path)
        .map(|contents| parse_resolv_conf_nameservers(&contents))
        .unwrap_or_default();
    if servers.is_empty() {
        fallback.to_vec()
    } else {
        servers
    }
}

/// Parses `nameserver` entries from resolv.conf text.
///
/// Filters out problematic upstreams (loopback, fake-IP VPN ranges) when
/// better alternatives are available. Falls back to loopback if it's the
/// only IPv4 option — `forward_dns_async` binds `0.0.0.0:0` so only IPv4
/// upstreams are usable today. Returns an empty list when nothing usable is
/// left, so the caller keeps its fallback.
fn parse_resolv_conf_nameservers(contents: &str) -> Vec<SocketAddr> {
    let mut all: Vec<SocketAddr> = Vec::new();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        let Some(ip) = parts.next().and_then(|raw| raw.parse::<IpAddr>().ok()) else {
            continue;
        };
        let addr = SocketAddr::new(ip, DNS_PORT);
        if !all.contains(&addr) {
            all.push(addr);
        }
    }

    let usable = |addr: &SocketAddr| match addr.ip() {
        IpAddr::V4(v4) => !is_fake_ip(v4),
        IpAddr::V6(_) => false,
    };

    // Prefer non-loopback servers; keep loopback as the fallback when it is
    // the only IPv4 option (a common macOS default).
    let preferred: Vec<SocketAddr> = all
        .iter()
        .copied()
        .filter(|a| usable(a) && !a.ip().is_loopback())
        .collect();
    if !preferred.is_empty() {
        return preferred;
    }
    all.into_iter().filter(usable).collect()
}

/// Whether `ip` lies in the 198.18.0.0/15 range fake-IP proxies (Surge,
/// Clash) answer from. Such a resolver only works inside that proxy's tunnel.
fn is_fake_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn v4(ip: Ipv4Addr) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(ip), DNS_PORT)
    }

    /// A config following `path` with the recheck interval disabled, so
    /// every `refresh` re-stats the file.
    fn following(path: &Path) -> DnsConfig {
        DnsConfig {
            system_resolver: Some(path.to_path_buf()),
            system_resolver_recheck: Duration::ZERO,
            ..DnsConfig::default()
        }
    }

    /// Writes `contents` and stamps a distinct mtime, since two writes within
    /// the same filesystem timestamp granularity would otherwise look
    /// unchanged.
    fn write_resolver(path: &Path, contents: &str, mtime_secs: u64) {
        fs::write(path, contents).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs))
            .unwrap();
    }

    #[test]
    fn parse_prefers_ipv4_non_loopback_and_dedups() {
        let conf = r"
# comment
nameserver 10.0.0.2
search local
nameserver 2001:4860:4860::8888
nameserver invalid
nameserver 10.0.0.2
";
        // IPv6 servers are filtered because forward_dns_async binds 0.0.0.0:0.
        assert_eq!(
            parse_resolv_conf_nameservers(conf),
            vec![v4(Ipv4Addr::new(10, 0, 0, 2))]
        );
    }

    #[test]
    fn parse_keeps_loopback_when_it_is_the_only_ipv4_option() {
        let conf = "nameserver 127.0.0.1\nnameserver 2001:4860:4860::8888\n";
        assert_eq!(
            parse_resolv_conf_nameservers(conf),
            vec![v4(Ipv4Addr::LOCALHOST)]
        );
    }

    #[test]
    fn parse_filters_fake_ip() {
        let conf = "nameserver 198.18.0.2\nnameserver 8.8.8.8\n";
        assert_eq!(
            parse_resolv_conf_nameservers(conf),
            vec![v4(Ipv4Addr::new(8, 8, 8, 8))]
        );
    }

    #[test]
    fn parse_returns_empty_when_only_fake_ip_is_listed() {
        let conf = "nameserver 198.18.0.2\nnameserver 198.19.1.1\n";
        assert!(parse_resolv_conf_nameservers(conf).is_empty());
    }

    #[test]
    fn pinned_config_never_refreshes() {
        let upstreams = Upstreams::new(&DnsConfig::default());
        assert_eq!(upstreams.current(), super::super::default_upstream());
        assert!(!upstreams.refresh());
    }

    /// Regression for arcboxlabs/arcbox#714: the Mac moves to a network whose
    /// resolver differs, and the next query must go to the new one.
    #[test]
    fn follows_resolver_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        write_resolver(&path, "nameserver 192.168.1.1\n", 1_000);

        let upstreams = Upstreams::new(&following(&path));
        assert_eq!(upstreams.current(), vec![v4(Ipv4Addr::new(192, 168, 1, 1))]);
        assert!(!upstreams.refresh(), "an unchanged file is not a change");

        write_resolver(&path, "nameserver 10.8.0.1\n", 2_000);
        assert!(upstreams.refresh(), "a rewritten file switches the list");
        assert_eq!(upstreams.current(), vec![v4(Ipv4Addr::new(10, 8, 0, 1))]);
        assert!(!upstreams.refresh());
    }

    #[test]
    fn same_servers_under_a_new_mtime_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        write_resolver(&path, "nameserver 192.168.1.1\n", 1_000);
        let upstreams = Upstreams::new(&following(&path));

        write_resolver(&path, "# regenerated\nnameserver 192.168.1.1\n", 2_000);
        assert!(
            !upstreams.refresh(),
            "the cache must survive a no-op rewrite"
        );
    }

    #[test]
    fn unusable_or_missing_file_falls_back_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        let fallback = vec![v4(Ipv4Addr::new(9, 9, 9, 9))];
        let config = DnsConfig {
            upstream: fallback.clone(),
            ..following(&path)
        };

        let upstreams = Upstreams::new(&config);
        assert_eq!(upstreams.current(), fallback, "missing file → fallback");

        write_resolver(&path, "nameserver 198.18.0.2\n", 1_000);
        assert!(!upstreams.refresh(), "fake-IP only → still the fallback");
        assert_eq!(upstreams.current(), fallback);

        write_resolver(&path, "nameserver 192.168.1.1\n", 2_000);
        assert!(upstreams.refresh());
        assert_eq!(upstreams.current(), vec![v4(Ipv4Addr::new(192, 168, 1, 1))]);

        fs::remove_file(&path).unwrap();
        assert!(
            upstreams.refresh(),
            "a vanished file → back to the fallback"
        );
        assert_eq!(upstreams.current(), fallback);
    }

    #[test]
    fn recheck_interval_rate_limits_the_stat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        write_resolver(&path, "nameserver 192.168.1.1\n", 1_000);
        let config = DnsConfig {
            system_resolver_recheck: Duration::from_secs(3600),
            ..following(&path)
        };
        let upstreams = Upstreams::new(&config);

        write_resolver(&path, "nameserver 10.8.0.1\n", 2_000);
        assert!(
            !upstreams.refresh(),
            "within the recheck interval the file is not consulted"
        );
        assert_eq!(upstreams.current(), vec![v4(Ipv4Addr::new(192, 168, 1, 1))]);
    }
}
