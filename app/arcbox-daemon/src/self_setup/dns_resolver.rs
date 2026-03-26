//! Ensures `/etc/resolver/<domain>` points to the daemon's DNS server.

use arcbox_helper::client::{Client, ClientError};

use super::SetupTask;

pub struct DnsResolver {
    pub domain: String,
    pub port: u16,
}

#[async_trait::async_trait]
impl SetupTask for DnsResolver {
    fn name(&self) -> &'static str {
        "DNS resolver"
    }

    fn is_satisfied(&self) -> bool {
        let path = format!("/etc/resolver/{}", self.domain);
        std::fs::read_to_string(&path).is_ok_and(|content| {
            content.contains("nameserver 127.0.0.1")
                && content.contains(&format!("port {}", self.port))
        })
    }

    async fn apply(&self, client: &Client) -> Result<(), ClientError> {
        client.dns_install(&self.domain, self.port).await
    }
}
