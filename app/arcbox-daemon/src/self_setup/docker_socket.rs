//! Ensures `/var/run/docker.sock` is symlinked to the daemon's Docker socket.

use std::path::PathBuf;

use arcbox_helper::client::{Client, ClientError};

use super::SetupTask;

pub struct DockerSocket {
    pub target: PathBuf,
}

#[async_trait::async_trait]
impl SetupTask for DockerSocket {
    fn name(&self) -> &'static str {
        "Docker socket"
    }

    fn is_satisfied(&self) -> bool {
        std::fs::read_link(arcbox_constants::paths::privileged::DOCKER_SOCKET)
            .is_ok_and(|existing| existing == self.target)
    }

    async fn apply(&self, client: &Client) -> Result<(), ClientError> {
        client.socket_link(&self.target.to_string_lossy()).await
    }
}
