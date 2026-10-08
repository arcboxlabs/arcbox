use std::sync::Mutex;

use arcbox_api::{SetupState, SystemServiceImpl};
use arcbox_connect::v1::{
    Empty, LocalHttpsAccess, LocalHttpsStatus, SystemService,
    local_https_status::{CertificateState, TrustState},
};
use connectrpc::{HasMessageView, RequestContext, ServiceRequest, ServiceStream};
use tokio_stream::StreamExt;

use super::*;

#[derive(Clone, Default)]
struct Recording {
    calls: Arc<Mutex<Vec<ReconcileHttpsAccessRequest>>>,
    response: Arc<Mutex<ReconcileHttpsAccessResponse>>,
    delay: Arc<Mutex<Duration>>,
}

impl Recording {
    fn calls(&self) -> Vec<ReconcileHttpsAccessRequest> {
        self.calls.lock().unwrap().clone()
    }
}

impl AccessSource for Recording {
    fn host_addresses(&self) -> Result<Vec<String>> {
        Ok(vec!["192.168.64.1".into()])
    }

    fn resolve(&self, hostname: &str) -> Option<IpAddr> {
        (hostname == "web.arcbox.local").then(|| "10.0.2.3".parse().unwrap())
    }

    async fn reconcile(
        &mut self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        let enabled = request.enabled;
        self.calls.lock().unwrap().push(request);
        let response = self.response.lock().unwrap().clone();
        let delay = *self.delay.lock().unwrap();
        if enabled {
            tokio::select! {
                () = cancelled.cancelled() => bail!("cancelled"),
                () = tokio::time::sleep(delay) => {},
            }
        }
        Ok(response)
    }
}

fn status(setup: &SetupState, state: TrustState) {
    setup.set_local_https(LocalHttpsStatus {
        trust_state: state.into(),
        certificate_state: CertificateState::Valid.into(),
        certificate_sha256: "root".into(),
        ..Default::default()
    });
}

async fn watch(setup: Arc<SetupState>) -> ServiceStream<LocalHttpsAccess> {
    let service = SystemServiceImpl::new(setup, Arc::default(), Arc::default());
    let body = bytes::Bytes::new();
    let view = Empty::decode_view(&body).unwrap();
    service
        .watch_https_access(
            RequestContext::new(Default::default()),
            ServiceRequest::from_parts(&view, &body),
        )
        .await
        .unwrap()
        .body
}

async fn settle(duration: Duration) {
    tokio::time::sleep(duration).await;
    tokio::task::yield_now().await;
}

async fn assert_no_event(client: &mut ServiceStream<LocalHttpsAccess>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(1), client.next())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn observes_only_with_a_ready_vm_and_client_then_disables_on_loss() {
    let setup = Arc::new(SetupState::new());
    status(&setup, TrustState::Untrusted);
    let source = Recording::default();
    let (vm_tx, vm) = watch::channel(VmLifecycleState::Stopped);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(observe(
        source.clone(),
        setup.https_access.clone(),
        vm,
        shutdown.clone(),
    ));
    settle(POLL_INTERVAL).await;
    let client = watch(setup.clone()).await;
    settle(POLL_INTERVAL).await;
    assert_eq!(source.calls(), []);
    vm_tx.send(VmLifecycleState::Running).unwrap();
    settle(Duration::ZERO).await;
    let first = source.calls().last().unwrap().clone();
    assert!(first.enabled);
    assert_eq!(first.host_addresses, ["192.168.64.1"]);
    settle(POLL_INTERVAL).await;
    assert!(source.calls().iter().filter(|call| call.enabled).count() >= 2);
    drop(client);
    settle(POLL_INTERVAL).await;
    let disabled = source.calls().last().unwrap().clone();
    assert!(!disabled.enabled);
    assert_eq!(disabled.observation_id, first.observation_id);
    let count = source.calls().len();
    settle(POLL_INTERVAL).await;
    assert_eq!(source.calls().len(), count);
    let _client = watch(setup.clone()).await;
    settle(POLL_INTERVAL).await;
    status(&setup, TrustState::Trusted);
    settle(POLL_INTERVAL).await;
    assert!(!source.calls().last().unwrap().enabled);
    status(&setup, TrustState::Untrusted);
    settle(POLL_INTERVAL).await;
    let current = source.calls().last().unwrap().clone();
    assert!(current.enabled);
    assert_ne!(first.observation_id, current.observation_id);
    vm_tx.send(VmLifecycleState::Stopped).unwrap();
    settle(Duration::ZERO).await;
    assert!(!source.calls().last().unwrap().enabled);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn validates_dns_and_rejects_an_inflight_response_from_an_earlier_cycle() {
    let setup = Arc::new(SetupState::new());
    status(&setup, TrustState::Untrusted);
    let mut client = watch(setup.clone()).await;
    assert_eq!(client.next().await.unwrap().unwrap().request_id, "");
    let source = Recording::default();
    *source.response.lock().unwrap() = ReconcileHttpsAccessResponse {
        hostname: "web.arcbox.local".into(),
        destination_ip: "10.0.2.4".into(),
        sequence: 1,
        ..Default::default()
    };
    let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(observe(
        source.clone(),
        setup.https_access.clone(),
        vm,
        shutdown.clone(),
    ));
    settle(Duration::ZERO).await;
    assert_no_event(&mut client).await;
    source.response.lock().unwrap().destination_ip = "invalid".into();
    settle(POLL_INTERVAL).await;
    assert_no_event(&mut client).await;
    source.response.lock().unwrap().destination_ip = "10.0.2.3".into();
    *source.delay.lock().unwrap() = Duration::from_millis(500);
    settle(POLL_INTERVAL).await;
    let previous = setup.https_access.observing_cycle().unwrap();
    status(&setup, TrustState::Trusted);
    status(&setup, TrustState::Untrusted);
    assert_ne!(setup.https_access.observing_cycle().unwrap(), previous);
    settle(Duration::from_millis(500)).await;
    assert_no_event(&mut client).await;
    *source.delay.lock().unwrap() = Duration::ZERO;
    settle(POLL_INTERVAL).await;
    let event = client.next().await.unwrap().unwrap();
    assert_eq!(event.hostname, "web.arcbox.local");
    assert_eq!(event.certificate_sha256, "root");
    shutdown.cancel();
    task.await.unwrap();
    assert!(!source.calls().last().unwrap().enabled);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_rpc_times_out_and_shutdown_disables_the_lease() {
    let setup = Arc::new(SetupState::new());
    status(&setup, TrustState::Untrusted);
    let _client = watch(setup.clone()).await;
    let source = Recording::default();
    *source.delay.lock().unwrap() = Duration::from_secs(60);
    let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(observe(
        source.clone(),
        setup.https_access.clone(),
        vm,
        shutdown.clone(),
    ));
    settle(Duration::ZERO).await;
    settle(POLL_INTERVAL).await;
    assert_eq!(source.calls().iter().filter(|call| call.enabled).count(), 2);
    shutdown.cancel();
    task.await.unwrap();
    let calls = source.calls();
    assert!(!calls.last().unwrap().enabled);
    assert_eq!(
        calls[0].observation_id,
        calls.last().unwrap().observation_id
    );
}
