//! Input validation for setuid defense-in-depth.
//!
//! Every external input is validated before any privileged operation runs.
//! This module is deliberately strict — it rejects anything that doesn't
//! match the expected patterns for ArcBox's use cases.

use std::net::Ipv4Addr;
use std::path::Component;

/// Validates a CIDR subnet string. Only private ranges are allowed:
/// - 10.0.0.0/8
/// - 172.16.0.0/12
/// - 192.168.0.0/16
pub fn validate_subnet(s: &str) -> Result<(), String> {
    let (ip_str, prefix_str) = s
        .split_once('/')
        .ok_or_else(|| format!("invalid CIDR: missing '/' in '{s}'"))?;

    let ip: Ipv4Addr = ip_str
        .parse()
        .map_err(|e| format!("invalid IP in CIDR '{s}': {e}"))?;

    let prefix: u8 = prefix_str
        .parse()
        .map_err(|e| format!("invalid prefix in CIDR '{s}': {e}"))?;

    if prefix > 32 {
        return Err(format!("prefix /{prefix} out of range (max 32)"));
    }

    let octets = ip.octets();
    let is_private = octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168);

    if !is_private {
        return Err(format!(
            "subnet {s} is not in a private range (10/8, 172.16/12, 192.168/16)"
        ));
    }

    Ok(())
}

/// Validates a bridge interface name. Must match `^bridge[0-9]+$`.
pub fn validate_iface(s: &str) -> Result<(), String> {
    if !s.starts_with("bridge") {
        return Err(format!("interface '{s}' must start with 'bridge'"));
    }

    let suffix = &s["bridge".len()..];
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "interface '{s}' must match bridge<N> (e.g. bridge100)"
        ));
    }

    Ok(())
}

/// Validates a DNS domain. Must match `^[a-z0-9.-]+$`, max 253 chars,
/// no empty labels, no leading/trailing dots, no consecutive dots.
/// Also rejects `.` and `..` to prevent path traversal in `/etc/resolver/`.
pub fn validate_domain(s: &str) -> Result<(), String> {
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
    Ok(())
}

/// Validates a DNS port. Must be in the range 1024..=65535.
pub fn validate_port(port: u16) -> Result<(), String> {
    if port < 1024 {
        return Err(format!("port {port} is below 1024 (privileged range)"));
    }
    Ok(())
}

/// Validates a socket target path. Must match `^/Users/[^/]+/.arcbox/.+\.sock$`.
pub fn validate_socket_target(s: &str) -> Result<(), String> {
    // Must start with /Users/
    if !s.starts_with("/Users/") {
        return Err(format!("socket target '{s}' must be under /Users/"));
    }

    // Extract the rest after /Users/
    let rest = &s["/Users/".len()..];

    // Must have a username (non-empty, no slash)
    let (username, after_user) = rest
        .split_once('/')
        .ok_or_else(|| format!("socket target '{s}' has no path after username"))?;

    if username.is_empty() {
        return Err(format!("socket target '{s}' has empty username"));
    }

    // Must contain .arcbox/
    if !after_user.starts_with(".arcbox/") {
        return Err(format!("socket target '{s}' must be under ~/.arcbox/"));
    }

    // Must end with .sock
    if !std::path::Path::new(s)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sock"))
    {
        return Err(format!("socket target '{s}' must end with .sock"));
    }

    // Reject path traversal (e.g. /Users/alice/.arcbox/../../../../var/run/other.sock).
    if std::path::Path::new(s)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "socket target '{s}' contains '..' path traversal component"
        ));
    }

    Ok(())
}

/// Allowed CLI tool names for `/usr/local/bin/` symlinks.
const ALLOWED_CLI_NAMES: &[&str] = &[
    "docker",
    "docker-buildx",
    "docker-compose",
    "docker-credential-osxkeychain",
];

/// Validates a CLI tool name for `/usr/local/bin/` symlink creation.
pub fn validate_cli_name(name: &str) -> Result<(), String> {
    if ALLOWED_CLI_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "CLI name '{name}' is not in the allow list: {}",
            ALLOWED_CLI_NAMES.join(", ")
        ))
    }
}

/// Validates a CLI symlink target. Must point inside an app bundle's
/// `Contents/MacOS/xbin/` directory. Rejects path traversal.
pub fn validate_cli_target(target: &str) -> Result<(), String> {
    let path = std::path::Path::new(target);

    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "CLI target '{target}' contains '..' path traversal"
        ));
    }

    // Must be inside an .app bundle's Contents/MacOS/xbin/
    if !target.contains(".app/Contents/MacOS/xbin/") {
        return Err(format!(
            "CLI target '{target}' must be inside an .app bundle's Contents/MacOS/xbin/"
        ));
    }

    if !path.is_absolute() {
        return Err(format!("CLI target '{target}' must be an absolute path"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_subnets() {
        assert!(validate_subnet("10.0.0.0/8").is_ok());
        assert!(validate_subnet("172.16.0.0/12").is_ok());
        assert!(validate_subnet("192.168.1.0/24").is_ok());
        assert!(validate_subnet("10.10.10.0/24").is_ok());
    }

    #[test]
    fn invalid_subnets() {
        // Public ranges
        assert!(validate_subnet("8.8.8.0/24").is_err());
        assert!(validate_subnet("1.1.1.0/24").is_err());
        // Malformed
        assert!(validate_subnet("not-a-cidr").is_err());
        assert!(validate_subnet("10.0.0.0").is_err());
        assert!(validate_subnet("10.0.0.0/33").is_err());
    }

    #[test]
    fn valid_ifaces() {
        assert!(validate_iface("bridge0").is_ok());
        assert!(validate_iface("bridge100").is_ok());
        assert!(validate_iface("bridge999").is_ok());
    }

    #[test]
    fn invalid_ifaces() {
        assert!(validate_iface("eth0").is_err());
        assert!(validate_iface("bridge").is_err());
        assert!(validate_iface("bridge100a").is_err());
        assert!(validate_iface("BRIDGE100").is_err());
    }

    #[test]
    fn valid_domains() {
        assert!(validate_domain("arcbox.local").is_ok());
        assert!(validate_domain("test.arcbox.local").is_ok());
        assert!(validate_domain("my-domain.local").is_ok());
    }

    #[test]
    fn invalid_domains() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("UPPER.case").is_err());
        assert!(validate_domain("has space.local").is_err());
        // 254 chars
        let long = "a".repeat(254);
        assert!(validate_domain(&long).is_err());
        // Path traversal / malformed labels
        assert!(validate_domain(".").is_err());
        assert!(validate_domain("..").is_err());
        assert!(validate_domain(".leading").is_err());
        assert!(validate_domain("trailing.").is_err());
        assert!(validate_domain("empty..label").is_err());
    }

    #[test]
    fn valid_ports() {
        assert!(validate_port(1024).is_ok());
        assert!(validate_port(5553).is_ok());
        assert!(validate_port(65535).is_ok());
    }

    #[test]
    fn invalid_ports() {
        assert!(validate_port(0).is_err());
        assert!(validate_port(80).is_err());
        assert!(validate_port(1023).is_err());
    }

    #[test]
    fn valid_socket_targets() {
        assert!(validate_socket_target("/Users/alice/.arcbox/run/docker.sock").is_ok());
        assert!(validate_socket_target("/Users/bob/.arcbox/run/arcbox.sock").is_ok());
    }

    #[test]
    fn invalid_socket_targets() {
        assert!(validate_socket_target("/tmp/docker.sock").is_err());
        assert!(validate_socket_target("/Users//.arcbox/run/docker.sock").is_err());
        assert!(validate_socket_target("/Users/alice/.config/docker.sock").is_err());
        assert!(validate_socket_target("/Users/alice/.arcbox/run/docker.txt").is_err());
    }

    #[test]
    fn socket_target_rejects_path_traversal() {
        let result = validate_socket_target("/Users/alice/.arcbox/../../../../var/run/other.sock");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".."));
    }
}
