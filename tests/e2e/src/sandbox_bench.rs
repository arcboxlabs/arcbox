//! Admission timing shared by sandbox benchmarks.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const ADMISSION_TIMEOUT: Duration = Duration::from_secs(60);
const RPC_TIMEOUT: Duration = Duration::from_secs(180);
const ADMISSION_POLL: Duration = Duration::from_millis(50);

/// Successful RPC response and the time spent waiting for admission before that attempt.
pub struct Admitted<T> {
    pub response: T,
    pub started: Instant,
    pub wait: Duration,
}

/// Retries only explicit network-cleanup admission rejections for the requested sandbox.
/// Transport failures and uncertain commits must surface without replaying the mutation.
pub async fn admit<T>(
    id: &str,
    mut rpc: impl AsyncFnMut() -> Result<T, tonic::Status>,
) -> Result<Admitted<T>> {
    let first = Instant::now();
    let mut started = first;
    loop {
        match tokio::time::timeout(RPC_TIMEOUT, rpc())
            .await
            .with_context(|| format!("{id}: sandbox RPC timed out"))?
        {
            Ok(response) => {
                return Ok(Admitted {
                    response,
                    started,
                    wait: started.duration_since(first),
                });
            }
            Err(status) => {
                let remaining = ADMISSION_TIMEOUT.saturating_sub(first.elapsed());
                if !retryable_admission(id, &status) || remaining.is_zero() {
                    return Err(status).with_context(|| format!("{id}: sandbox admission failed"));
                }
                tokio::time::sleep(ADMISSION_POLL.min(remaining)).await;
                started = Instant::now();
            }
        }
    }
}

fn retryable_admission(id: &str, status: &tonic::Status) -> bool {
    status.code() == tonic::Code::Unavailable
        && (status
            .message()
            .ends_with("sandbox startup cleanup is awaiting host finalization")
            || status.message().ends_with(&format!(
                "sandbox {id} network cleanup is awaiting host finalization"
            )))
}

/// Routes a sandbox request to the default System VM.
pub fn with_machine<T>(msg: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(msg);
    request.metadata_mut().insert(
        "x-machine",
        tonic::metadata::MetadataValue::from_static("default"),
    );
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admission_wait_excludes_the_successful_attempt() {
        let mut calls = 0;
        let admitted = admit("probe", async || {
            calls += 1;
            if calls == 1 {
                Err(tonic::Status::unavailable(
                    "guest: sandbox probe network cleanup is awaiting host finalization",
                ))
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(())
            }
        })
        .await
        .expect("admitted");
        assert_eq!(calls, 2);
        assert!(admitted.wait >= ADMISSION_POLL);
        assert!(admitted.started.elapsed() >= Duration::from_millis(5));
        let immediate = admit("probe", async || Ok(())).await.expect("admitted");
        assert_eq!(immediate.wait, Duration::ZERO);
    }

    #[tokio::test]
    async fn other_failures_are_never_replayed() {
        for status in [
            tonic::Status::unavailable("transport connection closed"),
            tonic::Status::unavailable("restored sandbox is already committed"),
            tonic::Status::unavailable(
                "sandbox other network cleanup is awaiting host finalization",
            ),
            tonic::Status::internal("sandbox startup cleanup is awaiting host finalization"),
        ] {
            let mut calls = 0;
            let result: Result<Admitted<()>> = admit("probe", async || {
                calls += 1;
                Err(status.clone())
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
        assert!(retryable_admission(
            "probe",
            &tonic::Status::unavailable(
                "guest: sandbox startup cleanup is awaiting host finalization"
            )
        ));
    }
}
