//! Integration suite against a real Redis (`redis:7-alpine` in Docker).
//!
//! Every test is `#[ignore]`d so plain `cargo test` stays Docker-free. Run:
//!
//! ```sh
//! cargo test --test integration -- --ignored --test-threads=1
//! ```
//!
//! Each test starts its own container and drives the background runner through
//! its channels, the way `App` does: `RedisEvent`s in, `Action`s out, results
//! in `SharedState`.

mod support;

use std::{collections::HashSet, time::Duration};

use clap::Parser;
use redis::AsyncCommands;
use redis_rover::{
    internals::{
        Action, ConnStatus, EntryKind, ItemDelete, ItemEdit, KeyItem, KeyValue, NewKeySpec,
        PubSubCmd, RedisEvent, RedisType, Target,
    },
    Cli,
};
use support::{Harness, WAIT};

/// Poll `check` every 50 ms until it holds, for up to `limit`.
async fn eventually(what: &str, limit: Duration, check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Select `key` and fetch its first value page, as the value pane does.
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

/// Send a write and wait for the runner's answer: `RequestSelectedValue` /
/// `RefreshSpace` on success, `Error` on failure.
async fn write(h: &mut Harness, event: RedisEvent) -> Action {
    h.send(event);
    h.expect("the write's result", |a| {
        matches!(
            a,
            Action::RequestSelectedValue | Action::RefreshSpace | Action::Error(_)
        )
    })
    .await
}

/// `CLIENT KILL TYPE <kind>` from `con` (which is skipped). Returns how many
/// clients were killed.
async fn client_kill(con: &mut redis::aio::ConnectionManager, kind: &str) -> i64 {
    redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg(kind)
        .query_async(con)
        .await
        .expect("CLIENT KILL")
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn scan_paging_sees_every_key_once() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    // 500 of each of 5 types + 1 stream = 2,501 keys.
    let seeded = support::seed(&mut admin, &support::prefix("scan"), 500).await;
    let expected = seeded.all();
    assert_eq!(expected.len(), 2_501);

    let mut h = Harness::new(server.client(0), false).await;
    let shown = h.fetch_all_pages().await;

    let unique: HashSet<String> = shown.iter().cloned().collect();
    assert_eq!(unique.len(), shown.len(), "a key was shown twice");
    assert_eq!(unique, expected, "keys missing or extra");
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn filtered_paging_matches_the_count() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("filter");
    let users = support::seed_strings(&mut admin, &format!("{p}user:"), 300).await;
    support::seed_strings(&mut admin, &format!("{p}order:"), 300).await;
    support::seed(&mut admin, &p, 20).await;
    let pattern = format!("{p}user:*");

    let mut h = Harness::new(server.client(0), false).await;
    h.state
        .keyspace_state
        .lock()
        .set_pattern(Some(pattern.clone()));
    let shown = h.fetch_all_pages().await;

    assert!(shown.iter().all(|k| k.starts_with(&format!("{p}user:"))));
    let shown: HashSet<String> = shown.into_iter().collect();
    assert_eq!(shown, users.into_iter().collect::<HashSet<_>>());

    h.send(RedisEvent::CountMatches {
        pattern: pattern.clone(),
    });
    let count = h
        .expect("FilterMatchCount", |a| {
            matches!(a, Action::FilterMatchCount { .. })
        })
        .await;
    assert_eq!(
        count,
        Action::FilterMatchCount {
            pattern,
            count: shown.len(),
            complete: true,
        }
    );
    h.shutdown().await;
}

/// AC4: the filter changes while a page fetch is still scanning. The stale
/// result is dropped and the new filter's page is what ends up shown.
#[tokio::test]
#[ignore = "needs Docker"]
async fn filter_change_mid_fetch_shows_new_filter() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("midfetch");
    // A rare match: filling a 100-row page of `user:` takes the whole sweep,
    // so the first fetch is still scanning when the filter changes.
    support::seed_strings(&mut admin, &format!("{p}order:"), 20_000).await;
    support::seed_strings(&mut admin, &format!("{p}user:"), 50).await;

    let mut h = Harness::new(server.client(0), false).await;
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_page_size(100, 0);
        ks.set_pattern(Some(format!("{p}user:*")));
    }
    h.send(RedisEvent::FetchKeys);
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.state
        .keyspace_state
        .lock()
        .set_pattern(Some(format!("{p}order:*")));
    h.send(RedisEvent::FetchKeys);

    let is_reply = |a: &Action| {
        matches!(
            a,
            Action::LoadKeysIntoKeySpace | Action::KeysLoadAborted | Action::Error(_)
        )
    };
    let first = h.expect("first reply", is_reply).await;
    let second = h.expect("second reply", is_reply).await;
    assert!(
        matches!(
            first,
            Action::KeysLoadAborted | Action::LoadKeysIntoKeySpace
        ),
        "first reply: {first:?}"
    );
    assert_eq!(second, Action::LoadKeysIntoKeySpace);
    if first == Action::KeysLoadAborted {
        eprintln!("stale user:* fetch was aborted");
    }

    let shown: Vec<String> = h.state.keys.lock().iter().map(|k| k.key.clone()).collect();
    assert_eq!(shown.len(), 100);
    assert!(
        shown.iter().all(|k| k.starts_with(&format!("{p}order:"))),
        "a stale key is shown: {shown:?}"
    );
    h.shutdown().await;
}

/// Keys deleted after the user paged deep: the rescan lands on the last
/// page that still has keys.
#[tokio::test]
#[ignore = "needs Docker"]
async fn deleted_keys_clamp_the_page() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("clamp");
    let keys = support::seed_strings(&mut admin, &p, 250).await;

    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().set_page_size(100, 0);
    h.fetch_page().await;
    for page in 1..=2 {
        assert!(h.state.keyspace_state.lock().next_page());
        let shown = h.fetch_page().await;
        assert_eq!(h.state.keyspace_state.lock().page, page);
        if page == 2 {
            assert_eq!(shown.len(), 50);
        }
    }

    let (gone, kept) = keys.split_at(200);
    let deleted: usize = admin.del(gone).await.expect("DEL");
    assert_eq!(deleted, 200);
    h.state.keyspace_state.lock().rescan();
    let shown = h.fetch_page().await;

    assert_eq!(h.state.keyspace_state.lock().page, 0, "page clamped");
    let shown: HashSet<String> = shown.into_iter().collect();
    assert_eq!(shown, kept.iter().cloned().collect::<HashSet<_>>());
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn db_switch_survives_a_reconnect() {
    let server = support::start_redis().await;
    let p = support::prefix("db");
    support::seed_strings(&mut server.admin(0).await, &format!("{p}db0:"), 40).await;
    let db3: HashSet<String> =
        support::seed_strings(&mut server.admin(3).await, &format!("{p}db3:"), 25)
            .await
            .into_iter()
            .collect();

    let mut h = Harness::new(server.client(0), false).await;
    h.send(RedisEvent::SelectDb(3));
    h.expect("DbSwitched", |a| *a == Action::DbSwitched).await;
    assert_eq!(*h.state.db.lock(), 3);
    let shown: HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert_eq!(shown, db3, "after SelectDb(3)");

    // Drop every normal connection, the runner's included. The connection
    // manager reconnects; it must come back to DB 3, not DB 0.
    let mut admin = server.admin(0).await;
    assert!(
        client_kill(&mut admin, "normal").await >= 1,
        "nothing was killed"
    );
    let mut reloaded = false;
    for _ in 0..20 {
        h.send(RedisEvent::FetchKeys);
        let action = h
            .expect("keys or a failure", |a| {
                matches!(
                    a,
                    Action::LoadKeysIntoKeySpace | Action::KeysLoadAborted | Action::Error(_)
                )
            })
            .await;
        if action == Action::LoadKeysIntoKeySpace {
            reloaded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(reloaded, "the runner never reconnected");
    let shown: HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert_eq!(shown, db3, "after the reconnect");
    assert_eq!(*h.state.db.lock(), 3);
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn read_only_rejects_every_write() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("ro");
    let seeded = support::seed(&mut admin, &p, 2).await;
    let mut keys: Vec<String> = seeded.all().into_iter().collect();
    keys.sort();
    let before = support::fingerprint(&mut admin, &keys).await;

    let mut h = Harness::new(server.client(0), true).await;
    let (string, hash, list, set, zset) = (
        seeded.strings[0].clone(),
        seeded.hashes[0].clone(),
        seeded.lists[0].clone(),
        seeded.sets[0].clone(),
        seeded.zsets[0].clone(),
    );
    let writes = [
        RedisEvent::SetString {
            key: string.clone(),
            value: "changed".into(),
        },
        RedisEvent::DeleteKey {
            key: string.clone(),
        },
        RedisEvent::SetTtl {
            key: hash.clone(),
            secs: 5,
        },
        RedisEvent::CreateKey {
            key: format!("{p}new"),
            spec: NewKeySpec::String("new".into()),
        },
        RedisEvent::AddItem {
            key: list.clone(),
            item: KeyItem::ListValue("d".into()),
        },
        RedisEvent::EditItem {
            key: zset.clone(),
            edit: ItemEdit::ZsetScore {
                member: "a".into(),
                score: 42.0,
            },
        },
        RedisEvent::DeleteItem {
            key: set.clone(),
            delete: ItemDelete::SetMember("a".into()),
        },
        RedisEvent::Publish {
            channel: format!("{p}chan"),
            payload: "hello".into(),
        },
    ];
    for event in writes {
        assert!(event.is_write(), "{event:?} must count as a write");
        let name = format!("{event:?}");
        h.send(event);
        // The first answer must be the rejection, not a success.
        let action = h
            .expect("the rejection", |a| {
                matches!(
                    a,
                    Action::Error(_)
                        | Action::RefreshSpace
                        | Action::RequestSelectedValue
                        | Action::Published { .. }
                )
            })
            .await;
        match action {
            Action::Error(msg) => assert!(msg.contains("Read-only"), "{name}: {msg}"),
            other => panic!("{name} was not rejected: got {other:?}"),
        }
    }

    let late = h.drain(Duration::from_millis(500)).await;
    assert!(
        !late.iter().any(|a| matches!(
            a,
            Action::RefreshSpace | Action::RequestSelectedValue | Action::Published { .. }
        )),
        "a write went through: {late:?}"
    );
    assert_eq!(
        support::fingerprint(&mut admin, &keys).await,
        before,
        "data changed"
    );
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)] // one block per type, kept side by side
async fn value_edits_round_trip() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let seeded = support::seed(&mut admin, &support::prefix("edit"), 1).await;
    let mut h = Harness::new(server.client(0), false).await;

    // STRING: edited with SetString (`EditItem` covers collections only).
    let key = &seeded.strings[0];
    let ok = write(
        &mut h,
        RedisEvent::SetString {
            key: key.clone(),
            value: "edited".into(),
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "SetString: {ok:?}");
    match load_value(&mut h, key, RedisType::String).await {
        KeyValue::String { value, .. } => assert_eq!(value, "edited"),
        other => panic!("string: {other:?}"),
    }

    // LIST: `LSET 1 B`.
    let key = &seeded.lists[0];
    let edit = ItemEdit::ListSet {
        index: 1,
        value: "B".into(),
    };
    let ok = write(
        &mut h,
        RedisEvent::EditItem {
            key: key.clone(),
            edit,
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "list: {ok:?}");
    match load_value(&mut h, key, RedisType::List).await {
        KeyValue::List(items) => assert_eq!(items, ["a", "B", "c"]),
        other => panic!("list: {other:?}"),
    }

    // SET: replace `b` with `z`.
    let key = &seeded.sets[0];
    let edit = ItemEdit::SetReplace {
        old: "b".into(),
        new: "z".into(),
    };
    let ok = write(
        &mut h,
        RedisEvent::EditItem {
            key: key.clone(),
            edit,
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "set: {ok:?}");
    match load_value(&mut h, key, RedisType::Set).await {
        KeyValue::Set(members) => {
            let members: HashSet<String> = members.into_iter().collect();
            let want: HashSet<String> = ["a", "c", "z"].map(String::from).into();
            assert_eq!(members, want);
        }
        other => panic!("set: {other:?}"),
    }

    // HASH: `f2 = changed`.
    let key = &seeded.hashes[0];
    let edit = ItemEdit::HashSet {
        field: "f2".into(),
        value: "changed".into(),
    };
    let ok = write(
        &mut h,
        RedisEvent::EditItem {
            key: key.clone(),
            edit,
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "hash: {ok:?}");
    match load_value(&mut h, key, RedisType::Hash).await {
        KeyValue::Hash(fields) => {
            assert!(
                fields.contains(&("f2".into(), "changed".into())),
                "{fields:?}"
            );
            assert_eq!(fields.len(), 3);
        }
        other => panic!("hash: {other:?}"),
    }

    // ZSET: score of `a` becomes 9.5.
    let key = &seeded.zsets[0];
    let edit = ItemEdit::ZsetScore {
        member: "a".into(),
        score: 9.5,
    };
    let ok = write(
        &mut h,
        RedisEvent::EditItem {
            key: key.clone(),
            edit,
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "zset: {ok:?}");
    match load_value(&mut h, key, RedisType::Zset).await {
        KeyValue::Zset(members) => {
            let a = members.iter().find(|(m, _)| m == "a").map(|(_, s)| *s);
            assert_eq!(a, Some(9.5), "{members:?}");
        }
        other => panic!("zset: {other:?}"),
    }
    // JSON and stream values have no edit yet, so there's nothing to round-trip.
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn pubsub_reconnects_and_resubscribes() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let channel = format!("{}events", support::prefix("pubsub"));
    let target = Target::Channel(channel.clone());

    let h = Harness::new(server.client(0), false).await;
    // As `App` does: add to the store, then tell the Pub/Sub task.
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target));
    let store = h.state.pubsub.clone();
    let has_payload = |text: &str| {
        let guard = store.lock();
        let buffer = &guard.subs[0].buffer;
        let found = buffer
            .range(0, buffer.len())
            .any(|m| m.kind == EntryKind::Message && m.payload.copy_text() == text);
        found
    };

    // Publish until the first message lands (the SUBSCRIBE may still be in
    // flight when the first PUBLISH goes out).
    let deadline = tokio::time::Instant::now() + WAIT;
    while !has_payload("before") {
        assert!(tokio::time::Instant::now() < deadline, "never subscribed");
        let _: i64 = admin.publish(&channel, "before").await.expect("PUBLISH");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(store.lock().status, ConnStatus::Connected);

    assert!(
        client_kill(&mut admin, "pubsub").await >= 1,
        "subscriber not killed"
    );

    // The task waits 1 s, reconnects and resubscribes. Keep publishing until
    // a message gets through.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !has_payload("after") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no message after the kill"
        );
        let _: i64 = admin.publish(&channel, "after").await.expect("PUBLISH");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eventually("status back to Connected", WAIT, || {
        store.lock().status == ConnStatus::Connected
    })
    .await;

    let (marker, after) = {
        let guard = store.lock();
        let buffer = &guard.subs[0].buffer;
        let rows: Vec<_> = buffer.range(0, buffer.len()).collect();
        let marker = rows.iter().position(|m| m.kind == EntryKind::Reconnected);
        let after = rows.iter().position(|m| m.payload.copy_text() == "after");
        (marker, after)
    };
    let marker = marker.expect("a reconnect marker row");
    let after = after.expect("the message sent after the kill");
    assert!(marker < after, "the marker comes before the new message");
    h.shutdown().await;
}

/// A CA and a server certificate for `localhost` / `127.0.0.1` signed by it.
struct TestCerts {
    ca: String,
    server_cert: String,
    server_key: String,
}

fn test_certs() -> TestCerts {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "rrover test CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca.self_signed(&ca_key).expect("CA cert");
    let issuer = Issuer::new(ca, ca_key);

    let server_key = KeyPair::generate().expect("server key");
    let mut server = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
        .expect("server params");
    server
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server.signed_by(&server_key, &issuer).expect("server cert");

    TestCerts {
        ca: ca_cert.pem(),
        server_cert: server_cert.pem(),
        server_key: server_key.serialize_pem(),
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn tls_needs_the_ca_or_insecure() {
    use testcontainers::{
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };

    support::init();
    let certs = test_certs();
    let container = GenericImage::new("redis", support::REDIS_TAG)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_copy_to("/tls/ca.crt", certs.ca.clone().into_bytes())
        .with_copy_to("/tls/server.crt", certs.server_cert.into_bytes())
        .with_copy_to("/tls/server.key", certs.server_key.into_bytes())
        .with_cmd([
            "redis-server",
            "--port",
            "0",
            "--tls-port",
            "6379",
            "--tls-cert-file",
            "/tls/server.crt",
            "--tls-key-file",
            "/tls/server.key",
            "--tls-ca-cert-file",
            "/tls/ca.crt",
            "--tls-auth-clients",
            "no",
        ])
        .start()
        .await
        .expect("start TLS redis container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(6379).await.expect("port");
    let url = format!("rediss://{host}:{port}");

    let dir = std::env::temp_dir().join(format!("rrover-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let ca_path = dir.join("ca.pem");
    std::fs::write(&ca_path, &certs.ca).expect("write CA");
    let ca_arg = ca_path.to_string_lossy().into_owned();

    let client_for = |extra: &[&str]| {
        let mut args = vec!["rrover", "--url", url.as_str()];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args)
            .expect("valid flags")
            .client()
            .expect("client from flags")
    };

    // `--tls-ca`: verified against our CA, and the runner works over it.
    let mut h = Harness::new(client_for(&["--tls-ca", &ca_arg]), false).await;
    assert!(h.fetch_page().await.is_empty(), "empty server");
    h.shutdown().await;

    // No CA: the bundled roots don't know our CA, so the handshake fails with
    // an error that says why.
    let err = match tokio::time::timeout(
        Duration::from_mins(1),
        Harness::try_new(client_for(&[]), false),
    )
    .await
    {
        Ok(Err(err)) => err.to_string(),
        Ok(Ok(h)) => {
            h.shutdown().await;
            panic!("connected without the CA");
        }
        Err(elapsed) => panic!("connecting without the CA hung: {elapsed}"),
    };
    eprintln!("without the CA: {err}");
    assert!(
        err.to_lowercase().contains("certificate"),
        "error doesn't explain the failure: {err}"
    );

    // `--insecure`: no verification, connects.
    let mut h = Harness::new(client_for(&["--insecure"]), false).await;
    assert!(h.fetch_page().await.is_empty(), "empty server");
    h.shutdown().await;

    let _ = std::fs::remove_dir_all(&dir);
}
