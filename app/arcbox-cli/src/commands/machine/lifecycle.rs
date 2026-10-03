//! Machine lifecycle beyond create, start, stop and remove: clone, export,
//! import, resize and the default machine.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arcbox_connect::v1::{
    CloneMachineRequest, ExportMachineRequest, ImportMachineRequest, ListMachinesRequest,
    SetDefaultMachineRequest, SetMachineResourcesRequest,
};
use clap::Args;

use super::machine_client;
use crate::error::machine_lifecycle;

#[derive(Args)]
pub struct CloneArgs {
    /// Machine to clone; must be stopped
    pub source: String,
    /// Name of the clone
    pub name: String,
}

#[derive(Args)]
pub struct ExportArgs {
    /// Machine to export; must be stopped
    pub name: String,
    /// Where to write the archive (default: ./<name>.tar.zst)
    pub path: Option<PathBuf>,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Archive written by `abctl machine export`
    pub path: PathBuf,
    /// Name for the machine (default: the name in the archive)
    #[arg(long)]
    pub name: Option<String>,
    /// CPUs (default: the archive's)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub cpus: Option<u32>,
    /// Memory in MiB (default: the archive's)
    #[arg(long, value_name = "MIB")]
    pub memory: Option<u64>,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("size").multiple(true).required(true))]
pub struct ResizeArgs {
    /// Machine name
    pub name: String,
    /// CPUs, 1 through the host's logical CPU count
    #[arg(long, group = "size", value_parser = clap::value_parser!(u32).range(1..))]
    pub cpus: Option<u32>,
    /// Memory in MiB, 512 through the host's physical memory
    #[arg(long, group = "size", value_name = "MIB")]
    pub memory: Option<u64>,
}

#[derive(Args)]
pub struct DefaultArgs {
    /// Machine to make the default; omit to show the current one
    #[arg(conflicts_with = "unset")]
    pub name: Option<String>,
    /// Clear the default machine
    #[arg(long)]
    pub unset: bool,
}

pub async fn execute_clone(args: CloneArgs) -> Result<()> {
    machine_client()
        .clone_machine(CloneMachineRequest {
            id: args.source.clone(),
            name: args.name.clone(),
            ..Default::default()
        })
        .await
        .map_err(|error| machine_lifecycle(error, &args.source, "clone"))?;
    println!(
        "Cloned machine '{}' into '{}'. Start it with: abctl machine start {}",
        args.source, args.name, args.name
    );
    Ok(())
}

pub async fn execute_export(args: ExportArgs) -> Result<()> {
    let path = args
        .path
        .unwrap_or_else(|| PathBuf::from(format!("{}.tar.zst", args.name)));
    let path = std::path::absolute(&path)
        .with_context(|| format!("Failed to resolve {}", path.display()))?;
    println!("Exporting machine '{}' to {}...", args.name, path.display());
    let exported = machine_client()
        .export(ExportMachineRequest {
            id: args.name.clone(),
            path: path.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .map_err(|error| machine_lifecycle(error, &args.name, "export"))?
        .into_owned();
    println!(
        "Exported machine '{}' to {} ({})",
        args.name,
        exported.path,
        human_size(exported.size)
    );
    Ok(())
}

pub async fn execute_import(args: ImportArgs) -> Result<()> {
    let path = std::fs::canonicalize(&args.path)
        .with_context(|| format!("Failed to read {}", args.path.display()))?;
    let label = args.name.clone().unwrap_or_else(|| archive_label(&path));
    println!("Importing machine from {}...", path.display());
    let imported = machine_client()
        .import(ImportMachineRequest {
            path: path.to_string_lossy().into_owned(),
            name: args.name.unwrap_or_default(),
            cpus: args.cpus.unwrap_or(0),
            memory: args.memory.unwrap_or(0).saturating_mul(1024 * 1024),
            ..Default::default()
        })
        .await
        .map_err(|error| machine_lifecycle(error, &label, "import"))?
        .into_owned();
    let distro = if imported.distro_version.is_empty() {
        imported.distro
    } else {
        format!("{} {}", imported.distro, imported.distro_version)
    };
    println!(
        "Imported machine '{}' ({distro}). Start it with: abctl machine start {}",
        imported.id, imported.id
    );
    Ok(())
}

pub async fn execute_resize(args: ResizeArgs) -> Result<()> {
    let resources = machine_client()
        .set_resources(SetMachineResourcesRequest {
            id: args.name.clone(),
            cpus: args.cpus.unwrap_or(0),
            memory: args.memory.unwrap_or(0).saturating_mul(1024 * 1024),
            ..Default::default()
        })
        .await
        .map_err(|error| machine_lifecycle(error, &args.name, "resize"))?
        .into_owned();
    let mib = 1024 * 1024;
    println!(
        "Machine '{}' boots with {} CPUs and {} MiB (host: {} CPUs, {} MiB)",
        args.name,
        resources.cpus,
        resources.memory / mib,
        resources.host_cpus,
        resources.host_memory / mib
    );
    if resources.restart_required {
        println!(
            "It is running with its previous size. Restart it to apply the new one:\n  \
             abctl machine stop {} && abctl machine start {}",
            args.name, args.name
        );
    }
    Ok(())
}

pub async fn execute_default(args: DefaultArgs) -> Result<()> {
    let client = machine_client();
    if args.unset {
        client
            .set_default(SetDefaultMachineRequest::default())
            .await
            .context("Failed to clear the default machine")?;
        println!("Default machine cleared");
        return Ok(());
    }
    if let Some(name) = args.name {
        client
            .set_default(SetDefaultMachineRequest {
                id: name.clone(),
                ..Default::default()
            })
            .await
            .map_err(|error| crate::error::machine_operation(error, &name, "set as default"))?;
        println!("Default machine: {name}");
        println!("`abctl machine exec` and `abctl machine ssh` now use it when given no name.");
        return Ok(());
    }
    let listed = client
        .list(ListMachinesRequest {
            all: true,
            ..Default::default()
        })
        .await
        .context("Failed to list machines")?
        .into_owned();
    if listed.default_machine.is_empty() {
        bail!("No default machine. Set one with `abctl machine default <name>`.");
    }
    println!("{}", listed.default_machine);
    Ok(())
}

/// The machine name an archive implies before its manifest is read: the
/// file name without `.tar.zst`, for error messages only.
fn archive_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| {
            name.to_string_lossy()
                .trim_end_matches(".tar.zst")
                .to_owned()
        },
    )
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_the_nearest_unit() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(734_003_200), "700.0 MiB");
        assert_eq!(human_size(5 << 30), "5.0 GiB");
    }

    #[test]
    fn the_archive_label_is_the_file_stem() {
        assert_eq!(archive_label(Path::new("/tmp/dev.tar.zst")), "dev");
        assert_eq!(archive_label(Path::new("backup.tgz")), "backup.tgz");
    }
}
