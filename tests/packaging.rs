//! Release-asset contract tests for Phase 12.

use std::path::Path;

use fauna_scan::configuration::Config;

#[test]
fn checked_in_example_configuration_loads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_dir = root.join(".config/fauna-scan");
    let state_dir = root.join(".local/state/fauna-scan");
    let output_dir = root.join("Pictures/fauna-scan");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::create_dir_all(&output_dir).unwrap();
    std::fs::write(config_dir.join("nvr-password"), "nvr-test-secret\n").unwrap();
    std::fs::write(
        config_dir.join("classifier-api-key"),
        "classifier-test-key\n",
    )
    .unwrap();

    let contents =
        include_str!("../config.example.toml").replace("/home/user", &root.display().to_string());
    let config_path = config_dir.join("config.toml");
    std::fs::write(&config_path, contents).unwrap();

    let config = Config::load(Some(&config_path)).unwrap();
    assert_eq!(
        config.general.database_path,
        state_dir.join("fauna-scan.sqlite3")
    );
    assert_eq!(config.general.output_directory, output_dir);
    assert_eq!(config.nvr.search.max_results, 50);
    assert_eq!(config.nvr.download.concurrency, 2);
    assert!(config.nvr.download.rebase_playback_urls);
    let endpoint = &config.classifier.endpoints[0];
    assert_eq!(endpoint.model, "vision-model");
    assert_eq!(endpoint.prompt_version, "wildlife-v1");
    assert_eq!(config.nvr.password.unwrap().expose(), "nvr-test-secret");
    assert_eq!(
        endpoint.api_key.as_ref().unwrap().expose(),
        "classifier-test-key"
    );
}

#[test]
fn systemd_service_has_required_user_service_contract() {
    let service = include_str!("../fauna-scan.service");
    for required in [
        "[Unit]",
        "After=network-online.target",
        "Wants=network-online.target",
        "Type=simple",
        "ExecStart=%h/.local/bin/fauna-scan",
        "run",
        "Restart=on-failure",
        "RestartSec=10",
        "TimeoutStopSec=30",
        "NoNewPrivileges=true",
        "PrivateTmp=true",
        "ProtectSystem=strict",
        "ProtectHome=read-only",
        "ReadWritePaths=",
        "[Install]",
        "WantedBy=default.target",
    ] {
        assert!(
            service.contains(required),
            "service is missing {required:?}"
        );
    }
    assert!(!service.contains("password ="));
    assert!(!service.contains("api_key ="));
}

#[test]
fn readme_contains_required_operator_workflows() {
    let readme = include_str!("../README.md");
    for required in [
        "cargo build --release",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "install -m 600 /dev/null",
        "secure editor",
        "check-config",
        "discover",
        "download --once",
        "scan --once",
        "status",
        "systemctl --user daemon-reload",
        "systemctl --user enable --now",
        "systemctl --user restart",
        "systemctl --user stop",
        "journalctl --user -u",
        "ReadWritePaths",
        "SIGTERM",
        "migrations",
    ] {
        assert!(readme.contains(required), "README is missing {required:?}");
    }
    assert!(Path::new("config.example.toml").exists());
}
