//! One pending authorization per root; acknowledgement never grants native trust.

use arcbox_connect::v1::{
    AcknowledgeHttpsAccessRequest, LocalHttpsAccess, LocalHttpsStatus,
    acknowledge_https_access_request::Outcome, local_https_status::TrustState,
};
use connectrpc::{ConnectError, ServiceStream};
use tokio::sync::watch;

#[derive(Clone, Debug, Default)]
struct State {
    fingerprint: String,
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
    /// Returns the root to observe, or `None` when no GUI can authorize it.
    pub fn observing_fingerprint(&self) -> Option<String> {
        let state = self.state.borrow();
        (self.state.receiver_count() > 0 && state.untrusted && !state.suppressed)
            .then(|| state.fingerprint.clone())
    }

    /// Reconciles actual trust. Only native inspection can end the untrusted state.
    pub(super) fn update_status(&self, status: &LocalHttpsStatus) {
        self.state.send_if_modified(|state| {
            let untrusted = status.trust_state == TrustState::Untrusted
                && !status.certificate_sha256.is_empty();
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
            state.fingerprint.clone_from(&status.certificate_sha256);
            state.untrusted = untrusted;
            state.pending = None;
            true
        });
    }

    /// Publishes a validated Mac access and merges concurrent requests for the same root.
    pub fn record(&self, hostname: &str, fingerprint: &str) {
        if self.state.receiver_count() == 0 {
            return;
        }
        self.state.send_if_modified(|state| {
            if !state.untrusted
                || state.suppressed
                || state.fingerprint != fingerprint
                || state.pending.is_some()
            {
                return false;
            }
            state.pending = Some(LocalHttpsAccess {
                request_id: uuid::Uuid::new_v4().to_string(),
                hostname: hostname.into(),
                certificate_sha256: fingerprint.into(),
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
            certificate_sha256: fingerprint.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn concurrent_accesses_and_reconnections_keep_one_prompt() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("first"));
        access.record("ignored.arcbox.local", "first");
        assert!(access.observing_fingerprint().is_none());
        let mut first = access.watch();
        assert_eq!(first.next().await.unwrap().unwrap().request_id, "");
        access.record("web.arcbox.local", "first");
        let pending = first.next().await.unwrap().unwrap();
        access.record("other.arcbox.local", "first");
        let mut reconnect = access.watch();
        assert_eq!(reconnect.next().await.unwrap().unwrap(), pending);
        drop(first);
        drop(reconnect);
        assert!(access.observing_fingerprint().is_none());
    }

    #[tokio::test]
    async fn acknowledgement_suppresses_without_granting_trust() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("first"));
        let mut stream = access.watch();
        access.record("web.arcbox.local", "first");
        let request = AcknowledgeHttpsAccessRequest {
            request_id: stream.next().await.unwrap().unwrap().request_id,
            outcome: Outcome::Cancelled.into(),
            ..Default::default()
        };
        access.acknowledge(&request).unwrap();
        access.acknowledge(&request).unwrap();
        assert!(access.state.borrow().untrusted);
        assert!(access.observing_fingerprint().is_none());
        access.record("web.arcbox.local", "first");
        assert_eq!(stream.next().await.unwrap().unwrap().request_id, "");
        access.update_status(&untrusted("second"));
        assert_eq!(access.observing_fingerprint().as_deref(), Some("second"));
        access.record("web.arcbox.local", "first");
        assert!(access.state.borrow().pending.is_none());
        access.record("web.arcbox.local", "second");
        assert!(access.acknowledge(&request).is_err());
    }

    #[tokio::test]
    async fn native_trust_can_finish_before_acknowledgement() {
        let access = HttpsAccess::default();
        access.update_status(&untrusted("root"));
        let mut stream = access.watch();
        access.record("web.arcbox.local", "root");
        let mut request = AcknowledgeHttpsAccessRequest {
            request_id: stream.next().await.unwrap().unwrap().request_id,
            ..Default::default()
        };
        assert!(access.acknowledge(&request).is_err());
        assert!(access.state.borrow().pending.is_some());
        access.update_status(&LocalHttpsStatus {
            trust_state: TrustState::Trusted.into(),
            certificate_sha256: "root".into(),
            ..Default::default()
        });
        request.outcome = Outcome::Completed.into();
        access.acknowledge(&request).unwrap();
        assert!(access.state.borrow().pending.is_none());
        assert!(access.observing_fingerprint().is_none());
        access.update_status(&untrusted("root"));
        access.record("web.arcbox.local", "root");
        let next = access.state.borrow().pending.clone().unwrap();
        assert_ne!(next.request_id, request.request_id);
        access.acknowledge(&request).unwrap();
        assert_eq!(access.state.borrow().pending.as_ref(), Some(&next));
    }
}
