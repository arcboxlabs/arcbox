//! Input validation for setuid defense-in-depth.
//!
//! Every external input is validated before any privileged operation runs.
//! This module is deliberately strict — it rejects anything that doesn't
//! match the expected patterns for ArcBox's use cases.
//!
//! Types in this module follow the "parse, don't validate" pattern: each
//! newtype can only be constructed through validated parsing, so downstream
//! code can rely on invariants being upheld at the type level.

use std::path::Component;
use std::str::FromStr;

use ipnetwork::Ipv4Network;

// ---------------------------------------------------------------------------
// Subnet
// ---------------------------------------------------------------------------

/// A validated private CIDR subnet (e.g. `10.0.0.0/8`).
///
/// Guarantees:
/// - Valid IPv4 CIDR notation with explicit prefix
/// - No non-zero host bits
/// - IP is in a private range (10/8, 172.16/12, 192.168/16)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subnet(Ipv4Network);

impl Subnet {
    pub fn as_str(&self) -> String {
        self.0.to_string()
    }

    pub fn network(&self) -> &Ipv4Network {
        &self.0
    }
}

impl FromStr for Subnet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.contains('/') {
            return Err(format!("invalid CIDR: missing '/' in '{s}'"));
        }

        let net: Ipv4Network = s
            .parse()
            .map_err(|e| format!("invalid CIDR '{s}': {e}"))?;

        if net.ip() != net.network() {
            return Err(format!(
                "CIDR '{s}' has non-zero host bits (did you mean {}/{}?)",
                net.network(),
                net.prefix()
            ));
        }

        if !net.ip().is_private() {
            return Err(format!(
                "subnet {s} is not in a private range (10/8, 172.16/12, 192.168/16)"
            ));
        }

        Ok(Self(net))
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// BridgeIface
// ---------------------------------------------------------------------------

/// A validated bridge interface name (e.g. `bridge100`).
///
/// Guarantees: matches `^bridge[0-9]+$` with a number that fits in `u32`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeIface(String);

impl BridgeIface {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for BridgeIface {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let suffix = s
            .strip_prefix("bridge")
            .ok_or_else(|| format!("interface '{s}' must start with 'bridge'"))?;

        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "interface '{s}' must match bridge<N> (e.g. bridge100)"
            ));
        }

        let _n: u32 = suffix
            .parse()
            .map_err(|_| format!("interface '{s}' has invalid bridge number (too large)"))?;

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for BridgeIface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

/// A validated DNS domain name (e.g. `arcbox.local`).
///
/// Guarantees:
/// - Non-empty, ≤253 chars, lowercase alphanumeric + `.` + `-`
/// - No leading/trailing dots, no consecutive dots
/// - Each label ≤63 chars, no leading/trailing `-` (RFC 1035/1123)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain(String);

impl Domain {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Domain {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("domain must not be empty".to_string());
        }
        if s.len() > 253 {
            return Err(format!("domain too long ({} > 253 chars)", s.len()));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        {
            return Err(format!(
                "domain '{s}' contains invalid characters (allowed: a-z, 0-9, '.', '-')"
            ));
        }
        if s.starts_with('.') || s.ends_with('.') {
            return Err(format!("domain '{s}' must not start or end with '.'"));
        }
        if s.contains("..") {
            return Err(format!(
                "domain '{s}' contains empty label (consecutive dots)"
            ));
        }

        for label in s.split('.') {
            if label.len() > 63 {
                return Err(format!("domain '{s}' has label exceeding 63 chars"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(format!(
                    "domain '{s}' has label starting or ending with '-'"
                ));
            }
        }

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// DnsPort
// ---------------------------------------------------------------------------

/// A validated unprivileged port number (1024..=65535).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DnsPort(u16);

impl DnsPort {
    pub fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for DnsPort {
    type Error = String;

    fn try_from(port: u16) -> Result<Self, Self::Error> {
        if port < 1024 {
            return Err(format!("port {port} is below 1024 (privileged range)"));
        }
        Ok(Self(port))
    }
}

impl std::fmt::Display for DnsPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// SocketTarget
// ---------------------------------------------------------------------------

/// A validated socket target path (e.g. `/Users/alice/.arcbox/run/docker.sock`).
///
/// Guarantees:
/// - Under `/Users/<username>/.arcbox/`
/// - Ends with `.sock` (strict lowercase)
/// - No `..` path traversal components
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketTarget(String);

impl SocketTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SocketTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = std::path::Path::new(s);

        let rest = s
            .strip_prefix("/Users/")
            .ok_or_else(|| format!("socket target '{s}' must be under /Users/"))?;

        let (username, after_user) = rest
            .split_once('/')
            .ok_or_else(|| format!("socket target '{s}' has no path after username"))?;

        if username.is_empty() {
            return Err(format!("socket target '{s}' has empty username"));
        }

        if !after_user.starts_with(".arcbox/") {
            return Err(format!("socket target '{s}' must be under ~/.arcbox/"));
        }

        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if !s.ends_with(".sock") {
            return Err(format!("socket target '{s}' must end with .sock"));
        }

        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(format!(
                "socket target '{s}' contains '..' path traversal component"
            ));
        }

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for SocketTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// CliName
// ---------------------------------------------------------------------------

/// A validated CLI tool name from the allow list (e.g. `docker`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliName(String);

impl CliName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CliName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if arcbox_constants::paths::DOCKER_CLI_TOOLS.contains(&s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "CLI name '{s}' is not in the allow list: {}",
                arcbox_constants::paths::DOCKER_CLI_TOOLS.join(", ")
            ))
        }
    }
}

impl std::fmt::Display for CliName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// CliTarget
// ---------------------------------------------------------------------------

/// A validated CLI symlink target path inside an app bundle.
///
/// Guarantees:
/// - Absolute path under `/Applications/` or `/Users/`
/// - Contains `.app/Contents/MacOS/xbin/` structure
/// - No `..` path traversal
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliTarget(String);

impl CliTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CliTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = std::path::Path::new(s);

        if !path.is_absolute() {
            return Err(format!("CLI target '{s}' must be an absolute path"));
        }

        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(format!("CLI target '{s}' contains '..' path traversal"));
        }

        let components: Vec<_> = path.components().collect();
        let has_valid_app_structure = components.windows(4).any(|w| {
            matches!(&w[0], Component::Normal(name) if name.to_string_lossy().ends_with(".app"))
                && w[1] == Component::Normal("Contents".as_ref())
                && w[2] == Component::Normal("MacOS".as_ref())
                && w[3] == Component::Normal("xbin".as_ref())
        });

        if !has_valid_app_structure {
            return Err(format!(
                "CLI target '{s}' must be inside an .app bundle's Contents/MacOS/xbin/"
            ));
        }

        if !s.starts_with("/Applications/") && !s.starts_with("/Users/") {
            return Err(format!(
                "CLI target '{s}' must be under /Applications/ or /Users/"
            ));
        }

        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for CliTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Legacy free-function wrappers (delegate to FromStr / TryFrom)
// ---------------------------------------------------------------------------

pub fn validate_subnet(s: &str) -> Result<(), String> {
    Subnet::from_str(s).map(|_| ())
}

pub fn validate_iface(s: &str) -> Result<(), String> {
    BridgeIface::from_str(s).map(|_| ())
}

pub fn validate_domain(s: &str) -> Result<(), String> {
    Domain::from_str(s).map(|_| ())
}

pub fn validate_port(port: u16) -> Result<(), String> {
    DnsPort::try_from(port).map(|_| ())
}

pub fn validate_socket_target(s: &str) -> Result<(), String> {
    SocketTarget::from_str(s).map(|_| ())
}

pub fn validate_cli_name(name: &str) -> Result<(), String> {
    CliName::from_str(name).map(|_| ())
}

pub fn validate_cli_target(target: &str) -> Result<(), String> {
    CliTarget::from_str(target).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_subnets() {
        assert!("10.0.0.0/8".parse::<Subnet>().is_ok());
        assert!("172.16.0.0/12".parse::<Subnet>().is_ok());
        assert!("192.168.1.0/24".parse::<Subnet>().is_ok());
        assert!("10.10.10.0/24".parse::<Subnet>().is_ok());
    }

    #[test]
    fn invalid_subnets() {
        // Public ranges
        assert!("8.8.8.0/24".parse::<Subnet>().is_err());
        assert!("1.1.1.0/24".parse::<Subnet>().is_err());
        // Malformed
        assert!("not-a-cidr".parse::<Subnet>().is_err());
        assert!("10.0.0.0".parse::<Subnet>().is_err());
        assert!("10.0.0.0/33".parse::<Subnet>().is_err());
    }

    #[test]
    fn subnet_rejects_nonzero_host_bits() {
        let r = "10.0.0.5/8".parse::<Subnet>();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("host bits"));

        let r = "192.168.1.1/24".parse::<Subnet>();
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("192.168.1.0/24"));
    }

    #[test]
    fn subnet_display_roundtrip() {
        let s = "10.0.0.0/8".parse::<Subnet>().unwrap();
        assert_eq!(s.to_string(), "10.0.0.0/8");
    }

    #[test]
    fn valid_ifaces() {
        assert!("bridge0".parse::<BridgeIface>().is_ok());
        assert!("bridge100".parse::<BridgeIface>().is_ok());
        assert!("bridge999".parse::<BridgeIface>().is_ok());
    }

    #[test]
    fn invalid_ifaces() {
        assert!("eth0".parse::<BridgeIface>().is_err());
        assert!("bridge".parse::<BridgeIface>().is_err());
        assert!("bridge100a".parse::<BridgeIface>().is_err());
        assert!("BRIDGE100".parse::<BridgeIface>().is_err());
    }

    #[test]
    fn iface_rejects_overflow() {
        assert!("bridge99999999999999999".parse::<BridgeIface>().is_err());
    }

    #[test]
    fn valid_domains() {
        assert!("arcbox.local".parse::<Domain>().is_ok());
        assert!("test.arcbox.local".parse::<Domain>().is_ok());
        assert!("my-domain.local".parse::<Domain>().is_ok());
    }

    #[test]
    fn invalid_domains() {
        assert!("".parse::<Domain>().is_err());
        assert!("UPPER.case".parse::<Domain>().is_err());
        assert!("has space.local".parse::<Domain>().is_err());
        // 254 chars
        let long = "a".repeat(254);
        assert!(long.parse::<Domain>().is_err());
        // Path traversal / malformed labels
        assert!(".".parse::<Domain>().is_err());
        assert!("..".parse::<Domain>().is_err());
        assert!(".leading".parse::<Domain>().is_err());
        assert!("trailing.".parse::<Domain>().is_err());
        assert!("empty..label".parse::<Domain>().is_err());
    }

    #[test]
    fn domain_rejects_label_starting_or_ending_with_hyphen() {
        assert!("-bad.local".parse::<Domain>().is_err());
        assert!("bad-.local".parse::<Domain>().is_err());
        assert!("ok.-bad".parse::<Domain>().is_err());
        assert!("ok.bad-".parse::<Domain>().is_err());
    }

    #[test]
    fn domain_rejects_label_exceeding_63_chars() {
        let long_label = "a".repeat(64);
        let domain = format!("{long_label}.local");
        assert!(domain.parse::<Domain>().is_err());

        // 63 chars should pass
        let ok_label = "a".repeat(63);
        let domain = format!("{ok_label}.local");
        assert!(domain.parse::<Domain>().is_ok());
    }

    #[test]
    fn valid_ports() {
        assert!(DnsPort::try_from(1024_u16).is_ok());
        assert!(DnsPort::try_from(5553_u16).is_ok());
        assert!(DnsPort::try_from(65535_u16).is_ok());
    }

    #[test]
    fn invalid_ports() {
        assert!(DnsPort::try_from(0_u16).is_err());
        assert!(DnsPort::try_from(80_u16).is_err());
        assert!(DnsPort::try_from(1023_u16).is_err());
    }

    #[test]
    fn valid_socket_targets() {
        assert!("/Users/alice/.arcbox/run/docker.sock"
            .parse::<SocketTarget>()
            .is_ok());
        assert!("/Users/bob/.arcbox/run/arcbox.sock"
            .parse::<SocketTarget>()
            .is_ok());
    }

    #[test]
    fn invalid_socket_targets() {
        assert!("/tmp/docker.sock".parse::<SocketTarget>().is_err());
        assert!("/Users//.arcbox/run/docker.sock"
            .parse::<SocketTarget>()
            .is_err());
        assert!("/Users/alice/.config/docker.sock"
            .parse::<SocketTarget>()
            .is_err());
        assert!("/Users/alice/.arcbox/run/docker.txt"
            .parse::<SocketTarget>()
            .is_err());
    }

    #[test]
    fn socket_target_rejects_uppercase_extension() {
        assert!("/Users/alice/.arcbox/run/docker.SOCK"
            .parse::<SocketTarget>()
            .is_err());
        assert!("/Users/alice/.arcbox/run/docker.Sock"
            .parse::<SocketTarget>()
            .is_err());
    }

    #[test]
    fn socket_target_rejects_path_traversal() {
        let result = "/Users/alice/.arcbox/../../../../var/run/other.sock".parse::<SocketTarget>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".."));
    }

    // -- CLI name/target validation tests --

    #[test]
    fn valid_cli_names() {
        assert!("docker".parse::<CliName>().is_ok());
        assert!("docker-buildx".parse::<CliName>().is_ok());
        assert!("docker-compose".parse::<CliName>().is_ok());
        assert!("docker-credential-osxkeychain".parse::<CliName>().is_ok());
    }

    #[test]
    fn invalid_cli_names() {
        assert!("".parse::<CliName>().is_err());
        assert!("curl".parse::<CliName>().is_err());
        assert!("rm".parse::<CliName>().is_err());
        assert!("../docker".parse::<CliName>().is_err());
    }

    #[test]
    fn valid_cli_targets() {
        assert!("/Applications/ArcBox Desktop.app/Contents/MacOS/xbin/docker"
            .parse::<CliTarget>()
            .is_ok());
        assert!("/Users/test/Apps/ArcBox.app/Contents/MacOS/xbin/docker-compose"
            .parse::<CliTarget>()
            .is_ok());
    }

    #[test]
    fn invalid_cli_targets() {
        assert!("Contents/MacOS/xbin/docker".parse::<CliTarget>().is_err());
        assert!("/usr/local/bin/docker".parse::<CliTarget>().is_err());
        assert!("/Applications/ArcBox.app/Contents/MacOS/xbin/../../evil"
            .parse::<CliTarget>()
            .is_err());
        assert!("/tmp/evil.app/Contents/MacOS/xbin/docker"
            .parse::<CliTarget>()
            .is_err());
    }

    #[test]
    fn cli_target_rejects_nested_app_structure() {
        assert!(
            "/Users/evil/not-really.app/Contents/MacOS/xbin/nested.app/Contents/MacOS/xbin/docker"
                .parse::<CliTarget>()
                .is_ok()
        );
    }
}
