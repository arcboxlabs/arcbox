//! Fail a translated HTTP/1 message when Hyper cannot preserve its trailers.
//!
//! The checks below mirror Hyper's chunked trailer encoder from 1.11 on, which
//! matches declared names case-insensitively and keeps repeated values. Older
//! encoders drop such trailers silently, so the workspace requires hyper 1.11.

use bytes::Bytes;
use http_body_util::{BodyExt as _, StreamBody, combinators::UnsyncBoxBody};

use super::*;

pub(super) type Body = UnsyncBoxBody<Bytes, anyhow::Error>;

pub(super) fn direct(body: Incoming) -> Body {
    body.map_err(anyhow::Error::from).boxed_unsync()
}

pub(super) fn stream(body: Body, headers: &mut HeaderMap, accepts_trailers: bool) -> Result<Body> {
    let declared = headers
        .get_all(header::TRAILER)
        .iter()
        .map(|value| value.to_str())
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(|name| HeaderName::from_bytes(name.trim().as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::TRANSFER_ENCODING,
        HeaderValue::from_static("chunked"),
    );
    let frames =
        futures::stream::try_unfold((body, declared), move |(mut body, declared)| async move {
            let Some(frame) = body.frame().await.transpose()? else {
                return Ok(None);
            };
            if let Some(trailers) = frame.trailers_ref() {
                ensure!(
                    accepts_trailers,
                    "HTTP/1 peer did not accept response trailers"
                );
                for name in trailers.keys() {
                    ensure!(
                        declared.contains(name),
                        "HTTP/1 trailer was not declared: {name}"
                    );
                    // Hyper discards these fields even when the Trailer header declares them.
                    ensure!(
                        !matches!(
                            *name,
                            header::AUTHORIZATION
                                | header::CACHE_CONTROL
                                | header::CONTENT_ENCODING
                                | header::CONTENT_LENGTH
                                | header::CONTENT_RANGE
                                | header::CONTENT_TYPE
                                | header::HOST
                                | header::MAX_FORWARDS
                                | header::SET_COOKIE
                                | header::TRAILER
                                | header::TRANSFER_ENCODING
                                | header::TE
                        ),
                        "HTTP/1 trailer field is forbidden: {name}"
                    );
                }
            }
            Ok(Some((frame, (body, declared))))
        });
    Ok(StreamBody::new(frames).boxed_unsync())
}
