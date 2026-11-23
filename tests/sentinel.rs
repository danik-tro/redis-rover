//! Connecting through Sentinel, against the fixture in `tests/docker` (one
//! master, two replicas, three Sentinels). Needs Docker:
//! `cargo test --test sentinel -- --ignored --test-threads=1`.

mod sentinel_support;
#[path = "support/mod.rs"]
mod support;

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use redis::ConnectionAddr;
use redis_rover::internals::{
    Action, KeyChange, LiveCmd, Liveness, MasterRoute, MonitorCmd, MonitorStatus, PubSubCmd,
    RedisEvent, SentinelTarget, Target, Ttl,
};
use sentinel_support::{SentinelFixture, MASTER, REPLICAS, SENTINELS, URL};
use support::Harness;

/// AC6: how long after the failover the app must be back.
const RECONNECT_WITHIN: Duration = Duration::from_secs(10);

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

/// `credentials` is the URL's `user:password@` part, or empty.
fn route(credentials: &str) -> (redis::Client, MasterRoute) {
    let client =
        redis::Client::open(format!("redis://{credentials}127.0.0.1:{MASTER}/0")).expect("a URL");
    let hosts = SENTINELS
        .iter()
        .map(|port| ConnectionAddr::Tcp("127.0.0.1".to_owned(), *port))
        .collect();
    let target = SentinelTarget::new(hosts, "mymaster".to_owned(), None);
    (client.clone(), MasterRoute::new(target, client))
}

/// Everything the app keeps open, on the first master.
async fn open_everything(fixture: &SentinelFixture, h: &mut Harness) {
    fixture.cli(MASTER, &["SET", "k", "before"]);
    assert_eq!(h.fetch_page().await, ["k"]);

    let channel = Target::Channel("ch".into());
    h.state.pubsub.lock().add(channel.clone());
    h.pubsub(PubSubCmd::Subscribe(channel));

    h.monitor(MonitorCmd::Start);
    let deadline = Instant::now() + RECONNECT_WITHIN;
    while *h.state.monitor.lock().status() != MonitorStatus::Running {
        assert!(Instant::now() < deadline, "MONITOR never ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    h.live(LiveCmd::Track {
        key: "k".into(),
        db: 0,
    });
    h.expect("live", |a| *a == Action::LiveStatus(Liveness::Live))
        .await;
}

/// The port of the master named by `sentinel mymaster → 127.0.0.1:port`.
fn port_of(label: &str) -> u16 {
    let (_, port) = label.rsplit_once(':').expect("host:port");
    port.parse().expect("a port")
}

/// After the move: keys, writes, Pub/Sub, MONITOR and live tracking all work
/// on `master`, within what is left of [`RECONNECT_WITHIN`].
async fn everything_works_on(
    fixture: &SentinelFixture,
    h: &mut Harness,
    master: u16,
    from: Instant,
) {
    let deadline = from + RECONNECT_WITHIN;
    let mut heard = false;
    let mut monitored = false;
    let mut tracked = false;
    let mut n = 0;
    while !(heard && monitored && tracked) {
        assert!(
            Instant::now() < deadline,
            "not back in time: pubsub {heard}, monitor {monitored}, live {tracked}"
        );
        n += 1;
        fixture.cli(master, &["PUBLISH", "ch", "after"]);
        fixture.cli(master, &["SET", "k", &format!("after-{n}")]);
        tokio::time::sleep(Duration::from_millis(150)).await;
        heard = h.state.pubsub.lock().subs[0].stats.total > 0;
        monitored = h
            .state
            .monitor
            .lock()
            .iter()
            .any(|line| line.line.args.iter().any(|arg| arg.starts_with("after-")));
        tracked |= h.drain_actions().iter().any(|action| {
            matches!(
                action,
                Action::OpenKeyChanged {
                    change: KeyChange::Changed { .. },
                    ..
                }
            )
        });
    }

    // The sweep restarted on the new master, and a write lands there.
    assert_eq!(h.fetch_page().await, ["k"]);
    h.send(RedisEvent::SetTtl {
        key: "k".into(),
        ttl: Ttl::Expire(500),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ttl: i64 = fixture
            .cli(master, &["TTL", "k"])
            .trim()
            .parse()
            .expect("a TTL");
        if ttl > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the write missed the new master: TTL {ttl}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn follows(fail: impl FnOnce(&SentinelFixture)) {
    follows_on(&SentinelFixture::start(), "", fail).await;
}

async fn follows_on(
    fixture: &SentinelFixture,
    credentials: &str,
    fail: impl FnOnce(&SentinelFixture),
) {
    let (client, route) = route(credentials);
    let mut h = Harness::sentinel(client, route).await;
    open_everything(fixture, &mut h).await;

    let started = Instant::now();
    fail(fixture);
    let moved = h
        .expect("the new master", |a| matches!(a, Action::MasterChanged(_)))
        .await;
    let Action::MasterChanged(label) = moved else {
        unreachable!("matched above");
    };
    assert!(
        label.starts_with("sentinel mymaster → 127.0.0.1:"),
        "{label}"
    );
    let master = port_of(&label);
    assert_ne!(master, MASTER, "{label}");
    everything_works_on(fixture, &mut h, master, started).await;
    eprintln!("back on {master} after {:?}", started.elapsed());
    h.shutdown().await;
}

/// AC6.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_sentinel_failover_moves_every_connection() {
    follows(SentinelFixture::failover).await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_killed_master_moves_every_connection() {
    follows(|fixture| fixture.kill(MASTER)).await;
}

const MASTER_SECRET: &str = "master-s3cret";

/// A named user on every data node (ACLs aren't replicated).
fn add_user(fixture: &SentinelFixture, user: &str, password: &str) {
    for port in std::iter::once(MASTER).chain(REPLICAS) {
        fixture.cli(
            port,
            &[
                "ACL",
                "SETUSER",
                user,
                "on",
                &format!(">{password}"),
                "~*",
                "&*",
                "+@all",
            ],
        );
    }
}

fn sentinel_url(credentials: &str) -> String {
    URL.replacen(
        "redis+sentinel://",
        &format!("redis+sentinel://{credentials}"),
        1,
    )
}

/// #94: the URL's credentials reach the master, a missing or wrong password
/// is refused, and no output shows the password.
#[test]
#[ignore = "needs Docker"]
fn a_password_protected_master_needs_the_urls_password() {
    let _fixture = SentinelFixture::start_with_password(MASTER_SECRET);

    for credentials in [String::new(), ":wrong-pass@".to_owned()] {
        let refused = rrover("{}", &["--url", &sentinel_url(&credentials), "--probe"]);
        let text = format!(
            "{}{}",
            stdout(&refused),
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(!refused.status.success(), "{text}");
        assert!(!text.contains("wrong-pass"), "{text}");
    }

    let url = sentinel_url(&format!(":{MASTER_SECRET}@"));
    let out = rrover("{}", &["--url", &url, "--probe"]);
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(text.contains("role      master"), "{text}");
    assert!(!text.contains(MASTER_SECRET), "{text}");

    let printed = rrover("{}", &["--url", &url, "--print-target"]);
    assert!(!stdout(&printed).contains(MASTER_SECRET));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failover_keeps_the_masters_password() {
    let fixture = SentinelFixture::start_with_password(MASTER_SECRET);
    follows_on(
        &fixture,
        &format!(":{MASTER_SECRET}@"),
        SentinelFixture::failover,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_killed_master_keeps_the_masters_password() {
    let fixture = SentinelFixture::start_with_password(MASTER_SECRET);
    follows_on(&fixture, &format!(":{MASTER_SECRET}@"), |f| f.kill(MASTER)).await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failover_keeps_a_named_acl_user() {
    let fixture = SentinelFixture::start_with_password(MASTER_SECRET);
    add_user(&fixture, "app", "app-s3cret");
    follows_on(&fixture, "app:app-s3cret@", SentinelFixture::failover).await;
}
