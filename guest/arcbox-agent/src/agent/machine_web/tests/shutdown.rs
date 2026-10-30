//! The exclusive session outlives real stream shutdown and withdraws before waiting.

use super::*;
use crate::domains::{EndpointFacts, SelectedRoutes};
use crate::rpc::{MessageType, write_message};
use buffa::Message as _;
use std::{
    future::Future as _,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{io::ReadBuf, net::UnixStream, sync::oneshot};

#[test]
fn invalid_open_identity_or_address_cannot_claim_the_session() {
    let receiver = receiver();
    for request in [
        OpenMachineWebRequest {
            machine_name: String::new(),
            ..open()
        },
        OpenMachineWebRequest {
            hostname: "UPPER.machine.arcbox.local".into(),
            ..open()
        },
        OpenMachineWebRequest {
            hostname: "*.machine.arcbox.local".into(),
            ..open()
        },
        OpenMachineWebRequest {
            ipv4_address: "::1".into(),
            ..open()
        },
        OpenMachineWebRequest {
            ipv4_address: "192.168.64.999".into(),
            ..open()
        },
    ] {
        assert!(receiver.open(request, Some(bridge())).is_err());
        assert!(!receiver.state.lock().unwrap().active);
        assert!(receiver.state.lock().unwrap().configuration.is_none());
        assert_eq!(receiver.registry.read().unwrap().epoch(), 0);
    }
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}

struct ShutdownGate {
    stream: UnixStream,
    entered: Option<oneshot::Sender<()>>,
    resume: Option<oneshot::Receiver<()>>,
    completed: Option<oneshot::Sender<()>>,
    closed: Option<oneshot::Sender<()>>,
}

impl Drop for ShutdownGate {
    fn drop(&mut self) {
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(());
        }
    }
}

impl AsyncRead for ShutdownGate {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ShutdownGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
        }
        if let Some(resume) = self.resume.as_mut() {
            std::task::ready!(Pin::new(resume).poll(cx)).unwrap();
            self.resume = None;
        }
        let result = std::task::ready!(Pin::new(&mut self.stream).poll_shutdown(cx));
        if let Some(completed) = self.completed.take() {
            completed.send(()).unwrap();
        }
        Poll::Ready(result)
    }
}

fn publish_cleanup_witness(receiver: &Receiver) {
    let name = "witness.arcbox.local".parse().unwrap();
    receiver
        .registry
        .write()
        .unwrap()
        .replace(
            "witness".into(),
            vec![Claim::exclusive(
                NamePattern::Exact(name),
                "witness".into(),
                "container".into(),
            )],
            Some("192.168.64.9".parse().unwrap()),
        )
        .unwrap();
    let facts = EndpointFacts::from_inspect(
        &serde_json::json!({
            "Id": "witness", "Name": "/witness", "State": { "Pid": 1 },
            "NetworkSettings": { "SandboxKey": "/proc/self/ns/net", "Networks": {
                "bridge": { "IPAddress": "192.168.64.9" }
            }}
        }),
        &"arcbox.local".parse().unwrap(),
    )
    .unwrap();
    receiver.web.publish(
        &facts,
        SelectedRoutes {
            http: Some(8080),
            https: None,
        },
    );
    assert_eq!(receiver.web.snapshot(None).len(), 1);
}

#[test]
fn dropping_a_session_withdraws_routes_and_preserves_unrelated_dns() {
    let receiver = receiver();
    let directory = receiver.web.directory_source();
    directory.ready();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    publish_cleanup_witness(&receiver);
    drop(session);
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
    assert!(receiver.registry.read().unwrap().get("witness").is_some());
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}

#[test]
fn a_new_revision_with_identical_policies_invalidates_installed_routes() {
    let receiver = receiver();
    let directory = receiver.web.directory_source();
    directory.ready();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    let mut request = replacement(1, Selection::Port(8080), Selection::Port(8443));
    session.replace(request.clone()).unwrap();
    publish_cleanup_witness(&receiver);
    request.source_revision = 2;
    assert_eq!(session.replace(request.clone()).unwrap().source_revision, 2);
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
    assert_eq!(
        receiver
            .state
            .lock()
            .unwrap()
            .configuration
            .as_ref()
            .unwrap()
            .source_revision,
        request.source_revision
    );
}

#[tokio::test]
async fn eof_withdraws_before_shutdown_and_preserves_exclusivity_until_shutdown_finishes() {
    check_shutdown("eof", "actor").await;
}

#[tokio::test]
async fn canceled_caller_withdraws_but_retains_admission_until_owned_shutdown_finishes() {
    for (phase, first) in [
        ("read", "actor"),
        ("read", "stream"),
        ("shutdown", "actor"),
        ("shutdown", "stream"),
    ] {
        check_shutdown(phase, first).await;
    }
}

async fn check_shutdown(phase: &str, first: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let receiver = receiver();
        let directory = receiver.web.directory_source();
        directory.ready();
        let (mut client, stream) = UnixStream::pair().unwrap();
        let (entered, waiting) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        let (completed, completion) = oneshot::channel();
        let (closed, closure) = oneshot::channel();
        let (cleaning, cleanup_started) = oneshot::channel();
        let (finish_cleanup, cleanup_gate) = oneshot::channel();
        let (cleaned, cleanup_finished) = oneshot::channel();
        let serving = receiver.clone();
        let mut task = tokio::spawn(async move {
            let stream = ShutdownGate {
                stream,
                entered: Some(entered),
                resume: Some(released),
                completed: Some(completed),
                closed: Some(closed),
            };
            serving
                .serve_with(stream, "open", open(), Some(bridge()), |session, ready| {
                    let (routes, work) = test_routes(session, ready);
                    (routes, async move {
                        work.await?;
                        cleaning.send(()).unwrap();
                        cleanup_gate.await.unwrap();
                        cleaned.send(()).unwrap();
                        Ok(())
                    })
                })
                .await
        });
        let (kind, trace, _) = read_message(&mut client).await.unwrap();
        assert_eq!(kind, MessageType::OpenMachineWebResponse);
        assert_eq!(trace, "open");
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
        let (kind, trace, payload) = read_message(&mut client).await.unwrap();
        assert_eq!(kind, MessageType::MachineWebAck);
        assert_eq!(trace, "replace");
        assert_eq!(
            MachineWebAck::decode_from_slice(&payload)
                .unwrap()
                .source_revision,
            1
        );
        let authority = receiver.authority.subscribe();
        assert_eq!(
            authority.borrow().as_ref().unwrap().der(),
            PublicAuthority::from_pem(CA_PEM).unwrap().der()
        );
        publish_cleanup_witness(&receiver);
        if phase == "read" {
            task.abort();
            assert!((&mut task).await.unwrap_err().is_cancelled());
        } else {
            client.shutdown().await.unwrap();
        }
        waiting.await.unwrap();
        cleanup_started.await.unwrap();
        if phase == "shutdown" {
            task.abort();
            assert!((&mut task).await.unwrap_err().is_cancelled());
        }
        assert!(receiver.state.lock().unwrap().active);
        assert!(receiver.state.lock().unwrap().configuration.is_none());
        assert!(authority.borrow().is_none());
        {
            let registry = receiver.registry.read().unwrap();
            assert!(registry.get("machine:my_vm").is_none());
            assert!(registry.get("witness").is_some());
        }
        assert_eq!(
            receiver.web.snapshot(None),
            [] as [arcbox_connect::v1::GuestWebService; 0]
        );
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
        if first == "stream" {
            resume.send(()).unwrap();
            completion.await.unwrap();
            assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
            finish_cleanup.send(()).unwrap();
            cleanup_finished.await.unwrap();
        } else {
            finish_cleanup.send(()).unwrap();
            cleanup_finished.await.unwrap();
            assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
            resume.send(()).unwrap();
            completion.await.unwrap();
        }
        if phase == "eof" {
            assert!(task.await.unwrap().is_err());
        }
        closure.await.unwrap();
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
    })
    .await
    .unwrap();
}

#[test]
fn invalid_or_stale_replacements_preserve_the_last_complete_policy() {
    let receiver = receiver();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    assert!(
        session
            .replace(replacement(0, Selection::Port(80), Selection::Port(443)))
            .is_err()
    );
    let mut accepted = replacement(2, Selection::Port(8080), Selection::Port(8443));
    accepted.ca_certificate_pem = Some(CA_PEM.to_vec());
    session.replace(accepted.clone()).unwrap();
    let directory = receiver.web.directory_source();
    directory.ready();
    publish_cleanup_witness(&receiver);
    let mut invalid = vec![
        replacement(0, Selection::Port(80), Selection::Port(443)),
        replacement(1, Selection::Port(80), Selection::Port(443)),
        accepted,
        replacement(3, Selection::Port(0), Selection::Port(443)),
        replacement(3, Selection::Port(80), Selection::Port(65536)),
        ReplaceMachineWebRequest {
            source_revision: 3,
            ..Default::default()
        },
    ];
    let mut invalid_root = replacement(3, Selection::Port(80), Selection::Port(443));
    invalid_root.ca_certificate_pem = Some(b"invalid root".to_vec());
    invalid.push(invalid_root);
    let mut invalid_ports = replacement(3, Selection::Port(80), Selection::Port(65536));
    invalid_ports.ca_certificate_pem = Some(CA_PEM.to_vec());
    invalid.push(invalid_ports);
    for http in [true, false] {
        let mut missing = replacement(3, Selection::Port(80), Selection::Port(443));
        let ports = missing.ports.as_option_mut().unwrap();
        if http {
            ports.http_port = WebPortPolicy::default().into();
        } else {
            ports.https_port = WebPortPolicy::default().into();
        }
        invalid.push(missing);
    }
    let accepted_configuration = receiver.state.lock().unwrap().configuration.clone();
    let authority = receiver.authority.borrow().clone().unwrap();
    for request in invalid {
        assert!(session.replace(request).is_err());
        assert_eq!(
            receiver.state.lock().unwrap().configuration,
            accepted_configuration
        );
        assert!(Arc::ptr_eq(
            &authority,
            receiver.authority.borrow().as_ref().unwrap()
        ));
        assert_eq!(receiver.web.snapshot(None).len(), 1);
    }
    session
        .replace(replacement(3, Selection::Port(80), Selection::Port(443)))
        .unwrap();
    assert!(receiver.authority.borrow().is_none());
    assert_eq!(
        receiver
            .state
            .lock()
            .unwrap()
            .configuration
            .as_ref()
            .unwrap()
            .source_revision,
        3
    );
}

#[test]
fn bridge_admission_requires_the_observed_machine_interface() {
    let receiver = receiver();
    for observed in [None, Some("192.168.64.9".parse().unwrap())] {
        assert!(receiver.open(open(), observed).is_err());
        assert!(!receiver.state.lock().unwrap().active);
        assert_eq!(receiver.registry.read().unwrap().epoch(), 0);
    }
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}
