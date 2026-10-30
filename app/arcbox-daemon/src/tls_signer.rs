//! Retained guest workers sign verified, host-owned CSRs with the current published root.

mod lifecycle;
mod machines;

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_core::{Runtime, runtime::CapturedWebGuest};
use arcbox_engine::vm_lifecycle::DEFAULT_MACHINE_NAME;
use arcbox_local_ca::{KeychainAuthority, MAX_CERTIFICATE_SIZE, VerifiedCertificateRequest};
use arcbox_transport::vsock::{VsockShutdown, VsockStream};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::watch,
    task::JoinSet,
};
use tokio_util::{codec::LengthDelimitedCodec, sync::CancellationToken};

use crate::{context::DaemonContext, guest_workers::join_services};

use lifecycle::{drain_on_change, guest_changed};

const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const IDENTITY_INTERVAL: Duration = Duration::from_secs(30);

pub async fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let authority = ctx.local_ca.subscribe();
    let runtime = Arc::clone(runtime);
    let shutdown = ctx.shutdown.clone();
    ctx.tls_signer
        .start(run(runtime, authority, shutdown))
        .await;
}

async fn run(
    runtime: Arc<Runtime>,
    authority: watch::Receiver<Option<Arc<KeychainAuthority>>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let shutdown = shutdown.child_token();
    let mut tasks = JoinSet::new();
    tasks.spawn(run_system(
        Arc::clone(&runtime),
        authority.clone(),
        shutdown.clone(),
    ));
    tasks.spawn(machines::run(runtime, authority, shutdown.clone()));
    join_services(tasks, shutdown).await
}

async fn run_system(
    runtime: Arc<Runtime>,
    authority: watch::Receiver<Option<Arc<KeychainAuthority>>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut state = runtime.subscribe_system_vm_state();
    loop {
        let ready = state.wait_for(|state| state.is_ready());
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            ready = ready => if ready.is_err() { return Ok(()); },
        }
        let result = serve_once(
            &runtime,
            &authority,
            DEFAULT_MACHINE_NAME,
            state.clone(),
            &shutdown,
        )
        .await;
        if shutdown.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = result {
            tracing::warn!(%error, "local CA signing connection failed");
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(RETRY_INTERVAL) => {}
            }
        }
    }
}

async fn serve_once<T: Send + Sync>(
    runtime: &Arc<Runtime>,
    authority: &watch::Receiver<Option<Arc<KeychainAuthority>>>,
    name: &str,
    changed: watch::Receiver<T>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let guest = runtime.capture_web_guest(name)?;
    let _hold = if name == runtime.default_machine_name() {
        None
    } else {
        Some(runtime.hold_web_guest(&guest)?)
    };
    let session = shutdown.child_token();
    drain_on_change(
        sign_once(runtime, authority, &guest, &session),
        guest_changed(changed, || Ok(runtime.capture_web_guest(name)? == guest)),
        &session,
    )
    .await
}

async fn sign_once(
    runtime: &Arc<Runtime>,
    authority: &watch::Receiver<Option<Arc<KeychainAuthority>>>,
    guest: &CapturedWebGuest,
    shutdown: &CancellationToken,
) -> Result<()> {
    let connecting_guest = guest.clone();
    let connecting_runtime = Arc::clone(runtime);
    let stream = connect_native(shutdown, move || {
        let fd = connecting_runtime.connect_web_guest_signer(&connecting_guest)?;
        // SAFETY: the hypervisor returns a newly opened descriptor and transfers ownership.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    })
    .await?;
    SigningSession {
        current: authority,
        shutdown,
        authorize: authorization(Arc::clone(runtime), guest.clone()),
    }
    .serve(stream, |authority, request| Ok(authority.sign(request)?))
    .await
}

fn authorization(
    runtime: Arc<Runtime>,
    guest: CapturedWebGuest,
) -> impl Fn(&str) -> Result<Option<u64>> + Clone {
    move |name| Ok(runtime.with_web_guest_claim(&guest, name, |_, revision| revision)?)
}

async fn connect_native(
    shutdown: &CancellationToken,
    connect: impl FnOnce() -> Result<OwnedFd> + Send + 'static,
) -> Result<VsockStream> {
    // Native connection work must finish before the retained service can stop.
    let cancelled = shutdown.clone();
    let socket = tokio::task::spawn_blocking(move || {
        ensure!(!cancelled.is_cancelled(), "TLS signer cancelled");
        connect()
    })
    .await??;
    ensure!(!shutdown.is_cancelled(), "TLS signer cancelled");
    Ok(VsockStream::from_fd_with_shutdown(
        socket,
        VsockShutdown::CloseOnDropOnly,
    )?)
}

struct SigningSession<'a, A, F> {
    current: &'a watch::Receiver<Option<Arc<A>>>,
    shutdown: &'a CancellationToken,
    authorize: F,
}

impl<A, F> SigningSession<'_, A, F>
where
    A: Send + Sync + 'static,
    F: Fn(&str) -> Result<Option<u64>> + Clone + Send + Sync + 'static,
{
    async fn serve<S>(
        self,
        stream: S,
        sign: impl FnOnce(&A, &VerifiedCertificateRequest) -> Result<Vec<u8>> + Send + 'static,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut frames = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_CERTIFICATE_SIZE)
            .new_framed(stream);
        // VZ requires the host to keep reading while the connection is parked.
        let request = self
            .shutdown
            .run_until_cancelled(frames.next())
            .await
            .context("TLS signer cancelled")?
            .context("guest closed the signing connection")??;
        let request = VerifiedCertificateRequest::try_from(request.as_ref())?;
        let name = request.dns_name().to_owned();
        let revision = (self.authorize)(&name)?
            .context("CSR DNS name is not owned by this guest's current web service")?;
        let authority = self
            .current
            .borrow()
            .clone()
            .context("local CA is not prepared")?;
        let signing_authority = Arc::clone(&authority);
        let authorize = self.authorize.clone();
        let shutdown = self.shutdown.clone();
        let certificate = tokio::task::spawn_blocking(move || {
            ensure!(!shutdown.is_cancelled(), "TLS signer cancelled");
            ensure!(
                authorize(request.dns_name())? == Some(revision),
                "DNS ownership changed during signing"
            );
            sign(&signing_authority, &request)
        })
        .await??;
        ensure!(
            (self.authorize)(&name)? == Some(revision),
            "DNS ownership changed during signing"
        );
        ensure!(
            self.current
                .borrow()
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &authority)),
            "local CA changed during signing"
        );
        self.shutdown
            .run_until_cancelled(tokio::time::timeout(
                Duration::from_secs(10),
                frames.send(certificate.into()),
            ))
            .await
            .context("TLS signer cancelled")?
            .context("guest did not read its certificate")??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_local_ca::CertificateRequest;

    async fn serve_request<A, S>(
        stream: S,
        current: &watch::Receiver<Option<Arc<A>>>,
        runtime: &Arc<Runtime>,
        sign: impl FnOnce(&A, &VerifiedCertificateRequest) -> Result<Vec<u8>> + Send + 'static,
    ) -> Result<()>
    where
        A: Send + Sync + 'static,
        S: AsyncRead + AsyncWrite + Unpin,
    {
        SigningSession {
            current,
            shutdown: &CancellationToken::new(),
            authorize: authorization(
                Arc::clone(runtime),
                runtime.capture_web_guest(DEFAULT_MACHINE_NAME)?,
            ),
        }
        .serve(stream, sign)
        .await
    }

    async fn runtime() -> (Arc<Runtime>, tempfile::TempDir) {
        runtime_with_fixture(arcbox_core::runtime::NetworkFixture::default()).await
    }

    async fn runtime_with_fixture(
        fixture: arcbox_core::runtime::NetworkFixture,
    ) -> (Arc<Runtime>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            Runtime::new(arcbox_core::Config::for_test(directory.path()))
                .unwrap()
                .with_network_fixture(fixture),
        );
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await
            .unwrap();
        (runtime, directory)
    }

    #[tokio::test]
    async fn unowned_or_tampered_requests_never_reach_the_signer() {
        let (runtime, _directory) = runtime().await;
        let unknown = CertificateRequest::new("unknown.arcbox.local").unwrap();
        let pending = CertificateRequest::new("web.arcbox.local").unwrap();
        let mut tampered = pending.der().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        for request in [unknown.der().to_vec(), tampered] {
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (host, guest) = tokio::io::duplex(1024);
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            guest.send(request.into()).await.unwrap();
            let (signed, signature) = tokio::sync::oneshot::channel();
            let result = serve_request(host, &current, &runtime, move |_, _| {
                signed.send(()).unwrap();
                Ok(vec![1])
            })
            .await;
            assert!(result.is_err());
            assert!(signature.await.is_err());
            assert!(guest.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn signing_requires_an_unchanged_effective_dns_registration() {
        for (owner, address, removed) in [
            (None, "10.0.2.3", true),
            (Some("replacement"), "10.0.2.3", false),
            (Some("container"), "10.0.2.4", false),
            (Some("container"), "10.0.2.3", true),
            (Some("container"), "10.0.2.3", false),
        ] {
            let (runtime, _directory) = runtime().await;
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let signing_runtime = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                serve_request(host, &current, &signing_runtime, move |authority, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![*authority])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            guest.send(request.der().to_vec().into()).await.unwrap();
            signing.await.unwrap();
            if removed {
                runtime.deregister_dns_by_id("container").await;
            }
            if let Some(owner) = owner {
                runtime
                    .register_dns(owner, &["web".into()], address.parse().unwrap())
                    .await
                    .unwrap();
            }
            finish.send(()).unwrap();
            let result = task.await.unwrap();
            if owner == Some("container") && address == "10.0.2.3" && !removed {
                result.unwrap();
                assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[1]);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("DNS ownership changed during signing")
                );
                assert!(guest.next().await.is_none());
            }
        }
    }

    #[tokio::test]
    async fn parked_connection_selects_authority_when_the_request_arrives() {
        let (runtime, _directory) = runtime().await;
        let (sender, current) = watch::channel(None::<Arc<u8>>);
        let (host, guest) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            serve_request(host, &current, &runtime, |authority, request| {
                assert_eq!(request.dns_name(), "web.arcbox.local");
                Ok(vec![*authority])
            })
            .await
        });
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(1)));
        tokio::task::yield_now().await;
        sender.send_replace(Some(Arc::new(2)));
        let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
        let request = CertificateRequest::new("web.arcbox.local").unwrap();
        guest.send(request.der().to_vec().into()).await.unwrap();
        assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[2]);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn changed_authority_discards_the_in_flight_signature() {
        let (runtime, _directory) = runtime().await;
        let original = Arc::new(1_u8);
        for replacement in [None, Some(Arc::new(2)), Some(Arc::clone(&original))] {
            let unchanged = replacement
                .as_ref()
                .is_some_and(|authority| Arc::ptr_eq(authority, &original));
            let (sender, current) = watch::channel(Some(Arc::clone(&original)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let runtime = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                serve_request(host, &current, &runtime, move |authority, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![*authority])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            guest.send(request.der().to_vec().into()).await.unwrap();
            signing.await.unwrap();
            sender.send_replace(replacement);
            finish.send(()).unwrap();
            let result = task.await.unwrap();
            if unchanged {
                result.unwrap();
                assert_eq!(&guest.next().await.unwrap().unwrap()[..], &[1]);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("changed during signing")
                );
                assert!(guest.next().await.is_none());
            }
        }
    }

    #[tokio::test]
    async fn identity_source_failure_waits_for_native_connect_and_closes_its_socket() {
        use std::{io::Read, os::unix::net::UnixStream, pin::pin, task::Poll};

        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let (invalidated, invalidation) = tokio::sync::oneshot::channel();
        let task = crate::background_task::BackgroundTask::default();
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (started, connecting) = tokio::sync::oneshot::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        task.start(async move {
            drain_on_change(
                async {
                    connect_native(&cancelled, move || {
                        started.send(()).unwrap();
                        finished.recv().unwrap();
                        Ok(host.into())
                    })
                    .await?;
                    Ok(())
                },
                async {
                    invalidation.await?;
                    anyhow::bail!("VM identity source failed")
                },
                &cancelled,
            )
            .await
        })
        .await;
        connecting.await.unwrap();
        invalidated.send(()).unwrap();
        shutdown.cancelled().await;
        let mut joining = pin!(task.join());
        assert!(matches!(futures::poll!(&mut joining), Poll::Pending));
        finish.send(()).unwrap();
        assert_eq!(
            joining.await.unwrap_err().to_string(),
            "VM identity source failed"
        );
        assert_eq!(guest.read(&mut [0]).unwrap(), 0);
    }

    #[tokio::test]
    async fn cancellation_waits_for_signing_and_discards_the_certificate() {
        use std::{pin::pin, task::Poll};

        let (runtime, _directory) = runtime().await;
        let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let task = crate::background_task::BackgroundTask::default();
        let (host, guest) = tokio::io::duplex(1024);
        let (started, signing) = tokio::sync::oneshot::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        task.start(async move {
            SigningSession {
                current: &current,
                shutdown: &cancelled,
                authorize: authorization(
                    Arc::clone(&runtime),
                    runtime.capture_web_guest(DEFAULT_MACHINE_NAME)?,
                ),
            }
            .serve(host, move |_, _| {
                started.send(()).unwrap();
                finished.recv().unwrap();
                Ok(vec![1])
            })
            .await
        })
        .await;
        let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
        let request = CertificateRequest::new("web.arcbox.local").unwrap();
        guest.send(request.der().to_vec().into()).await.unwrap();
        signing.await.unwrap();
        shutdown.cancel();
        let mut joining = pin!(task.join());
        assert!(matches!(futures::poll!(&mut joining), Poll::Pending));
        finish.send(()).unwrap();
        assert_eq!(
            joining.await.unwrap_err().to_string(),
            "TLS signer cancelled"
        );
        assert!(guest.next().await.is_none());
    }

    #[tokio::test]
    async fn vm_replacement_fences_signing_and_delivery() {
        for replaced_before_request in [true, false] {
            let fixture = arcbox_core::runtime::NetworkFixture::default();
            let (runtime, _directory) = runtime_with_fixture(fixture.clone()).await;
            let target = runtime.capture_web_guest(DEFAULT_MACHINE_NAME).unwrap();
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (host, guest) = tokio::io::duplex(1024);
            let (started, signing) = tokio::sync::oneshot::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            if replaced_before_request {
                fixture.restart();
            }
            let task = tokio::spawn(async move {
                SigningSession {
                    current: &current,
                    shutdown: &CancellationToken::new(),
                    authorize: authorization(runtime, target),
                }
                .serve(host, move |_, _| {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(vec![1])
                })
                .await
            });
            let mut guest = LengthDelimitedCodec::builder().new_framed(guest);
            let request = CertificateRequest::new("web.arcbox.local").unwrap();
            guest.send(request.der().to_vec().into()).await.unwrap();
            if replaced_before_request {
                assert!(signing.await.is_err());
            } else {
                signing.await.unwrap();
                fixture.restart();
                finish.send(()).unwrap();
            }
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("VM incarnation changed")
            );
            assert!(guest.next().await.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_changes_close_parked_sessions_from_events_or_rescan() {
        use std::{pin::pin, task::Poll};

        for notify in [true, false] {
            let fixture = arcbox_core::runtime::NetworkFixture::default();
            let (runtime, _directory) = runtime_with_fixture(fixture.clone()).await;
            let target = runtime.capture_web_guest(DEFAULT_MACHINE_NAME).unwrap();
            let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
            let (changed, changes) = watch::channel(());
            let shutdown = CancellationToken::new();
            let (host, mut guest) = tokio::io::duplex(1024);
            let work = SigningSession {
                current: &current,
                shutdown: &shutdown,
                authorize: authorization(Arc::clone(&runtime), target.clone()),
            }
            .serve(host, |_, _| panic!("parked sessions must not sign"));
            let mut session = pin!(drain_on_change(
                work,
                guest_changed(changes, || {
                    Ok(runtime.capture_web_guest(DEFAULT_MACHINE_NAME)? == target)
                }),
                &shutdown,
            ));
            assert!(matches!(futures::poll!(&mut session), Poll::Pending));
            fixture.restart();
            if notify {
                changed.send_replace(());
            } else {
                tokio::time::advance(IDENTITY_INTERVAL).await;
            }
            assert_eq!(
                session.await.unwrap_err().to_string(),
                "TLS signer cancelled"
            );
            assert_eq!(
                tokio::io::AsyncReadExt::read(&mut guest, &mut [0])
                    .await
                    .unwrap(),
                0
            );
        }
    }

    #[tokio::test]
    async fn shutdown_closes_a_parked_request() {
        use std::{pin::pin, task::Poll};

        let (runtime, _directory) = runtime().await;
        let (_sender, current) = watch::channel(Some(Arc::new(1_u8)));
        let shutdown = CancellationToken::new();
        let (host, mut guest) = tokio::io::duplex(1024);
        let session = SigningSession {
            current: &current,
            shutdown: &shutdown,
            authorize: authorization(
                Arc::clone(&runtime),
                runtime.capture_web_guest(DEFAULT_MACHINE_NAME).unwrap(),
            ),
        };
        let mut serving = pin!(session.serve(host, |_, _| panic!("no request was sent")));
        assert!(matches!(futures::poll!(&mut serving), Poll::Pending));
        shutdown.cancel();
        assert_eq!(
            serving.await.unwrap_err().to_string(),
            "TLS signer cancelled"
        );
        assert_eq!(
            tokio::io::AsyncReadExt::read(&mut guest, &mut [0])
                .await
                .unwrap(),
            0
        );
    }
}
