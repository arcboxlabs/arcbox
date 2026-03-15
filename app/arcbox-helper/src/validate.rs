//! Input validation for helper operations.

use std::net::Ipv4Addr;

/// Validates a utun interface name (e.g. "utun13").
pub fn is_valid_utun_name(name: &str) -> bool {
    name.starts_with("utun")
        && name.len() > 4
        && name[4..].chars().all(|c| c.is_ascii_digit())
}

/// Validates an IPv4 address string.
pub fn is_valid_ipv4(ip: &str) -> bool {
    ip.parse::<Ipv4Addr>().is_ok()
}

/// Validates a CIDR subnet string (e.g. "172.16.0.0/12").
pub fn is_valid_cidr(cidr: &str) -> bool {
    let Some((addr, prefix)) = cidr.split_once('/') else {
        return false;
    };
    addr.parse::<Ipv4Addr>().is_ok()
        && prefix
            .parse::<u8>()
            .is_ok_and(|p| p <= 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utun_names() {
        assert!(is_valid_utun_name("utun0"));
        assert!(is_valid_utun_name("utun13"));
        assert!(is_valid_utun_name("utun999"));
        assert!(!is_valid_utun_name("utun"));
        assert!(!is_valid_utun_name("utunX"));
        assert!(!is_valid_utun_name("lo0"));
        assert!(!is_valid_utun_name("en0"));
        assert!(!is_valid_utun_name(""));
    }

    #[test]
    fn ipv4() {
        assert!(is_valid_ipv4("240.0.0.1"));
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(!is_valid_ipv4("not-ip"));
        assert!(!is_valid_ipv4(""));
        assert!(!is_valid_ipv4("::1"));
    }

    #[test]
    fn cidr() {
        assert!(is_valid_cidr("172.16.0.0/12"));
        assert!(is_valid_cidr("10.88.0.0/16"));
        assert!(is_valid_cidr("0.0.0.0/0"));
        assert!(!is_valid_cidr("172.16.0.0"));
        assert!(!is_valid_cidr("172.16.0.0/33"));
        assert!(!is_valid_cidr("bad/12"));
        assert!(!is_valid_cidr(""));
    }
}
