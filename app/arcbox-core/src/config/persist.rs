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

    let vm = doc
        .entry("vm")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            CoreError::config(format!(
                "{}: `vm` is not a table; cannot store the System VM's resources",
                path.display()
            ))
        })?;
    set_integer(vm, "cpus", i64::from(cpus));
    let memory_mb = i64::try_from(memory_mb).map_err(|_| {
        CoreError::config(format!(
            "memory_mb {memory_mb} does not fit in a TOML integer"
        ))
    })?;
    set_integer(vm, "memory_mb", memory_mb);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    arcbox_atomic_file::write(path, doc.to_string().as_bytes())
        .map_err(|e| CoreError::config(format!("failed to write {}: {e}", path.display())))?;
    Ok(())
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
    fn refuses_a_broken_file_rather_than_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[vm\ncpus = 2\n").unwrap();
        assert!(set_vm_resources(&path, 4, 4096).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[vm\ncpus = 2\n");
    }
}
