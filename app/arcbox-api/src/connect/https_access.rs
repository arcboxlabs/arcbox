//! One pending authorization per root; acknowledgement never grants native trust.

use arcbox_connect::v1::{
    AcknowledgeHttpsAccessRequest, LocalHttpsAccess, LocalHttpsStatus,
    acknowledge_https_access_request::Outcome,
    local_https_status::{CertificateState, TrustState},
};
use connectrpc::{ConnectError, ServiceStream};
use tokio::sync::watch;

#[derive(Clone, Debug, Default)]
struct State {
    fingerprint: String,
    observation_id: String,
    untrusted: bool,
    suppressed: bool,
    pending: Option<LocalHttpsAccess>,
    acknowledged: Option<String>,
}

/// Tracks browser-originated requests while a login-session client observes them.
#[derive(Clone, Debug)]
pub struct HttpsAccess {
    state: watch::Sender<State>,
}

impl Default for HttpsAccess {
    fn default() -> Self {
        let (state, _) = watch::channel(State::default());
        Self { state }
    }
}

impl HttpsAccess {
    /// Returns the current trust cycle while a GUI can authorize a valid certificate.
    pub fn observing_cycle(&self) -> Option<String> {
        let state = self.state.borrow();
        (self.state.receiver_count() > 0 && state.untrusted && !state.suppressed)
            .then(|| state.observation_id.clone())
    }

    /// Reconciles certificate readiness. Acknowledgement never grants native trust.
    pub(super) fn update_status(&self, status: &LocalHttpsStatus) {
        self.state.send_if_modified(|state| {
            let untrusted = status.trust_state == TrustState::Untrusted
                && !status.certificate_sha256.is_empty()
                && (status.certificate_state == CertificateState::Valid
                    || status.certificate_state == CertificateState::RenewalDue);
            if state.fingerprint == status.certificate_sha256
                && state.untrusted == untrusted
                && !(status.trust_state == TrustState::Trusted && state.suppressed)
            {
                return false;
            }
            if state.fingerprint != status.certificate_sha256 {
                state.suppressed = false;
                state.acknowledged = None;
            } else if status.trust_state == TrustState::Trusted {
                state.suppressed = false;
                if let Some(pending) = &state.pending {
                    // Native inspection can finish before Desktop acknowledges the same request.
                    state.acknowledged = Some(pending.request_id.clone());
                }
            }
            if untrusted {
                // A reused certificate must not accept an earlier trust cycle's RPC response.
                state.observation_id = uuid::Uuid::new_v4().to_string();
            }
            state.fingerprint.clone_from(&status.certificate_sha256);
            state.untrusted = untrusted;
            state.pending = None;
            true
        });
    }

    /// Publishes a validated Mac access and merges concurrent requests for the same root.
    pub fn record(&self, hostname: &str, observation_id: &str) {
        if self.state.receiver_count() == 0 {
            return;
        }
        self.state.send_if_modified(|state| {
            if !state.untrusted
                || state.suppressed
                || state.observation_id != observation_id
                || state.pending.is_some()
            {
                return false;
            }
            state.pending = Some(LocalHttpsAccess {
                request_id: uuid::Uuid::new_v4().to_string(),
                hostname: hostname.into(),
                certificate_sha256: state.fingerprint.clone(),
                ..Default::default()
            });
            true
        });
    }

    pub(super) fn acknowledge(
        &self,
        request: &AcknowledgeHttpsAccessRequest,
    ) -> Result<(), ConnectError> {
        if request.outcome != Outcome::Completed
            && request.outcome != Outcome::Cancelled
            && request.outcome != Outcome::Failed
        {
            return Err(ConnectError::invalid_argument(
                "an HTTPS prompt outcome is required",
            ));
        }
        let mut matched = false;
        self.state.send_if_modified(|state| {
            if state.acknowledged.as_deref() == Some(&request.request_id) {
                matched = true;
                return false;
            }
            if state
                .pending
                .as_ref()
                .is_none_or(|pending| pending.request_id != request.request_id)
            {
                return false;
            }
            matched = true;
            state.suppressed = true;
            state.acknowledged = Some(request.request_id.clone());
            state.pending = None;
            true
        });
        if matched {
            Ok(())
        } else {
            Err(ConnectError::failed_precondition(
                "the HTTPS prompt is no longer pending",
            ))
        }
    }

    pub(super) fn watch(&self) -> ServiceStream<LocalHttpsAccess> {
        let mut receiver = self.state.subscribe();
        Box::pin(async_stream::stream! {
            let mut previous = None;
            loop {
                let pending = receiver.borrow_and_update().pending.clone().unwrap_or_default();
                if previous.as_ref() != Some(&pending) {
                    previous = Some(pending.clone());
                    yield Ok(pending);
                }
                if receiver.changed().await.is_err() { break; }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    fn untrusted(fingerprint: &str) -> LocalHttpsStatus {
        LocalHttpsStatus {
            trust_state: TrustState::Untrusted.into(),
            certificate_state: CertificateState::Valid.into(),
            certificate_sha256: fingerprint.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn certificate_validity_starts_and_cancels_observation_for_the_same_root() {
        for valid in [CertificateState::Valid, CertificateState::RenewalDue] {
            for inactive in [
                CertificateState::Unknown,
                CertificateState::Missing,
                CertificateState::Expired,
                CertificateState::NotYetValid,
            ] {
                let access = HttpsAccess::default();
                let mut stream = access.watch();
                let mut status = untrusted("root");
                status.certificate_state = inactive.into();
                access.update_status(&status);
                assert!(access.observing_cycle().is_none());

                status.certificate_state = valid.into();
                access.update_status(&status);
                let previous = access.observing_cycle().unwrap();
                access.record("web.arcbox.local", &previous);
                assert_ne!(stream.next().await.unwrap().unwrap().request_id, "");

                status.certificate_state = inactive.into();
                access.update_status(&status);
                assert!(access.observing_cycle().is_none());
                assert_eq!(stream.next().await.unwrap().unwrap().request_id, "");

                status.certificate_state = valid.into();
                access.update_status(&status);
                let current = access.observing_cycle().unwrap();
                assert_ne!(previous, current);
                access.record("stale.arcbox.local", &previous);
                assert!(access.state.borrow().pending.is_none());
                access.record("web.arcbox.local", &current);
                assert_eq!(
                    stream.next().await.unwrap().unwrap().hostname,
                    "web.arcbox.local"
                );
            }
        }
    }

    #[tokio::test]
    async fn concurrent_accesses_and_reconnections_keep_one_prompt() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("first"));
        access.record("ignored.arcbox.local", "first");
        assert!(access.observing_cycle().is_none());
        let mut first = access.watch();
        assert_eq!(first.next().await.unwrap().unwrap().request_id, "");
        let cycle = access.observing_cycle().unwrap();
        access.record("web.arcbox.local", &cycle);
        let pending = first.next().await.unwrap().unwrap();
        access.record("other.arcbox.local", &cycle);
        let mut reconnect = access.watch();
        assert_eq!(reconnect.next().await.unwrap().unwrap(), pending);
        drop(first);
        drop(reconnect);
        assert!(access.observing_cycle().is_none());
    }

    #[tokio::test]
    async fn acknowledgement_suppresses_without_granting_trust() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("first"));
        let mut stream = access.watch();
        let first_cycle = access.observing_cycle().unwrap();
        access.record("web.arcbox.local", &first_cycle);
        let request = AcknowledgeHttpsAccessRequest {
            request_id: stream.next().await.unwrap().unwrap().request_id,
            outcome: Outcome::Cancelled.into(),
            ..Default::default()
        };
        access.acknowledge(&request).unwrap();
        access.acknowledge(&request).unwrap();
        assert!(access.state.borrow().untrusted);
        assert!(access.observing_cycle().is_none());
        access.record("web.arcbox.local", &first_cycle);
        assert_eq!(stream.next().await.unwrap().unwrap().request_id, "");
        access.update_status(&untrusted("second"));
        let second_cycle = access.observing_cycle().unwrap();
        assert_ne!(first_cycle, second_cycle);
        access.record("web.arcbox.local", &first_cycle);
        assert!(access.state.borrow().pending.is_none());
        access.record("web.arcbox.local", &second_cycle);
        assert!(access.acknowledge(&request).is_err());
    }

    #[tokio::test]
    async fn native_trust_can_finish_before_acknowledgement() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("root"));
        let mut stream = access.watch();
        let first_cycle = access.observing_cycle().unwrap();
        access.record("web.arcbox.local", &first_cycle);
        let mut request = AcknowledgeHttpsAccessRequest {
            request_id: stream.next().await.unwrap().unwrap().request_id,
            ..Default::default()
        };
        assert!(access.acknowledge(&request).is_err());
        assert!(access.state.borrow().pending.is_some());
        access.update_status(&LocalHttpsStatus {
            trust_state: TrustState::Trusted.into(),
            certificate_state: CertificateState::Valid.into(),
            certificate_sha256: "root".into(),
            ..Default::default()
        });
        request.outcome = Outcome::Completed.into();
        access.acknowledge(&request).unwrap();
        assert!(access.state.borrow().pending.is_none());
        assert!(access.observing_cycle().is_none());
        access.update_status(&untrusted("root"));
        let next_cycle = access.observing_cycle().unwrap();
        assert_ne!(first_cycle, next_cycle);
        access.record("web.arcbox.local", &next_cycle);
        let next = access.state.borrow().pending.clone().unwrap();
        assert_ne!(next.request_id, request.request_id);
        access.acknowledge(&request).unwrap();
        assert_eq!(access.state.borrow().pending.as_ref(), Some(&next));
    }

    #[tokio::test]
    async fn service_watch_counts_clients_and_rejects_a_previous_trust_cycle() {
        use std::sync::Arc;

        use arcbox_connect::v1::{Empty, SystemService};
        use buffa::Message as _;
        use connectrpc::{HasMessageView, RequestContext, ServiceRequest};

        use crate::{SetupState, SystemServiceImpl};

        let setup = Arc::new(SetupState::new());
        let access = &setup.https_access;
        setup.set_local_https(untrusted("root"));
        assert!(access.observing_cycle().is_none());
        let service = SystemServiceImpl::new(setup.clone(), Arc::default(), Arc::default());
        let body = bytes::Bytes::new();
        let view = Empty::decode_view(&body).unwrap();
        let request = ServiceRequest::from_parts(&view, &body);
        let mut stream = service
            .watch_https_access(RequestContext::new(Default::default()), request)
            .await
            .unwrap()
            .body;
        let previous = access.observing_cycle().unwrap();
        assert_eq!(access.state.receiver_count(), 1);
        setup.set_local_https(LocalHttpsStatus {
            trust_state: TrustState::Trusted.into(),
            certificate_state: CertificateState::Valid.into(),
            certificate_sha256: "root".into(),
            ..Default::default()
        });
        setup.set_local_https(untrusted("root"));
        let current = access.observing_cycle().unwrap();
        assert_ne!(previous, current);
        access.record("stale.arcbox.local", &previous);
        assert_eq!(stream.next().await.unwrap().unwrap().request_id, "");
        access.record("web.arcbox.local", &current);
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.hostname, "web.arcbox.local");
        assert_eq!(event.certificate_sha256, "root");
        let body = bytes::Bytes::from(
            AcknowledgeHttpsAccessRequest {
                request_id: event.request_id,
                outcome: Outcome::Cancelled.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        let view = AcknowledgeHttpsAccessRequest::decode_view(&body).unwrap();
        service
            .acknowledge_https_access(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&view, &body),
            )
            .await
            .unwrap();
        assert!(access.observing_cycle().is_none());
        assert_eq!(access.state.receiver_count(), 1);
        drop(stream);
        assert_eq!(access.state.receiver_count(), 0);
        assert!(access.observing_cycle().is_none());
    }
}
