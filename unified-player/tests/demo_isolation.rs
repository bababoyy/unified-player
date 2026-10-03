use std::{fs, path::Path, process::Command};

fn run_demo(config: &Path, cache: &Path, demo: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_unified-player"))
        .arg("-c")
        .arg(config)
        .arg("-C")
        .arg(cache)
        .args(demo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty());
}

#[test]
fn demos_ignore_user_configuration_and_never_bootstrap_user_folders() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let screen = [
        "demo",
        "screen",
        "home",
        "--scenario",
        "showcase",
        "--size",
        "80x24",
        "--color",
        "never",
    ];
    run_demo(&config, &cache, &screen);
    assert!(!config.exists());
    assert!(!cache.exists());

    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&cache).unwrap();
    for name in ["app.toml", "setup.toml", "accounts.toml", "keymap.toml"] {
        fs::write(config.join(name), "invalid user configuration [").unwrap();
    }
    fs::write(cache.join("sentinel"), "user cache").unwrap();
    run_demo(&config, &cache, &screen);
    run_demo(
        &config,
        &cache,
        &["demo", "welcome", "--width", "80", "--height", "24"],
    );
    assert_eq!(fs::read_dir(&config).unwrap().count(), 4);
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    for name in ["app.toml", "setup.toml", "accounts.toml", "keymap.toml"] {
        assert_eq!(
            fs::read_to_string(config.join(name)).unwrap(),
            "invalid user configuration ["
        );
    }
    assert_eq!(
        fs::read_to_string(cache.join("sentinel")).unwrap(),
        "user cache"
    );
}
