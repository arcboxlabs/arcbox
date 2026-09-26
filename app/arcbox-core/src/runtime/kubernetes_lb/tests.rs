use super::*;
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

    /// A runtime whose LoadBalancer listeners bind loopback, with a listener
    /// manager standing in for the System VM's relay and the Kubernetes hold
    /// taken, as after a successful start.
    async fn runtime() -> (Runtime, mpsc::Receiver<InboundCommand>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        config.docker.expose_ports_to_lan = false;
        let runtime = Runtime::new(config).unwrap();
        let (commands_tx, commands) = mpsc::channel(16);
        runtime.inbound_listeners.write().await.insert(
            DEFAULT_MACHINE_NAME.into(),
            InboundListenerManager::new(commands_tx),
        );
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
            .into_iter()
            .map(|port| (port.name, port.port, port.state.as_known().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn listeners_follow_the_published_ports() {
        let (runtime, _commands, _dir) = runtime().await;
        let (web, api) = (u32::from(free_port()), u32::from(free_port()));

        runtime
            .apply_kubernetes_load_balancers(&listing(vec![
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
            .apply_kubernetes_load_balancers(&listing(vec![
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
            .apply_kubernetes_load_balancers(&listing(vec![load_balancer(
                "api",
                &[("TCP", api)],
                false,
            )]))
            .await;
        assert!(!accepts(moved).await, "a deleted Service's port closes");
        assert_eq!(
            report(&runtime).await,
            [("api".into(), api, State::Pending)]
        );

        runtime
            .apply_kubernetes_load_balancers(&listing(vec![load_balancer(
                "web",
                &[("TCP", web)],
                true,
            )]))
            .await;
        runtime.close_kubernetes_load_balancers().await;
        assert!(!accepts(web).await, "closing removes every listener");
        assert!(report(&runtime).await.is_empty());
    }

    #[tokio::test]
    async fn a_listing_that_raced_a_stop_opens_nothing() {
        let (runtime, _commands, _dir) = runtime().await;
        let port = u32::from(free_port());
        runtime.release_kubernetes().await;

        runtime
            .apply_kubernetes_load_balancers(&listing(vec![load_balancer(
                "web",
                &[("TCP", port)],
                true,
            )]))
            .await;

        assert!(!accepts(port).await);
        assert!(report(&runtime).await.is_empty());
    }

    #[tokio::test]
    async fn ports_the_host_cannot_bind_say_why_and_failures_are_retried() {
        let (runtime, _commands, _dir) = runtime().await;
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy = u32::from(taken.local_addr().unwrap().port());
        let sctp = u32::from(free_port());
        let web = listing(vec![load_balancer(
            "web",
            &[("TCP", 80), ("SCTP", sctp), ("TCP", busy)],
            true,
        )]);

        runtime.apply_kubernetes_load_balancers(&web).await;
        let ports = runtime.kubernetes_host_ports().await;
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
        runtime.apply_kubernetes_load_balancers(&web).await;
        assert!(!accepts(busy).await);

        {
            let mut ports = runtime.kubernetes_lb_ports.lock().await;
            for outcome in ports.0.values_mut() {
                if let Outcome::Failed { at, .. } = outcome {
                    *at = Instant::now().checked_sub(BIND_RETRY_INTERVAL).unwrap();
                }
            }
        }
        runtime.apply_kubernetes_load_balancers(&web).await;
        assert!(
            accepts(busy).await,
            "a failed bind is retried once the interval is up"
        );
    }
}
