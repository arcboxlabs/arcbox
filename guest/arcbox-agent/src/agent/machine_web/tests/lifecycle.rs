//! Readiness, tracking acknowledgments, and cancellation retain one machine owner.

use super::*;
use crate::domains::Command;
use crate::rpc::{MessageType, write_message};
use buffa::Message as _;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, DuplexStream};

async fn assert_no_output(client: &mut DuplexStream) {
    let mut byte = [0];
    let read = std::pin::pin!(client.read(&mut byte));
    assert!(futures::poll!(read).is_pending());
}

fn assert_withdrawn(receiver: &Receiver) {
    assert!(receiver.state.lock().unwrap().configuration.is_none());
    assert!(receiver.authority.borrow().is_none());
    assert!(
        receiver
            .registry
            .read()
            .unwrap()
            .get("machine:my_vm")
            .is_none()
    );
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
}

async fn wait_for_release(receiver: &Receiver) {
    while receiver.state.lock().unwrap().active {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn open_waits_for_frontend_and_ack_waits_for_tracking_or_cancellation() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for cancel_update in [false, true] {
            let receiver = receiver();
            let (mut client, stream) = tokio::io::duplex(4096);
            let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
            let (routes, mut commands) =
                DomainRoutes::test_channel_with_services(receiver.web.clone());
            let (ready, readiness) = oneshot::channel();
            let (finish_cleanup, cleanup) = oneshot::channel();
            let routing = Routing {
                routes,
                ready: readiness,
                task: tokio::spawn(async move {
                    cleanup.await.unwrap();
                    Ok(())
                }),
            };
            let serving = session.serve(stream, "open", routing);
            tokio::pin!(serving);
            assert!(futures::poll!(&mut serving).is_pending());
            assert!(
                receiver
                    .registry
                    .read()
                    .unwrap()
                    .get("machine:my_vm")
                    .is_none()
            );
            assert_no_output(&mut client).await;
            ready.send(()).unwrap();
            assert!(futures::poll!(&mut serving).is_pending());
            assert_eq!(
                read_message(&mut client).await.unwrap().0,
                MessageType::OpenMachineWebResponse
            );
            assert!(
                receiver
                    .registry
                    .read()
                    .unwrap()
                    .get("machine:my_vm")
                    .is_some()
            );
            let mut request = replacement(1, Selection::Port(8080), Selection::Off(Box::default()));
            request.ca_certificate_pem = Some(CA_PEM.to_vec());
            write_message(
                &mut client,
                MessageType::ReplaceMachineWebRequest,
                "replace",
                &request.encode_to_vec(),
            )
            .await
            .unwrap();
            assert!(futures::poll!(&mut serving).is_pending());
            let Command::Update(owner, Some(facts), reply) = commands.try_recv().unwrap() else {
                panic!("expected complete machine tracking update");
            };
            let mut expected = EndpointFacts::machine(
                open().machine_name,
                open().hostname.parse().unwrap(),
                bridge(),
            );
            expected.replace_machine_ports(&request.ports).unwrap();
            assert_eq!(owner, "machine:my_vm");
            assert_eq!(facts, expected);
            assert!(receiver.authority.borrow().is_some());
            assert_no_output(&mut client).await;
            if cancel_update {
                session.withdraw();
                assert!(futures::poll!(&mut serving).is_pending());
                assert_withdrawn(&receiver);
                assert!(reply.send(()).is_err());
            } else {
                reply.send(()).unwrap();
                assert!(futures::poll!(&mut serving).is_pending());
                let (kind, trace, payload) = read_message(&mut client).await.unwrap();
                assert_eq!(kind, MessageType::MachineWebAck);
                assert_eq!(trace, "replace");
                assert_eq!(
                    MachineWebAck::decode_from_slice(&payload)
                        .unwrap()
                        .source_revision,
                    1
                );
                client.shutdown().await.unwrap();
            }
            finish_cleanup.send(()).unwrap();
            assert_eq!(serving.await.is_ok(), cancel_update);
            let mut remaining = Vec::new();
            client.read_to_end(&mut remaining).await.unwrap();
            assert!(remaining.is_empty(), "unexpected response after shutdown");
            assert_withdrawn(&receiver);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_during_initialization_retains_admission_without_publishing() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let receiver = receiver();
        let (mut client, stream) = tokio::io::duplex(4096);
        let (entered, starting) = oneshot::channel();
        let (resume, operation) = oneshot::channel();
        let serving = receiver.clone();
        let task = tokio::spawn(async move {
            serving
                .serve_with(stream, "open", open(), Some(bridge()), |session, ready| {
                    let (routes, _commands) =
                        DomainRoutes::test_channel_with_services(session.receiver.web.clone());
                    (routes, async move {
                        entered.send(()).unwrap();
                        operation.await.unwrap();
                        let _ = ready.send(());
                        Ok(())
                    })
                })
                .await
        });
        starting.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
        assert_withdrawn(&receiver);
        resume.send(()).unwrap();
        wait_for_release(&receiver).await;
        assert_withdrawn(&receiver);
        let mut responses = Vec::new();
        client.read_to_end(&mut responses).await.unwrap();
        assert!(
            responses.is_empty(),
            "canceled initialization wrote a response"
        );
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
    })
    .await
    .unwrap();
}
