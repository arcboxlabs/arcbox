//! Real legacy NAT ordering and cleanup in an explicitly isolated network namespace.

use super::*;
use tokio::{net::TcpListener, process::Command};

async fn nat(args: &[&str]) -> String {
    let output = Command::new("/sbin/iptables")
        .args(["-t", "nat", "-w", "2"])
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "iptables {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
#[ignore = "requires an empty isolated Linux network namespace with NET_ADMIN and NET_RAW"]
async fn kernel_index_repairs_order_and_preserves_unrelated_rules() {
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
    let baseline = nat(&["-S"]).await;
    assert!(baseline.lines().all(|line| line.starts_with("-P ")));

    nat(&["-N", "DOCKER"]).await;
    nat(&[
        "-A",
        "PREROUTING",
        "-m",
        "addrtype",
        "--dst-type",
        "LOCAL",
        "-j",
        "DOCKER",
    ])
    .await;
    let unrelated = NatRule::new(
        "unrelated:",
        "witness",
        &["-p", "tcp", "--dport", "12345"],
        &["-j", "RETURN"],
    );
    let mut foreign = TaggedRules::new("unrelated:");
    foreign.replace("witness", vec![unrelated]).await.unwrap();

    let dns = DnsRegistry::default();
    let apex: Domain = "arcbox.local".parse().unwrap();
    let gateway = "192.0.2.1".parse().unwrap();
    let previous_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let previous_port = previous_listener.local_addr().unwrap().port();
    let mut previous = Publisher::new(dns.clone(), apex.clone(), gateway);
    previous.reconcile(previous_port).await.unwrap();
    drop(previous);
    assert!(dns.read().unwrap().get(OWNER).is_none());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut publisher = Publisher::new(dns.clone(), apex, gateway);
    publisher.reconcile(port).await.unwrap();
    let wanted = publisher.redirect_rules(port);
    let installed = iptables::list_prerouting().await.unwrap();
    assert!(precedes_docker_and_has_one_pair(&installed, &wanted));
    assert!(!installed.contains(&format!("--to-ports {previous_port}")));
    assert!(dns.read().unwrap().get(OWNER).is_some());
    TaggedRules::new(RULE_OWNER)
        .replace("gateway", wanted.clone())
        .await
        .unwrap();
    publisher.reconcile(port).await.unwrap();
    assert_eq!(iptables::list_prerouting().await.unwrap(), installed);

    let duplicate: Vec<_> = ["-I", "PREROUTING"]
        .into_iter()
        .chain(wanted[0].spec.iter().map(String::as_str))
        .collect();
    nat(&duplicate).await;
    nat(&[
        "-D",
        "PREROUTING",
        "-m",
        "addrtype",
        "--dst-type",
        "LOCAL",
        "-j",
        "DOCKER",
    ])
    .await;
    nat(&[
        "-I",
        "PREROUTING",
        "-m",
        "addrtype",
        "--dst-type",
        "LOCAL",
        "-j",
        "DOCKER",
    ])
    .await;
    let damaged = nat(&["-S"]).await;
    assert!(!precede_docker(&damaged, &wanted));
    let foreign_baseline: String = damaged
        .split_inclusive('\n')
        .filter(|line| !line.contains(RULE_OWNER))
        .collect();
    publisher.reconcile(port).await.unwrap();
    assert!(precedes_docker_and_has_one_pair(
        &iptables::list_prerouting().await.unwrap(),
        &wanted,
    ));

    publisher.cleanup().await.unwrap();
    publisher.cleanup().await.unwrap();
    assert!(dns.read().unwrap().get(OWNER).is_none());
    assert_eq!(nat(&["-S"]).await, foreign_baseline);
    foreign.remove("witness").await.unwrap();
    nat(&[
        "-D",
        "PREROUTING",
        "-m",
        "addrtype",
        "--dst-type",
        "LOCAL",
        "-j",
        "DOCKER",
    ])
    .await;
    nat(&["-X", "DOCKER"]).await;
    assert_eq!(nat(&["-S"]).await, baseline);
}

fn precedes_docker_and_has_one_pair(listing: &str, rules: &[NatRule]) -> bool {
    precede_docker(listing, rules) && listing.matches(RULE_OWNER).count() == 2
}
