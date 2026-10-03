//! Writing settings back to the user's `config.toml`.
//!
//! The daemon reads configuration from several files (see
//! [`user_config_paths`](super::user_config_paths)); a setting changed at
//! runtime is written to the documented one, `~/.config/arcbox/config.toml`
//! (or `$XDG_CONFIG_HOME/arcbox/config.toml`), which is also the one merged
//! last and so wins over every other file. The file is edited in place with
//! `toml_edit`, so an operator's comments and formatting survive.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{CoreError, Result};

/// Sets `[vm] cpus` and `[vm] memory_mb` in the config file at `path`,
/// creating the file (and its directory) when it does not exist yet.
///
/// # Errors
///
/// Returns an error if the file exists but is not valid TOML, or cannot be
/// read or replaced atomically.
pub fn set_vm_resources(path: &Path, cpus: u32, memory_mb: u64) -> Result<()> {
    let memory_mb = i64::try_from(memory_mb).map_err(|_| {
        CoreError::config(format!(
            "memory_mb {memory_mb} does not fit in a TOML integer"
        ))
    })?;
    edit(path, |doc| {
        let vm = table(doc, path, "vm", "the System VM's resources")?;
        set_integer(vm, "cpus", i64::from(cpus));
        set_integer(vm, "memory_mb", memory_mb);
        Ok(())
    })
}

/// Sets `[machine] default_machine` in the config file at `path`, or
/// removes the key for `None`; creates the file when it does not exist yet.
///
/// # Errors
///
/// Returns an error if the file exists but is not valid TOML, or cannot be
/// read or replaced atomically.
pub fn set_default_machine(path: &Path, name: Option<&str>) -> Result<()> {
    edit(path, |doc| {
        let machine = table(doc, path, "machine", "the default machine")?;
        match name {
            Some(name) => match machine
                .get_mut("default_machine")
                .and_then(Item::as_value_mut)
            {
                Some(existing) => {
                    let decor = existing.decor().clone();
                    *existing = Value::from(name);
                    *existing.decor_mut() = decor;
                }
                None => machine["default_machine"] = Item::Value(Value::from(name)),
            },
            None => {
                machine.remove("default_machine");
            }
        }
        Ok(())
    })
}

/// Loads `path` (an absent file is an empty document), applies `change`,
/// and replaces the file atomically.
fn edit(path: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map_err(|e| CoreError::config(format!("{} is not valid TOML: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(e) => {
            return Err(CoreError::config(format!(
                "failed to read {}: {e}",
                path.display()
            )));
        }
    };

    change(&mut doc)?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    arcbox_atomic_file::write(path, doc.to_string().as_bytes())
        .map_err(|e| CoreError::config(format!("failed to write {}: {e}", path.display())))?;
    Ok(())
}

/// The `[name]` table of `doc`, created when absent.
fn table<'doc>(
    doc: &'doc mut DocumentMut,
    path: &Path,
    name: &str,
    stores: &str,
) -> Result<&'doc mut Table> {
    doc.entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            CoreError::config(format!(
                "{}: `{name}` is not a table; cannot store {stores}",
                path.display()
            ))
        })
}

/// Sets `key` to `n`, keeping the comment and spacing around an existing
/// value rather than replacing the whole entry.
fn set_integer(table: &mut Table, key: &str, n: i64) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = Value::from(n);
            *existing.decor_mut() = decor;
        }
        None => {
            table[key] = Item::Value(Value::from(n));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_file_and_keeps_unrelated_content_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        set_vm_resources(&path, 4, 4096).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = written.parse().unwrap();
        assert_eq!(parsed["vm"]["cpus"].as_integer(), Some(4));
        assert_eq!(parsed["vm"]["memory_mb"].as_integer(), Some(4096));

        std::fs::write(
            &path,
            "# my settings\n[network]\nproxy = \"none\" # keep\n\n[vm]\ncpus = 2 # old\nautostart = true\n",
        )
        .unwrap();
        set_vm_resources(&path, 6, 8192).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# my settings\n"), "{written}");
        assert!(written.contains("proxy = \"none\" # keep"), "{written}");
        assert!(written.contains("cpus = 6 # old"), "{written}");
        assert!(written.contains("autostart = true"), "{written}");
        assert!(written.contains("memory_mb = 8192"), "{written}");
    }

    #[test]
    fn the_default_machine_is_set_kept_and_cleared_next_to_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[machine]\ndisk_gb = 20 # small\n").unwrap();

        set_default_machine(&path, Some("dev")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("disk_gb = 20 # small"), "{written}");
        assert!(written.contains("default_machine = \"dev\""), "{written}");

        set_default_machine(&path, Some("ci")).unwrap();
        let parsed: toml::Value = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(parsed["machine"]["default_machine"].as_str(), Some("ci"));

        set_default_machine(&path, None).unwrap();
        let parsed: toml::Value = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(parsed["machine"].get("default_machine").is_none());
        assert_eq!(parsed["machine"]["disk_gb"].as_integer(), Some(20));

        // Clearing what is not there creates no file.
        let absent = dir.path().join("none.toml");
        set_default_machine(&absent, None).unwrap();
        assert!(
            absent.exists(),
            "the file is written even when nothing changed"
        );
    }

    #[test]
    fn refuses_a_broken_file_rather_than_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[vm\ncpus = 2\n").unwrap();
        assert!(set_vm_resources(&path, 4, 4096).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[vm\ncpus = 2\n");
    }
}
