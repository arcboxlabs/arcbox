//! Request completion and cancellation ownership at the HTTP/1 boundary.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::body::{Body as _, Frame, SizeHint};
use tokio::sync::oneshot;
use tokio_util::sync::DropGuard;

use super::h1_body::Body;

pub(super) enum Upload {
    Complete,
    Abandoned,
}

pub(super) type Uploaded = oneshot::Receiver<anyhow::Result<Upload>>;

pub(super) fn track(body: Body) -> (Body, Uploaded) {
    let (completed, uploaded) = oneshot::channel();
    (wrap(body, Some(completed), None), uploaded)
}

pub(super) fn guard(body: Body, cancellation: DropGuard) -> Body {
    wrap(body, None, Some(cancellation))
}

fn wrap(
    body: Body,
    completed: Option<oneshot::Sender<anyhow::Result<Upload>>>,
    cancellation: Option<DropGuard>,
) -> Body {
    let mut body = LifecycleBody {
        body,
        completed,
        cancellation,
    };
    if body.body.is_end_stream() {
        body.finish(Ok(()));
    }
    body.boxed_unsync()
}

struct LifecycleBody {
    body: Body,
    completed: Option<oneshot::Sender<anyhow::Result<Upload>>>,
    cancellation: Option<DropGuard>,
}

impl LifecycleBody {
    fn finish(&mut self, result: anyhow::Result<()>) {
        if result.is_ok()
            && let Some(cancellation) = self.cancellation.take()
        {
            cancellation.disarm();
        }
        if let Some(completed) = self.completed.take() {
            let _receiver_closed = completed.send(result.map(|()| Upload::Complete));
        }
    }
}

impl Drop for LifecycleBody {
    fn drop(&mut self) {
        if !std::thread::panicking()
            && let Some(completed) = self.completed.take()
        {
            let _receiver_closed = completed.send(Ok(Upload::Abandoned));
        }
    }
}

impl hyper::body::Body for LifecycleBody {
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        match &frame {
            Poll::Ready(Some(Err(error))) => self.finish(Err(anyhow::anyhow!(error.to_string()))),
            Poll::Ready(None) => self.finish(Ok(())),
            Poll::Ready(Some(Ok(frame))) if frame.trailers_ref().is_some() => self.finish(Ok(())),
            _ if self.body.is_end_stream() => self.finish(Ok(())),
            _ => {}
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
