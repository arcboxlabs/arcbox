use super::*;
#[cfg(target_os = "macos")]
use crate::config::Config;
use arcbox_connect::v1::{KubernetesLoadBalancer, KubernetesServicePort};

fn load_balancer(name: &str, ports: &[(&str, u32)], published: bool) -> KubernetesLoadBalancer {
    KubernetesLoadBalancer {
        namespace: "default".into(),
        name: name.into(),
        ports: ports
            .iter()
            .map(|(protocol, port)| KubernetesServicePort {
                protocol: (*protocol).into(),
                port: *port,
                ..Default::default()
            })
            .collect(),
        ingress: if published {
            vec!["10.0.2.2".into()]
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}

fn listing(load_balancers: Vec<KubernetesLoadBalancer>) -> KubernetesLoadBalancersResponse {
    KubernetesLoadBalancersResponse {
        running: true,
        load_balancers,
        ..Default::default()
    }
}

#[test]
fn only_valid_ports_of_a_running_cluster_are_requested() {
    let mut bad_name = load_balancer("web", &[("TCP", 8080)], true);
    bad_name.namespace = "a/b".into();
    let mut stopped = listing(vec![load_balancer("web", &[("TCP", 8080)], true)]);
    stopped.running = false;

    let requested = requested_ports(&listing(vec![
        bad_name,
        load_balancer("api", &[("TCP", 0), ("TCP", 70_000), ("UDP", 53)], false),
        load_balancer("web", &[("TCP", 8080)], true),
    ]));

    let got: Vec<_> = requested
        .iter()
        .map(|(key, published)| {
            (
                key.name.as_str(),
                key.port,
                key.protocol.as_str(),
                *published,
            )
        })
        .collect();
    assert_eq!(got, [("api", 53, "UDP", false), ("web", 8080, "TCP", true)]);
    assert!(requested_ports(&stopped).is_empty());
}

#[cfg(target_os = "macos")]
mod listeners {
    use std::net::Ipv4Addr;

    use arcbox_net::darwin::inbound_relay::{InboundCommand, InboundListenerManager};
    use tokio::sync::mpsc;

    use super::*;

    impl Runtime {
        async fn apply_test_load_balancers(&self, listing: &KubernetesLoadBalancersResponse) {
            self.apply_kubernetes_load_balancers(
                &self.container_network_incarnation().unwrap(),
                listing,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        }
    }

    /// A runtime whose LoadBalancer listeners bind loopback, with a listener
    /// manager standing in for the System VM's relay and the Kubernetes hold
    /// taken, as after a successful start.
    async fn runtime() -> (Runtime, mpsc::Receiver<InboundCommand>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_test(dir.path());
        config.docker.expose_ports_to_lan = false;
        let runtime = Runtime::new(config)
            .unwrap()
            .with_network_fixture(crate::runtime::NetworkFixture::default());
        let (commands_tx, commands) = mpsc::channel(16);
        runtime
            .adopt_test_manager(
                DEFAULT_MACHINE_NAME,
                InboundListenerManager::new(commands_tx),
            )
            .await;
        runtime.vm_lifecycle().set_kubernetes_hold(true).await;
        (runtime, commands, dir)
    }

    fn free_port() -> u16 {
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        probe.local_addr().unwrap().port()
    }

    async fn accepts(port: u32) -> bool {
        let port = u16::try_from(port).unwrap();
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
    }

    async fn report(runtime: &Runtime) -> Vec<(String, u32, State)> {
        runtime
            .kubernetes_host_ports()
            .await
            .unwrap()
            .into_iter()
            .map(|port| (port.name, port.port, port.state.as_known().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn listeners_follow_the_published_ports() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let (runtime, _commands, _dir) = runtime().await;
        let (web, api) = (u32::from(free_port()), u32::from(free_port()));

        runtime
            .apply_test_load_balancers(&listing(vec![
                load_balancer("web", &[("TCP", web)], true),
                load_balancer("api", &[("TCP", api)], false),
            ]))
            .await;
        assert!(accepts(web).await, "a published port is forwarded");
        assert!(!accepts(api).await, "an unpublished port is not");
        assert_eq!(
            report(&runtime).await,
            [
                ("api".into(), api, State::Pending),
                ("web".into(), web, State::Forwarded),
            ]
        );
        assert!(
            runtime.registered_container_ids().await.is_empty(),
            "the Docker reconciler must not mistake these listeners for containers"
        );

        let moved = u32::from(free_port());
        runtime
            .apply_test_load_balancers(&listing(vec![
                load_balancer("web", &[("TCP", moved)], true),
                load_balancer("api", &[("TCP", api)], false),
            ]))
            .await;
        assert!(
            !accepts(web).await,
            "the old port closes when the Service moves"
        );
        assert!(accepts(moved).await);

        runtime
            .apply_test_load_balancers(&listing(vec![load_balancer("api", &[("TCP", api)], false)]))
            .await;
        assert!(!accepts(moved).await, "a deleted Service's port closes");
        assert_eq!(
            report(&runtime).await,
            [("api".into(), api, State::Pending)]
        );

        runtime
            .apply_test_load_balancers(&listing(vec![load_balancer("web", &[("TCP", web)], true)]))
            .await;
        runtime.close_kubernetes_load_balancers().await;
        assert!(!accepts(web).await, "closing removes every listener");
        assert_eq!(report(&runtime).await, []);
    }

    #[tokio::test]
    async fn a_listing_that_raced_a_stop_opens_nothing() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let (runtime, _commands, _dir) = runtime().await;
        let port = u32::from(free_port());
        let expected = runtime.container_network_incarnation().unwrap();
        runtime.release_kubernetes().await;
        assert!(
            runtime
                .apply_kubernetes_load_balancers(
                    &expected,
                    &listing(vec![load_balancer("web", &[("TCP", port)], true)]),
                    &CancellationToken::new(),
                )
                .await
                .is_err()
        );

        assert!(!accepts(port).await);
        assert_eq!(report(&runtime).await, []);
    }

    #[tokio::test]
    async fn pending_publication_rejects_cancellation_and_vm_replacement() {
        for replace_vm in [false, true] {
            let (runtime, _commands, _dir) = runtime().await;
            let expected = runtime.container_network_incarnation().unwrap();
            let cancelled = CancellationToken::new();
            let port = u32::from(free_port());
            let services = listing(vec![load_balancer("web", &[("TCP", port)], true)]);
            let entered = tokio::sync::Notify::new();
            let release = tokio::sync::Notify::new();
            let forward = |owner: String, binding| {
                let (runtime, expected, entered, release) =
                    (&runtime, &expected, &entered, &release);
                async move {
                    runtime
                        .update_container_ports(expected, &owner, &[binding])
                        .await?;
                    entered.notify_one();
                    release.notified().await;
                    Ok(())
                }
            };
            let operation =
                runtime.apply_load_balancers_with(&expected, &services, &cancelled, forward);
            let invalidate = async {
                entered.notified().await;
                assert!(accepts(port).await);
                if replace_vm {
                    runtime.restart_network_fixture();
                } else {
                    cancelled.cancel();
                }
                release.notify_one();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(operation, invalidate)
            })
            .await
            .unwrap();
            assert!(result.is_err());
            assert!(!accepts(port).await);
            assert_eq!(report(&runtime).await, []);
        }
    }

    #[tokio::test]
    async fn replacement_vm_retries_old_failures_and_rejects_old_snapshots() {
        let (runtime, _commands, _dir) = runtime().await;
        let old = runtime.container_network_incarnation().unwrap();
        let busy = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let retry = u32::from(busy.local_addr().unwrap().port());
        let stable = u32::from(free_port());
        let services = listing(vec![load_balancer(
            "web",
            &[("TCP", retry), ("TCP", stable)],
            true,
        )]);
        runtime.apply_test_load_balancers(&services).await;
        assert!(
            report(&runtime)
                .await
                .iter()
                .any(|(_, port, state)| *port == retry && *state == State::Failed)
        );
        drop(busy);
        runtime.restart_network_fixture();
        assert!(runtime.kubernetes_host_ports().await.is_err());
        let (commands, _replacement_commands) = mpsc::channel(16);
        runtime
            .adopt_test_manager(DEFAULT_MACHINE_NAME, InboundListenerManager::new(commands))
            .await;
        let current = runtime.container_network_incarnation().unwrap();
        let cancelled = CancellationToken::new();
        runtime
            .apply_kubernetes_load_balancers(&current, &services, &cancelled)
            .await
            .unwrap();
        assert!(accepts(retry).await);
        assert!(accepts(stable).await);
        let before = report(&runtime).await;
        assert!(
            before
                .iter()
                .all(|(_, _, state)| *state == State::Forwarded)
        );
        assert!(
            runtime
                .apply_kubernetes_load_balancers(&old, &listing(Vec::new()), &cancelled)
                .await
                .is_err()
        );
        assert!(accepts(retry).await);
        assert!(accepts(stable).await);
        assert_eq!(report(&runtime).await, before);
        runtime.close_kubernetes_load_balancers().await;
    }

    #[tokio::test]
    async fn ports_the_host_cannot_bind_say_why_and_failures_are_retried() {
        let _children = crate::test_support::CHILD_PROCESSES.read().await;
        let (runtime, _commands, _dir) = runtime().await;
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy = u32::from(taken.local_addr().unwrap().port());
        let sctp = u32::from(free_port());
        let web = listing(vec![load_balancer(
            "web",
            &[("TCP", 80), ("SCTP", sctp), ("TCP", busy)],
            true,
        )]);

        runtime.apply_test_load_balancers(&web).await;
        let ports = runtime.kubernetes_host_ports().await.unwrap();
        let state = |port: u32, protocol: &str| {
            let found = ports
                .iter()
                .find(|p| p.port == port && p.protocol == protocol)
                .unwrap();
            (found.state.as_known().unwrap(), found.detail.clone())
        };
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            let (privileged, reason) = state(80, "TCP");
            assert_eq!(privileged, State::Skipped);
            assert!(reason.contains("below 1024"), "{reason}");
        }
        let (unsupported, reason) = state(sctp, "SCTP");
        assert_eq!(unsupported, State::Skipped);
        assert!(reason.contains("SCTP"), "{reason}");
        assert_eq!(state(busy, "TCP").0, State::Failed);

        // Freed, the port is not retried before the interval is up.
        drop(taken);
        runtime.apply_test_load_balancers(&web).await;
        assert!(!accepts(busy).await);

        {
            let mut ports = runtime.kubernetes_lb_ports.lock().await;
            for outcome in ports.outcomes.values_mut() {
                if let Outcome::Failed { at, .. } = outcome {
                    *at = Instant::now().checked_sub(BIND_RETRY_INTERVAL).unwrap();
                }
            }
        }
        runtime.apply_test_load_balancers(&web).await;
        assert!(
            accepts(busy).await,
            "a failed bind is retried once the interval is up"
        );
    }
}
