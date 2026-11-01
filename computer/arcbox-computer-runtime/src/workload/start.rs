//! OCI spec → the guest agent's [`StartCommand`] and bind mounts.

use std::collections::HashMap;

use arcbox_oci::{Mount, Process, Spec};

use super::WorkloadError;
use crate::{ComputerMountSpec, StartCommand};

/// What a computer runs, translated from an OCI spec.
#[derive(Debug)]
pub struct Workload {
    /// The process to start, or `None` when the spec has no process or no
    /// args (the computer boots and waits for an exec). Its
    /// `timeout_seconds` is 0; OCI has no such field.
    pub start: Option<StartCommand>,
    /// Bind mounts, each a VirtioFS share of a host directory.
    pub mounts: Vec<ComputerMountSpec>,
}

impl Workload {
    /// Check `spec` against what a computer honors and translate it.
    ///
    /// Supported: `process.{args, env, cwd, user.{uid, gid}, terminal,
    /// consoleSize}`, bind mounts, `root.path`, and annotations. Every other
    /// field that is set fails, so nothing in a spec is silently ignored.
    pub fn from_spec(spec: &Spec) -> Result<Self, WorkloadError> {
        spec.validate()?;
        super::check::check_spec(spec)?;
        let start = match &spec.process {
            Some(process) if !process.args.is_empty() => Some(start_command(process)?),
            _ => None,
        };
        let mounts = spec
            .mounts
            .iter()
            .enumerate()
            .map(|(index, mount)| bind_mount(index, mount))
            .collect::<Result<_, _>>()?;
        Ok(Self { start, mounts })
    }
}

fn start_command(process: &Process) -> Result<StartCommand, WorkloadError> {
    let env = process
        .env
        .iter()
        .map(|entry| {
            entry
                .split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .ok_or_else(|| WorkloadError::Unsupported(format!("process.env entry {entry:?}")))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    let (tty_width, tty_height) = match &process.console_size {
        None => (80, 24),
        Some(size) => (
            u16::try_from(size.width)
                .map_err(|_| WorkloadError::Unsupported("process.consoleSize.width".into()))?,
            u16::try_from(size.height)
                .map_err(|_| WorkloadError::Unsupported("process.consoleSize.height".into()))?,
        ),
    };
    Ok(StartCommand {
        cmd: process.args.clone(),
        env,
        working_dir: process.cwd.clone(),
        // Numeric `uid:gid` resolves without a passwd entry in the guest,
        // so the agent runs exactly the ids the spec names.
        user: process
            .user
            .as_ref()
            .map(|user| format!("{}:{}", user.uid, user.gid))
            .unwrap_or_default(),
        tty: process.terminal,
        tty_width,
        tty_height,
        timeout_seconds: 0,
    })
}

/// A bind mount of a host directory, the one mount a computer shares into
/// the guest (as VirtioFS). The guest agent mounts `/proc`, `/sys`, `/dev`
/// and `/run` itself.
fn bind_mount(index: usize, mount: &Mount) -> Result<ComputerMountSpec, WorkloadError> {
    let field = |name: &str| WorkloadError::Unsupported(format!("mounts[{index}].{name}"));
    let options = mount.options.as_deref().unwrap_or_default();
    let is_bind = match mount.mount_type.as_deref() {
        Some("bind") => true,
        None => options
            .iter()
            .any(|option| option == "bind" || option == "rbind"),
        Some(_) => false,
    };
    if !is_bind {
        return Err(field("type"));
    }
    let source = mount
        .source
        .as_deref()
        .filter(|source| source.starts_with('/'))
        .ok_or_else(|| field("source"))?;
    if !mount.uid_mappings.is_empty() || !mount.gid_mappings.is_empty() {
        return Err(field("uidMappings"));
    }
    let mut readonly = false;
    for option in options {
        match option.as_str() {
            "bind" | "rbind" => {}
            "ro" => readonly = true,
            "rw" => readonly = false,
            other => return Err(field(&format!("options {other:?}"))),
        }
    }
    Ok(ComputerMountSpec {
        source: source.to_owned(),
        target: mount.destination.clone(),
        readonly,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A `config.json` with every supported field set.
    fn base() -> Value {
        json!({
            "ociVersion": "1.2.0",
            "root": {"path": "rootfs"},
            "process": {
                "cwd": "/srv",
                "env": ["PATH=/bin", "GREETING=a=b"],
                "args": ["/bin/serve", "--port", "80"],
                "user": {"uid": 1000, "gid": 1001}
            },
            "annotations": {"org.example": "kept"}
        })
    }

    /// `base()` with `patch` merged in (RFC 7386 without deletion).
    pub(in crate::workload) fn spec(patch: Value) -> Spec {
        fn merge(target: &mut Value, patch: Value) {
            match (target, patch) {
                (Value::Object(target), Value::Object(patch)) => {
                    for (key, value) in patch {
                        merge(target.entry(key).or_insert(Value::Null), value);
                    }
                }
                (target, patch) => *target = patch,
            }
        }
        let mut spec = base();
        merge(&mut spec, patch);
        serde_json::from_value(spec).unwrap()
    }

    #[test]
    fn translates_the_supported_fields() {
        let workload = Workload::from_spec(&spec(json!({
            "process": {"terminal": true, "consoleSize": {"height": 40, "width": 120}},
            "mounts": [
                {"destination": "/work", "source": "/Users/me/work", "type": "bind",
                 "options": ["rbind", "ro"]},
                {"destination": "/cache", "source": "/tmp/cache", "options": ["bind"]}
            ]
        })))
        .unwrap();
        let start = workload.start.unwrap();
        assert_eq!(start.cmd, ["/bin/serve", "--port", "80"]);
        // Only the first `=` splits: values may contain `=`.
        assert_eq!(start.env["GREETING"], "a=b");
        assert_eq!(start.env["PATH"], "/bin");
        assert_eq!(start.working_dir, "/srv");
        assert_eq!(start.user, "1000:1001");
        assert!(start.tty);
        assert_eq!((start.tty_width, start.tty_height), (120, 40));
        assert_eq!(start.timeout_seconds, 0);
        let mount = |source: &str, target: &str, readonly| ComputerMountSpec {
            source: source.into(),
            target: target.into(),
            readonly,
        };
        assert_eq!(
            workload.mounts,
            [
                mount("/Users/me/work", "/work", true),
                mount("/tmp/cache", "/cache", false)
            ]
        );
    }

    #[test]
    fn a_spec_without_args_starts_nothing() {
        let mut spec = spec(json!({}));
        spec.process.as_mut().unwrap().args.clear();
        assert!(Workload::from_spec(&spec).unwrap().start.is_none());
        spec.process = None;
        assert!(Workload::from_spec(&spec).unwrap().start.is_none());
    }

    #[test]
    fn values_the_agent_cannot_carry_are_rejected() {
        let bind = |extra: Value| {
            let mut mount = json!({"destination": "/work", "source": "/work", "type": "bind"});
            mount
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"mounts": [mount]})
        };
        let cases = [
            (
                "process.env entry \"NOVALUE\"",
                json!({"process": {"env": ["NOVALUE"]}}),
            ),
            (
                "process.consoleSize.width",
                json!({"process": {"consoleSize": {"height": 24, "width": 70_000}}}),
            ),
            (
                "mounts[0].type",
                bind(json!({"type": "proc", "source": "proc"})),
            ),
            // Without a type, only a bind option makes a mount a bind mount.
            ("mounts[0].type", bind(json!({"type": null}))),
            ("mounts[0].source", bind(json!({"source": "relative"}))),
            (
                "mounts[0].options \"nosuid\"",
                bind(json!({"options": ["nosuid"]})),
            ),
            (
                "mounts[0].uidMappings",
                bind(json!({"uidMappings": [{"containerId": 0, "hostId": 1, "size": 1}]})),
            ),
        ];
        for (expected, patch) in cases {
            match Workload::from_spec(&spec(patch)) {
                Err(WorkloadError::Unsupported(field)) => assert_eq!(field, expected),
                other => panic!("{expected}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_spec_is_validated_first() {
        assert!(matches!(
            Workload::from_spec(&spec(json!({"process": {"cwd": "relative"}}))),
            Err(WorkloadError::Invalid(_))
        ));
    }
}
