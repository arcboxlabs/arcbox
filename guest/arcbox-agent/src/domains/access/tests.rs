use super::*;

fn request(id: &str, enabled: bool) -> ReconcileHttpsAccessRequest {
    ReconcileHttpsAccessRequest {
        observation_id: id.into(),
        host_addresses: vec!["192.168.64.1".into()],
        enabled,
        ..Default::default()
    }
}

fn connect(access: &Access, name: &'static str) -> tokio::task::JoinHandle<()> {
    let access = access.clone();
    tokio::spawn(async move {
        access
            .observe(
                name,
                Ipv4Addr::new(172, 17, 0, 2),
                "192.168.64.1".parse().unwrap(),
            )
            .await;
    })
}

#[tokio::test(start_paused = true)]
async fn no_observer_siblings_and_expired_observers_never_wait_or_report() {
    let access = Access::default();
    let start = Instant::now();
    connect(&access, "web.arcbox.local").await.unwrap();
    access.reconcile(request("one", true)).unwrap();
    access
        .observe(
            "web.arcbox.local",
            Ipv4Addr::LOCALHOST,
            "172.17.0.3".parse().unwrap(),
        )
        .await;
    assert_eq!(Instant::now(), start);
    assert_eq!(access.reconcile(request("one", true)).unwrap().sequence, 0);
    tokio::time::advance(LEASE).await;
    connect(&access, "web.arcbox.local").await.unwrap();
    assert_eq!(Instant::now(), start + LEASE);
    assert_eq!(access.reconcile(request("one", true)).unwrap().sequence, 0);
}

#[tokio::test(start_paused = true)]
async fn concurrent_accesses_keep_the_first_event_and_cancel_together() {
    let access = Access::default();
    access.reconcile(request("one", true)).unwrap();
    let first = connect(&access, "first.arcbox.local");
    tokio::task::yield_now().await;
    let second = connect(&access, "second.arcbox.local");
    tokio::task::yield_now().await;
    let updates = access.0.subscribe();
    let event = access.reconcile(request("one", true)).unwrap();
    assert_eq!(event.hostname, "first.arcbox.local");
    assert_eq!(event.destination_ip, "172.17.0.2");
    assert_eq!(event.sequence, 1);
    assert!(
        !updates.has_changed().unwrap(),
        "renewal must not wake waiters"
    );
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    assert_eq!(access.reconcile(request("one", false)).unwrap(), event);
    first.await.unwrap();
    second.await.unwrap();
    connect(&access, "retry.arcbox.local").await.unwrap();
    assert_eq!(access.reconcile(request("one", false)).unwrap(), event);
}

#[tokio::test(start_paused = true)]
async fn new_observation_clears_the_event_and_releases_old_handshakes() {
    let access = Access::default();
    access.reconcile(request("one", true)).unwrap();
    let first = connect(&access, "web.arcbox.local");
    tokio::task::yield_now().await;
    assert_eq!(access.reconcile(request("two", true)).unwrap().sequence, 0);
    first.await.unwrap();
    let second = connect(&access, "web.arcbox.local");
    tokio::task::yield_now().await;
    assert_eq!(access.reconcile(request("two", true)).unwrap().sequence, 2);
    access.reconcile(request("two", false)).unwrap();
    second.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn lease_renewal_extends_wait_but_host_loss_releases_it() {
    let access = Access::default();
    access.reconcile(request("one", true)).unwrap();
    let start = Instant::now();
    let handshake = connect(&access, "web.arcbox.local");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    access.reconcile(request("one", true)).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!handshake.is_finished());
    handshake.await.unwrap();
    assert_eq!(Instant::now(), start + Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn continual_renewal_cannot_exceed_the_handshake_wait_budget() {
    let access = Access::default();
    access.reconcile(request("one", true)).unwrap();
    let start = Instant::now();
    let handshake = connect(&access, "web.arcbox.local");
    tokio::task::yield_now().await;
    for _ in 0..29 {
        tokio::time::advance(Duration::from_secs(1)).await;
        access.reconcile(request("one", true)).unwrap();
    }
    assert!(!handshake.is_finished());
    handshake.await.unwrap();
    assert_eq!(Instant::now(), start + MAX_WAIT);
}

#[test]
fn invalid_host_addresses_do_not_replace_the_observation() {
    let access = Access::default();
    access.reconcile(request("one", true)).unwrap();
    let mut invalid = request("two", true);
    invalid.host_addresses = vec!["192.168.64.0/24".into()];
    assert!(access.reconcile(invalid).is_err());
    assert_eq!(access.0.borrow().observation.as_ref().unwrap().id, "one");
    assert!(access.reconcile(request("", true)).is_err());
}
