use super::*;
use crate::config::Config;
use crate::vm_lifecycle::DEFAULT_MACHINE_NAME;
use arcbox_net::darwin::inbound_relay::{InboundCommand, InboundProtocol};
use std::future::{Future, poll_fn};
use std::net::Ipv4Addr;
use std::task::Poll;
use tokio::sync::mpsc;

fn runtime() -> (Runtime, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(Config {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    })
    .unwrap();
    (runtime, dir)
}

async fn cancel_pending<T>(future: impl Future<Output = T>) {
    let mut future = std::pin::pin!(future);
    assert!(
        poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}

impl Runtime {
    pub(in super::super) async fn adopt_test_manager(
        &self,
        name: &str,
        manager: InboundListenerManager,
    ) {
        self.adopt_inbound_listener_manager(name, || {
            Ok(Some((VmIncarnation::for_test(), manager)))
        })
        .await
        .unwrap();
    }

    pub(in super::super) async fn start_test_port_forwarding(
        &self,
        name: &str,
        owner: &str,
        bindings: &[(String, u16, u16, String)],
    ) -> crate::Result<()> {
        let incarnation = self.inbound_listeners.read().await[name]
            .incarnation
            .clone();
        self.start_port_forwarding_inbound(name, owner, bindings, &incarnation)
            .await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn canceled_adoption_retains_both_managers_and_defers_the_next_transfer() {
    let (runtime, _dir) = runtime();
    let (stale_tx, stale_rx) = mpsc::channel(4);
    let mut stale = InboundListenerManager::new(stale_tx);
    for ip in [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED] {
        stale
            .add_rule(ip, 0, 80, InboundProtocol::Tcp)
            .await
            .unwrap();
    }
    runtime
        .inbound_listeners
        .write()
        .await
        .insert("test".into(), stale.into());
    runtime.inbound_rules.write().await.insert(
        "web".into(),
        (
            "test".into(),
            vec![(Ipv4Addr::LOCALHOST, 0, 80, InboundProtocol::Tcp)],
        ),
    );
    let (fresh_tx, mut fresh_rx) = mpsc::channel(4);
    let mut source = Some(InboundListenerManager::new(fresh_tx));
    let blocked = runtime.inbound_listeners.write().await;
    cancel_pending(runtime.adopt_inbound_listener_manager("test", || {
        Ok(source
            .take()
            .map(|manager| (VmIncarnation::for_test(), manager)))
    }))
    .await;
    assert!(
        source.is_some(),
        "the source retains its manager while Runtime waits for locks"
    );
    drop(blocked);

    // Current-thread execution cannot finish an aborted task before the first pending poll.
    cancel_pending(runtime.adopt_inbound_listener_manager("test", || {
        Ok(source
            .take()
            .map(|manager| (VmIncarnation::for_test(), manager)))
    }))
    .await;
    assert!(source.is_none());
    assert!(
        !fresh_rx.is_closed(),
        "Runtime must retain the fresh command sender"
    );
    assert!(runtime.inbound_rules.read().await.is_empty());
    assert_eq!(
        runtime.inbound_listeners.read().await["test"]
            .retiring
            .as_ref()
            .unwrap()
            .len(),
        2
    );

    let (next_tx, _next_rx) = mpsc::channel(4);
    let mut next = Some(InboundListenerManager::new(next_tx));
    cancel_pending(runtime.adopt_inbound_listener_manager("test", || {
        Ok(next
            .take()
            .map(|manager| (VmIncarnation::for_test(), manager)))
    }))
    .await;
    assert!(
        next.is_some(),
        "retirement must finish before another source transfer"
    );
    runtime
        .adopt_inbound_listener_manager("test", || Ok(None))
        .await
        .unwrap();
    assert!(
        stale_rx.is_closed(),
        "all stale listener tasks and their manager must be gone"
    );
    let port = {
        let mut slots = runtime.inbound_listeners.write().await;
        let slot = slots.get_mut("test").unwrap();
        assert!(slot.retiring.is_none());
        slot.current
            .add_rule(Ipv4Addr::LOCALHOST, 0, 81, InboundProtocol::Tcp)
            .await
            .unwrap()
    };
    let _stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let command = tokio::time::timeout(std::time::Duration::from_secs(1), fresh_rx.recv())
        .await
        .unwrap();
    assert!(matches!(
        command,
        Some(InboundCommand::TcpAccepted {
            container_port: 81,
            ..
        })
    ));
    runtime.stop_port_forwarding_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn canceled_retirement_and_shutdown_keep_cleanup_owners() {
    for retire_system_vm in [true, false] {
        let (runtime, _dir) = runtime();
        let (current_tx, current_rx) = mpsc::channel(4);
        let (stale_tx, stale_rx) = mpsc::channel(4);
        let mut current = InboundListenerManager::new(current_tx);
        let mut stale = InboundListenerManager::new(stale_tx);
        current
            .add_rule(Ipv4Addr::LOCALHOST, 0, 80, InboundProtocol::Tcp)
            .await
            .unwrap();
        stale
            .add_rule(Ipv4Addr::LOCALHOST, 0, 80, InboundProtocol::Tcp)
            .await
            .unwrap();
        runtime.inbound_listeners.write().await.insert(
            DEFAULT_MACHINE_NAME.into(),
            Slot {
                current,
                incarnation: VmIncarnation::for_test(),
                retiring: Some(stale),
            },
        );
        let cleanup = || async {
            if retire_system_vm {
                runtime.retire_system_vm_container_networking().await;
            } else {
                runtime.stop_port_forwarding_all().await;
            }
        };
        cancel_pending(cleanup()).await;
        let slots = runtime.inbound_listeners.read().await;
        let slot = &slots[DEFAULT_MACHINE_NAME];
        assert!(slot.retiring.is_some());
        assert_eq!(slot.current.len(), 1);
        drop(slots);
        cleanup().await;
        assert!(runtime.inbound_listeners.read().await.is_empty());
        assert!(current_rx.is_closed());
        assert!(stale_rx.is_closed());
    }
}

#[tokio::test]
async fn stale_forwarding_cannot_remove_or_bind_through_a_replacement_slot() {
    let (runtime, _dir) = runtime();
    let (old_tx, _old_rx) = mpsc::channel(4);
    runtime
        .adopt_test_manager("test", InboundListenerManager::new(old_tx))
        .await;
    let old = runtime.inbound_listeners.read().await["test"]
        .incarnation
        .clone();
    let (new_tx, mut new_rx) = mpsc::channel(4);
    let mut fresh = InboundListenerManager::new(new_tx);
    let port = fresh
        .add_rule(Ipv4Addr::LOCALHOST, 0, 80, InboundProtocol::Tcp)
        .await
        .unwrap();
    runtime.adopt_test_manager("test", fresh).await;
    runtime.inbound_rules.write().await.insert(
        "web".into(),
        (
            "test".into(),
            vec![(Ipv4Addr::LOCALHOST, 0, 80, InboundProtocol::Tcp)],
        ),
    );
    for bindings in [vec![], vec![("127.0.0.1".into(), 0, 80, "tcp".into())]] {
        assert!(
            runtime
                .start_port_forwarding_inbound("test", "web", &bindings, &old)
                .await
                .is_err()
        );
        assert!(runtime.inbound_rules.read().await.contains_key("web"));
    }
    let _stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), new_rx.recv())
            .await
            .unwrap(),
        Some(InboundCommand::TcpAccepted {
            container_port: 80,
            ..
        })
    ));
    runtime.stop_port_forwarding_all().await;
}
