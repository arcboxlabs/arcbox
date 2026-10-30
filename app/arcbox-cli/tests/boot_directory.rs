//! Boot commands must honor an explicit resource directory without changing user caches.

use std::process::Command;

#[test]
fn explicit_boot_directory_overrides_the_configured_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let config_home = temporary.path().join("config");
    let configured = temporary.path().join("configured");
    let selected = temporary.path().join("selected resources");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(config_home.join("arcbox")).unwrap();
    std::fs::write(
        config_home.join("arcbox/config.toml"),
        format!("data_dir = {:?}\n", configured.to_str().unwrap()),
    )
    .unwrap();
    let version = arcbox_image::boot_assets::boot_asset_version();
    for (directory, sentinel) in [
        (&configured, "configured manifest"),
        (&selected, "selected manifest"),
    ] {
        let cache = directory.join("boot").join(version);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("manifest.json"), sentinel).unwrap();
    }

    let run = |operation: &str, directory: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abctl"));
        command
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config_home)
            .args(["boot", operation, "--format", "json"]);
        if let Some(directory) = directory {
            command.arg("--data-dir").arg(directory);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };

    assert_eq!(run("clear", Some(&selected))["cleared"], true);
    assert!(!selected.join("boot").join(version).exists());
    assert_eq!(
        std::fs::read_to_string(configured.join("boot").join(version).join("manifest.json"))
            .unwrap(),
        "configured manifest"
    );
    assert_eq!(run("list", None)["versions"], serde_json::json!([version]));
    assert_eq!(
        run("list", Some(&selected))["versions"],
        serde_json::json!([])
    );
}
