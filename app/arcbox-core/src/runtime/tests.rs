use super::Runtime;
use super::assets::{check_executable, ensure_guest_binaries};
use super::kubeconfig::rewrite_kubeconfig;
use crate::config::Config;
use std::path::PathBuf;

#[test]
fn test_ensure_guest_binaries_ok() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path();
    let generation = "0.6.13";

    std::fs::create_dir_all(data_dir.join("bin")).unwrap();
    std::fs::create_dir_all(data_dir.join("runtime").join(generation).join("bin")).unwrap();

    for name in [
        "bin/arcbox-agent",
        "runtime/0.6.13/bin/dockerd",
        "runtime/0.6.13/bin/containerd",
        "runtime/0.6.13/bin/containerd-shim-runc-v2",
        "runtime/0.6.13/bin/runc",
        "runtime/0.6.13/bin/docker-init",
        "runtime/0.6.13/bin/k3s",
    ] {
        let path = data_dir.join(name);
        std::fs::write(&path, b"binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    let result = ensure_guest_binaries(data_dir, generation);
    assert!(result.is_ok(), "expected success, got {result:?}");
}

#[test]
fn test_ensure_guest_binaries_missing_agent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let err = ensure_guest_binaries(temp_dir.path(), "0.6.13").unwrap_err();
    assert!(
        err.to_string().contains("agent binary not found"),
        "got: {err}"
    );
}

#[test]
fn test_ensure_guest_binaries_missing_runtime() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path();

    let agent = data_dir.join("bin/arcbox-agent");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, b"agent").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let err = ensure_guest_binaries(data_dir, "0.6.13").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("runtime binary"), "got: {msg}");
}

#[test]
fn test_rewrite_kubeconfig_updates_endpoint_and_context() {
    let kubeconfig = "apiVersion: v1\nclusters:\n- cluster:\n    server: https://127.0.0.1:6443\n  name: default\ncontexts:\n- context:\n    cluster: default\n    user: default\n  name: default\ncurrent-context: default\nusers:\n- name: default\n  user: {}\n";

    let rewritten = rewrite_kubeconfig(
        kubeconfig,
        "https://127.0.0.1:24123",
        "arcbox-dev-worktree-1",
    );
    assert!(rewritten.contains("server: https://127.0.0.1:24123"));
    assert!(rewritten.contains("name: arcbox-dev-worktree-1"));
    assert!(rewritten.contains("- name: arcbox-dev-worktree-1"));
    assert!(rewritten.contains("cluster: arcbox-dev-worktree-1"));
    assert!(rewritten.contains("user: arcbox-dev-worktree-1"));
    assert!(rewritten.contains("current-context: arcbox-dev-worktree-1"));
}

#[tokio::test]
async fn kubernetes_status_uses_the_bound_host_port() {
    let temp_dir = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(Config {
        data_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_kubernetes_host_endpoint(24_123, "arcbox-dev-worktree-1".to_owned())
        .unwrap();

    let status = runtime.kubernetes_status().await.unwrap();
    assert_eq!(status.endpoint, "https://127.0.0.1:24123");
    assert!(
        runtime
            .set_kubernetes_host_endpoint(24_124, "arcbox-dev-worktree-2".to_owned())
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn test_check_executable_not_executable() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("not-exec");
    std::fs::write(&path, b"data").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let err = check_executable(&path, "test").unwrap_err();
    assert!(err.to_string().contains("not executable"), "got: {err}");
}

fn networking_test_runtime() -> (Runtime, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let runtime = Runtime::new(config).expect("runtime init should succeed");
    (runtime, temp_dir)
}

/// VM-host-only mode reports no milestones, because no VM starts.
///
/// The guarantee is the `!vm.autostart` early return in `init`. Four places
/// state the invariant — `InitProgress`'s docs, the `Phase` enum in
/// `api.proto`, `app/AGENTS.md`, `docs/daemon-lifecycle.md` — and nothing
/// enforced it: work moved above that return would announce `VM_STARTING`
/// for a daemon that boots no guest, and all four would quietly go false.
#[tokio::test]
async fn init_reports_no_milestones_without_a_linux_vm() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    config.vm.autostart = false;
    let runtime = Runtime::new(config).expect("runtime init should succeed");

    let reported = std::sync::Mutex::new(Vec::new());
    runtime
        .init(|milestone| reported.lock().unwrap().push(milestone))
        .await
        .expect("VM-host-only init only creates data directories");

    let reported = reported.into_inner().unwrap();
    assert!(reported.is_empty(), "expected silence, got {reported:?}");
}

#[tokio::test]
async fn resolve_registered_container_by_id_alias_and_prefix() {
    let (runtime, _tmp) = networking_test_runtime();
    let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let id = "0a1b2c3d4e5f00112233445566778899";
    runtime.register_dns(id, &["web.local".into()], ip).await;
    runtime.register_container_alias("web", id).await;

    // Exact canonical ID.
    assert_eq!(
        runtime.resolve_registered_container(id).await.as_deref(),
        Some(id)
    );
    // Name alias.
    assert_eq!(
        runtime.resolve_registered_container("web").await.as_deref(),
        Some(id)
    );
    // Unique short-ID prefix.
    assert_eq!(
        runtime
            .resolve_registered_container("0a1b2c3d4e5f")
            .await
            .as_deref(),
        Some(id)
    );
    // Unregistered token resolves to nothing.
    assert_eq!(runtime.resolve_registered_container("nope").await, None);
}

#[tokio::test]
async fn resolve_registered_container_rejects_ambiguous_prefix() {
    let (runtime, _tmp) = networking_test_runtime();
    let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    runtime
        .register_dns("abcd1111", &["a.local".into()], ip)
        .await;
    runtime
        .register_dns("abcd2222", &["b.local".into()], ip)
        .await;

    // Prefix matching two registered IDs must not guess.
    assert_eq!(runtime.resolve_registered_container("abcd").await, None);
    // Longer, unique prefixes still resolve.
    assert_eq!(
        runtime
            .resolve_registered_container("abcd1")
            .await
            .as_deref(),
        Some("abcd1111")
    );
}

#[tokio::test]
async fn container_names_invert_aliases_and_prefer_the_shortest() {
    let (runtime, _tmp) = networking_test_runtime();
    let id = "0a1b2c3d4e5f00112233445566778899";
    // A container accreting two names: the short primary and a longer alias.
    runtime.register_container_alias("db", id).await;
    runtime
        .register_container_alias("arcbox-postgres-1", id)
        .await;
    // A second container with a single name.
    runtime.register_container_alias("cache", "beef5678").await;

    let names = runtime.container_names().await;
    assert_eq!(names.get(id).map(String::as_str), Some("db"));
    assert_eq!(names.get("beef5678").map(String::as_str), Some("cache"));
    // Unknown IDs have no name (the consumer falls back to the short ID).
    assert!(!names.contains_key("unknown"));
}

#[tokio::test]
async fn deregister_dns_clears_aliases_even_without_dns_entry() {
    let (runtime, _tmp) = networking_test_runtime();
    // Alias registered without any DNS entry (e.g. port forwarding only).
    runtime
        .register_container_alias("ports-only", "feed1234")
        .await;
    assert_eq!(
        runtime
            .resolve_registered_container("ports-only")
            .await
            .as_deref(),
        Some("feed1234")
    );

    runtime.deregister_dns_by_id("feed1234").await;
    assert_eq!(
        runtime.resolve_registered_container("ports-only").await,
        None
    );
}

#[tokio::test]
async fn sandbox_dns_cleanup_restores_same_named_container() {
    let (runtime, _tmp) = networking_test_runtime();
    let container_ip = "172.17.0.2".parse().unwrap();
    let sandbox_ip = "172.31.0.2".parse().unwrap();
    runtime
        .register_dns("container", &["same".into()], container_ip)
        .await;
    runtime.register_sandbox_dns("SAME", sandbox_ip).await;

    let entries = runtime.dns_entries.read().await;
    assert!(entries.contains_key("container"));
    assert!(entries.contains_key("sandbox:SAME"));
    drop(entries);
    assert_eq!(
        runtime.registered_container_ids().await,
        std::collections::HashSet::from(["container".to_string()])
    );
    let hosts = runtime.network_manager.local_hosts_table();
    assert_eq!(hosts.read().unwrap().get("same"), Some(&sandbox_ip));
    assert_eq!(
        hosts.read().unwrap().get("same.arcbox.local"),
        Some(&sandbox_ip)
    );

    runtime.deregister_sandbox_dns("SAME").await;
    let entries = runtime.dns_entries.read().await;
    assert!(entries.contains_key("container"));
    assert!(!entries.contains_key("sandbox:SAME"));
    drop(entries);
    assert_eq!(hosts.read().unwrap().get("same"), Some(&container_ip));
    assert_eq!(
        hosts.read().unwrap().get("same.arcbox.local"),
        Some(&container_ip)
    );
}

#[tokio::test]
async fn sandbox_dns_cleanup_restores_host_alias() {
    let (runtime, _tmp) = networking_test_runtime();
    let host_ip = "10.0.2.1".parse().unwrap();
    let sandbox_ip = "172.31.0.2".parse().unwrap();
    runtime.register_host_dns(&["host".into()], host_ip).await;
    runtime.register_sandbox_dns("host", sandbox_ip).await;

    assert!(runtime.registered_container_ids().await.is_empty());
    runtime.deregister_sandbox_dns("host").await;

    let hosts = runtime.network_manager.local_hosts_table();
    assert_eq!(hosts.read().unwrap().get("host"), Some(&host_ip));
}

#[tokio::test]
async fn shared_dns_restores_the_latest_live_owner_and_drops_old_aliases() {
    let (runtime, _tmp) = networking_test_runtime();
    let first_ip = "172.17.0.2".parse().unwrap();
    let second_ip = "172.17.0.3".parse().unwrap();
    let latest_ip = "172.17.0.4".parse().unwrap();
    runtime
        .register_dns("first", &["shared".into(), "old".into()], first_ip)
        .await;
    runtime
        .register_dns("second", &["shared".into()], second_ip)
        .await;
    runtime
        .register_dns("latest", &["shared".into()], latest_ip)
        .await;
    let hosts = runtime.network_manager.local_hosts_table();

    runtime.deregister_dns_by_id("first").await;
    assert_eq!(hosts.read().unwrap().get("shared"), Some(&latest_ip));
    assert!(!hosts.read().unwrap().contains_key("old"));

    runtime.deregister_dns_by_id("latest").await;
    assert_eq!(hosts.read().unwrap().get("shared"), Some(&second_ip));

    runtime
        .register_dns("second", &["replacement".into()], second_ip)
        .await;
    assert!(!hosts.read().unwrap().contains_key("shared"));
    assert_eq!(hosts.read().unwrap().get("replacement"), Some(&second_ip));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_cleanup_discovers_authority_without_secondary_indexes() {
    let (runtime, _tmp) = networking_test_runtime();
    let key = "sandbox:orphan:80/tcp".to_owned();
    runtime.inbound_rules.write().await.insert(
        key.clone(),
        (
            "missing-machine".into(),
            vec![(
                std::net::Ipv4Addr::LOCALHOST,
                45_678,
                super::InboundProtocol::Tcp,
            )],
        ),
    );
    runtime.dns_entries.write().await.insert(
        "sandbox:orphan".into(),
        super::DnsRegistration {
            hostnames: vec!["orphan".into()],
            ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            revision: 0,
        },
    );

    runtime.clear_sandbox_host_state().await;

    assert!(!runtime.inbound_rules.read().await.contains_key(&key));
    assert!(
        !runtime
            .dns_entries
            .read()
            .await
            .contains_key("sandbox:orphan")
    );
}

#[tokio::test]
async fn sandbox_cleanup_matches_the_exact_authority_owner() {
    let (runtime, _tmp) = networking_test_runtime();
    let owned = Runtime::sandbox_port_key("a", 80, "tcp");
    let other = Runtime::sandbox_port_key("a:other", 80, "tcp");

    #[cfg(target_os = "macos")]
    {
        let mut rules = runtime.inbound_rules.write().await;
        rules.insert(owned.clone(), ("missing-machine".into(), Vec::new()));
        rules.insert(other.clone(), ("missing-machine".into(), Vec::new()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut forwarders = runtime.port_forwarders.write().await;
        forwarders.insert(owned.clone(), super::PortForwarder::new());
        forwarders.insert(other.clone(), super::PortForwarder::new());
    }
    assert!(runtime.registered_container_ids().await.is_empty());

    runtime.remove_sandbox_ports("a").await;

    #[cfg(target_os = "macos")]
    {
        let rules = runtime.inbound_rules.read().await;
        assert!(!rules.contains_key(&owned));
        assert!(rules.contains_key(&other));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let forwarders = runtime.port_forwarders.read().await;
        assert!(!forwarders.contains_key(&owned));
        assert!(forwarders.contains_key(&other));
    }
}

#[tokio::test]
async fn sandbox_port_mappings_filter_sort_and_fence_cleanup() {
    let (runtime, _tmp) = networking_test_runtime();

    #[cfg(target_os = "macos")]
    {
        use std::net::Ipv4Addr;

        let mut rules = runtime.inbound_rules.write().await;
        rules.insert(
            Runtime::sandbox_port_key("target", 8080, "tcp"),
            (
                "default".into(),
                vec![(Ipv4Addr::LOCALHOST, 45_001, super::InboundProtocol::Tcp)],
            ),
        );
        rules.insert(
            Runtime::sandbox_port_key("target", 5353, "udp"),
            (
                "default".into(),
                vec![(Ipv4Addr::LOCALHOST, 45_002, super::InboundProtocol::Udp)],
            ),
        );
        rules.insert(
            Runtime::sandbox_port_key("target", 8080, "udp"),
            (
                "default".into(),
                vec![(Ipv4Addr::LOCALHOST, 45_000, super::InboundProtocol::Udp)],
            ),
        );
        rules.insert(
            Runtime::sandbox_port_key("target:other", 80, "tcp"),
            ("default".into(), Vec::new()),
        );
        rules.insert("container-id".into(), ("default".into(), Vec::new()));
    }

    #[cfg(not(target_os = "macos"))]
    {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let mut forwarders = runtime.port_forwarders.write().await;
        for (key, rule) in [
            (
                Runtime::sandbox_port_key("target", 8080, "tcp"),
                super::PortForwardRule::tcp(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_001).into(),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40_001).into(),
                ),
            ),
            (
                Runtime::sandbox_port_key("target", 5353, "udp"),
                super::PortForwardRule::udp(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_002).into(),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40_002).into(),
                ),
            ),
            (
                Runtime::sandbox_port_key("target", 8080, "udp"),
                super::PortForwardRule::udp(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_000).into(),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40_000).into(),
                ),
            ),
            (
                Runtime::sandbox_port_key("target:other", 80, "tcp"),
                super::PortForwardRule::tcp(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_003).into(),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40_003).into(),
                ),
            ),
            (
                "container-id".into(),
                super::PortForwardRule::tcp(
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_004).into(),
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80).into(),
                ),
            ),
        ] {
            let mut forwarder = super::PortForwarder::new();
            forwarder.add_rule(rule);
            forwarders.insert(key, forwarder);
        }
    }

    assert!(runtime.sandbox_port_keys.read().await.is_empty());
    let generation = runtime.sandbox_host_state_generation().await;
    let expected = vec![
        super::SandboxPortMapping {
            sandbox_port: 5353,
            host_port: 45_002,
            protocol: super::SandboxPortProtocol::Udp,
        },
        super::SandboxPortMapping {
            sandbox_port: 8080,
            host_port: 45_001,
            protocol: super::SandboxPortProtocol::Tcp,
        },
        super::SandboxPortMapping {
            sandbox_port: 8080,
            host_port: 45_000,
            protocol: super::SandboxPortProtocol::Udp,
        },
    ];
    assert_eq!(
        runtime
            .sandbox_port_mappings_if_unchanged("target", generation)
            .await,
        Some(expected)
    );

    *runtime.lock_sandbox_host_state().await += 1;
    assert_eq!(
        runtime
            .sandbox_port_mappings_if_unchanged("target", generation)
            .await,
        None,
        "a cleanup race must not return an empty or stale snapshot"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn conflicting_sandbox_cleanup_does_not_remove_the_existing_listener() {
    use arcbox_net::darwin::inbound_relay::{InboundCommand, InboundListenerManager};

    let (runtime, _tmp) = networking_test_runtime();
    let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_port = reservation.local_addr().unwrap().port();
    drop(reservation);

    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(4);
    runtime.inbound_listeners.write().await.insert(
        "test-machine".into(),
        InboundListenerManager::new(command_tx),
    );
    let first = [("127.0.0.1".to_owned(), host_port, 80, "tcp".to_owned())];
    runtime
        .start_port_forwarding_for("test-machine", "sandbox:first", &first)
        .await
        .unwrap();

    let conflicting = [("127.0.0.1".to_owned(), host_port, 81, "tcp".to_owned())];
    runtime
        .start_port_forwarding_for("test-machine", "sandbox:second", &conflicting)
        .await
        .expect_err("a second owner must not reuse the first owner's listener");
    runtime.stop_port_forwarding_by_id("sandbox:second").await;

    let _connection = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, host_port))
        .await
        .expect("the first owner's listener must remain bound");
    let command = tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
        .await
        .expect("the listener should accept the connection")
        .expect("the listener command channel should remain open");
    match command {
        InboundCommand::TcpAccepted { container_port, .. } => assert_eq!(container_port, 80),
        InboundCommand::UdpReceived { .. } => panic!("expected a TCP listener"),
    }

    runtime.stop_port_forwarding_by_id("sandbox:first").await;
}

/// Whether something accepts TCP connections on `port` at loopback. A port
/// whose listener is gone refuses; it is not re-bound here, because macOS
/// can refuse to re-bind a port for a moment after its listener closed.
#[cfg(target_os = "macos")]
async fn accepting(port: u16) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .is_ok_and(|connected| connected.is_ok())
}

/// A VM restart (resize, backend switch) hands the runtime a fresh listener
/// manager. The previous incarnation's listeners must go with it: they
/// inject into a datapath that is gone, and they hold the very host ports
/// the same publishes rebind on the new VM.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_fresh_listener_manager_retires_the_previous_incarnations_listeners() {
    use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};

    let (runtime, _tmp) = networking_test_runtime();
    let localhost = std::net::Ipv4Addr::LOCALHOST;

    let (previous_tx, _previous_rx) = tokio::sync::mpsc::channel(4);
    let mut previous = InboundListenerManager::new(previous_tx);
    let host_port = previous
        .add_rule(localhost, 0, 80, InboundProtocol::Tcp)
        .await
        .unwrap();
    runtime
        .adopt_inbound_listener_manager("test-machine", previous)
        .await;
    runtime.inbound_rules.write().await.insert(
        "web".to_owned(),
        (
            "test-machine".to_owned(),
            vec![(localhost, 0, InboundProtocol::Tcp)],
        ),
    );
    assert!(accepting(host_port).await);

    // The VM comes back: its new VMM hands over a manager of its own.
    let (fresh_tx, _fresh_rx) = tokio::sync::mpsc::channel(4);
    runtime
        .adopt_inbound_listener_manager("test-machine", InboundListenerManager::new(fresh_tx))
        .await;

    assert!(
        !accepting(host_port).await,
        "the previous incarnation's listener must be gone"
    );
    assert!(
        runtime.inbound_rules.read().await.is_empty(),
        "no ownership record may outlive the listeners it describes"
    );

    // And publishing binds again, on the new VM.
    let publish = [("127.0.0.1".to_owned(), 0u16, 80, "tcp".to_owned())];
    runtime
        .start_port_forwarding_for("test-machine", "web", &publish)
        .await
        .expect("a publish binds on the new incarnation");
    runtime.stop_port_forwarding_by_id("web").await;
}

/// When the System VM goes down, everything its containers left on the host
/// goes with it: listeners (which would otherwise hold their ports while
/// injecting into a dead datapath), ownership records, DNS and aliases.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn retiring_system_vm_container_networking_drops_every_trace_of_its_containers() {
    use arcbox_net::darwin::inbound_relay::{InboundListenerManager, InboundProtocol};

    let (runtime, _tmp) = networking_test_runtime();
    let localhost = std::net::Ipv4Addr::LOCALHOST;
    let (commands_tx, _commands_rx) = tokio::sync::mpsc::channel(4);
    let mut manager = InboundListenerManager::new(commands_tx);
    let host_port = manager
        .add_rule(localhost, 0, 80, InboundProtocol::Tcp)
        .await
        .unwrap();
    runtime
        .adopt_inbound_listener_manager(super::DEFAULT_MACHINE_NAME, manager)
        .await;
    runtime.inbound_rules.write().await.insert(
        "web".to_owned(),
        (
            super::DEFAULT_MACHINE_NAME.to_owned(),
            vec![(localhost, 0, InboundProtocol::Tcp)],
        ),
    );
    runtime
        .register_dns("web", &["web.local".to_owned()], localhost.into())
        .await;
    runtime.register_container_alias("web-name", "web").await;
    assert!(accepting(host_port).await);

    runtime.retire_system_vm_container_networking().await;

    assert!(
        !accepting(host_port).await,
        "the listener must be gone with the VM"
    );
    assert!(runtime.registered_container_ids().await.is_empty());
    assert!(runtime.inbound_rules.read().await.is_empty());
    assert!(
        !runtime
            .inbound_listeners
            .read()
            .await
            .contains_key(super::DEFAULT_MACHINE_NAME),
        "the stopped VM's manager must not be reused"
    );
}

#[test]
fn test_runtime_new_propagates_config_vm_defaults() {
    let temp_dir = tempfile::tempdir().unwrap();

    let mut config = Config {
        data_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    config.vm.cpus = 6;
    config.vm.memory_mb = 3072;
    config.vm.kernel_path = Some(PathBuf::from("/tmp/arcbox-test-kernel"));

    let runtime = Runtime::new(config).expect("runtime init should succeed");
    let default_vm = runtime.vm_lifecycle().default_vm_config();

    assert_eq!(default_vm.cpus, 6);
    assert_eq!(default_vm.memory_mb, 3072);
    assert_eq!(
        default_vm.kernel,
        Some(PathBuf::from("/tmp/arcbox-test-kernel"))
    );
}

#[test]
fn host_capacity_bounds_system_vm_sizes() {
    let host = super::HostCapacity {
        cpus: 8,
        memory_mb: 16 * 1024,
    };
    assert!(host.check(1, 512).is_ok());
    assert!(host.check(8, 16 * 1024).is_ok());
    assert!(host.check(0, 4096).is_err(), "zero cpus");
    assert!(host.check(9, 4096).is_err(), "more cpus than the host");
    assert!(host.check(4, 511).is_err(), "below the 512 MiB floor");
    assert!(
        host.check(4, 16 * 1024 + 1).is_err(),
        "more memory than the host"
    );
}

#[test]
fn resolve_bind_ip_defaults_and_loopback() {
    use std::net::Ipv4Addr;
    let lan = Ipv4Addr::UNSPECIFIED;
    let mac_only = Ipv4Addr::LOCALHOST;
    // Sandbox exposures pass "127.0.0.1" and must bind loopback only.
    assert_eq!(
        super::resolve_bind_ip("127.0.0.1", lan),
        Some(Ipv4Addr::LOCALHOST)
    );
    // Published container ports without a particular address (empty or
    // 0.0.0.0) follow the policy: all interfaces or loopback only.
    for unspecified in ["", "0.0.0.0"] {
        assert_eq!(super::resolve_bind_ip(unspecified, lan), Some(lan));
        assert_eq!(
            super::resolve_bind_ip(unspecified, mac_only),
            Some(mac_only)
        );
    }
    // A specific address is honored whatever the policy; garbage is rejected.
    assert_eq!(
        super::resolve_bind_ip("10.0.0.5", mac_only),
        Some(Ipv4Addr::new(10, 0, 0, 5))
    );
    assert_eq!(super::resolve_bind_ip("not-an-ip", lan), None);
}

/// A machine's name is published under the local domain at its bridge
/// address and owned apart from containers: the Docker reconciler must not
/// see it as a container that stopped running, or every machine would lose
/// its name within one reconcile interval.
#[tokio::test]
async fn machine_dns_is_published_under_the_local_domain_and_owned_apart_from_containers() {
    let (runtime, _tmp) = networking_test_runtime();
    let ip: std::net::IpAddr = "192.168.64.5".parse().unwrap();
    runtime.register_machine_dns("dev", ip).await;

    let hosts = runtime.network_manager.local_hosts_table();
    assert_eq!(hosts.read().unwrap().get("dev"), Some(&ip));
    assert_eq!(hosts.read().unwrap().get("dev.arcbox.local"), Some(&ip));
    assert!(runtime.registered_container_ids().await.is_empty());
    assert_eq!(runtime.registered_machine_dns_names().await, vec!["dev"]);

    runtime.deregister_machine_dns("dev").await;
    assert!(hosts.read().unwrap().get("dev.arcbox.local").is_none());
    assert_eq!(
        runtime.registered_machine_dns_names().await,
        Vec::<String>::new()
    );
}

/// The published name follows the daemon's DNS domain, not a literal, and
/// uses the machine's hostname: `_` and `.` in the name become `-`.
#[tokio::test]
async fn machine_dns_name_follows_the_configured_domain_and_the_hostname() {
    let (runtime, _tmp) = networking_test_runtime();
    assert_eq!(
        runtime.machine_dns_name("dev").as_deref(),
        Some("dev.arcbox.local")
    );
    assert_eq!(
        runtime.machine_dns_name("my_box.v2").as_deref(),
        Some("my-box-v2.arcbox.local")
    );
    runtime.network_manager().set_dns_domain("test.local");
    assert_eq!(
        runtime.machine_dns_name("dev").as_deref(),
        Some("dev.test.local")
    );

    let ip: std::net::IpAddr = "192.168.64.5".parse().unwrap();
    runtime.register_machine_dns("my_box.v2", ip).await;
    let hosts = runtime.network_manager.local_hosts_table();
    assert_eq!(hosts.read().unwrap().get("my-box-v2.test.local"), Some(&ip));
    assert_eq!(
        runtime.registered_machine_dns_names().await,
        vec!["my_box.v2"]
    );
}

/// Machines created before `create` refused hostname collisions can still
/// share a label; the registration must notice the other holder so the
/// override is logged rather than silent. Containers using the same label
/// are not machines and do not count.
#[tokio::test]
async fn a_machine_sharing_a_hostname_with_another_machine_is_detected() {
    let (runtime, _tmp) = networking_test_runtime();
    let ip: std::net::IpAddr = "192.168.64.5".parse().unwrap();
    runtime.register_machine_dns("my_box", ip).await;
    runtime.register_dns("c0ffee", &["my-box".into()], ip).await;

    assert_eq!(
        runtime.machine_holding_hostname("my-box", "my-box").await,
        Some("my_box".to_owned())
    );
    assert_eq!(
        runtime.machine_holding_hostname("my_box", "my-box").await,
        None
    );
    assert_eq!(
        runtime.machine_holding_hostname("other", "other").await,
        None
    );
}

#[tokio::test]
async fn the_default_machine_must_exist_and_lands_in_the_config_file() {
    let (runtime, temp_dir) = networking_test_runtime();
    let config = temp_dir.path().join("config.toml");
    assert_eq!(runtime.default_machine(), None);

    let missing = runtime
        .store_default_machine(&config, Some("dev"))
        .unwrap_err();
    assert!(missing.to_string().contains("dev"), "{missing}");
    assert!(!config.exists());

    runtime
        .machine_manager()
        .create(crate::machine::MachineConfig {
            name: "dev".to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    runtime.store_default_machine(&config, Some("dev")).unwrap();
    assert_eq!(runtime.default_machine().as_deref(), Some("dev"));
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.contains("default_machine = \"dev\""), "{written}");

    runtime.store_default_machine(&config, None).unwrap();
    assert_eq!(runtime.default_machine(), None);
    assert!(
        !std::fs::read_to_string(&config)
            .unwrap()
            .contains("default_machine")
    );
}

#[tokio::test]
async fn machine_resources_are_checked_against_the_host() {
    let (runtime, _temp_dir) = networking_test_runtime();
    runtime
        .machine_manager()
        .create(crate::machine::MachineConfig {
            name: "dev".to_owned(),
            cpus: 1,
            memory_mb: 1024,
            ..Default::default()
        })
        .await
        .unwrap();

    let resources = runtime.set_machine_resources("dev", 0, 2048).unwrap();
    assert_eq!((resources.cpus, resources.memory_mb), (1, 2048));
    assert!(!resources.restart_required);
    assert_eq!(resources.host, super::HostCapacity::probe());
    assert_eq!(
        runtime.machine_manager().get("dev").unwrap().memory_mb,
        2048
    );

    let too_many = runtime
        .set_machine_resources("dev", resources.host.cpus + 1, 0)
        .unwrap_err();
    assert!(too_many.to_string().contains("cpus must be"), "{too_many}");
    let too_little = runtime.set_machine_resources("dev", 0, 256).unwrap_err();
    assert!(
        too_little.to_string().contains("memory_mb must be"),
        "{too_little}"
    );
    assert_eq!(runtime.machine_manager().get("dev").unwrap().cpus, 1);

    let system_vm = runtime
        .set_machine_resources(super::DEFAULT_MACHINE_NAME, 2, 0)
        .unwrap_err();
    assert!(
        system_vm.to_string().contains("abctl system resources"),
        "{system_vm}"
    );
    assert!(runtime.set_machine_resources("nope", 1, 0).is_err());
}
