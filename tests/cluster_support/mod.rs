//! Starts the fixture in `tests/docker` for one test and removes it on drop.

#![allow(dead_code)]

use std::{
    path::Path,
    process::{Command, Stdio},
    sync::Once,
    time::{Duration, Instant},
};

use redis::cluster::ClusterClient;

const IMAGE: &str = "rrover-test-cluster";
pub const PORTS: [u16; 6] = [7100, 7101, 7102, 7103, 7104, 7105];

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
        let dockerfile = dir.join("cluster.Dockerfile");
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

/// The running cluster. Dropping it removes the container, also when the
/// test panics.
pub struct ClusterFixture {
    name: String,
    seeds: Vec<String>,
}

impl ClusterFixture {
    pub fn start() -> Self {
        build_image();
        let name = format!("rrover-test-cluster-{}", std::process::id());
        let _ = docker(&["rm", "-f", &name]);
        let out = docker(&[
            "run",
            "-d",
            "--name",
            &name,
            "-p",
            "7100-7105:7100-7105",
            IMAGE,
        ]);
        assert!(
            out.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seeds = PORTS[..2]
            .iter()
            .map(|port| format!("redis://127.0.0.1:{port}"))
            .collect();
        let fixture = Self { name, seeds };
        fixture.wait_ready();
        fixture
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_mins(1);
        while !docker(&["exec", &self.name, "test", "-f", "/data/ready"])
            .status
            .success()
        {
            assert!(Instant::now() < deadline, "the cluster never became ready");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// A plain client on the first seed.
    pub fn seed(&self) -> redis::Client {
        redis::Client::open(self.seeds[0].as_str()).expect("valid seed")
    }

    pub fn client(&self) -> ClusterClient {
        ClusterClient::new(self.seeds.clone()).expect("valid seeds")
    }
}

impl Drop for ClusterFixture {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}
