use std::future::pending;

use super::*;

#[tokio::test]
async fn graceful_failure_returns_an_error_without_forcing() {
    let stopped = stop_runtime_with(
        async { anyhow::bail!("recovery cleanup failed") },
        async { panic!("a completed graceful stop must not be forced") },
        pending(),
        None,
    )
    .await;
    let RuntimeStop::Graceful(result) = stopped else {
        panic!("expected a completed graceful stop");
    };
    assert!(
        RuntimeStop::Graceful(result)
            .finish()
            .unwrap_err()
            .to_string()
            .contains("recovery cleanup failed")
    );
}

#[tokio::test(start_paused = true)]
async fn forced_stop_errors_survive_both_signal_and_deadline() {
    for deadline in [false, true] {
        let signal = async move {
            if deadline {
                pending::<()>().await;
            }
        };
        let stopped = stop_runtime_with(
            pending(),
            async { anyhow::bail!("reserved VM stop failed") },
            signal,
            deadline.then_some(Duration::from_secs(10)),
        )
        .await;
        let RuntimeStop::Forced(result) = stopped else {
            panic!("expected a forced stop");
        };
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("reserved VM stop failed")
        );
    }
}

#[tokio::test]
async fn graceful_task_panic_survives_successful_or_failed_force_cleanup() {
    for force_fails in [false, true] {
        let stopped = stop_runtime_with(
            async { panic!("graceful worker panicked") },
            async move {
                anyhow::ensure!(!force_fails, "forced cleanup failed");
                Ok(())
            },
            pending(),
            None,
        )
        .await;
        let RuntimeStop::Forced(result) = stopped else {
            panic!("a task panic must force cleanup");
        };
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("graceful worker panicked"));
        assert_eq!(error.contains("forced cleanup failed"), force_fails);
    }
}

#[tokio::test]
async fn forced_task_panic_is_returned_for_daemon_cleanup() {
    let stopped = stop_runtime_with(
        pending(),
        async { panic!("forced worker panicked") },
        async {},
        None,
    )
    .await;
    let RuntimeStop::Forced(result) = stopped else {
        panic!("expected a forced stop");
    };
    assert!(format!("{:#}", result.unwrap_err()).contains("forced worker panicked"));
}
