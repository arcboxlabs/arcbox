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
