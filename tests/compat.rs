//! The server compatibility matrix (#57): every feature against one image.
//!
//! `RROVER_TEST_IMAGE` picks the server and `RROVER_COMPAT_LABEL` names it in
//! `tests/compat/expected.json`. Each feature first asks the server whether it
//! has the capability, then asserts the real behaviour or the fallback, so
//! nothing is skipped silently. A result that differs from the expected table
//! fails the test; a known `failed` stays visible in the table and is green.
//! `scripts/compat.sh` runs every image and writes the result files.

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;
use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use redis::{aio::ConnectionManager, AsyncCommands, Value};
use redis_rover::internals::{
    Action, ItemDelete, ItemEdit, KeyItem, KeyValue, LiveCmd, Liveness, MonitorCmd, MonitorStatus,
    PubSubCmd, RedisEvent, RedisType, Target, Ttl, TypeFilter,
};
use serde_json::{json, Value as Json};
use support::{Harness, Server, TEST_IMAGE_ENV, WAIT};

const FEATURE_LIMIT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ok,
    Fallback,
    Failed,
    NotApplicable,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fallback => "fallback",
            Self::Failed => "failed",
            Self::NotApplicable => "n/a",
        }
    }
}

struct Outcome {
    status: Status,
    note: String,
}

fn ok() -> Outcome {
    Outcome {
        status: Status::Ok,
        note: String::new(),
    }
}

fn fallback(note: &str) -> Outcome {
    Outcome {
        status: Status::Fallback,
        note: note.to_owned(),
    }
}

/// Whether the server accepts this command at all. Only "no such command"
/// style refusals count: a wrong-argument reply still proves it exists.
async fn has(admin: &mut ConnectionManager, args: &[&str]) -> bool {
    let mut cmd = redis::cmd(args[0]);
    for arg in &args[1..] {
        cmd.arg(*arg);
    }
    match cmd.query_async::<Value>(admin).await {
        Ok(_) => true,
        Err(e) => {
            let text = e.to_string().to_lowercase();
            !(text.contains("unknown command")
                || text.contains("unknown subcommand")
                || text.contains("syntax error")
                || text.contains("not supported")
                || text.contains("unsupported"))
        }
    }
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn write_done(h: &mut Harness, event: RedisEvent) {
    h.send(event);
    let answer = h
        .expect("the write's result", |a| {
            matches!(
                a,
                Action::KeysChanged | Action::Notice(_) | Action::Error(_)
            )
        })
        .await;
    assert!(
        !matches!(answer, Action::Error(_)),
        "write refused: {answer:?}"
    );
}

async fn load_value(h: &mut Harness, key: &str, r_type: RedisType) -> KeyValue {
    h.state.value_state.lock().select(key, r_type);
    h.send(RedisEvent::FetchValue);
    let action = h
        .expect("the value or a failure", |a| {
            matches!(
                a,
                Action::LoadSelectedValueIntoView | Action::ValueLoadFailed | Action::Error(_)
            )
        })
        .await;
    assert_eq!(
        action,
        Action::LoadSelectedValueIntoView,
        "FetchValue {key}"
    );
    h.state
        .selected_value
        .lock()
        .as_ref()
        .map(|loaded| loaded.value.clone())
        .expect("a loaded value")
}

async fn browse(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let p = support::prefix("browse");
    let seeded = support::seed(&mut admin, &p, 5).await;
    let mut h = Harness::new(server.client(0), false).await;
    let shown: std::collections::HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert!(shown.is_superset(&seeded.all()), "a seeded key is missing");
    for (key, r_type) in [
        (&seeded.strings[0], RedisType::String),
        (&seeded.hashes[0], RedisType::Hash),
        (&seeded.lists[0], RedisType::List),
        (&seeded.sets[0], RedisType::Set),
        (&seeded.zsets[0], RedisType::Zset),
    ] {
        load_value(&mut h, key, r_type).await;
    }
    h.shutdown().await;
    ok()
}

async fn edit(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let p = support::prefix("edit");
    let (string, hash, list, set, zset) = (
        format!("{p}s"),
        format!("{p}h"),
        format!("{p}l"),
        format!("{p}set"),
        format!("{p}z"),
    );
    let () = admin.set(&string, "old").await.expect("seed");
    let () = admin.hset(&hash, "f", "1").await.expect("seed");
    let () = admin.rpush(&list, &["a", "b"]).await.expect("seed");
    let () = admin.sadd(&set, "a").await.expect("seed");
    let () = admin.zadd(&zset, "a", 1).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;

    write_done(
        &mut h,
        RedisEvent::SetString {
            key: string.clone(),
            value: "new".into(),
        },
    )
    .await;
    write_done(
        &mut h,
        RedisEvent::AddItem {
            key: hash.clone(),
            item: KeyItem::HashField {
                field: "g".into(),
                value: "2".into(),
            },
        },
    )
    .await;
    write_done(
        &mut h,
        RedisEvent::EditItem {
            key: list.clone(),
            edit: ItemEdit::ListSet {
                index: 0,
                expected: "a".into(),
                value: "z".into(),
            },
        },
    )
    .await;
    write_done(
        &mut h,
        RedisEvent::EditItem {
            key: set.clone(),
            edit: ItemEdit::SetReplace {
                old: "a".into(),
                new: "b".into(),
            },
        },
    )
    .await;
    write_done(
        &mut h,
        RedisEvent::EditItem {
            key: zset.clone(),
            edit: ItemEdit::ZsetScore {
                member: "a".into(),
                score: 7.0,
            },
        },
    )
    .await;
    write_done(
        &mut h,
        RedisEvent::DeleteItem {
            key: hash.clone(),
            delete: ItemDelete::HashField("f".into()),
        },
    )
    .await;

    let value: String = admin.get(&string).await.expect("GET");
    assert_eq!(value, "new");
    let fields: Vec<String> = admin.hkeys(&hash).await.expect("HKEYS");
    assert_eq!(fields, ["g"]);
    let items: Vec<String> = admin.lrange(&list, 0, -1).await.expect("LRANGE");
    assert_eq!(items, ["z", "b"]);
    let members: Vec<String> = admin.smembers(&set).await.expect("SMEMBERS");
    assert_eq!(members, ["b"]);
    let score: f64 = admin.zscore(&zset, "a").await.expect("ZSCORE");
    assert!((score - 7.0).abs() < f64::EPSILON);
    h.shutdown().await;
    ok()
}

async fn ttl(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let key = format!("{}k", support::prefix("ttl"));
    let () = admin.set(&key, "v").await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;
    write_done(
        &mut h,
        RedisEvent::SetTtl {
            key: key.clone(),
            ttl: Ttl::Expire(900),
        },
    )
    .await;
    let left: i64 = admin.ttl(&key).await.expect("TTL");
    assert!((890..=900).contains(&left), "{left}");
    write_done(
        &mut h,
        RedisEvent::ShiftTtl {
            key: key.clone(),
            delta_secs: 100,
        },
    )
    .await;
    let left: i64 = admin.ttl(&key).await.expect("TTL");
    assert!((990..=1000).contains(&left), "{left}");
    write_done(
        &mut h,
        RedisEvent::SetTtl {
            key: key.clone(),
            ttl: Ttl::Persist,
        },
    )
    .await;
    let left: i64 = admin.ttl(&key).await.expect("TTL");
    assert_eq!(left, -1);
    h.shutdown().await;
    ok()
}

async fn rename(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let p = support::prefix("rename");
    let (from, to, hash) = (format!("{p}a"), format!("{p}b"), format!("{p}h"));
    let () = admin.set(&from, "v").await.expect("seed");
    let () = admin.hset(&hash, "old", "1").await.expect("seed");
    let scripting = redis::cmd("EVAL")
        .arg("return 1")
        .arg(0)
        .query_async::<i64>(&mut admin)
        .await
        .is_ok();
    let mut h = Harness::new(server.client(0), false).await;
    write_done(
        &mut h,
        RedisEvent::RenameKey {
            from: from.clone(),
            to: to.clone(),
            overwrite: false,
        },
    )
    .await;
    let moved: String = admin.get(&to).await.expect("GET");
    assert_eq!(moved, "v");
    write_done(
        &mut h,
        RedisEvent::RenameField {
            key: hash.clone(),
            from: "old".into(),
            to: "new".into(),
        },
    )
    .await;
    let value: Option<String> = admin.hget(&hash, "new").await.expect("HGET");
    assert_eq!(value.as_deref(), Some("1"));
    h.shutdown().await;
    if scripting {
        ok()
    } else {
        fallback("no EVAL: plain commands that never overwrite")
    }
}

async fn copy(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let mut db2 = server.admin(2).await;
    let key = format!("{}k", support::prefix("copy"));
    let () = admin.hset(&key, "f", "v").await.expect("seed");
    let () = admin.expire(&key, 900).await.expect("seed");
    let native = has(
        &mut admin,
        &["COPY", "rrover:none", "rrover:none2", "DB", "2"],
    )
    .await;
    let mut h = Harness::new(server.client(0), false).await;
    write_done(
        &mut h,
        RedisEvent::CopyKey {
            from: key.clone(),
            to: key.clone(),
            db: Some(2),
            overwrite: false,
        },
    )
    .await;
    let value: Option<String> = db2.hget(&key, "f").await.expect("HGET");
    assert_eq!(value.as_deref(), Some("v"));
    let left: i64 = db2.ttl(&key).await.expect("TTL");
    assert!((880..=900).contains(&left), "TTL kept: {left}");
    h.shutdown().await;
    if native {
        ok()
    } else {
        fallback("no COPY … DB: DUMP + RESTORE, TTL kept")
    }
}

async fn type_filter(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let p = support::prefix("types");
    let seeded = support::seed(&mut admin, &p, 5).await;
    let native = has(&mut admin, &["SCAN", "0", "TYPE", "string"]).await;
    let mut h = Harness::new(server.client(0), false).await;
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_pattern(Some(format!("{p}*")));
        ks.set_type_filter(TypeFilter::new(["hash"]));
    }
    let shown: std::collections::HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert_eq!(shown, seeded.hashes.iter().cloned().collect());
    h.shutdown().await;
    if native {
        ok()
    } else {
        fallback("no SCAN TYPE: filtered by TYPE on the client")
    }
}

async fn bulk_delete(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let p = support::prefix("bulk");
    let keys = support::seed_strings(&mut admin, &p, 20).await;
    let native = has(&mut admin, &["UNLINK", "rrover:none"]).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.send(RedisEvent::DeleteKeys { keys: keys.clone() });
    let answer = h
        .expect("deleted", |a| {
            matches!(a, Action::KeysDeleted(_) | Action::Error(_))
        })
        .await;
    assert_eq!(answer, Action::KeysDeleted(20));
    let left: i64 = admin.exists(&keys).await.expect("EXISTS");
    assert_eq!(left, 0);
    h.shutdown().await;
    if native {
        ok()
    } else {
        fallback("no UNLINK: DEL")
    }
}

async fn live(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let key = format!("{}k", support::prefix("live"));
    let () = admin.hset(&key, "f", "1").await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;
    load_value(&mut h, &key, RedisType::Hash).await;
    h.live(LiveCmd::Track {
        key: key.clone(),
        db: 0,
    });
    let status = h
        .expect("a live status", |a| {
            matches!(
                a,
                Action::LiveStatus(
                    Liveness::Live | Liveness::Polling { .. } | Liveness::NotLive(_)
                )
            )
        })
        .await;
    let outcome = match &status {
        Action::LiveStatus(Liveness::Live) => ok(),
        Action::LiveStatus(Liveness::Polling { every_ms }) => {
            fallback(&format!("polling every {every_ms} ms"))
        }
        other => panic!("not following changes: {other:?}"),
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let () = admin.hset(&key, "f", "2").await.expect("HSET");
    h.expect_within(
        "the change",
        Duration::from_secs(6),
        |a| matches!(a, Action::OpenKeyChanged { key: k, .. } if *k == key),
    )
    .await;
    h.shutdown().await;
    outcome
}

async fn pubsub(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let mut h = Harness::new(server.client(0), false).await;
    let target = Target::Channel("compat".into());
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let heard: i64 = admin.publish("compat", "hi").await.expect("PUBLISH");
        if heard > 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never subscribed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let store = h.state.pubsub.clone();
    until("the message", || store.lock().subs[0].stats.total >= 1).await;
    h.send(RedisEvent::Publish {
        channel: "compat".into(),
        payload: "second".into(),
    });
    h.expect("published", |a| matches!(a, Action::Published { .. }))
        .await;
    until("our own message", || store.lock().subs[0].stats.total >= 2).await;
    h.shutdown().await;
    ok()
}

async fn sharded_pubsub(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let native = has(&mut admin, &["SPUBLISH", "compat", "probe"]).await;
    let mut h = Harness::new(server.client(0), false).await;
    let target = Target::Shard("compat".into());
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target));
    let store = h.state.pubsub.clone();
    let outcome = if native {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let heard: i64 = redis::cmd("SPUBLISH")
                .arg("compat")
                .arg("hi")
                .query_async(&mut admin)
                .await
                .expect("SPUBLISH");
            if heard > 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "never subscribed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        until("the message", || store.lock().subs[0].stats.total >= 1).await;
        ok()
    } else {
        h.expect("a clear refusal", |a| matches!(a, Action::Error(_)))
            .await;
        Outcome {
            status: Status::NotApplicable,
            note: "no SSUBSCRIBE: refused with a message, plain Pub/Sub still works".into(),
        }
    };
    h.shutdown().await;
    outcome
}

async fn monitor(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let h = Harness::new(server.client(0), false).await;
    h.monitor(MonitorCmd::Start);
    let store = h.state.monitor.clone();
    until("MONITOR running", || {
        *store.lock().status() == MonitorStatus::Running
    })
    .await;
    let key = format!("{}k", support::prefix("monitor"));
    let () = admin.set(&key, "seen").await.expect("SET");
    until("the line", || {
        store.lock().iter().any(|l| l.line.args.contains(&key))
    })
    .await;
    h.monitor(MonitorCmd::Stop);
    h.shutdown().await;
    ok()
}

async fn slowlog(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let configured = redis::cmd("CONFIG")
        .arg("SET")
        .arg("slowlog-log-slower-than")
        .arg("0")
        .query_async::<Value>(&mut admin)
        .await
        .is_ok();
    let () = admin.set("compat:slow", "v").await.expect("SET");
    let mut h = Harness::new(server.client(0), false).await;
    h.send(RedisEvent::FetchSlowlog);
    h.expect("loaded", |a| *a == Action::SlowlogLoaded).await;
    let data = h.state.slowlog.lock().take().expect("data");
    h.shutdown().await;
    if let Some(error) = data.error {
        return fallback(&format!("refusal shown inline: {error}"));
    }
    if !configured {
        return fallback("CONFIG SET refused: the log is read only");
    }
    assert!(
        data.entries
            .iter()
            .any(|e| e.args.iter().any(|a| a == "compat:slow")),
        "the SET is not listed"
    );
    ok()
}

async fn streams(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let key = format!("{}s", support::prefix("stream"));
    for id in ["1-0", "2-0", "3-0"] {
        let _: String = redis::cmd("XADD")
            .arg(&key)
            .arg(id)
            .arg("f")
            .arg("v")
            .query_async(&mut admin)
            .await
            .expect("XADD");
    }
    let () = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(&key)
        .arg("billing")
        .arg("0")
        .query_async(&mut admin)
        .await
        .expect("XGROUP");
    let _: Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("billing")
        .arg("worker")
        .arg("COUNT")
        .arg(2)
        .arg("STREAMS")
        .arg(&key)
        .arg(">")
        .query_async(&mut admin)
        .await
        .expect("XREADGROUP");
    let mut h = Harness::new(server.client(0), true).await;
    let KeyValue::Stream(stream) = load_value(&mut h, &key, RedisType::Stream).await else {
        panic!("not a stream");
    };
    let groups = stream.groups.expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!((groups[0].consumers, groups[0].pending), (1, 2));
    let has_lag = groups[0].lag.is_some();
    h.send(RedisEvent::FetchStreamGroup {
        key,
        group: "billing".into(),
    });
    h.expect("group", |a| *a == Action::StreamGroupLoaded).await;
    let detail = h.state.stream_group.lock().take().expect("detail");
    assert_eq!(detail.error, None);
    assert_eq!(detail.consumers.len(), 1);
    assert_eq!(detail.pending.len(), 2);
    h.shutdown().await;
    if has_lag {
        ok()
    } else {
        fallback("no XINFO lag field: the column stays empty")
    }
}

async fn redis_json(server: Arc<Server>) -> Outcome {
    let mut admin = server.admin(0).await;
    let key = format!("{}j", support::prefix("json"));
    let stored = redis::cmd("JSON.SET")
        .arg(&key)
        .arg("$")
        .arg(r#"{"hello":"world"}"#)
        .query_async::<Value>(&mut admin)
        .await;
    if stored.is_err() {
        return Outcome {
            status: Status::NotApplicable,
            note: "no RedisJSON module".into(),
        };
    }
    let mut h = Harness::new(server.client(0), false).await;
    let value = load_value(&mut h, &key, RedisType::Json).await;
    assert!(format!("{value:?}").contains("world"), "{value:?}");
    h.shutdown().await;
    ok()
}

type Run = fn(Arc<Server>) -> Pin<Box<dyn Future<Output = Outcome> + Send>>;

const FEATURES: [(&str, Run); 14] = [
    ("browse", |s| Box::pin(browse(s))),
    ("edit", |s| Box::pin(edit(s))),
    ("ttl", |s| Box::pin(ttl(s))),
    ("rename", |s| Box::pin(rename(s))),
    ("copy", |s| Box::pin(copy(s))),
    ("type_filter", |s| Box::pin(type_filter(s))),
    ("bulk_delete", |s| Box::pin(bulk_delete(s))),
    ("live", |s| Box::pin(live(s))),
    ("pubsub", |s| Box::pin(pubsub(s))),
    ("sharded_pubsub", |s| Box::pin(sharded_pubsub(s))),
    ("monitor", |s| Box::pin(monitor(s))),
    ("slowlog", |s| Box::pin(slowlog(s))),
    ("streams", |s| Box::pin(streams(s))),
    ("json", |s| Box::pin(redis_json(s))),
];

/// One feature in its own task, so a panic or a hang becomes a `failed` row
/// with its reason instead of ending the run.
async fn run_feature(run: Run, server: Arc<Server>) -> Outcome {
    let task = tokio::spawn(run(server));
    let abort = task.abort_handle();
    match tokio::time::timeout(FEATURE_LIMIT, task).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            let note = error.try_into_panic().ok().map_or_else(
                || "the task was cancelled".to_owned(),
                |panic| {
                    panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                        .unwrap_or_else(|| "panicked".to_owned())
                },
            );
            let note: String = note
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(160)
                .collect();
            Outcome {
                status: Status::Failed,
                note,
            }
        }
        Err(_) => {
            abort.abort();
            Outcome {
                status: Status::Failed,
                note: "timed out".into(),
            }
        }
    }
}

async fn server_version(server: &Server) -> String {
    let mut admin = server.admin(0).await;
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut admin)
        .await
        .unwrap_or_default();
    let field = |name: &str| {
        info.lines().find_map(|l| {
            l.strip_prefix(name)?
                .strip_prefix(':')
                .map(|v| v.trim().to_owned())
        })
    };
    let redis = field("redis_version").unwrap_or_else(|| "unknown".into());
    match (field("valkey_version"), field("dragonfly_version")) {
        (Some(valkey), _) => format!("valkey {valkey} (reports redis {redis})"),
        (_, Some(dragonfly)) => format!("{dragonfly} (reports redis {redis})"),
        _ => redis,
    }
}

/// Differences from `tests/compat/expected.json`.
fn differences(label: &str, statuses: &BTreeMap<&str, &str>) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/compat/expected.json");
    let text = std::fs::read_to_string(&path).expect("tests/compat/expected.json");
    let expected: Json = serde_json::from_str(&text).expect("valid expected.json");
    let Some(row) = expected.get(label) else {
        return vec![format!("no row for `{label}` in expected.json")];
    };
    statuses
        .iter()
        .filter_map(|(feature, status)| {
            let want = row
                .get(*feature)
                .and_then(Json::as_str)
                .unwrap_or("(missing)");
            (want != *status).then(|| format!("{feature}: expected {want}, got {status}"))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker"]
async fn compat_matrix() {
    let image = std::env::var(TEST_IMAGE_ENV).unwrap_or_else(|_| "redis:7-alpine".to_owned());
    let label = std::env::var("RROVER_COMPAT_LABEL").unwrap_or_else(|_| "default".to_owned());
    let server = Arc::new(support::start_redis().await);
    let version = server_version(&server).await;

    let mut rows = serde_json::Map::new();
    let mut statuses = BTreeMap::new();
    for (name, run) in FEATURES {
        let outcome = run_feature(run, Arc::clone(&server)).await;
        eprintln!("{label} {name}: {} {}", outcome.status.name(), outcome.note);
        statuses.insert(name, outcome.status.name());
        rows.insert(
            name.to_owned(),
            json!({ "status": outcome.status.name(), "note": outcome.note }),
        );
    }

    if let Ok(dir) = std::env::var("RROVER_COMPAT_OUT") {
        std::fs::create_dir_all(&dir).expect("the results dir");
        let result =
            json!({ "label": label, "image": image, "version": version, "features": rows });
        let text = serde_json::to_string_pretty(&result).expect("JSON");
        std::fs::write(
            PathBuf::from(dir).join(format!("{label}.json")),
            text + "\n",
        )
        .expect("write the result");
    }
    if std::env::var_os("RROVER_COMPAT_RECORD").is_none() {
        let diff = differences(&label, &statuses);
        assert!(
            diff.is_empty(),
            "{label} differs from expected.json:\n{}",
            diff.join("\n")
        );
    }
}
