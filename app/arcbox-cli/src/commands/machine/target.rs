//! Which machine a command acts on when its name may be left out.
//!
//! `abctl machine exec [NAME] [COMMAND]...` cannot tell a machine name from
//! the first word of a command by shape alone, so the daemon's machine list
//! decides: a first word that names a machine is the machine; otherwise the
//! default machine (`abctl machine default`) runs the whole line, and with
//! no default the first word is reported as the unknown machine it would
//! have been before defaults existed.

use anyhow::{Context, Result, bail};
use arcbox_connect::v1::ListMachinesRequest;

/// A resolved command target.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Target {
    /// The machine to act on.
    pub machine: String,
    /// The command words, with the machine name taken out.
    pub command: Vec<String>,
}

/// Resolves `name` and `command` against the daemon's machines.
pub(super) async fn resolve(name: Option<String>, command: Vec<String>) -> Result<Target> {
    let listed = super::machine_client()
        .list(ListMachinesRequest {
            all: true,
            ..Default::default()
        })
        .await
        .context("Failed to list machines")?
        .into_owned();
    let names: Vec<String> = listed.machines.into_iter().map(|m| m.name).collect();
    let default = (!listed.default_machine.is_empty()).then_some(listed.default_machine.as_str());
    pick(name, command, &names, default)
}

/// The pure half of [`resolve`]: `names` are the machines that exist,
/// `default` the configured default machine.
pub(super) fn pick(
    name: Option<String>,
    command: Vec<String>,
    names: &[String],
    default: Option<&str>,
) -> Result<Target> {
    match (name, default) {
        (Some(name), _) if names.contains(&name) => Ok(Target {
            machine: name,
            command,
        }),
        (Some(word), Some(default)) => Ok(Target {
            machine: default.to_owned(),
            command: std::iter::once(word).chain(command).collect(),
        }),
        (Some(name), None) => bail!(
            "Machine '{name}' was not found. List machines with `abctl machine list`, or set \
             a default with `abctl machine default <name>` to leave the name out."
        ),
        (None, Some(default)) => Ok(Target {
            machine: default.to_owned(),
            command,
        }),
        (None, None) => bail!(
            "No machine named and no default machine set. Pass a name, or set a default with \
             `abctl machine default <name>`."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["dev".to_owned(), "ci".to_owned()]
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn a_first_word_naming_a_machine_is_the_machine() {
        let target = pick(
            Some("ci".to_owned()),
            words(&["ls", "-la"]),
            &names(),
            Some("dev"),
        )
        .unwrap();
        assert_eq!(
            target,
            Target {
                machine: "ci".to_owned(),
                command: words(&["ls", "-la"])
            }
        );
    }

    #[test]
    fn with_a_default_the_whole_line_is_the_command() {
        let target = pick(
            Some("ls".to_owned()),
            words(&["-la"]),
            &names(),
            Some("dev"),
        )
        .unwrap();
        assert_eq!(target.machine, "dev");
        assert_eq!(target.command, words(&["ls", "-la"]));

        let shell = pick(None, Vec::new(), &names(), Some("dev")).unwrap();
        assert_eq!(shell.machine, "dev");
        assert_eq!(shell.command, Vec::<String>::new());
    }

    #[test]
    fn without_a_default_an_unknown_first_word_is_the_missing_machine() {
        let err = pick(Some("ls".to_owned()), Vec::new(), &names(), None).unwrap_err();
        assert!(
            err.to_string().contains("Machine 'ls' was not found"),
            "{err}"
        );
        assert!(err.to_string().contains("abctl machine default"), "{err}");

        let err = pick(None, words(&["ls"]), &names(), None).unwrap_err();
        assert!(err.to_string().contains("no default machine"), "{err}");
    }
}
