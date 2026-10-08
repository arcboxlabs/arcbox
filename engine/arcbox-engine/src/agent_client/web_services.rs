//! Guest-owned web route snapshots over both agent transports.

use arcbox_connect::v1::{ListContainerWebServicesRequest, ListContainerWebServicesResponse};
use arcbox_constants::wire::MessageType;
use buffa::Message;

use super::AgentClient;
use crate::error::Result;

impl AgentClient {
    /// Lists installed container web routes and their current DNS aliases.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails.
    pub async fn list_container_web_services(
        &mut self,
    ) -> Result<ListContainerWebServicesResponse> {
        self.unary_rpc(
            MessageType::ListContainerWebServicesRequest,
            &ListContainerWebServicesRequest::default().encode_to_vec(),
            MessageType::ListContainerWebServicesResponse,
        )
        .await
    }

    /// Lists container web routes on the HV socketpair. Call from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent request fails.
    pub fn list_container_web_services_blocking(
        &mut self,
    ) -> Result<ListContainerWebServicesResponse> {
        self.unary_rpc_blocking(
            MessageType::ListContainerWebServicesRequest,
            &ListContainerWebServicesRequest::default().encode_to_vec(),
            MessageType::ListContainerWebServicesResponse,
        )
    }
}
