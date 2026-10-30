//! Adapt H2 frames without transferring ownership of the shared connection.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use h2::{Reason, RecvStream, SendStream};
use http_body_util::BodyExt as _;
use hyper::body::{Body as HttpBody, Frame};

use super::h1_body::Body;

pub(super) fn body(stream: RecvStream) -> Body {
    ReceiveBody {
        stream,
        data_done: false,
        complete: false,
    }
    .boxed_unsync()
}

struct ReceiveBody {
    stream: RecvStream,
    data_done: bool,
    complete: bool,
}

impl HttpBody for ReceiveBody {
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>>>> {
        if self.complete {
            return Poll::Ready(None);
        }
        if !self.data_done {
            match ready!(self.stream.poll_data(cx)) {
                Some(Ok(bytes)) => {
                    if let Err(error) = self.stream.flow_control().release_capacity(bytes.len()) {
                        self.complete = true;
                        return Poll::Ready(Some(Err(error.into())));
                    }
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                Some(Err(error)) => {
                    self.complete = true;
                    return Poll::Ready(Some(Err(error.into())));
                }
                None => self.data_done = true,
            }
        }
        let trailers = ready!(self.stream.poll_trailers(cx));
        self.complete = true;
        Poll::Ready(
            trailers
                .map(|value| value.map(Frame::trailers))
                .map_err(Into::into)
                .transpose(),
        )
    }

    fn is_end_stream(&self) -> bool {
        self.complete || self.stream.is_end_stream()
    }
}

// The caller keeps the stream after EOS to observe resets while the other direction remains open.
struct ResetOnDrop<'a> {
    stream: &'a mut SendStream<Bytes>,
    complete: bool,
}

impl Drop for ResetOnDrop<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.stream.send_reset(Reason::CANCEL);
        }
    }
}

/// Forward one body through an open send half. The caller handles header-only EOS.
pub(super) async fn send_body(stream: &mut SendStream<Bytes>, body: Body) -> Result<()> {
    let mut guard = ResetOnDrop {
        stream,
        complete: false,
    };
    let result = forward_body(guard.stream, body).await;
    if let Err(error) = &result {
        let reason = error
            .downcast_ref::<h2::Error>()
            .and_then(h2::Error::reason)
            .unwrap_or(Reason::INTERNAL_ERROR);
        guard.stream.send_reset(reason);
    }
    guard.complete = true;
    result
}

async fn forward_body(stream: &mut SendStream<Bytes>, mut body: Body) -> Result<()> {
    loop {
        let frame = tokio::select! {
            biased;
            reset = poll_fn(|cx| stream.poll_reset(cx)) => return Err(reset_error(reset)),
            frame = body.frame() => frame.transpose()?,
        };
        let Some(frame) = frame else {
            stream.reserve_capacity(0);
            stream.send_data(Bytes::new(), true)?;
            return Ok(());
        };
        match frame.into_data() {
            Ok(mut data) => {
                let end = body.is_end_stream();
                if data.is_empty() {
                    stream.send_data(data, end)?;
                } else {
                    stream.reserve_capacity(data.len());
                    while !data.is_empty() {
                        let capacity = poll_fn(|cx| {
                            if let Poll::Ready(reset) = stream.poll_reset(cx) {
                                return Poll::Ready(Err(reset_error(reset)));
                            }
                            match ready!(stream.poll_capacity(cx)) {
                                Some(Ok(capacity)) => Poll::Ready(Ok(capacity)),
                                Some(Err(error)) => Poll::Ready(Err(error.into())),
                                None => Poll::Ready(Err(anyhow!(
                                    "HTTP/2 send stream closed before body EOS"
                                ))),
                            }
                        })
                        .await?;
                        let chunk = data.split_to(capacity.min(data.len()));
                        stream.send_data(chunk, end && data.is_empty())?;
                    }
                    stream.reserve_capacity(0);
                }
                if end {
                    return Ok(());
                }
            }
            Err(frame) => {
                let trailers = frame
                    .into_trailers()
                    .map_err(|_| anyhow!("unsupported HTTP body frame"))?;
                stream.reserve_capacity(0);
                stream.send_trailers(trailers)?;
                return Ok(());
            }
        }
    }
}

pub(super) fn reset_error(reset: std::result::Result<Reason, h2::Error>) -> anyhow::Error {
    match reset {
        Ok(reason) => h2::Error::from(reason).into(),
        Err(error) => error.into(),
    }
}
