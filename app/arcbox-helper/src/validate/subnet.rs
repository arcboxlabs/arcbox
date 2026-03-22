use std::str::FromStr;

use ipnetwork::Ipv4Network;

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

        let net: Ipv4Network = s.parse().map_err(|e| format!("invalid CIDR '{s}': {e}"))?;

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
