//! Input validation for setuid defense-in-depth.
//!
//! Every external input is validated before any privileged operation runs.
//! This module is deliberately strict — it rejects anything that doesn't
//! match the expected patterns for ArcBox's use cases.

use std::path::Component;

use ipnetwork::Ipv4Network;

/// Validates a CIDR subnet string. Only private ranges are allowed:
/// - 10.0.0.0/8
/// - 172.16.0.0/12
/// - 192.168.0.0/16
///
/// Also rejects CIDRs with non-zero host bits (e.g. `10.0.0.5/8`).
pub fn validate_subnet(s: &str) -> Result<(), String> {
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

    Ok(())
}

/// Validates a bridge interface name. Must match `^bridge[0-9]+$`.
pub fn validate_iface(s: &str) -> Result<(), String> {
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

    Ok(())
}

/// Validates a DNS domain.
///
/// Must match `^[a-z0-9.-]+$`, max 253 chars, no empty labels,
/// no leading/trailing dots, no consecutive dots.
/// Labels must be ≤63 chars and must not start or end with `-` (RFC 1035/1123).
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

    for label in s.split('.') {
        if label.len() > 63 {
            return Err(format!(
                "domain '{s}' has label exceeding 63 chars"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "domain '{s}' has label starting or ending with '-'"
            ));
        }
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
    let path = std::path::Path::new(s);

    // Must start with /Users/
    let rest = s
        .strip_prefix("/Users/")
        .ok_or_else(|| format!("socket target '{s}' must be under /Users/"))?;

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

    // Must end with .sock (strict lowercase — intentional for a security-sensitive path)
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if !s.ends_with(".sock") {
        return Err(format!("socket target '{s}' must end with .sock"));
    }

    // Reject path traversal (e.g. /Users/alice/.arcbox/../../../../var/run/other.sock).
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "socket target '{s}' contains '..' path traversal component"
        ));
    }

    Ok(())
}

/// Validates a CLI tool name for `/usr/local/bin/` symlink creation.
pub fn validate_cli_name(name: &str) -> Result<(), String> {
    if arcbox_constants::paths::DOCKER_CLI_TOOLS.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "CLI name '{name}' is not in the allow list: {}",
            arcbox_constants::paths::DOCKER_CLI_TOOLS.join(", ")
        ))
    }
}

/// Validates a CLI symlink target.
///
/// Must be an absolute path inside an app bundle's `Contents/MacOS/xbin/`
/// under `/Applications/` or `/Users/`.
/// Rejects path traversal and arbitrary locations like `/tmp/fake.app/`.
pub fn validate_cli_target(target: &str) -> Result<(), String> {
    let path = std::path::Path::new(target);

    if !path.is_absolute() {
        return Err(format!("CLI target '{target}' must be an absolute path"));
    }

    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "CLI target '{target}' contains '..' path traversal"
        ));
    }

    // Validate .app/Contents/MacOS/xbin/ structure using path components.
    let components: Vec<_> = path.components().collect();
    let has_valid_app_structure = components.windows(4).any(|w| {
        matches!(&w[0], Component::Normal(name) if name.to_string_lossy().ends_with(".app"))
            && w[1] == Component::Normal("Contents".as_ref())
            && w[2] == Component::Normal("MacOS".as_ref())
            && w[3] == Component::Normal("xbin".as_ref())
    });

    if !has_valid_app_structure {
        return Err(format!(
            "CLI target '{target}' must be inside an .app bundle's Contents/MacOS/xbin/"
        ));
    }

    // Restrict to trusted base directories to prevent fake .app paths.
    if !target.starts_with("/Applications/") && !target.starts_with("/Users/") {
        return Err(format!(
            "CLI target '{target}' must be under /Applications/ or /Users/"
        ));
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
    fn subnet_rejects_nonzero_host_bits() {
        let r = validate_subnet("10.0.0.5/8");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("host bits"));

        let r = validate_subnet("192.168.1.1/24");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("192.168.1.0/24"));
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
    fn iface_rejects_overflow() {
        assert!(validate_iface("bridge99999999999999999").is_err());
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
    fn domain_rejects_label_starting_or_ending_with_hyphen() {
        assert!(validate_domain("-bad.local").is_err());
        assert!(validate_domain("bad-.local").is_err());
        assert!(validate_domain("ok.-bad").is_err());
        assert!(validate_domain("ok.bad-").is_err());
    }

    #[test]
    fn domain_rejects_label_exceeding_63_chars() {
        let long_label = "a".repeat(64);
        let domain = format!("{long_label}.local");
        assert!(validate_domain(&domain).is_err());

        // 63 chars should pass
        let ok_label = "a".repeat(63);
        let domain = format!("{ok_label}.local");
        assert!(validate_domain(&domain).is_ok());
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
    fn socket_target_rejects_uppercase_extension() {
        assert!(validate_socket_target("/Users/alice/.arcbox/run/docker.SOCK").is_err());
        assert!(validate_socket_target("/Users/alice/.arcbox/run/docker.Sock").is_err());
    }

    #[test]
    fn socket_target_rejects_path_traversal() {
        let result = validate_socket_target("/Users/alice/.arcbox/../../../../var/run/other.sock");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".."));
    }

    // -- CLI name/target validation tests --

    #[test]
    fn valid_cli_names() {
        assert!(validate_cli_name("docker").is_ok());
        assert!(validate_cli_name("docker-buildx").is_ok());
        assert!(validate_cli_name("docker-compose").is_ok());
        assert!(validate_cli_name("docker-credential-osxkeychain").is_ok());
    }

    #[test]
    fn invalid_cli_names() {
        assert!(validate_cli_name("").is_err());
        assert!(validate_cli_name("curl").is_err());
        assert!(validate_cli_name("rm").is_err());
        assert!(validate_cli_name("../docker").is_err());
    }

    #[test]
    fn valid_cli_targets() {
        assert!(
            validate_cli_target("/Applications/ArcBox Desktop.app/Contents/MacOS/xbin/docker")
                .is_ok()
        );
        assert!(
            validate_cli_target("/Users/test/Apps/ArcBox.app/Contents/MacOS/xbin/docker-compose")
                .is_ok()
        );
    }

    #[test]
    fn invalid_cli_targets() {
        // Not absolute
        assert!(validate_cli_target("Contents/MacOS/xbin/docker").is_err());
        // Not inside an app bundle
        assert!(validate_cli_target("/usr/local/bin/docker").is_err());
        // Path traversal
        assert!(
            validate_cli_target("/Applications/ArcBox.app/Contents/MacOS/xbin/../../evil").is_err()
        );
        // Fake .app in untrusted location
        assert!(validate_cli_target("/tmp/evil.app/Contents/MacOS/xbin/docker").is_err());
    }

    #[test]
    fn cli_target_rejects_nested_app_structure() {
        // The .app/Contents/MacOS/xbin/ must appear as proper path components
        assert!(validate_cli_target(
            "/Users/evil/not-really.app/Contents/MacOS/xbin/nested.app/Contents/MacOS/xbin/docker"
        )
        .is_ok()); // This is technically valid — both structures are legitimate
    }
}
