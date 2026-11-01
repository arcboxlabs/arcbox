//! Fields a computer refuses: in-guest isolation the VM boundary replaces,
//! and anything the guest agent cannot carry out.

use arcbox_oci::{Process, Spec};

use super::WorkloadError;

/// OCI fields that ask for in-guest isolation, rejected because the VM is
/// the boundary (ADR 0010, decision 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// `hooks`
    Hooks,
    /// `linux.namespaces`
    Namespaces,
    /// `linux.seccomp`
    Seccomp,
    /// `process.capabilities`
    Capabilities,
    /// `process.rlimits`
    Rlimits,
    /// `linux.devices` and `linux.resources.devices`
    Devices,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hooks => "hooks",
            Self::Namespaces => "namespaces",
            Self::Seccomp => "seccomp profiles",
            Self::Capabilities => "capabilities",
            Self::Rlimits => "rlimits",
            Self::Devices => "devices",
        })
    }
}

pub(super) fn check_spec(spec: &Spec) -> Result<(), WorkloadError> {
    use WorkloadError::{Rejected as Reject, Unsupported};

    if spec.hooks.as_ref().is_some_and(|hooks| !hooks.is_empty()) {
        return Err(Reject(Rejected::Hooks));
    }
    if spec.root.as_ref().is_some_and(|root| root.readonly) {
        return Err(Unsupported("root.readonly".into()));
    }
    if spec.hostname.is_some() {
        return Err(Unsupported("hostname".into()));
    }
    if spec.domainname.is_some() {
        return Err(Unsupported("domainname".into()));
    }
    if let Some(process) = &spec.process {
        check_process(process)?;
    }
    let Some(linux) = &spec.linux else {
        return Ok(());
    };
    if !linux.namespaces.is_empty() {
        return Err(Reject(Rejected::Namespaces));
    }
    if linux.seccomp.is_some() {
        return Err(Reject(Rejected::Seccomp));
    }
    if !linux.devices.is_empty()
        || linux
            .resources
            .as_ref()
            .is_some_and(|resources| !resources.devices.is_empty())
    {
        return Err(Reject(Rejected::Devices));
    }
    let unsupported = [
        ("linux.uidMappings", !linux.uid_mappings.is_empty()),
        ("linux.gidMappings", !linux.gid_mappings.is_empty()),
        ("linux.sysctl", !linux.sysctl.is_empty()),
        ("linux.cgroupsPath", linux.cgroups_path.is_some()),
        ("linux.resources", linux.resources.is_some()),
        (
            "linux.rootfsPropagation",
            linux.rootfs_propagation.is_some(),
        ),
        ("linux.maskedPaths", !linux.masked_paths.is_empty()),
        ("linux.readonlyPaths", !linux.readonly_paths.is_empty()),
        ("linux.mountLabel", linux.mount_label.is_some()),
        ("linux.timeOffsets", linux.time_offsets.is_some()),
    ];
    first_set(&unsupported)
}

fn check_process(process: &Process) -> Result<(), WorkloadError> {
    if process.capabilities.is_some() {
        return Err(WorkloadError::Rejected(Rejected::Capabilities));
    }
    if !process.rlimits.is_empty() {
        return Err(WorkloadError::Rejected(Rejected::Rlimits));
    }
    let user = process.user.as_ref();
    let unsupported = [
        (
            "process.apparmorProfile",
            process.apparmor_profile.is_some(),
        ),
        ("process.noNewPrivileges", process.no_new_privileges),
        ("process.oomScoreAdj", process.oom_score_adj.is_some()),
        ("process.selinuxLabel", process.selinux_label.is_some()),
        ("process.ioPriority", process.io_priority.is_some()),
        ("process.scheduler", process.scheduler.is_some()),
        (
            "process.execCPUAffinity",
            process.exec_cpu_affinity.is_some(),
        ),
        (
            "process.user.umask",
            user.is_some_and(|user| user.umask.is_some()),
        ),
        (
            "process.user.additionalGids",
            user.is_some_and(|user| !user.additional_gids.is_empty()),
        ),
    ];
    first_set(&unsupported)
}

fn first_set(fields: &[(&str, bool)]) -> Result<(), WorkloadError> {
    match fields.iter().find(|(_, set)| *set) {
        Some((field, _)) => Err(WorkloadError::Unsupported((*field).into())),
        None => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::workload::{Workload, start::tests::spec};

    #[test]
    fn every_isolation_field_is_rejected() {
        let cases = [
            (
                Rejected::Hooks,
                json!({"hooks": {"poststop": [{"path": "/bin/true"}]}}),
            ),
            (
                Rejected::Namespaces,
                json!({"linux": {"namespaces": [{"type": "pid"}]}}),
            ),
            (
                Rejected::Seccomp,
                json!({"linux": {"seccomp": {"defaultAction": "SCMP_ACT_ALLOW"}}}),
            ),
            (
                Rejected::Capabilities,
                json!({"process": {"capabilities": {}}}),
            ),
            (
                Rejected::Rlimits,
                json!({"process": {"rlimits": [{"type": "RLIMIT_NOFILE", "soft": 1, "hard": 1}]}}),
            ),
            (
                Rejected::Devices,
                json!({"linux": {"devices": [{"type": "c", "path": "/dev/fuse"}]}}),
            ),
            (
                Rejected::Devices,
                json!({"linux": {"resources": {"devices": [{"allow": false}]}}}),
            ),
        ];
        for (expected, patch) in cases {
            match Workload::from_spec(&spec(patch)) {
                Err(WorkloadError::Rejected(rejected)) => assert_eq!(rejected, expected),
                other => panic!("{expected:?}: {other:?}"),
            }
        }
        // An empty hooks object asks for nothing.
        Workload::from_spec(&spec(json!({"hooks": {}}))).unwrap();
    }

    #[test]
    fn every_field_the_agent_cannot_carry_is_rejected() {
        let mapping = json!([{"containerId": 0, "hostId": 1000, "size": 1}]);
        let cases = [
            ("root.readonly", json!({"root": {"readonly": true}})),
            ("hostname", json!({"hostname": "box"})),
            ("domainname", json!({"domainname": "local"})),
            (
                "process.apparmorProfile",
                json!({"process": {"apparmorProfile": "p"}}),
            ),
            (
                "process.noNewPrivileges",
                json!({"process": {"noNewPrivileges": true}}),
            ),
            (
                "process.oomScoreAdj",
                json!({"process": {"oomScoreAdj": 0}}),
            ),
            (
                "process.selinuxLabel",
                json!({"process": {"selinuxLabel": "l"}}),
            ),
            (
                "process.ioPriority",
                json!({"process": {"ioPriority": {"class": "BE", "priority": 4}}}),
            ),
            (
                "process.scheduler",
                json!({"process": {"scheduler": {"policy": "SCHED_OTHER"}}}),
            ),
            (
                "process.execCPUAffinity",
                json!({"process": {"execCpuAffinity": {"initial": "0"}}}),
            ),
            (
                "process.user.umask",
                json!({"process": {"user": {"umask": 18}}}),
            ),
            (
                "process.user.additionalGids",
                json!({"process": {"user": {"additionalGids": [10]}}}),
            ),
            (
                "linux.uidMappings",
                json!({"linux": {"uidMappings": mapping}}),
            ),
            (
                "linux.gidMappings",
                json!({"linux": {"gidMappings": mapping}}),
            ),
            (
                "linux.sysctl",
                json!({"linux": {"sysctl": {"net.ipv4.ip_forward": "1"}}}),
            ),
            (
                "linux.cgroupsPath",
                json!({"linux": {"cgroupsPath": "/arcbox"}}),
            ),
            ("linux.resources", json!({"linux": {"resources": {}}})),
            (
                "linux.rootfsPropagation",
                json!({"linux": {"rootfsPropagation": "private"}}),
            ),
            (
                "linux.maskedPaths",
                json!({"linux": {"maskedPaths": ["/proc/kcore"]}}),
            ),
            (
                "linux.readonlyPaths",
                json!({"linux": {"readonlyPaths": ["/proc/sys"]}}),
            ),
            ("linux.mountLabel", json!({"linux": {"mountLabel": "l"}})),
            (
                "linux.timeOffsets",
                json!({"linux": {"timeOffsets": {"boottime": {"secs": 1, "nanosecs": 0}}}}),
            ),
        ];
        for (expected, patch) in cases {
            match Workload::from_spec(&spec(patch)) {
                Err(WorkloadError::Unsupported(field)) => assert_eq!(field, expected),
                other => panic!("{expected}: {other:?}"),
            }
        }
    }
}
