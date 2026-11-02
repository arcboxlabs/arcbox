//! Host capability probes that answer without constructing a hypervisor.
//!
//! `Hypervisor::capabilities` is the richer surface, but reaching it means
//! opening `/dev/kvm` (or a VZ configuration) first. Callers that must
//! answer before any VM exists — the sandbox capability gate, which
//! `GetCapabilities` and the `Create` fail-fast path both consult — use
//! these instead. The concrete backends read the same probes when they
//! fill in `PlatformCapabilities`, so the two never drift.

/// Whether this host can run a VM at all, and the platform's own reason
/// when it cannot.
///
/// A Mac without it cannot gain it while the process lives; a Linux host
/// gains it only once the KVM module loads. Either way an immediate retry
/// meets the same host, so a start that fails for this reason is not
/// retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostVirtualization {
    /// True when at least one of this platform's hypervisors can create a
    /// VM.
    pub available: bool,
    /// Why not, when `available` is false; empty otherwise. A complete
    /// clause, so a caller can prefix its own context.
    pub reason: &'static str,
}

impl HostVirtualization {
    /// `Err` carrying [`Self::reason`] when the host cannot run a VM, for
    /// work that must stop before it prepares one.
    ///
    /// # Errors
    ///
    /// Returns the reason when `available` is false.
    pub const fn require(self) -> Result<(), &'static str> {
        if self.available {
            Ok(())
        } else {
            Err(self.reason)
        }
    }
}

/// Probes whether this host provides hardware virtualization.
///
/// On macOS a host is capable when either framework says so: the two
/// backends have no shared probe, and calling a capable Mac incapable
/// would refuse every VM on it. Cached there, like
/// [`host_nested_virt`]; re-probed elsewhere, where `/dev/kvm` can appear
/// once the module loads.
#[must_use]
pub fn host_virtualization() -> HostVirtualization {
    let available = virtualization_cached_or_reprobed();
    HostVirtualization {
        available,
        reason: if available {
            ""
        } else {
            NO_VIRTUALIZATION_REASON
        },
    }
}

#[cfg(target_os = "macos")]
fn virtualization_cached_or_reprobed() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(probe_virtualization)
}

#[cfg(not(target_os = "macos"))]
fn virtualization_cached_or_reprobed() -> bool {
    probe_virtualization()
}

#[cfg(target_os = "macos")]
const NO_VIRTUALIZATION_REASON: &str = concat!(
    "this Mac does not provide hardware virtualization (neither ",
    "Virtualization.framework nor Hypervisor.framework is available, ",
    "as inside a macOS virtual machine)",
);

/// `VZVirtualMachine.isSupported`, or `kern.hv_support` for
/// Hypervisor.framework.
///
/// A macOS guest answers no to both: `isSupported` is false and the sysctl
/// does not exist (measured 2026-11-02 on a `VirtualMac2,1` under macOS
/// 26.3.1, where `hv_vm_create` also returns `HV_UNSUPPORTED`).
#[cfg(target_os = "macos")]
fn probe_virtualization() -> bool {
    arcbox_vz::is_supported() || sysctl_hv_support()
}

#[cfg(target_os = "macos")]
fn sysctl_hv_support() -> bool {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: `sysctlbyname` writes at most `len` bytes into `value`, a
    // correctly sized `c_int` we own; an absent name fails with ENOENT
    // without writing.
    let ret = unsafe {
        libc::sysctlbyname(
            c"kern.hv_support".as_ptr(),
            std::ptr::addr_of_mut!(value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    ret == 0 && value == 1
}

#[cfg(target_os = "linux")]
const NO_VIRTUALIZATION_REASON: &str =
    "this host does not provide hardware virtualization (/dev/kvm is missing)";

#[cfg(target_os = "linux")]
fn probe_virtualization() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const NO_VIRTUALIZATION_REASON: &str = "this platform has no supported hypervisor";

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe_virtualization() -> bool {
    false
}

/// Whether this host can nest a VM, and the platform's own reason when it
/// cannot.
///
/// The reason is platform knowledge, not product copy: it names the
/// hardware or kernel requirement that failed, so a caller can surface it
/// without carrying a `#[cfg]` of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedVirtSupport {
    /// True when the hardware and kernel allow a nested guest.
    pub supported: bool,
    /// Why not, when `supported` is false; empty otherwise.
    ///
    /// Phrased as the requirement that failed, with no leading subject, so
    /// a caller can prefix its own context — `arcbox-api` renders it as
    /// `"sandboxes cannot run on this host: {reason}"` — without ending up
    /// with two stapled-together clauses.
    pub reason: &'static str,
}

/// Probes nested-virtualization support on this host.
///
/// Cached on macOS, where this is a fixed hardware/OS property that
/// cannot change while the process lives. Re-probed on every call
/// elsewhere: on Linux the answer depends on the `kvm_intel`/`kvm_amd`
/// module being loaded, and this function is reachable — via the sandbox
/// capability gate — before any VM has ever booted (in particular under
/// `--no-linux-vm`, where nothing in this process ever opens `/dev/kvm`).
/// A query that lands before the module autoloads would otherwise cache a
/// false negative for the rest of the process's life.
#[must_use]
pub fn host_nested_virt() -> NestedVirtSupport {
    let supported = cached_or_reprobed();
    NestedVirtSupport {
        supported,
        reason: if supported { "" } else { UNSUPPORTED_REASON },
    }
}

#[cfg(target_os = "macos")]
fn cached_or_reprobed() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(probe_nested_virt)
}

#[cfg(not(target_os = "macos"))]
fn cached_or_reprobed() -> bool {
    probe_nested_virt()
}

#[cfg(target_os = "macos")]
const UNSUPPORTED_REASON: &str = concat!(
    "nested virtualization requires Apple Silicon M3 or newer ",
    "with macOS 15 or newer",
);

/// `VZGenericPlatformConfiguration.isNestedVirtualizationSupported`.
#[cfg(target_os = "macos")]
fn probe_nested_virt() -> bool {
    arcbox_vz::GenericPlatform::is_nested_virt_supported()
}

#[cfg(target_os = "linux")]
const UNSUPPORTED_REASON: &str = concat!(
    "the host kernel does not expose nested KVM ",
    "(/sys/module/kvm_{intel,amd}/parameters/nested)",
);

/// Nested-KVM module parameters.
///
/// Not gated on x86_64: the files simply do not exist on other
/// architectures, so the read fails and the answer is false — which is
/// also the honest answer for ARM until the kernel grows a probe for
/// ARMv8.4 nested virtualization.
#[cfg(target_os = "linux")]
fn probe_nested_virt() -> bool {
    ["kvm_intel", "kvm_amd"].iter().any(|module| {
        std::fs::read_to_string(format!("/sys/module/{module}/parameters/nested"))
            .is_ok_and(|content| matches!(content.trim(), "1" | "Y" | "y"))
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const UNSUPPORTED_REASON: &str = "nested virtualization is not supported on this platform";

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe_nested_virt() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // The reason is user-facing: `arcbox-computer` surfaces it verbatim in
    // the `NESTED_VIRT_UNSUPPORTED` error a failing `Create` returns, so
    // the hardware requirement it names is contract, not an internal
    // string. Pin it here, where the constant lives.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_reason_names_the_hardware_requirement() {
        let reason = super::UNSUPPORTED_REASON;
        assert!(reason.contains("M3"), "{reason}");
        assert!(reason.contains("macOS 15"), "{reason}");
    }

    #[test]
    fn an_unsupported_host_always_carries_a_reason() {
        let support = host_nested_virt();
        assert_eq!(support.supported, support.reason.is_empty());
    }

    #[test]
    fn a_host_without_virtualization_always_carries_a_reason() {
        let host = host_virtualization();
        assert_eq!(host.available, host.reason.is_empty());
    }

    #[test]
    fn the_probe_is_stable_across_repeated_calls() {
        // Cached on macOS (a fixed hardware property); re-probed every call
        // on Linux (see `cached_or_reprobed`). Either way, two calls back to
        // back must agree — nothing in this process changes kernel module
        // state between them.
        assert_eq!(host_nested_virt(), host_nested_virt());
    }
}
