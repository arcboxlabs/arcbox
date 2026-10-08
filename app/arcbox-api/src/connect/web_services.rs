//! Complete web-route snapshots with bounded polling and no stale result after an error.

use std::{future::Future, time::Duration};

use arcbox_connect::v1::ListWebServicesResponse;
use connectrpc::{ConnectError, ServiceStream};

pub(super) fn watch<F>(
    mut snapshot: impl FnMut() -> F + Send + 'static,
) -> ServiceStream<ListWebServicesResponse>
where
    F: Future<Output = Result<ListWebServicesResponse, ConnectError>> + Send,
{
    Box::pin(async_stream::try_stream! {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous = None;
        loop {
            interval.tick().await;
            let current = snapshot().await?;
            if previous.as_ref() != Some(&current) {
                previous = Some(current.clone());
                yield current;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_connect::v1::WebService;
    use tokio_stream::StreamExt;

    #[tokio::test(start_paused = true)]
    async fn watch_skips_unchanged_routes_and_ends_on_guest_failure() {
        let initial = ListWebServicesResponse::default();
        let changed = ListWebServicesResponse {
            services: vec![WebService {
                owner: "container:test".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut snapshots = [
            Ok(initial.clone()),
            Ok(initial.clone()),
            Ok(changed.clone()),
            Err(ConnectError::unavailable("guest disconnected")),
        ]
        .into_iter();
        let mut stream = watch(move || std::future::ready(snapshots.next().unwrap()));
        assert_eq!(stream.next().await.unwrap().unwrap(), initial);
        assert_eq!(stream.next().await.unwrap().unwrap(), changed);
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }
}
