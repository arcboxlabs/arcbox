//! Exercise machine discovery and owned NAT cleanup in a disposable network namespace.

use super::*;
use arcbox_connect::v1::{
    GuestWebService, MachineWebPorts, WebPortPolicy, web_port_policy::Selection,
};
use arcbox_dns::{NamePattern, claims::Claim};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    process::Command,
};

const ADDRESS: &str = "192.0.2.8";
const HOSTNAME: &str = "test.machine.arcbox.local";

async fn command(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().await.unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn nat() -> String {
    command("/sbin/iptables", &["-t", "nat", "-w", "2", "-S"]).await
}

fn policy(http: Selection, https: Selection) -> MachineWebPorts {
    let policy = |selection| WebPortPolicy {
        selection: Some(selection),
        ..Default::default()
    };
    MachineWebPorts {
        http_port: policy(http).into(),
        https_port: policy(https).into(),
        ..Default::default()
    }
}

async fn http_server(cancel: CancellationToken) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {},
            () = async {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut buf = [0; 1024];
                        let read = stream.read(&mut buf).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buf[..read]);
                        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            break;
                        }
                    }
                    if request.starts_with(b"PRI * HTTP/2.0\r\n") {
                        continue;
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.starts_with("head / http/1.1\r\n"));
                    assert!(request.contains(&format!("\r\nhost: {HOSTNAME}\r\n")));
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await.unwrap();
                }
            } => {},
        }
    });
    (port, task)
}

async fn rescan(routes: &mut Routes, facts: &EndpointFacts) {
    routes.containers.get_mut(facts.owner()).unwrap().scans.next = Instant::now();
    routes.scan_due().await;
}

#[tokio::test]
#[ignore = "requires an empty isolated Linux network namespace with NET_ADMIN and NET_RAW"]
async fn kernel_machine_discovery_replaces_routes_and_cleans_up() {
    tokio::time::timeout(Duration::from_secs(30), exercise_machine_routes())
        .await
        .expect("machine route discovery or cleanup stalled");
}

async fn exercise_machine_routes() {
    assert_eq!(std::env::var("ARCBOX_TEST_ISOLATED_NAT"), Ok("1".into()));
    let interfaces: Vec<_> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        interfaces,
        ["lo"],
        "run in a fresh --network none container"
    );
    let baseline = nat().await;
    assert!(baseline.lines().all(|line| line.starts_with("-P ")));
    let addresses = command("ip", &["-o", "address", "show", "dev", "lo"]).await;
    command("ip", &["address", "add", "192.0.2.8/32", "dev", "lo"]).await;

    let mut foreign = TaggedRules::new("unrelated:");
    foreign
        .replace(
            "witness",
            vec![NatRule::new(
                "unrelated:",
                "witness",
                &["-p", "tcp", "--dport", "12345"],
                &["-j", "RETURN"],
            )],
        )
        .await
        .unwrap();
    let foreign_baseline = nat().await;

    let registry = crate::dns_server::DnsRegistry::default();
    let mut facts = EndpointFacts::machine(
        "test".into(),
        HOSTNAME.parse().unwrap(),
        ADDRESS.parse().unwrap(),
    );
    registry
        .write()
        .unwrap()
        .replace(
            facts.owner().into(),
            vec![Claim::exclusive(
                NamePattern::Exact(HOSTNAME.parse().unwrap()),
                facts.owner().into(),
                "machine".into(),
            )],
            Some(ADDRESS.parse().unwrap()),
        )
        .unwrap();
    let services = WebServices::new(registry.clone());
    let mut routes = Routes::new(services.clone());
    let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let frontend_port = frontend.local_addr().unwrap().port();
    routes.frontend_port = Some(frontend_port);
    let cancel_frontend = routes.frontend_cancel.clone();
    routes.frontend.spawn(async move {
        cancel_frontend.cancelled().await;
        drop(frontend);
    });
    let stop = CancellationToken::new();
    let (first_port, first) = http_server(stop.clone()).await;
    facts
        .replace_machine_ports(&policy(
            Selection::Auto(Box::default()),
            Selection::Auto(Box::default()),
        ))
        .unwrap();
    routes.track(facts.clone());
    routes.scan_due().await;
    assert_eq!(services.snapshot(None).len(), 1);
    let selected = routes.containers[facts.owner()].selected;
    assert_eq!(selected.http, Some(first_port));
    assert_eq!(
        selected.terminated_backend(),
        Some(HttpBackend {
            port: first_port,
            protocol: Some(super::super::HttpProtocol::Http1),
        })
    );
    let installed = nat().await;
    assert_eq!(installed.matches(RULE_OWNER).count(), 2);
    assert!(installed.contains(&format!("--to-destination {ADDRESS}:{first_port}")));
    assert!(installed.contains(&format!("--to-ports {frontend_port}")));
    assert!(!installed.contains("arcbox-index:"));
    rescan(&mut routes, &facts).await;
    assert_eq!(nat().await, installed);

    stop.cancel();
    first.await.unwrap();
    rescan(&mut routes, &facts).await;
    assert_eq!(services.snapshot(None), [] as [GuestWebService; 0]);
    assert_eq!(nat().await, foreign_baseline);

    let stop = CancellationToken::new();
    let (next_port, next) = http_server(stop.clone()).await;
    rescan(&mut routes, &facts).await;
    assert_eq!(
        routes.containers[facts.owner()].selected.http,
        Some(next_port)
    );
    assert_eq!(services.snapshot(None).len(), 1);
    assert!(
        nat()
            .await
            .contains(&format!("--to-destination {ADDRESS}:{next_port}"))
    );

    let (replacement_port, replacement) = http_server(stop.clone()).await;
    assert_ne!(replacement_port, next_port);
    facts
        .replace_machine_ports(&policy(
            Selection::Port(replacement_port.into()),
            Selection::Port(9443),
        ))
        .unwrap();
    routes.track(facts.clone());
    routes.scan_due().await;
    let native = nat().await;
    assert!(native.contains(&format!("--to-destination {ADDRESS}:{replacement_port}")));
    assert!(!native.contains(&format!("--to-destination {ADDRESS}:{next_port}")));
    assert!(native.contains(&format!("--to-destination {ADDRESS}:9443")));
    assert!(!native.contains("--to-ports"));

    facts
        .replace_machine_ports(&policy(
            Selection::Off(Box::default()),
            Selection::Off(Box::default()),
        ))
        .unwrap();
    routes.track(facts.clone());
    routes.scan_due().await;
    assert_eq!(services.snapshot(None), [] as [GuestWebService; 0]);
    assert_eq!(nat().await, foreign_baseline);

    facts
        .replace_machine_ports(&policy(
            Selection::Port(replacement_port.into()),
            Selection::Auto(Box::default()),
        ))
        .unwrap();
    routes.track(facts.clone());
    routes.scan_due().await;
    assert_eq!(services.snapshot(None).len(), 1);
    routes.forget(facts.owner()).await.unwrap();
    assert_eq!(services.snapshot(None), [] as [GuestWebService; 0]);
    assert_eq!(nat().await, foreign_baseline);
    routes.track(facts.clone());
    routes.scan_due().await;
    routes.shutdown().await.unwrap();
    assert_eq!(services.snapshot(None), [] as [GuestWebService; 0]);
    assert!(routes.frontend.is_empty());
    assert!(routes.frontend_cancel.is_cancelled());
    assert_eq!(nat().await, foreign_baseline);
    assert!(registry.read().unwrap().get(facts.owner()).is_some());
    let _released = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, frontend_port))
        .await
        .unwrap();
    stop.cancel();
    next.await.unwrap();
    replacement.await.unwrap();
    foreign.remove("witness").await.unwrap();
    command("ip", &["address", "del", "192.0.2.8/32", "dev", "lo"]).await;
    assert_eq!(nat().await, baseline);
    assert_eq!(
        command("ip", &["-o", "address", "show", "dev", "lo"]).await,
        addresses
    );
}
