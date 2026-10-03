//! Host facts reported to the gateway at enrollment and in heartbeats.

use arcbox_fleet_proto::v1::{Backend, Capability, HostTelemetry};
use serde::Serialize;
use tokio::sync::watch;

/// Map Rust's `target_os` to the gateway's lowercase `RunnerOs` naming.
pub fn map_os(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other, // linux, windows
    }
}

/// Map Rust's `target_arch` to the gateway's lowercase `RunnerArch` naming.
pub fn map_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Gateway OS string for this host (e.g. `darwin`).
pub fn host_os() -> String {
    map_os(std::env::consts::OS).to_string()
}

/// Gateway arch string for this host (e.g. `arm64`).
pub fn host_arch() -> String {
    map_arch(std::env::consts::ARCH).to_string()
}

/// Best-effort host name; falls back to `"unknown"`.
pub fn machine_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Logical CPU count, at least 1.
pub fn cpu_cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

/// Total physical memory in MiB.
pub fn mem_mib() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() / 1024 / 1024
}

/// Free-form host facts, the `host_info_json` of enrollment and of every
/// `Attach`. Descriptive only — nothing here influences placement (that's
/// [`telemetry`]). Fields are best-effort: `null` where the platform does
/// not report them, and `disks` also while the probe is still running (see
/// [`HostFacts`]).
#[derive(Debug, Serialize)]
pub struct HostInfo {
    os: Option<String>,
    os_version: Option<String>,
    kernel: Option<String>,
    arch: &'static str,
    cpu_model: Option<String>,
    hostname: Option<String>,
    boot_time_unix: u64,
    agent_pid: u32,
    disks: Option<Vec<DiskInfo>>,
    lan_ips: Vec<LanIp>,
}

#[derive(Debug, Serialize)]
struct DiskInfo {
    name: String,
    mount_point: String,
    file_system: String,
    total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct LanIp {
    interface: String,
    ip: String,
    prefix: u8,
}

impl HostInfo {
    /// The facts that answer at once — `sysctl`-backed values and the
    /// interface list. `disks` is left to [`Self::probe`].
    fn quick() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        let cpu_model = sys.cpus().first().map(|c| c.brand().to_string());

        let lan_ips = sysinfo::Networks::new_with_refreshed_list()
            .iter()
            .flat_map(|(iface, data)| {
                data.ip_networks()
                    .iter()
                    .filter(|net| !net.addr.is_loopback() && !net.addr.is_unspecified())
                    .map(move |net| LanIp {
                        interface: iface.clone(),
                        ip: net.addr.to_string(),
                        prefix: net.prefix,
                    })
            })
            .collect();

        Self {
            os: sysinfo::System::name(),
            os_version: sysinfo::System::long_os_version(),
            kernel: sysinfo::System::kernel_version(),
            arch: std::env::consts::ARCH,
            cpu_model,
            hostname: hostname::get().ok().and_then(|h| h.into_string().ok()),
            boot_time_unix: sysinfo::System::boot_time(),
            agent_pid: std::process::id(),
            disks: None,
            lan_ips,
        }
    }

    /// The full picture. Walking the volumes blocks for as long as the
    /// slowest one takes to answer — a stalled network mount was measured
    /// at 20 s — so this runs on the [`HostFacts`] thread, never on the
    /// runtime.
    fn probe() -> Self {
        let disks = sysinfo::Disks::new_with_refreshed_list()
            .iter()
            .map(|d| DiskInfo {
                name: d.name().to_string_lossy().into_owned(),
                mount_point: d.mount_point().display().to_string(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total_bytes: d.total_space(),
            })
            .collect();
        Self {
            disks: Some(disks),
            ..Self::quick()
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("host facts are plain data")
    }
}

/// The host facts, probed once per process on a thread of their own.
///
/// A reader never waits for the probe: [`Self::current`] answers with the
/// probed facts once they exist and with the quick facts (`disks` null)
/// until then, and [`Self::subscribe`] tells when they arrive — the attach
/// handshake sends what it has and completes the set with one
/// `HostFactsUpdate` when the probe finishes afterwards.
#[derive(Clone)]
pub struct HostFacts {
    probed: watch::Receiver<Option<String>>,
}

impl HostFacts {
    /// Start the probe. On a plain thread rather than `spawn_blocking`: the
    /// runtime's drop waits for blocking tasks, and a stalled probe must not
    /// hold up the exec handover of a self-update.
    pub fn probe() -> Self {
        let (tx, rx) = watch::channel(None);
        std::thread::Builder::new()
            .name("host-facts".to_owned())
            .spawn(move || {
                let _ = tx.send(Some(HostInfo::probe().to_json()));
            })
            .expect("spawn the host facts thread");
        Self { probed: rx }
    }

    /// Facts fed by the caller instead of the probe; `tx` completes them.
    #[cfg(test)]
    pub(crate) fn pending() -> (watch::Sender<Option<String>>, Self) {
        let (tx, rx) = watch::channel(None);
        (tx, Self { probed: rx })
    }

    /// The probed facts, or — while the probe has not answered — the quick
    /// facts with `disks` null.
    pub fn current(&self) -> String {
        self.probed
            .borrow()
            .clone()
            .unwrap_or_else(|| HostInfo::quick().to_json())
    }

    /// Where the probe's answer lands: `Some(json)` once, then unchanged.
    pub fn subscribe(&self) -> watch::Receiver<Option<String>> {
        self.probed.clone()
    }
}

/// Live host utilization for the heartbeat — a placement ranking hint for the
/// server and the input to this agent's own admission decision. Load average is
/// 0 on platforms that don't report it (e.g. Windows), which simply makes the
/// load gate a no-op there.
pub fn telemetry() -> HostTelemetry {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    HostTelemetry {
        load_avg_1m: sysinfo::System::load_average().one,
        cpu_count: cpu_cores(),
        mem_total_mib: sys.total_memory() / 1024 / 1024,
        mem_available_mib: sys.available_memory() / 1024 / 1024,
    }
}

/// Build the capabilities this agent advertises: which `(os, arch)` it can serve
/// and the backend serving each. There are no capacity numbers — whether the
/// host can take another job is decided per offer from live [`telemetry`].
///
/// - **Docker-served Linux capabilities.** `docker_arches` is the set of Linux
///   arches Docker can run (empty when Docker is absent), each backed by
///   `docker`. On Apple Silicon that is arm64 (native) plus amd64 (Rosetta).
/// - **VM capability.** `vm_active` means the local arcbox-daemon can boot
///   disposable macOS guests (see `crate::vm`); the native `(os, arch)` is
///   then served by `vm`, which wins over the host runner — isolation is the
///   point of the backend.
/// - **Host-runner capability.** The native `(os, arch)` served by the
///   pre-installed runner, backed by `host_runner`. Omitted when no runner
///   script is configured, or when the native pair is already served by
///   Docker (a Linux host) or by the VM backend (above).
/// - **Windows-via-interop capability.** `interop_active` means this Linux
///   agent runs inside WSL2 and the interop probe passed (see
///   `crate::interop`); `windows` on the native arch (WSL always matches the
///   Windows host's arch) is then served across the interop boundary.
///   Additive and host_runner-backed on the wire — it IS the host's
///   pre-installed Windows runner.
pub fn capabilities(
    runner_script_present: bool,
    docker_arches: &[String],
    vm_active: bool,
    interop_active: bool,
) -> Vec<Capability> {
    build_capabilities(
        &host_os(),
        &host_arch(),
        runner_script_present,
        docker_arches,
        vm_active,
        interop_active,
    )
}

/// Platform-independent core of [`capabilities`], taking the native `(os, arch)`
/// explicitly so it can be exercised for every platform in tests.
fn build_capabilities(
    native_os: &str,
    native_arch: &str,
    runner_script_present: bool,
    docker_arches: &[String],
    vm_active: bool,
    interop_active: bool,
) -> Vec<Capability> {
    let mut caps = Vec::new();

    for arch in docker_arches {
        caps.push(Capability {
            os: "linux".to_owned(),
            arch: arch.clone(),
            backed_by: Backend::Docker as i32,
        });
    }

    if interop_active {
        caps.push(Capability {
            os: "windows".to_owned(),
            arch: native_arch.to_owned(),
            backed_by: Backend::HostRunner as i32,
        });
    }

    if vm_active {
        caps.push(Capability {
            os: native_os.to_owned(),
            arch: native_arch.to_owned(),
            backed_by: Backend::Vm as i32,
        });
        return caps;
    }

    let host_served_by_docker = native_os == "linux" && !docker_arches.is_empty();
    if runner_script_present && !host_served_by_docker {
        caps.push(Capability {
            os: native_os.to_owned(),
            arch: native_arch.to_owned(),
            backed_by: Backend::HostRunner as i32,
        });
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_os_and_arch_to_gateway_naming() {
        assert_eq!(map_os("macos"), "darwin");
        assert_eq!(map_os("linux"), "linux");
        assert_eq!(map_arch("aarch64"), "arm64");
        assert_eq!(map_arch("x86_64"), "amd64");
    }

    fn triple(c: &Capability) -> (&str, &str, i32) {
        (c.os.as_str(), c.arch.as_str(), c.backed_by)
    }

    #[test]
    fn host_only_without_docker() {
        let caps = build_capabilities("darwin", "arm64", true, &[], false, false);
        assert_eq!(caps.len(), 1);
        assert_eq!(
            triple(&caps[0]),
            ("darwin", "arm64", Backend::HostRunner as i32)
        );
    }

    #[test]
    fn macos_adds_docker_linux_capabilities() {
        let arches = vec!["arm64".to_owned(), "amd64".to_owned()];
        let caps = build_capabilities("darwin", "arm64", true, &arches, false, false);
        // Two docker linux capabilities plus the darwin host-runner one.
        assert_eq!(caps.len(), 3);
        assert_eq!(triple(&caps[0]), ("linux", "arm64", Backend::Docker as i32));
        assert_eq!(triple(&caps[1]), ("linux", "amd64", Backend::Docker as i32));
        assert_eq!(
            triple(&caps[2]),
            ("darwin", "arm64", Backend::HostRunner as i32)
        );
    }

    #[test]
    fn linux_host_capability_is_docker_served() {
        // On a Linux host with Docker the native capability IS the docker linux
        // one — no separate host-runner capability is added.
        let caps = build_capabilities("linux", "amd64", true, &["amd64".to_owned()], false, false);
        assert_eq!(caps.len(), 1);
        assert_eq!(triple(&caps[0]), ("linux", "amd64", Backend::Docker as i32));
    }

    #[test]
    fn omits_host_capability_without_runner_script() {
        // Docker-only macOS: no runner script means no darwin capability.
        let caps = build_capabilities(
            "darwin",
            "arm64",
            false,
            &["arm64".to_owned()],
            false,
            false,
        );
        assert_eq!(caps.len(), 1);
        assert_eq!(triple(&caps[0]), ("linux", "arm64", Backend::Docker as i32));
    }

    #[test]
    fn vm_backend_serves_the_native_pair_and_wins_over_host_runner() {
        // With the VM backend active, darwin/arm64 is VM-served even though a
        // runner script is configured — isolation wins.
        let caps = build_capabilities("darwin", "arm64", true, &["arm64".to_owned()], true, false);
        assert_eq!(caps.len(), 2);
        assert_eq!(triple(&caps[0]), ("linux", "arm64", Backend::Docker as i32));
        assert_eq!(triple(&caps[1]), ("darwin", "arm64", Backend::Vm as i32));
    }

    /// The interop windows capability is additive: a WSL Linux host with
    /// Docker advertises its linux capability as before, plus windows on
    /// the native arch, host_runner-backed on the wire.
    #[test]
    fn interop_adds_a_windows_capability_alongside_docker_linux() {
        let caps = build_capabilities("linux", "amd64", false, &["amd64".to_owned()], false, true);
        assert_eq!(caps.len(), 2);
        assert_eq!(triple(&caps[0]), ("linux", "amd64", Backend::Docker as i32));
        assert_eq!(
            triple(&caps[1]),
            ("windows", "amd64", Backend::HostRunner as i32)
        );
    }

    /// Interop alone (no Docker, no runner script) still serves windows —
    /// the WSL distro exists purely to host the agent.
    #[test]
    fn interop_alone_serves_only_windows() {
        let caps = build_capabilities("linux", "amd64", false, &[], false, true);
        assert_eq!(caps.len(), 1);
        assert_eq!(
            triple(&caps[0]),
            ("windows", "amd64", Backend::HostRunner as i32)
        );
    }

    #[test]
    fn vm_backend_without_runner_script_still_serves_darwin() {
        let caps = build_capabilities("darwin", "arm64", false, &[], true, false);
        assert_eq!(caps.len(), 1);
        assert_eq!(triple(&caps[0]), ("darwin", "arm64", Backend::Vm as i32));
    }

    /// Until the probe answers, the quick facts stand in; once it has, the
    /// probed set is what every reader gets.
    #[tokio::test]
    async fn facts_are_the_quick_set_until_the_probe_answers() {
        let (probe, facts) = HostFacts::pending();

        let quick: serde_json::Value = serde_json::from_str(&facts.current()).unwrap();
        assert!(quick["disks"].is_null(), "{quick}");
        assert_eq!(quick["agent_pid"], std::process::id());

        probe.send_replace(Some(r#"{"probed":true}"#.to_owned()));
        assert_eq!(facts.current(), r#"{"probed":true}"#);
    }
}
