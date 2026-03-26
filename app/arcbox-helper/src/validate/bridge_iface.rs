use std::str::FromStr;

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
