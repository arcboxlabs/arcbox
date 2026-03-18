//! Ensures `/etc/resolver/<domain>` points to the daemon's DNS server.

use std::path::Path;

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
        Path::new(&format!("/etc/resolver/{}", self.domain)).exists()
    }

    async fn apply(&self, client: &Client) -> Result<(), ClientError> {
        client.dns_install(&self.domain, self.port).await
    }
}
