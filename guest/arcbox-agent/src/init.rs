//! System initialization for PID 1 agent.
//!
//! When the agent runs as PID 1 (EROFS boot path), the busybox trampoline has
//! already mounted /proc, /sys, /dev (devtmpfs), and /arcbox (VirtioFS).
//!
//! This module sets up everything else: writable tmpfs layers over the read-only
//! EROFS rootfs, populates /etc, mounts pseudo-filesystems, and configures networking.
//!
//! All operations are idempotent and best-effort — failures are logged but do not
//! abort, since PID 1 must not exit.

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use nix::mount::{MsFlags, mount};

    /// Runs one-time system initialization after trampoline hands off to agent.
    ///
    /// Trampoline already mounted: /proc, /sys, /dev, /arcbox (VirtioFS).
    /// EROFS rootfs is purely structural. All writable state goes on tmpfs.
    pub fn init_system() {
        // Writable layers on top of read-only EROFS.
        mount_tmpfs("/tmp");
        mount_tmpfs("/run");
        mount_tmpfs("/var");
        mount_tmpfs("/etc");

        // Populate /etc with files containerd/dockerd expect.
        write_etc_resolv_conf();
        write_etc_hosts();
        write_etc_passwd();
        write_etc_group();
        write_docker_daemon_dns();

        // TLS CA certificates: EROFS has /cacerts/ca-certificates.crt.
        // Symlink into tmpfs /etc so programs find it at the standard path.
        mkdir_p("/etc/ssl/certs");
        symlink_if_source_exists(
            "/cacerts/ca-certificates.crt",
            "/etc/ssl/certs/ca-certificates.crt",
        );

        // Writable subdirectories under /var.
        mkdir_p("/var/lib/docker");
        mkdir_p("/var/run/docker");
        mkdir_p("/run/containerd");

        // Pseudo-filesystems.
        mount_cgroup2();
        mount_devpts();
        mount_shm();

        // Network.
        setup_networking();

        // Optional host /Users share (non-fatal if not configured).
        mount_virtiofs_optional("users", "/Users");

        tracing::info!("PID 1 system initialization complete");
    }

    fn mount_tmpfs(target: &str) {
        if crate::mount::is_mounted(target) {
            return;
        }
        // Ensure mount point exists — EROFS may not have /etc or /var.
        mkdir_p(target);
        if let Err(e) = mount(
            Some("tmpfs"),
            target,
            Some("tmpfs"),
            MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
            None::<&str>,
        ) {
            tracing::warn!(target, error = %e, "failed to mount tmpfs");
        }
    }

    fn mount_cgroup2() {
        if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            return;
        }
        mkdir_p("/sys/fs/cgroup");
        if let Err(e) = mount(
            Some("cgroup2"),
            "/sys/fs/cgroup",
            Some("cgroup2"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            tracing::warn!(error = %e, "failed to mount cgroup2");
        }
    }

    fn mount_devpts() {
        if Path::new("/dev/pts/ptmx").exists() {
            return;
        }
        mkdir_p("/dev/pts");
        if let Err(e) = mount(
            Some("devpts"),
            "/dev/pts",
            Some("devpts"),
            MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID,
            Some("gid=5,mode=0620"),
        ) {
            tracing::warn!(error = %e, "failed to mount devpts");
        }
    }

    fn mount_shm() {
        if crate::mount::is_mounted("/dev/shm") {
            return;
        }
        mkdir_p("/dev/shm");
        if let Err(e) = mount(
            Some("shm"),
            "/dev/shm",
            Some("tmpfs"),
            MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            None::<&str>,
        ) {
            tracing::warn!(error = %e, "failed to mount /dev/shm");
        }
    }

    fn mount_virtiofs_optional(tag: &str, mountpoint: &str) {
        if crate::mount::is_mounted(mountpoint) {
            return;
        }
        mkdir_p(mountpoint);
        if let Err(e) = mount(
            Some(tag),
            mountpoint,
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            // debug, not warn — this share is optional.
            tracing::debug!(tag, mountpoint, error = %e, "virtiofs share not available");
        }
    }

    fn setup_networking() {
        // Enable IPv4 forwarding for Docker bridge networking.
        if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n") {
            tracing::warn!(error = %e, "failed to enable ip_forward");
        }
        // Bring up loopback interface.
        match std::process::Command::new("/bin/busybox")
            .args(["ip", "link", "set", "lo", "up"])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(
                exit_code = s.code().unwrap_or(-1),
                "loopback 'ip link set lo up' exited non-zero"
            ),
            Err(e) => tracing::warn!(error = %e, "failed to bring up loopback"),
        }

        // Configure the primary interface via DHCP so the guest can reach
        // gateway services (DNS/NAT at 192.168.64.1).
        configure_primary_interface_dhcp();

        // Configure the bridge NIC (eth1) via DHCP for inbound L3 routing.
        // This NIC is connected to Apple's vmnet bridge (bridge100) and
        // provides a real L2 path for host → container traffic.
        // We only take an IP — no default route (outbound stays on eth0).
        configure_bridge_nic();

        // Allow forwarding between the primary interface and sandbox TAP
        // interfaces. Docker/containerd sets the default FORWARD policy to
        // DROP, so blanket ACCEPT rules are required for sandbox traffic.
        setup_sandbox_forwarding();
    }

    fn configure_primary_interface_dhcp() {
        let Some(interface) = detect_primary_interface() else {
            tracing::warn!("no non-loopback network interface found for DHCP");
            return;
        };

        match std::process::Command::new("/bin/busybox")
            .args(["ip", "link", "set", interface.as_str(), "up"])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => {
                tracing::warn!(
                    interface,
                    exit_code = s.code().unwrap_or(-1),
                    "failed to bring interface up before DHCP"
                );
            }
            Err(e) => {
                tracing::warn!(interface, error = %e, "failed to execute 'ip link set up'");
            }
        }

        // BusyBox udhcpc requires a script to apply lease settings.
        let udhcpc_script = "/run/udhcpc.script";
        let script = r#"#!/bin/sh
set -e
case "$1" in
  deconfig)
    /bin/busybox ifconfig "$interface" 0.0.0.0 || true
    ;;
  renew|bound)
    /bin/busybox ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}" broadcast "${broadcast:-+}" up
    if [ -n "${router:-}" ]; then
      while /bin/busybox route del default gw 0.0.0.0 dev "$interface" 2>/dev/null; do :; done
      for r in $router; do
        /bin/busybox route add default gw "$r" dev "$interface" && break
      done
    fi
    ;;
esac
exit 0
"#;

        if let Err(e) = fs::write(udhcpc_script, script) {
            tracing::warn!(error = %e, "failed to write udhcpc script");
            return;
        }
        if let Err(e) = fs::set_permissions(udhcpc_script, fs::Permissions::from_mode(0o755)) {
            tracing::warn!(error = %e, "failed to chmod udhcpc script");
            return;
        }

        match std::process::Command::new("/bin/busybox")
            .args([
                "udhcpc",
                "-i",
                interface.as_str(),
                "-n",
                "-q",
                "-t",
                "3",
                "-T",
                "2",
                "-s",
                udhcpc_script,
            ])
            .status()
        {
            Ok(s) if s.success() => {
                tracing::info!(interface, "DHCP lease acquired");
            }
            Ok(s) => {
                tracing::warn!(
                    interface,
                    exit_code = s.code().unwrap_or(-1),
                    "DHCP request failed"
                );
            }
            Err(e) => {
                tracing::warn!(interface, error = %e, "failed to run udhcpc");
            }
        }
    }

    /// Configures the bridge NIC (second interface) via DHCP.
    ///
    /// Uses a custom udhcpc script that only sets the IP address — no default
    /// route, no DNS. This ensures outbound traffic still goes through eth0
    /// (socketpair datapath), while the bridge NIC is reachable from the host
    /// for inbound container traffic.
    fn configure_bridge_nic() {
        // Find the second non-loopback interface (eth1 or similar).
        let entries = match fs::read_dir("/sys/class/net") {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut candidates: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name == "lo"
                || name.starts_with("dummy")
                || name.starts_with("veth")
                || name.starts_with("br-")
                || name.starts_with("docker")
            {
                continue;
            }
            candidates.push(name);
        }
        candidates.sort();

        // The primary interface is the first one (eth0). The bridge NIC is the second.
        if candidates.len() < 2 {
            tracing::debug!("no bridge NIC found (only {} candidates)", candidates.len());
            return;
        }
        let bridge_iface = &candidates[1];

        // Bring up the interface.
        let _ = std::process::Command::new("/bin/busybox")
            .args(["ip", "link", "set", bridge_iface, "up"])
            .status();

        // DHCP script that only sets the IP, no default route.
        let script = r#"#!/bin/sh
case "$1" in
  deconfig)
    /bin/busybox ifconfig "$interface" 0.0.0.0 || true
    ;;
  renew|bound)
    /bin/busybox ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}" up
    # Intentionally no default route — outbound stays on eth0.
    ;;
esac
exit 0
"#;
        let script_path = "/run/udhcpc-bridge.script";
        if let Err(e) = fs::write(script_path, script) {
            tracing::warn!(error = %e, "failed to write bridge DHCP script");
            return;
        }
        let _ = fs::set_permissions(script_path, fs::Permissions::from_mode(0o755));

        match std::process::Command::new("/bin/busybox")
            .args(["udhcpc", "-i", bridge_iface, "-n", "-q", "-t", "3", "-T", "2", "-s", script_path])
            .status()
        {
            Ok(s) if s.success() => {
                tracing::info!(interface = bridge_iface, "bridge NIC DHCP lease acquired");
            }
            Ok(s) => {
                tracing::warn!(
                    interface = bridge_iface,
                    exit_code = s.code().unwrap_or(-1),
                    "bridge NIC DHCP failed"
                );
            }
            Err(e) => {
                tracing::warn!(interface = bridge_iface, error = %e, "bridge NIC udhcpc failed");
            }
        }

        // Add iptables FORWARD rules for the bridge NIC so container
        // traffic can flow through.
        run_iptables(
            &["-I", "FORWARD", "-i", bridge_iface, "-j", "ACCEPT"],
            "FORWARD accept from bridge NIC",
        );
        run_iptables(
            &["-I", "FORWARD", "-o", bridge_iface, "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED", "-j", "ACCEPT"],
            "FORWARD accept established to bridge NIC",
        );
    }

    /// Install blanket iptables FORWARD ACCEPT rules for the sandbox subnet.
    ///
    /// The subnet is read from the VMM config (default `10.88.0.0/16`).
    /// Uses `-I` (insert at chain top) so rules take effect even when
    /// Docker sets the default FORWARD policy to DROP.
    fn setup_sandbox_forwarding() {
        let config = crate::config::load();
        let subnet = &config.network.cidr;

        run_iptables(
            &["-I", "FORWARD", "-d", subnet, "-j", "ACCEPT"],
            "FORWARD accept to sandbox subnet",
        );
        run_iptables(
            &["-I", "FORWARD", "-s", subnet, "-j", "ACCEPT"],
            "FORWARD accept from sandbox subnet",
        );

        tracing::info!(subnet, "sandbox forwarding rules installed");
    }

    /// Run an iptables command, logging on failure.
    fn run_iptables(args: &[&str], desc: &str) {
        match std::process::Command::new("iptables").args(args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(
                desc,
                exit_code = s.code().unwrap_or(-1),
                "iptables rule failed"
            ),
            Err(e) => tracing::warn!(desc, error = %e, "failed to run iptables"),
        }
    }

    fn detect_primary_interface() -> Option<String> {
        let entries = fs::read_dir("/sys/class/net").ok()?;
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Skip loopback and virtual interfaces that are not real NICs.
            if name == "lo"
                || name.starts_with("dummy")
                || name.starts_with("veth")
                || name.starts_with("br-")
                || name.starts_with("docker")
            {
                continue;
            }
            candidates.push(name);
        }
        candidates.sort();
        candidates.into_iter().next()
    }

    fn write_etc_resolv_conf() {
        // Point to the local guest DNS server (dns_server.rs) which handles:
        // - Container/sandbox name resolution from its registries
        // - *.arcbox.local → authoritative NXDOMAIN if not registered
        // - Everything else → forward to gateway (192.168.64.1)
        let content = "nameserver 127.0.0.1\n";
        if let Err(e) = std::fs::write("/etc/resolv.conf", content) {
            tracing::warn!(error = %e, "failed to write /etc/resolv.conf");
        }
    }

    /// Configures Docker daemon to use the gateway as its DNS server.
    ///
    /// Containers get their DNS from the Docker daemon config, NOT from the
    /// guest's /etc/resolv.conf. We point them to 192.168.64.1 (the gateway)
    /// so container DNS queries go through the host-side forwarder which can
    /// resolve *.arcbox.local names registered from the host.
    pub fn write_docker_daemon_dns() {
        mkdir_p("/etc/docker");
        let content = r#"{"dns": ["192.168.64.1"]}"#;
        if let Err(e) = std::fs::write("/etc/docker/daemon.json", content) {
            tracing::warn!(error = %e, "failed to write /etc/docker/daemon.json");
        }
    }

    fn write_etc_hosts() {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "arcbox".to_string());
        let content = format!("127.0.0.1\tlocalhost\n::1\t\tlocalhost\n127.0.1.1\t{hostname}\n");
        if let Err(e) = std::fs::write("/etc/hosts", content) {
            tracing::warn!(error = %e, "failed to write /etc/hosts");
        }
    }

    fn write_etc_passwd() {
        let content =
            "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n";
        if let Err(e) = std::fs::write("/etc/passwd", content) {
            tracing::warn!(error = %e, "failed to write /etc/passwd");
        }
    }

    fn write_etc_group() {
        let content = "root:x:0:\ntty:x:5:\nnobody:x:65534:\n";
        if let Err(e) = std::fs::write("/etc/group", content) {
            tracing::warn!(error = %e, "failed to write /etc/group");
        }
    }

    fn mkdir_p(path: &str) {
        if let Err(e) = std::fs::create_dir_all(path) {
            tracing::warn!(path, error = %e, "failed to create directory");
        }
    }

    fn symlink_if_source_exists(source: &str, link: &str) {
        if !Path::new(source).exists() {
            tracing::debug!(source, "symlink source does not exist, skipping");
            return;
        }
        match unix_fs::symlink(source, link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Idempotent: symlink already in place.
            }
            Err(e) => {
                tracing::warn!(source, link, error = %e, "failed to create symlink");
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use platform::init_system;

#[cfg(not(target_os = "linux"))]
pub fn init_system() {
    tracing::warn!("init_system is only functional on Linux");
}
