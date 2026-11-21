//! Cluster tests against the six-node fixture in `tests/docker`. Every test
//! is `#[ignore]`d (they need Docker) and uses the fixed ports 7100-7105:
//!
//! ```bash
//! cargo test --test cluster -- --ignored --test-threads=1
//! ```

mod cluster_support;
#[path = "support/mod.rs"]
mod support;

use cluster_support::ClusterFixture;
use redis::AsyncCommands;
use redis_rover::internals::{
    connect_cluster, Action, KeyspaceState, Link, MasterNode, MemberKind, MonitorCmd,
    MonitorStatus, NewKeySpec, RedisEvent, Storage, SweepApply, Ttl, ValueEdit,
};
use support::Harness;

/// The sweep walks every master and finds each key exactly once.
#[tokio::test]
#[ignore = "needs Docker"]
async fn sweep_covers_every_master() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut seed = client.get_async_connection().await.unwrap();
    for i in 0..3000 {
        let () = seed.set(format!("sweep:{i}"), i).await.unwrap();
    }

    let storage = Storage::new(connect_cluster(&client, Link::Bulk).await.unwrap());
    let masters = storage.masters().await.unwrap();
    assert_eq!(masters.len(), 3, "{masters:?}");
    let slots: Vec<u16> = masters.iter().filter_map(MasterNode::route_slot).collect();

    let mut state = KeyspaceState::default();
    state.set_sweep_nodes(&slots);
    while let Some(request) = state.load_request() {
        let result = storage.scan_until(&request).await.unwrap();
        assert_eq!(state.apply_result(&request, result), SweepApply::Applied);
    }
    assert_eq!(state.scanned.len(), 3000);
}

/// `fetch_meta` answers for keys on all masters, in page order, and the
/// counts cover the whole cluster.
#[tokio::test]
#[ignore = "needs Docker"]
async fn metadata_and_counts_cover_every_master() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut seed = client.get_async_connection().await.unwrap();
    let keys: Vec<String> = (0..500).map(|i| format!("meta:{i}")).collect();
    for key in &keys {
        let () = seed.set(key, "value").await.unwrap();
    }
    let () = seed.set("other", "value").await.unwrap();

    let storage = Storage::new(connect_cluster(&client, Link::Interactive).await.unwrap());
    let metas = storage.fetch_meta(&keys).await.unwrap();
    let names: Vec<&String> = metas.iter().map(|meta| &meta.key).collect();
    assert_eq!(names, keys.iter().collect::<Vec<_>>());
    assert!(metas.iter().all(|meta| meta.r_type.name() == "string"));

    assert_eq!(storage.total_keys().await.unwrap(), 501);
    let budget = std::time::Duration::from_secs(5);
    assert_eq!(
        storage.count_matches("*", 200, budget).await.unwrap(),
        (501, true)
    );
    assert_eq!(
        storage.count_matches("meta:*", 200, budget).await.unwrap(),
        (500, true)
    );
}

/// Send one write and wait until the runner reports it done.
async fn write(harness: &mut Harness, event: RedisEvent) {
    let what = format!("{event:?}");
    harness.send(event);
    let action = harness
        .expect(&what, |a| {
            matches!(a, Action::KeysChanged | Action::Error(_))
        })
        .await;
    assert_eq!(action, Action::KeysChanged, "{what}");
}

/// AC2: every kind of write works on keys that live on different masters,
/// and none fails with `CROSSSLOT`. `foo`, `bar` and `c` are on three masters.
#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)] // one step per kind of write
async fn writes_reach_every_master() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut con = client.get_async_connection().await.unwrap();
    let mut harness = Harness::cluster(fixture.seed(), fixture.client()).await;
    harness.fetch_page().await;

    for key in ["foo", "bar", "c"] {
        write(
            &mut harness,
            RedisEvent::CreateKey {
                key: key.into(),
                spec: NewKeySpec::String("v1".into()),
            },
        )
        .await;
        write(
            &mut harness,
            RedisEvent::SetString {
                key: key.into(),
                value: "v2".into(),
            },
        )
        .await;
        let value: String = con.get(key).await.unwrap();
        assert_eq!(value, "v2", "{key}");

        // WATCH + MULTI must land on the key's node.
        write(
            &mut harness,
            RedisEvent::SetTtl {
                key: key.into(),
                ttl: Ttl::Expire(1000),
            },
        )
        .await;
        write(
            &mut harness,
            RedisEvent::ShiftTtl {
                key: key.into(),
                delta_secs: 500,
            },
        )
        .await;
        let ttl: i64 = con.ttl(key).await.unwrap();
        assert!(ttl > 1000, "{key}: {ttl}");

        // RENAME and COPY inside one slot, through a hash tag.
        let (from, to, copy) = (
            format!("{{{key}}}:from"),
            format!("{{{key}}}:to"),
            format!("{{{key}}}:copy"),
        );
        let () = con.set(&from, "moved").await.unwrap();
        write(
            &mut harness,
            RedisEvent::RenameKey {
                from: from.clone(),
                to: to.clone(),
                overwrite: false,
            },
        )
        .await;
        write(
            &mut harness,
            RedisEvent::CopyKey {
                from: to.clone(),
                to: copy.clone(),
                db: None,
                overwrite: false,
            },
        )
        .await;
        let value: String = con.get(&copy).await.unwrap();
        assert_eq!(value, "moved", "{key}");

        // EVAL routed by its key.
        let hash = format!("{{{key}}}:hash");
        let () = con.hset(&hash, "old", "x").await.unwrap();
        write(
            &mut harness,
            RedisEvent::RenameField {
                key: hash.clone(),
                from: "old".into(),
                to: "new".into(),
            },
        )
        .await;
        let renamed: Option<String> = con.hget(&hash, "new").await.unwrap();
        assert_eq!(renamed.as_deref(), Some("x"), "{key}");

        let set = format!("{{{key}}}:set");
        let () = con.sadd(&set, "old").await.unwrap();
        write(
            &mut harness,
            RedisEvent::RenameMember {
                key: set.clone(),
                kind: MemberKind::Set,
                from: "old".into(),
                to: "new".into(),
            },
        )
        .await;

        // MULTI with the TTL put back.
        let list = format!("{{{key}}}:list");
        let () = con.rpush(&list, "a").await.unwrap();
        let () = con.expire(&list, 1000).await.unwrap();
        write(
            &mut harness,
            RedisEvent::ApplyValueEdit {
                key: list.clone(),
                edit: ValueEdit::List {
                    items: vec!["x".into(), "y".into()],
                },
            },
        )
        .await;
        let items: Vec<String> = con.lrange(&list, 0, -1).await.unwrap();
        assert_eq!(items, ["x", "y"], "{key}");
        let ttl: i64 = con.ttl(&list).await.unwrap();
        assert!(ttl > 0, "{key}: the list kept its TTL");
    }

    // One bulk delete across every master and many slots.
    let mut marked: Vec<String> = (0..1200).map(|i| format!("bulk:{i}")).collect();
    for key in &marked {
        let () = con.set(key, 1).await.unwrap();
    }
    marked.extend(["foo".to_owned(), "bar".to_owned(), "c".to_owned()]);
    harness.send(RedisEvent::DeleteKeys {
        keys: marked.clone(),
    });
    let action = harness
        .expect("the bulk delete", |a| {
            matches!(a, Action::KeysDeleted(_) | Action::Error(_))
        })
        .await;
    assert_eq!(action, Action::KeysDeleted(1203));
    for key in ["foo", "bar", "c", "bulk:0", "bulk:1199"] {
        let exists: bool = con.exists(key).await.unwrap();
        assert!(!exists, "{key}");
    }
    harness.shutdown().await;
}

/// AC7: a rename or copy across slots is refused with both slot numbers and
/// changes nothing.
#[tokio::test]
#[ignore = "needs Docker"]
async fn cross_slot_moves_are_refused() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut con = client.get_async_connection().await.unwrap();
    let () = con.set("foo", "v").await.unwrap();
    let mut harness = Harness::cluster(fixture.seed(), fixture.client()).await;

    for event in [
        RedisEvent::RenameKey {
            from: "foo".into(),
            to: "bar".into(),
            overwrite: false,
        },
        RedisEvent::CopyKey {
            from: "foo".into(),
            to: "bar".into(),
            db: None,
            overwrite: false,
        },
    ] {
        harness.send(event);
        let Action::Error(message) = harness
            .expect("the refusal", |a| matches!(a, Action::Error(_)))
            .await
        else {
            unreachable!("expect matched an Error");
        };
        assert!(message.contains("12182 vs 5061"), "{message}");
        assert!(message.contains("hash tag"), "{message}");
    }
    let (foo, bar): (bool, bool) = (
        con.exists("foo").await.unwrap(),
        con.exists("bar").await.unwrap(),
    );
    assert!(foo && !bar);
    harness.shutdown().await;
}

/// AC3: a killed master is reported, the sweep restarts, and reads and writes
/// go on with the promoted replica.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failover_restarts_the_sweep() {
    let fixture = ClusterFixture::start();
    let client = fixture.client();
    let mut con = client.get_async_connection().await.unwrap();
    let keys: Vec<String> = (0..600).map(|i| format!("failover:{i}")).collect();
    for key in &keys {
        let () = con.set(key, "v").await.unwrap();
    }
    // Replicas must hold the keys before their master dies.
    let _: i64 = redis::cmd("WAIT")
        .arg(1)
        .arg(2000)
        .query_async(&mut con)
        .await
        .unwrap_or_default();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let mut harness = Harness::cluster(fixture.seed(), fixture.client()).await;
    assert_eq!(
        sorted(harness.fetch_all_pages().await),
        sorted(keys.clone())
    );
    let generation = harness.state.keyspace_state.lock().generation;

    // The master of slot 0, which also answers the node-wide commands.
    fixture.kill_node(7100);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let seen = harness.drain(std::time::Duration::from_millis(500)).await;
        if seen.iter().any(|action| {
            matches!(action, Action::Notice(text) if text == "cluster topology changed · rescanning")
        }) {
            break;
        }
        assert!(
            !seen.iter().any(|action| matches!(action, Action::Error(_))),
            "a failover is not an error popup: {seen:?}"
        );
        assert!(tokio::time::Instant::now() < deadline, "no topology toast");
    }
    assert!(harness.state.keyspace_state.lock().generation > generation);

    assert_eq!(
        sorted(harness.fetch_all_pages().await),
        sorted(keys.clone())
    );
    write(
        &mut harness,
        RedisEvent::SetString {
            key: "foo".into(),
            value: "after".into(),
        },
    )
    .await;
    // `bar` lived on the killed master.
    write(
        &mut harness,
        RedisEvent::SetString {
            key: "bar".into(),
            value: "after".into(),
        },
    )
    .await;
    harness.shutdown().await;
}

fn sorted(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys
}

/// The Slowlog and MONITOR follow the picked node, not the home node.
#[tokio::test]
#[ignore = "needs Docker"]
async fn slowlog_and_monitor_follow_the_picked_node() {
    let fixture = ClusterFixture::start();
    // `foo` (slot 12182) lives on 7102; the home node is 7100.
    let node = redis::Client::open("redis://127.0.0.1:7102").unwrap();
    let mut direct = node.get_multiplexed_async_connection().await.unwrap();
    let () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("slowlog-log-slower-than")
        .arg(0)
        .query_async(&mut direct)
        .await
        .unwrap();
    let () = direct.set("foo", "marker-on-7102").await.unwrap();

    let mut harness = Harness::cluster(fixture.seed(), fixture.client()).await;
    harness.fetch_page().await;
    *harness.state.ops_node.lock() = Some(("127.0.0.1".to_owned(), 7102));

    harness.send(RedisEvent::FetchSlowlog);
    harness
        .expect("the slowlog", |a| matches!(a, Action::SlowlogLoaded))
        .await;
    let data = harness.state.slowlog.lock().take().unwrap();
    assert!(
        data.entries
            .iter()
            .any(|entry| entry.args.iter().any(|arg| arg == "marker-on-7102")),
        "{data:?}"
    );

    harness.monitor(MonitorCmd::Start);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while *harness.state.monitor.lock().status() != MonitorStatus::Running {
        assert!(tokio::time::Instant::now() < deadline, "MONITOR never ran");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let () = direct.set("foo", "seen-by-monitor").await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let seen = harness
            .state
            .monitor
            .lock()
            .iter()
            .any(|line| line.line.args.iter().any(|arg| arg == "seen-by-monitor"));
        if seen {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the line never came"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    harness.monitor(MonitorCmd::Stop);
    harness.shutdown().await;
}
