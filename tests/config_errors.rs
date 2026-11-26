//! A broken user config ends the TUI start with a message and exit 3, the
//! way `--print-target` does, never a panic and a crash report. Runs the
//! binary; no Redis needed, the config is read before any connection.

use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// A fresh, empty directory under the target dir for one test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("config_errors-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Start the TUI (no `--print-target`) with `config` as the user's config.
fn start_with(name: &str, config: &str) -> Output {
    let dir = scratch_dir(name);
    std::fs::write(dir.join("config.json5"), config).unwrap();
    Command::new(env!("CARGO_BIN_EXE_rrover"))
        .args(["-u", "redis://127.0.0.1:1"])
        .env("REDIS_ROVER_CONFIG", &dir)
        .env("REDIS_ROVER_DATA", &dir)
        .env_remove("REDIS_URL")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn assert_config_error(out: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("crashed"),
        "a config error must not panic:\n{stderr}"
    );
    assert_eq!(out.status.code(), Some(3), "stderr:\n{stderr}");
    assert!(
        stderr.contains("invalid configuration") && stderr.contains(expected),
        "stderr should name the problem ({expected}):\n{stderr}"
    );
}

#[test]
fn unknown_theme_is_a_config_error() {
    let out = start_with("theme", r#"{ ui: { theme: "nope" } }"#);
    assert_config_error(&out, "unknown theme `nope`");
}

#[test]
fn missing_theme_file_is_a_config_error() {
    let out = start_with("theme-file", r#"{ ui: { theme: "missing.json5" } }"#);
    assert_config_error(&out, "no theme file at");
}

#[test]
fn unknown_command_is_a_config_error() {
    let out = start_with(
        "command",
        r#"{ keybindings: { KeySpace: { "x": "NoSuchCommand" } } }"#,
    );
    assert_config_error(&out, "NoSuchCommand");
}

#[test]
fn malformed_file_is_a_config_error() {
    let out = start_with("syntax", "{ keybindings: { ");
    assert_config_error(&out, "config.json5");
}
