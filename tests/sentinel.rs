//! Connecting through Sentinel, against the fixture in `tests/docker` (one
//! master, two replicas, three Sentinels). Needs Docker:
//! `cargo test --test sentinel -- --ignored --test-threads=1`.

mod sentinel_support;

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU32, Ordering},
};

use sentinel_support::{SentinelFixture, SENTINELS, URL};

/// `rrover args…` with `config` (JSON5) as its only configuration.
fn rrover(config: &str, args: &[&str]) -> Output {
    static RUNS: AtomicU32 = AtomicU32::new(0);
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sentinel-{}-{run}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a config dir");
    std::fs::write(dir.join("config.json5"), config).expect("write config");
    let out = Command::new(env!("CARGO_BIN_EXE_rrover"))
        .args(args)
        .env("REDIS_ROVER_CONFIG", &dir)
        .env_remove("REDIS_URL")
        .stdin(Stdio::null())
        .output()
        .expect("rrover runs");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs Docker"]
fn a_sentinel_url_reaches_the_master() {
    let _fixture = SentinelFixture::start();
    let out = rrover("{}", &["--url", URL, "--probe"]);
    let report = stdout(&out);
    assert!(out.status.success(), "{report}");
    assert!(
        report.contains("sentinel mymaster → 127.0.0.1:7200"),
        "{report}"
    );
    assert!(report.contains("role      master"), "{report}");
}

#[test]
#[ignore = "needs Docker"]
fn a_profile_authenticates_to_the_sentinels() {
    const SECRET: &str = "sentinel-s3cret";
    let fixture = SentinelFixture::start();
    for port in SENTINELS {
        fixture.cli(
            port,
            &["ACL", "SETUSER", "default", "on", &format!(">{SECRET}")],
        );
        fixture.cli(
            port,
            &[
                "ACL",
                "SETUSER",
                "default",
                "resetpass",
                &format!(">{SECRET}"),
            ],
        );
    }
    let hosts = r#"["127.0.0.1:7210", "127.0.0.1:7211", "127.0.0.1:7212"]"#;
    let with = format!(
        r#"{{ profiles: {{ ha: {{ sentinel: {{ hosts: {hosts}, master: "mymaster", password: "{SECRET}" }} }} }} }}"#
    );
    let without = format!(
        r#"{{ profiles: {{ ha: {{ sentinel: {{ hosts: {hosts}, master: "mymaster" }} }} }} }}"#
    );

    let refused = rrover(&without, &["ha", "--probe"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));

    let out = rrover(&with, &["ha", "--probe"]);
    let report = stdout(&out);
    assert!(
        out.status.success(),
        "{report}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(report.contains("role      master"), "{report}");
    assert!(!report.contains(SECRET), "{report}");

    let printed = stdout(&rrover(&with, &["ha", "--print-target"]));
    assert!(!printed.contains(SECRET), "{printed}");
    assert!(printed.contains("sentinel pw"), "{printed}");
}
