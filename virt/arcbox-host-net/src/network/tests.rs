use std::net::Ipv4Addr;

use super::*;
use crate::dns::NoDns;

fn vm(id: &str) -> VmId {
    VmId::new(id).unwrap()
}

fn nat() -> NetworkPolicy {
    NetworkPolicy {
        mode: NetworkMode::Nat,
    }
}

fn pool() -> Pool {
    Pool::new(Ipv4Addr::new(10, 200, 0, 0), 16).unwrap()
}

fn network_with(placement: DatapathPlacement, dns: Arc<dyn DnsPublisher>) -> HostNetwork {
    let net = HostNetwork::new(pool(), placement, DatapathServices::default(), dns);
    net.replay_complete();
    net
}

fn network(placement: DatapathPlacement) -> HostNetwork {
    network_with(placement, Arc::new(NoDns))
}

/// Records every publication, and fails a registration on request.
#[derive(Default)]
struct RecordingDns {
    calls: Mutex<Vec<String>>,
    fail_register: std::sync::atomic::AtomicBool,
}

impl RecordingDns {
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

#[async_trait]
impl DnsPublisher for RecordingDns {
    async fn register(&self, vm: &VmId, address: IpAddr) -> Result<()> {
        self.calls.lock().unwrap().push(format!("+{vm}={address}"));
        if self
            .fail_register
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(Error::Network("registry refused".into()));
        }
        Ok(())
    }

    async fn deregister(&self, vm: &VmId) {
        self.calls.lock().unwrap().push(format!("-{vm}"));
    }
}

/// Until the owner has replayed its leases, a guest that is still
/// running may hold any address: `reserve` is a come-back-later.
#[tokio::test]
async fn reserve_waits_for_the_owner_to_finish_replaying() {
    let net = HostNetwork::new(
        pool(),
        DatapathPlacement::Driver,
        DatapathServices::default(),
        Arc::new(NoDns),
    );
    assert!(matches!(
        net.reserve(&vm("a"), nat()).await,
        Err(Error::Unavailable(_))
    ));
    // Adoption is what the replay does, so it is not gated.
    let running = NetworkLease {
        vm: vm("running"),
        ip: "10.200.0.2".parse().unwrap(),
        prefix_len: 16,
        gateway: "10.200.0.1".parse().unwrap(),
        mac: Pool::mac(2),
        cleanup_token: "boot-1".into(),
    };
    net.adopt(&running, AttachMode::Invariant).await.unwrap();
    net.replay_complete();
    let fresh = net.reserve(&vm("a"), nat()).await.unwrap();
    assert_eq!(fresh.ip, "10.200.0.3".parse::<IpAddr>().unwrap());
}

#[tokio::test]
async fn only_nat_is_offered() {
    let net = network(DatapathPlacement::Driver);
    for mode in [NetworkMode::Isolated, NetworkMode::Bridged] {
        assert!(matches!(
            net.reserve(&vm("a"), NetworkPolicy { mode }).await,
            Err(Error::Network(_))
        ));
    }
    // Nothing was taken.
    let lease = net.reserve(&vm("a"), nat()).await.unwrap();
    assert_eq!(lease.ip, "10.200.0.2".parse::<IpAddr>().unwrap());
}

/// Each placement hands its driver the NIC that driver can run, and two
/// views over one ledger never lease one address twice.
#[tokio::test]
async fn each_placement_hands_out_its_nic_over_one_ledger() {
    let vz = network(DatapathPlacement::Network);
    let hv = vz.with_placement(DatapathPlacement::Driver);
    let a = vz.reserve(&vm("a"), nat()).await.unwrap();
    let b = hv.reserve(&vm("b"), nat()).await.unwrap();
    assert_ne!(a.ip, b.ip);

    let nic = vz.activate(&a, AttachMode::Invariant).await.unwrap();
    let NicAttachment::FileHandle { fd } = nic.attachment else {
        panic!("expected a file handle, got {:?}", nic.attachment);
    };
    // The descriptor is open in this process: the driver duplicates it.
    // SAFETY: fcntl F_GETFD only reads the descriptor's flags.
    assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    assert!(vz.datapath_control(&vm("a")).is_some());

    let nic = hv.activate(&b, AttachMode::Invariant).await.unwrap();
    assert_eq!(nic.attachment, NicAttachment::UserNat);
    assert_eq!(nic.mac, b.mac);
    // The driver runs that datapath; this network has none to offer.
    assert!(hv.datapath_control(&vm("b")).is_none());
    assert!(hv.take_port_forward(&vm("b")).is_none());
}

#[tokio::test]
async fn a_datapath_lives_from_activation_to_quarantine() {
    let net = network(DatapathPlacement::Network);
    let a = net.reserve(&vm("a"), nat()).await.unwrap();
    assert!(net.datapath_control(&vm("a")).is_none(), "reserved only");
    net.activate(&a, AttachMode::Invariant).await.unwrap();
    assert!(net.take_port_forward(&vm("a")).is_some());
    assert!(net.take_port_forward(&vm("a")).is_none(), "taken once");

    // The datapath runs: its loop answers (no flows, no resets).
    let control = net.datapath_control(&vm("a")).unwrap();
    let answer = control.clone();
    let resets =
        tokio::task::spawn_blocking(move || answer.abort_tcp(std::time::Duration::from_secs(5)))
            .await
            .unwrap()
            .unwrap();
    assert!(resets.is_empty());

    // Activating again replaces the datapath, with a fresh forward.
    net.activate(&a, AttachMode::Invariant).await.unwrap();
    assert!(net.take_port_forward(&vm("a")).is_some());
    // The replaced datapath stops: its loop stops answering.
    let stopped = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if control
                .abort_tcp(std::time::Duration::from_millis(200))
                .is_err()
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    })
    .await
    .unwrap();
    assert!(stopped, "the replaced datapath is still running");

    net.quarantine(a).await.unwrap();
    assert!(net.datapath_control(&vm("a")).is_none());
}

/// A datapath this network runs died with the process that ran it; the
/// owner must tear such a VM down, and the refusal takes nothing.
#[tokio::test]
async fn a_network_placed_lease_is_not_adoptable() {
    let net = network(DatapathPlacement::Network);
    let lease = NetworkLease {
        vm: vm("a"),
        ip: "10.200.0.2".parse().unwrap(),
        prefix_len: 16,
        gateway: "10.200.0.1".parse().unwrap(),
        mac: Pool::mac(2),
        cleanup_token: "boot-1".into(),
    };
    assert!(matches!(
        net.adopt(&lease, AttachMode::Invariant).await,
        Err(Error::Network(_))
    ));
    let fresh = net.reserve(&vm("b"), nat()).await.unwrap();
    assert_eq!(fresh.ip, lease.ip);
}

/// Every guest is told the fixed link its datapath serves, and nothing
/// that would put it on its pool address is accepted.
#[tokio::test]
async fn the_guest_is_on_the_fixed_link_and_legacy_attachments_are_refused() {
    let net = network(DatapathPlacement::Driver);
    let a = net.reserve(&vm("a"), nat()).await.unwrap();
    let b = net.reserve(&vm("b"), nat()).await.unwrap();
    let identity = net.identity(&a, AttachMode::Invariant);
    assert_eq!(identity.ip, "10.0.2.2".parse::<IpAddr>().unwrap());
    assert_eq!(identity.prefix_len, 24);
    assert_eq!(identity.gateway, "10.0.2.1".parse::<IpAddr>().unwrap());
    assert_eq!(identity.dns, vec![identity.gateway]);
    assert_eq!(identity.mac, a.mac);
    let other = net.identity(&b, AttachMode::Invariant);
    assert_eq!((other.ip, other.gateway), (identity.ip, identity.gateway));

    assert!(net.activate(&a, AttachMode::LegacySnapshot).await.is_err());
    let mut adoptable = b.clone();
    adoptable.vm = vm("c");
    adoptable.ip = "10.200.0.9".parse().unwrap();
    assert!(
        net.adopt(&adoptable, AttachMode::LegacySnapshot)
            .await
            .is_err()
    );
    // Nothing reaches a host VM through DNAT.
    assert!(net.host_ingress(&a).is_err());
}

/// The name follows the lease: published while live, withdrawn when it
/// stops being live, and a refused publication fails the activation or
/// adoption without leaving anything behind.
#[tokio::test]
async fn the_dns_name_follows_the_live_lease() {
    let dns = Arc::new(RecordingDns::default());
    let net = network_with(DatapathPlacement::Network, dns.clone());
    let a = net.reserve(&vm("a"), nat()).await.unwrap();
    assert_eq!(dns.calls(), Vec::<String>::new(), "reserved is not live");
    net.activate(&a, AttachMode::Invariant).await.unwrap();
    assert_eq!(dns.calls(), ["+a=10.200.0.2"]);
    net.quarantine(a.clone()).await.unwrap();
    assert_eq!(dns.calls(), ["-a"]);
    net.release(a).await.unwrap();
    assert_eq!(dns.calls(), ["-a"]);

    let b = net.reserve(&vm("b"), nat()).await.unwrap();
    dns.fail_register
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(net.activate(&b, AttachMode::Invariant).await.is_err());
    assert!(net.datapath_control(&vm("b")).is_none());
    // Still reserved: the activation can be retried.
    net.activate(&b, AttachMode::Invariant).await.unwrap();
    assert_eq!(dns.calls(), ["+b=10.200.0.2", "+b=10.200.0.2"]);

    let hv = network_with(DatapathPlacement::Driver, dns.clone());
    let running = NetworkLease {
        vm: vm("c"),
        ip: "10.200.0.7".parse().unwrap(),
        prefix_len: 16,
        gateway: "10.200.0.1".parse().unwrap(),
        mac: Pool::mac(7),
        cleanup_token: "boot-1".into(),
    };
    dns.fail_register
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(hv.adopt(&running, AttachMode::Invariant).await.is_err());
    // The refusal took nothing: the same adoption goes through.
    hv.adopt(&running, AttachMode::Invariant).await.unwrap();
    assert_eq!(dns.calls(), ["+c=10.200.0.7", "+c=10.200.0.7"]);
}
