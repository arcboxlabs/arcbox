/// Environment variable for selecting boot asset version.
pub const BOOT_ASSET_VERSION: &str = "ARCBOX_BOOT_ASSET_VERSION";

/// Environment variable for selecting the runtime profile.
pub const PROFILE: &str = "ARCBOX_PROFILE";

/// Environment variable for overriding the host data directory.
pub const DATA_DIR: &str = "ARCBOX_DATA_DIR";

/// Environment variable for overriding the host mount root (`~/ArcBox`).
///
/// The daemon mounts the guest's docker data and every running machine's
/// root under it. Test daemons point it into their data directory to stay
/// off the user's.
pub const HOST_MOUNT_DIR: &str = "ARCBOX_HOST_MOUNT_DIR";

/// Environment variable for selecting an instance-owned Docker context.
pub const DOCKER_CONTEXT: &str = "ARCBOX_DOCKER_CONTEXT";

/// Environment variable for overriding guest Docker API vsock port.
pub const GUEST_DOCKER_VSOCK_PORT: &str = "ARCBOX_GUEST_DOCKER_VSOCK_PORT";

/// Environment variable overriding the VM idle timeout, in seconds.
///
/// Dev/test knob: the idle-balloon e2e shortens the 5-minute default so an
/// idle shrink happens within a test budget.
pub const IDLE_TIMEOUT_SECS: &str = "ARCBOX_IDLE_TIMEOUT_SECS";

/// Environment variable naming an unpublished QEMU build.
///
/// Set together with [`QEMU_SHA256`]. Only a development daemon honours it;
/// a production daemon runs the build `assets.lock` pins.
pub const QEMU_VERSION: &str = "ARCBOX_QEMU_VERSION";

/// SHA-256 of the archive [`QEMU_VERSION`] names.
pub const QEMU_SHA256: &str = "ARCBOX_QEMU_SHA256";

/// Base URL serving the archive [`QEMU_VERSION`] names. Defaults to the
/// `assets.lock` CDN.
pub const QEMU_CDN: &str = "ARCBOX_QEMU_CDN";
