//! Configuration management.
//!
//! `ArcBox` configuration is loaded from multiple sources with the following priority:
//!
//! 1. Environment variables (ARCBOX_*)
//! 2. Configuration file (~/.config/arcbox/config.toml)
//! 3. Default values
//!
//! ## Example Configuration File
//!
//! ```toml
//! # ArcBox configuration file
//! data_dir = "~/.arcbox"
//!
//! [vm]
//! # cpus = 8         # default: host core count; `abctl system resources` writes these
//! # memory_mb = 8192  # default: half of host RAM (512–16384)
//! # autostart = true  # boot the default Linux VM (Docker/K8s); false = VM-host only
//!
//! [machine]
//! disk_gb = 50
//! default_distro = "ubuntu"
//!
//! [network]
//! subnet = "10.0.2.0/24"
//! dns = ["8.8.8.8", "8.8.4.4"]
//! # proxy = "system"                 # or "none", or "socks5://127.0.0.1:1080"
//! # proxy_exclude = [".corp.example"] # NO_PROXY-style hosts that stay direct
//!
//! [container]
//! guest_docker_vsock_port = 2375
//! cidr = "172.16.0.0/12"
//!
//! [docker]
//! # expose_ports_to_lan = true     # false: -p 8080:80 binds 127.0.0.1 only
//! # registry_mirrors = ["https://mirror.example.com"]
//! # insecure_registries = ["registry.corp:5000"]
//! # [docker.engine]                # any other dockerd daemon.json key
//! # max-concurrent-downloads = 6
//!
//! [logging]
//! level = "info"
//! ```

use arcbox_constants::container_network::ContainerNetwork;
use arcbox_constants::paths::{ArcboxProfile, HostLayout};
use arcbox_constants::ports::DOCKER_API_VSOCK_PORT;
use arcbox_fakeip::proxy_policy::{ProxyPolicy, ProxySettings};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub mod persist;

/// `ArcBox` configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Runtime profile selected by the daemon.
    #[serde(skip)]
    pub profile: ArcboxProfile,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Default VM configuration.
    pub vm: VmDefaults,
    /// Default machine configuration.
    pub machine: MachineDefaults,
    /// Network configuration.
    pub network: NetworkConfig,
    /// Docker API configuration.
    pub docker: DockerConfig,
    /// Container runtime backend configuration.
    pub container: ContainerRuntimeConfig,
    /// Logging configuration.
    pub logging: LoggingConfig,
    /// Storage configuration.
    pub storage: StorageConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self::for_profile(ArcboxProfile::Production)
    }
}

impl Config {
    /// Creates default configuration for a runtime profile.
    #[must_use]
    pub fn for_profile(profile: ArcboxProfile) -> Self {
        let layout = HostLayout::for_profile(profile);
        Self {
            profile,
            data_dir: layout.data_dir,
            vm: VmDefaults::default(),
            machine: MachineDefaults::default(),
            network: NetworkConfig::default(),
            docker: DockerConfig::for_profile(profile),
            container: ContainerRuntimeConfig::default(),
            logging: LoggingConfig::default(),
            storage: StorageConfig::default(),
        }
    }

    /// Loads configuration from files and environment.
    ///
    /// Configuration sources (in order of precedence):
    /// 1. Environment variables (ARCBOX_*)
    /// 2. User config file (~/.config/arcbox/config.toml)
    /// 3. System config file (/etc/arcbox/config.toml)
    /// 4. Default values
    ///
    /// # Errors
    ///
    /// Returns an error if configuration cannot be loaded.
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Self::load_for_profile(ArcboxProfile::from_env_or_default())
    }

    /// Loads configuration for a runtime profile from files and environment.
    ///
    /// Explicit `ARCBOX_*` environment values and config file values override
    /// profile defaults.
    pub fn load_for_profile(profile: ArcboxProfile) -> Result<Self, Box<figment::Error>> {
        let mut figment = Figment::new()
            .merge(Serialized::defaults(Self::for_profile(profile)))
            .merge(Toml::file(system_config_path()));
        for path in user_config_paths() {
            figment = figment.merge(Toml::file(path));
        }
        let mut config: Self = figment
            .merge(Env::prefixed("ARCBOX_").split("_"))
            .extract()
            .map_err(Box::new)?;
        config.profile = profile;
        Ok(config)
    }

    /// Loads configuration from a specific file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load_from(path: impl AsRef<std::path::Path>) -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("ARCBOX_").split("_"))
            .extract()
            .map_err(Box::new)
    }

    /// Returns the boot-asset policy selected by this runtime configuration.
    #[must_use]
    pub fn boot_asset_config(&self) -> crate::boot_assets::BootAssetConfig {
        crate::boot_assets::BootAssetConfig::with_cache_dir(self.data_dir.join("boot"))
            .with_custom_kernel(self.vm.kernel_path.clone())
            .with_unpinned_manifest_allowed(self.profile == ArcboxProfile::Development)
    }

    /// Returns the path to the persistent data directory (`data/`).
    #[must_use]
    pub fn data_subdir(&self) -> PathBuf {
        self.data_dir.join(arcbox_constants::paths::host::DATA)
    }

    /// Returns the path to the images directory (`data/images/`).
    #[must_use]
    pub fn images_dir(&self) -> PathBuf {
        self.data_subdir().join("images")
    }

    /// Returns the path to the containers directory (`data/containers/`).
    #[must_use]
    pub fn containers_dir(&self) -> PathBuf {
        self.data_subdir().join("containers")
    }

    /// Returns the path to the machines directory (`data/machines/`).
    #[must_use]
    pub fn machines_dir(&self) -> PathBuf {
        self.data_subdir().join("machines")
    }

    /// Returns the path to the volumes directory (`data/volumes/`).
    #[must_use]
    pub fn volumes_dir(&self) -> PathBuf {
        self.data_subdir().join("volumes")
    }

    /// Returns the path to the runtime state directory (`run/`).
    #[must_use]
    pub fn run_dir(&self) -> PathBuf {
        self.data_dir.join(arcbox_constants::paths::host::RUN)
    }

    /// Returns the path to the log directory (`log/`).
    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join(arcbox_constants::paths::host::LOG)
    }

    /// Returns the path to the persistent Docker data image (`data/docker.img`).
    #[must_use]
    pub fn docker_img_path(&self) -> PathBuf {
        self.data_subdir().join("docker.img")
    }

    /// Returns the path to the Docker metadata image (`data/docker-meta.img`).
    ///
    /// Paired with [`Self::docker_img_path`]: the ext4 volume carrying the
    /// fsync-hot boltdb metadata (see ../company/engineering/arcbox/plans/ext4-metadata-volume.md).
    #[must_use]
    pub fn docker_meta_img_path(&self) -> PathBuf {
        self.data_subdir().join("docker-meta.img")
    }
}

/// Default VM configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VmDefaults {
    /// Default number of CPUs (default: host core count).
    pub cpus: u32,
    /// Default memory in MB.
    pub memory_mb: u64,
    /// Kernel path (optional, uses embedded kernel if not set).
    pub kernel_path: Option<PathBuf>,
    /// macOS hypervisor backend for the System VM (`"vz"` or `"hv"`).
    ///
    /// First-boot default only: once the machine exists, its persisted
    /// backend (as switched via `abctl system backend`) wins. Settable
    /// non-interactively via `ARCBOX_VM_BACKEND` or `config.toml` — the
    /// entry point for the dual-backend e2e matrix.
    pub backend: arcbox_vmm::VmBackend,
    /// Whether to boot the default Linux VM (the Docker/Kubernetes system VM)
    /// on daemon startup. When `false`, the daemon runs as a VM host only:
    /// the Linux VM never starts and the Docker API, Docker CLI integration,
    /// and Kubernetes proxy are all disabled. macOS guest management is
    /// unaffected. Overridden to `false` by the daemon's `--no-linux-vm` flag.
    pub autostart: bool,
}

impl VmDefaults {
    /// Returns the effective CPU count, resolving `0` to the host core
    /// count default.
    ///
    /// `0` means "use the default" both on the wire (gRPC
    /// `CreateMachineRequest.cpus`) and in `config.toml`, so callers must
    /// never propagate it into a VM configuration verbatim.
    #[must_use]
    pub fn effective_cpus(&self) -> u32 {
        if self.cpus == 0 {
            arcbox_hypervisor::default_vm_cpu_count()
        } else {
            self.cpus
        }
    }
}

impl Default for VmDefaults {
    fn default() -> Self {
        Self {
            cpus: arcbox_hypervisor::default_vm_cpu_count(),
            memory_mb: arcbox_hypervisor::default_vm_memory_size() / (1024 * 1024),
            kernel_path: None,
            backend: arcbox_vmm::VmBackend::default(),
            autostart: true,
        }
    }
}

/// Default machine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineDefaults {
    /// Default disk size in GB.
    pub disk_gb: u64,
    /// Default Linux distribution.
    pub default_distro: String,
    /// Default distribution version.
    pub default_version: Option<String>,
    /// Auto-mount home directory.
    pub auto_mount_home: bool,
}

impl Default for MachineDefaults {
    fn default() -> Self {
        Self {
            disk_gb: 50,
            default_distro: "ubuntu".to_string(),
            default_version: None,
            auto_mount_home: true,
        }
    }
}

/// Network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Subnet for NAT networking.
    pub subnet: String,
    /// Gateway address (first address in subnet if not specified).
    pub gateway: Option<String>,
    /// DNS servers.
    pub dns: Vec<String>,
    /// Enable IPv6.
    pub ipv6: bool,
    /// MTU for virtual network interfaces.
    pub mtu: u16,
    /// Where guest egress goes: `system` (follow the Mac's proxy settings,
    /// the default), `none` (always direct), or a proxy URL such as
    /// `socks5://127.0.0.1:1080` or `http://proxy.corp:3128`.
    #[serde(
        serialize_with = "serialize_proxy_policy",
        deserialize_with = "deserialize_proxy_policy"
    )]
    pub proxy: ProxyPolicy,
    /// Hosts that bypass the proxy, in `NO_PROXY` form (`example.com`,
    /// `.example.com`, `*.example.com`). Added to the Mac's own exclusions
    /// under `proxy = "system"`.
    pub proxy_exclude: Vec<String>,
}

impl NetworkConfig {
    /// The guest egress policy as the datapath consumes it.
    #[must_use]
    pub fn proxy_settings(&self) -> ProxySettings {
        ProxySettings {
            policy: self.proxy.clone(),
            exclude: self.proxy_exclude.clone(),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            subnet: "10.0.2.0/24".to_string(),
            gateway: None,
            dns: vec!["8.8.8.8".to_string(), "8.8.4.4".to_string()],
            ipv6: false,
            mtu: 1500,
            proxy: ProxyPolicy::System,
            proxy_exclude: Vec::new(),
        }
    }
}

fn serialize_proxy_policy<S>(policy: &ProxyPolicy, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_str(policy)
}

fn deserialize_proxy_policy<'de, D>(deserializer: D) -> Result<ProxyPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    String::deserialize(deserializer)?
        .parse()
        .map_err(D::Error::custom)
}

/// Docker API configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// Unix socket path for Docker API clients.
    ///
    /// Daemon startup replaces configured values with its resolved host layout.
    pub socket_path: PathBuf,
    /// Enable Docker API.
    pub enabled: bool,
    /// Whether a port published without a specific address (`-p 8080:80`,
    /// `-p 0.0.0.0:8080:80`) is reachable from other devices on the
    /// network. When `false` such ports bind the Mac's loopback only. A
    /// binding that names a specific address (`-p 192.168.1.5:8080:80`) is
    /// honoured either way.
    pub expose_ports_to_lan: bool,
    /// Registry mirrors `dockerd` pulls through, tried in order before the
    /// upstream registry. Written to `registry-mirrors` in the guest's
    /// `daemon.json`.
    pub registry_mirrors: Vec<String>,
    /// Registries reached over plain HTTP or with an untrusted certificate.
    /// Written to `insecure-registries`.
    pub insecure_registries: Vec<String>,
    /// Further `daemon.json` keys, merged last. ArcBox owns `dns`, `bip`,
    /// `default-address-pools`, `allow-direct-routing`, the `nofile` ulimit
    /// and `features.containerd-snapshotter`; a value for one of those here
    /// is ignored with a warning in the guest log.
    pub engine: serde_json::Map<String, serde_json::Value>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self::for_profile(ArcboxProfile::Production)
    }
}

impl DockerConfig {
    /// Creates default Docker configuration for a runtime profile.
    #[must_use]
    pub fn for_profile(profile: ArcboxProfile) -> Self {
        Self {
            socket_path: HostLayout::for_profile(profile).docker_socket,
            enabled: true,
            expose_ports_to_lan: true,
            registry_mirrors: Vec::new(),
            insecure_registries: Vec::new(),
            engine: serde_json::Map::new(),
        }
    }

    /// The host address a port published without one binds to.
    #[must_use]
    pub const fn default_publish_address(&self) -> Ipv4Addr {
        if self.expose_ports_to_lan {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        }
    }

    /// The `daemon.json` fragment the guest merges over its own keys.
    ///
    /// The two typed lists win over same-named keys in `engine`, so an
    /// operator who set both cannot be surprised by which one applied.
    #[must_use]
    pub fn engine_overrides(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut overrides = self.engine.clone();
        if !self.registry_mirrors.is_empty() {
            overrides.insert(
                "registry-mirrors".into(),
                serde_json::Value::from(self.registry_mirrors.clone()),
            );
        }
        if !self.insecure_registries.is_empty() {
            overrides.insert(
                "insecure-registries".into(),
                serde_json::Value::from(self.insecure_registries.clone()),
            );
        }
        overrides
    }
}

/// Container runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerRuntimeConfig {
    /// Private address pool shared by Docker, guest firewall rules, and the
    /// host route to this runtime's bridge.
    #[serde(
        serialize_with = "serialize_container_network",
        deserialize_with = "deserialize_container_network"
    )]
    pub cidr: ContainerNetwork,
    /// Guest dockerd API vsock port.
    pub guest_docker_vsock_port: u32,
    /// Backend startup timeout in milliseconds.
    ///
    /// Must exceed the guest agent's worst-case runtime bring-up with
    /// headroom: readiness gates on dockerd answering `/_ping`, and the
    /// guest can spend up to ~30 s waiting for containerd plus its ~90 s
    /// dockerd readiness poll (~120 s total) on large data volumes. A
    /// shorter host timeout would abort boots the guest was still going
    /// to finish.
    pub startup_timeout_ms: u64,
}

impl Default for ContainerRuntimeConfig {
    fn default() -> Self {
        Self {
            cidr: ContainerNetwork::default(),
            guest_docker_vsock_port: DOCKER_API_VSOCK_PORT,
            startup_timeout_ms: 150_000,
        }
    }
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde serialize_with requires a shared reference"
)]
fn serialize_container_network<S>(
    network: &ContainerNetwork,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_str(network)
}

fn deserialize_container_network<'de, D>(deserializer: D) -> Result<ContainerNetwork, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    String::deserialize(deserializer)?
        .parse()
        .map_err(D::Error::custom)
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error).
    pub level: String,
    /// Log to file.
    pub file: Option<PathBuf>,
    /// Log format (text, json).
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: None,
            format: "text".to_string(),
        }
    }
}

/// Storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Storage driver (overlay2, btrfs, zfs).
    pub driver: String,
    /// Image storage backend.
    pub image_backend: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            driver: "overlay2".to_string(),
            image_backend: "oci".to_string(),
        }
    }
}

/// User configuration files, lowest precedence first.
///
/// The documented location is `~/.config/arcbox/config.toml`
/// (`$XDG_CONFIG_HOME` when set). `dirs::config_dir()` is the platform
/// convention instead — `~/Library/Application Support` on macOS — and
/// was the only path read for a long time, so a file there keeps working
/// but the documented one wins when both exist. On Linux the two coincide
/// and the list has one entry.
/// The config file runtime setting changes are written to: the documented
/// `~/.config/arcbox/config.toml` (or its `$XDG_CONFIG_HOME` equivalent),
/// which is also the last one merged and so overrides every other file.
#[must_use]
pub fn writable_user_config_path() -> PathBuf {
    user_config_paths()
        .pop()
        .expect("the XDG config path is always resolvable")
}

fn user_config_paths() -> Vec<PathBuf> {
    let relative = Path::new("arcbox").join("config.toml");
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .map(|dir| dir.join(&relative));
    let platform = dirs::config_dir().map(|dir| dir.join(&relative));

    let mut paths: Vec<PathBuf> = platform.into_iter().chain(xdg).collect();
    paths.dedup();
    paths
}

fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/arcbox/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.profile, ArcboxProfile::Production);
        assert_eq!(config.vm.cpus, arcbox_hypervisor::default_vm_cpu_count());
        // Default memory is half of host RAM, clamped to [512, 16384] MB.
        let expected_mb = arcbox_hypervisor::default_vm_memory_size() / (1024 * 1024);
        assert_eq!(config.vm.memory_mb, expected_mb);
        assert!(config.vm.memory_mb >= 512);
        assert!(config.vm.memory_mb <= 16384);
        assert_eq!(config.machine.disk_gb, 50);
        assert!(config.docker.enabled);
        assert_eq!(config.container.cidr.to_string(), "172.16.0.0/12");
        assert_eq!(
            config.container.guest_docker_vsock_port,
            DOCKER_API_VSOCK_PORT
        );
    }

    #[test]
    fn boot_asset_policy_preserves_profile_and_custom_kernel() {
        let mut config = Config::for_profile(ArcboxProfile::Development);
        config.vm.kernel_path = Some(PathBuf::from("/custom/kernel"));

        let boot = config.boot_asset_config();

        assert!(boot.allow_unpinned_manifest);
        assert_eq!(boot.custom_kernel, config.vm.kernel_path);
        assert_eq!(boot.cache_dir, config.data_dir.join("boot"));
    }

    #[test]
    fn test_effective_cpus_zero_resolves_to_default() {
        let vm = VmDefaults {
            cpus: 0,
            ..VmDefaults::default()
        };
        assert_eq!(
            vm.effective_cpus(),
            arcbox_hypervisor::default_vm_cpu_count()
        );
    }

    #[test]
    fn test_effective_cpus_explicit_passes_through() {
        let vm = VmDefaults {
            cpus: 3,
            ..VmDefaults::default()
        };
        assert_eq!(vm.effective_cpus(), 3);
    }

    #[test]
    fn vm_backend_defaults_to_vz() {
        assert_eq!(Config::default().vm.backend, arcbox_vmm::VmBackend::Vz);
    }

    #[test]
    fn vm_backend_parses_from_toml() {
        let config: Config = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string("[vm]\nbackend = \"hv\""))
            .extract()
            .expect("config with vm.backend");
        assert_eq!(config.vm.backend, arcbox_vmm::VmBackend::Hv);
    }

    #[test]
    fn published_ports_reach_the_lan_unless_turned_off() {
        let default = Config::default();
        assert!(default.docker.expose_ports_to_lan);
        assert_eq!(
            default.docker.default_publish_address(),
            Ipv4Addr::UNSPECIFIED
        );
        let local: Config = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string("[docker]\nexpose_ports_to_lan = false"))
            .extract()
            .expect("valid docker config");
        assert_eq!(local.docker.default_publish_address(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn docker_engine_overrides_merge_typed_lists_over_free_form_keys() {
        let config: Config = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                r#"
[docker]
registry_mirrors = ["https://mirror.example.com"]
insecure_registries = ["registry.corp:5000"]

[docker.engine]
max-concurrent-downloads = 6
registry-mirrors = ["https://ignored.example.com"]
"#,
            ))
            .extract()
            .expect("valid docker engine config");

        let overrides = config.docker.engine_overrides();
        assert_eq!(overrides["max-concurrent-downloads"], 6);
        assert_eq!(
            overrides["registry-mirrors"],
            serde_json::json!(["https://mirror.example.com"])
        );
        assert_eq!(
            overrides["insecure-registries"],
            serde_json::json!(["registry.corp:5000"])
        );
        assert!(Config::default().docker.engine_overrides().is_empty());
    }

    #[test]
    fn network_proxy_defaults_to_following_the_system() {
        let network = Config::default().network;
        assert_eq!(network.proxy, ProxyPolicy::System);
        assert!(network.proxy_exclude.is_empty());
    }

    #[test]
    fn network_proxy_parses_keywords_and_urls_from_toml() {
        let load = |toml: &str| -> Config {
            Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Toml::string(toml))
                .extract()
                .expect("valid network proxy config")
        };

        assert_eq!(
            load("[network]\nproxy = \"none\"").network.proxy,
            ProxyPolicy::None
        );

        let custom = load(
            "[network]\nproxy = \"socks5://127.0.0.1:1080\"\nproxy_exclude = [\".corp.example\"]",
        );
        assert_eq!(custom.network.proxy.to_string(), "socks5://127.0.0.1:1080");
        assert_eq!(custom.network.proxy_exclude, vec![".corp.example"]);

        let settings = custom.network.proxy_settings();
        let env = settings.resolve().expect("a custom proxy resolves");
        assert_eq!(env.socks_proxy.as_ref().map(|p| p.port), Some(1080));
        assert!(env.should_bypass("api.corp.example"));
    }

    #[test]
    fn network_proxy_rejects_an_unusable_url() {
        let invalid = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string("[network]\nproxy = \"ftp://proxy.corp:21\""))
            .extract::<Config>();
        assert!(invalid.is_err());
    }

    #[test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    fn network_proxy_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("ARCBOX_NETWORK_PROXY", "none");
            let config: Config = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Env::prefixed("ARCBOX_").split("_"))
                .extract()?;
            assert_eq!(config.network.proxy, ProxyPolicy::None);
            Ok(())
        });
    }

    #[test]
    fn container_cidr_is_validated_while_loading_config() {
        let config: Config = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string("[container]\ncidr = \"10.80.0.0/20\""))
            .extract()
            .expect("valid container address pool");
        assert_eq!(config.container.cidr.to_string(), "10.80.0.0/20");

        let invalid = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::string("[container]\ncidr = \"10.80.1.0/20\""))
            .extract::<Config>();
        assert!(invalid.is_err());
    }

    #[test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    fn container_cidr_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("ARCBOX_CONTAINER_CIDR", "10.96.0.0/16");
            let config: Config = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Env::prefixed("ARCBOX_").split("_"))
                .extract()?;
            assert_eq!(config.container.cidr.to_string(), "10.96.0.0/16");
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    fn vm_backend_parses_from_env() {
        // Mirrors the env layer of `load_for_profile` without reading the
        // host's real config files.
        figment::Jail::expect_with(|jail| {
            jail.set_env("ARCBOX_VM_BACKEND", "hv");
            let config: Config = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Env::prefixed("ARCBOX_").split("_"))
                .extract()?;
            assert_eq!(config.vm.backend, arcbox_vmm::VmBackend::Hv);
            Ok(())
        });
    }

    #[test]
    fn test_config_paths() {
        let config = Config::default();
        assert!(config.images_dir().ends_with("data/images"));
        assert!(config.containers_dir().ends_with("data/containers"));
        assert!(config.machines_dir().ends_with("data/machines"));
        assert!(config.volumes_dir().ends_with("data/volumes"));
        assert!(config.run_dir().ends_with("run"));
        assert!(config.log_dir().ends_with("log"));
        assert!(config.docker_img_path().ends_with("data/docker.img"));
    }

    /// The documented `~/.config/arcbox/config.toml` must be read — on macOS
    /// `dirs::config_dir()` is `~/Library/Application Support`, and reading
    /// only that silently ignored the file every doc tells users to write.
    #[test]
    #[allow(clippy::result_large_err, reason = "figment::Jail closure signature")]
    fn user_config_reads_the_xdg_path_and_it_wins_over_the_platform_path() {
        figment::Jail::expect_with(|jail| {
            let xdg = jail.directory().join("xdg");
            std::fs::create_dir_all(xdg.join("arcbox")).unwrap();
            jail.set_env("XDG_CONFIG_HOME", xdg.to_str().unwrap());

            let paths = user_config_paths();
            let documented = xdg.join("arcbox").join("config.toml");
            assert_eq!(
                paths.last(),
                Some(&documented),
                "XDG path has the last word"
            );

            std::fs::write(&documented, "[network]\nproxy = \"none\"\n").unwrap();
            let config = Config::load_for_profile(ArcboxProfile::Production).unwrap();
            assert_eq!(config.network.proxy, ProxyPolicy::None);
            Ok(())
        });
    }
}
