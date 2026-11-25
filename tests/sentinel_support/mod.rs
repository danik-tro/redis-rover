//! Starts the Sentinel fixture in `tests/docker` for one test and removes it
//! on drop.

#![allow(dead_code)]

use std::{
    path::Path,
    process::{Command, Stdio},
    sync::Once,
    time::{Duration, Instant},
};

const IMAGE: &str = "rrover-test-sentinel";
pub const MASTER: u16 = 7200;
pub const REPLICAS: [u16; 2] = [7201, 7202];
pub const SENTINELS: [u16; 3] = [7210, 7211, 7212];
pub const URL: &str = "redis+sentinel://127.0.0.1:7210,127.0.0.1:7211,127.0.0.1:7212/mymaster/0";

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("docker runs")
}

fn build_image() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/docker");
        let dockerfile = dir.join("sentinel.Dockerfile");
        let out = docker(&[
            "build",
            "-q",
            "-t",
            IMAGE,
            "-f",
            dockerfile.to_str().expect("UTF-8 path"),
            dir.to_str().expect("UTF-8 path"),
        ]);
        assert!(
            out.status.success(),
            "docker build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
}

/// The running setup. Dropping it removes the container, also when the test
/// panics.
pub struct SentinelFixture {
    name: String,
    password: Option<String>,
}

impl SentinelFixture {
    pub fn start() -> Self {
        Self::launch(None)
    }

    /// Every data node requires `password` (`requirepass` + `masterauth`);
    /// the Sentinels monitor with it and need none themselves.
    pub fn start_with_password(password: &str) -> Self {
        Self::launch(Some(password))
    }

    fn launch(password: Option<&str>) -> Self {
        build_image();
        let name = format!("rrover-test-sentinel-{}", std::process::id());
        let _ = docker(&["rm", "-f", &name]);
        let auth_env = password.map(|p| format!("RROVER_FIXTURE_PASSWORD={p}"));
        let mut args = vec![
            "run",
            "-d",
            "--name",
            &name,
            "-p",
            "7200-7202:7200-7202",
            "-p",
            "7210-7212:7210-7212",
        ];
        if let Some(env) = &auth_env {
            args.extend(["-e", env]);
        }
        args.push(IMAGE);
        let out = docker(&args);
        assert!(
            out.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let fixture = Self {
            name,
            password: password.map(str::to_owned),
        };
        let deadline = Instant::now() + Duration::from_mins(1);
        while !docker(&["exec", &fixture.name, "test", "-f", "/data/ready"])
            .status
            .success()
        {
            assert!(Instant::now() < deadline, "Sentinel never became ready");
            std::thread::sleep(Duration::from_millis(200));
        }
        fixture
    }

    /// `redis-cli -p port args…` inside the container; its output.
    pub fn cli(&self, port: u16, args: &[&str]) -> String {
        let port_number = port;
        let port = port.to_string();
        let auth = self
            .password
            .as_ref()
            .filter(|_| !SENTINELS.contains(&port_number))
            .map(|p| format!("REDISCLI_AUTH={p}"));
        let mut full = vec!["exec"];
        if let Some(env) = &auth {
            full.extend(["-e", env]);
        }
        full.extend([self.name.as_str(), "redis-cli", "-p", &port]);
        full.extend_from_slice(args);
        let out = docker(&full);
        assert!(out.status.success(), "redis-cli {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `SENTINEL FAILOVER mymaster`, repeated while the Sentinels aren't
    /// ready for one yet.
    pub fn failover(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let reply = self.cli(SENTINELS[0], &["SENTINEL", "FAILOVER", "mymaster"]);
            if reply.trim() == "OK" {
                return;
            }
            assert!(Instant::now() < deadline, "no failover: {reply}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Kill the process of a Redis node; the container stays up.
    pub fn kill(&self, port: u16) {
        let pidfile = format!("/data/{port}/redis.pid");
        let script = format!("kill -9 \"$(cat {pidfile})\"");
        let out = docker(&["exec", &self.name, "sh", "-c", &script]);
        assert!(out.status.success(), "kill {port} failed");
    }
}

impl Drop for SentinelFixture {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}
