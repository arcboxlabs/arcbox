//! Rejects stale or cancelled observation requests before opening a guest connection.

use super::*;

#[tokio::test]
async fn rejected_observation_does_not_connect_to_a_replacement_vm() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = crate::runtime::NetworkFixture::default();
    let runtime = std::sync::Arc::new(
        Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap()
        .with_network_fixture(fixture.clone()),
    );
    let previous = runtime
        .capture_web_guest(runtime.default_machine_name())
        .unwrap();
    fixture.restart();
    for enabled in [false, true] {
        let error = runtime
            .reconcile_https_access(
                &previous,
                ReconcileHttpsAccessRequest {
                    enabled,
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("fixture VM incarnation changed"));
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = runtime
        .reconcile_https_access(
            &runtime
                .capture_web_guest(runtime.default_machine_name())
                .unwrap(),
            ReconcileHttpsAccessRequest::default(),
            &cancelled,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("observation cancelled"));
}
