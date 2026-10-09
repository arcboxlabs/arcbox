use super::*;

#[tokio::test]
async fn readiness_stops_retrying_after_agent_scope_closes() {
    let directory = tempdir().unwrap();
    let manager = test_machine_manager(directory.path());
    let expected = model_running_machine(&manager, "ready");
    manager.close_agent_connections().unwrap();

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        manager.wait_for_machine_ready("ready", &expected),
    )
    .await
    .expect("closed connection admission must end readiness without its 60-second retry")
    .unwrap_err();
    assert!(
        error.to_string().contains("socket scope is closed"),
        "{error}"
    );
    assert!(manager.host_holds.wait_released("ready", Duration::ZERO));
    let machine = manager.get("ready").unwrap();
    assert!(machine.ip_address.is_none());
    assert!(machine.bridge_ip_address.is_none());
}
