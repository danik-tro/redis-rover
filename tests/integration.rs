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

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashSet,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use clap::Parser;
use redis::AsyncCommands;
use redis_rover::{
    internals::{
        Action, ConnStatus, EntryKind, ItemDelete, ItemEdit, JsonPreview, JsonPreviewKind,
        KeyChange, KeyItem, KeyValue, LiveCmd, Liveness, NewKeySpec, PageRow, PubSubCmd,
        RedisEvent, RedisType, Sort, SortKey, Target, TypeFilter,
    },
    Cli,
};
use support::{Harness, ValueDelay, WAIT};

/// Counts live heap bytes and their peak, so AC3 can bound the memory a JSON
/// preview takes (the heap part of RSS, measured the same on every OS).
struct CountingAlloc;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; only counters change.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Start measuring: the peak restarts from the live heap, which is returned.
fn reset_peak() -> usize {
    let live = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(live, Ordering::Relaxed);
    live
}

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

    let shown: Vec<String> = h
        .state
        .keys
        .lock()
        .as_ref()
        .expect("a page was loaded")
        .iter()
        .map(|k| k.key.clone())
        .collect();
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

/// AC3: on a 1M-key DB, holding `n` to page 1,000 shows every page fast.
/// Each `FetchKeys` reply (SCAN + metadata + merge) must land within 100 ms.
#[tokio::test]
#[ignore = "needs Docker"]
async fn deep_paging_stays_fast() {
    const KEYS: usize = 1_000_000;
    const PAGES: usize = 1_000;
    const PAGE_SIZE: usize = 30;
    const LIMIT: Duration = Duration::from_millis(100);

    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("deep");
    // 1,000 MSETs of 1,000 keys, sent 50 at a time.
    for chunk in (0..KEYS).collect::<Vec<_>>().chunks(50_000) {
        let mut pipe = redis::pipe();
        for keys in chunk.chunks(1_000) {
            let mut mset = redis::cmd("MSET");
            for i in keys {
                mset.arg(format!("{p}user:{}:{i}", i / 1000)).arg("v");
            }
            pipe.add_command(mset).ignore();
        }
        let () = pipe.query_async(&mut admin).await.expect("seed MSET");
    }
    let size: usize = redis::cmd("DBSIZE")
        .query_async(&mut admin)
        .await
        .expect("DBSIZE");
    assert_eq!(size, KEYS);

    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().set_page_size(PAGE_SIZE, 0);
    let mut times = Vec::with_capacity(PAGES);
    let mut shown = HashSet::new();
    for page in 0..PAGES {
        if page > 0 {
            assert!(h.state.keyspace_state.lock().next_page(), "page {page}");
        }
        let started = std::time::Instant::now();
        let keys = h.fetch_page().await;
        times.push(started.elapsed());
        assert_eq!(keys.len(), PAGE_SIZE, "page {page} is full");
        shown.extend(keys);
    }
    assert_eq!(shown.len(), PAGES * PAGE_SIZE, "a key was shown twice");

    let swept = h.state.keyspace_state.lock().scanned.len();
    times.sort();
    let (p50, p99, max) = (times[PAGES / 2], times[PAGES * 99 / 100], times[PAGES - 1]);
    eprintln!(
        "deep paging over {KEYS} keys, {PAGES} pages of {PAGE_SIZE}: p50 {p50:?}, \
         p99 {p99:?}, max {max:?}; sweep holds {swept} keys"
    );
    assert!(max < LIMIT, "slowest page took {max:?}");
    h.shutdown().await;
}

/// `TYPE` calls the server has run since the last `CONFIG RESETSTAT`.
async fn type_calls(admin: &mut redis::aio::ConnectionManager) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(admin)
        .await
        .expect("INFO commandstats");
    info.lines()
        .find_map(|line| line.strip_prefix("cmdstat_type:calls="))
        .and_then(|rest| rest.split(',').next())
        .map_or(0, |calls| calls.parse().expect("calls is a number"))
}

/// Pick hashes in the type filter and page through everything: only the
/// seeded hashes are listed. `server_side`: the server filtered (`SCAN …
/// TYPE`), so `TYPE` ran only for the listed rows' metadata, not for every
/// scanned key.
async fn hashes_only(server: &support::Server, server_side: bool) {
    let p = support::prefix("types");
    let mut admin = server.admin(0).await;
    let seeded = support::seed(&mut admin, &p, 20).await;
    let hashes: HashSet<String> = seeded.hashes.iter().cloned().collect();
    let mut h = Harness::new(server.client(0), false).await;
    h.state
        .keyspace_state
        .lock()
        .set_type_filter(TypeFilter::new(["hash"]));
    let () = redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async(&mut admin)
        .await
        .expect("CONFIG RESETSTAT");

    let shown: HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert_eq!(shown, hashes);
    let calls = type_calls(&mut admin).await;
    if server_side {
        assert_eq!(calls, 20, "TYPE only for the page's metadata");
    } else {
        assert!(calls > 100, "TYPE for every scanned key: {calls}");
    }

    // Several types are filtered on the client, on any version.
    h.state
        .keyspace_state
        .lock()
        .set_type_filter(TypeFilter::new(["hash", "zset"]));
    let shown: HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    let expected: HashSet<String> = seeded.hashes.iter().chain(&seeded.zsets).cloned().collect();
    assert_eq!(shown, expected);
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn type_filter_scans_by_type_on_redis_7() {
    hashes_only(&support::start_redis().await, true).await;
}

/// Redis 5 rejects `SCAN … TYPE`; the runner falls back to the client
/// filter and lists the same keys.
#[tokio::test]
#[ignore = "needs Docker"]
async fn type_filter_falls_back_to_the_client_on_redis_5() {
    hashes_only(&support::start_redis_tag("5-alpine").await, false).await;
}

/// The type filter stays after a rescan and a DB switch.
#[tokio::test]
#[ignore = "needs Docker"]
async fn type_filter_survives_rescan_and_db_switch() {
    let server = support::start_redis().await;
    let p = support::prefix("types-db");
    support::seed(&mut server.admin(0).await, &format!("{p}db0:"), 5).await;
    let db3 = support::seed(&mut server.admin(3).await, &format!("{p}db3:"), 7).await;

    let mut h = Harness::new(server.client(0), false).await;
    h.state
        .keyspace_state
        .lock()
        .set_type_filter(TypeFilter::new(["zset"]));
    assert_eq!(h.fetch_all_pages().await.len(), 5, "db 0 zsets");
    h.send(RedisEvent::SelectDb(3));
    h.expect("DbSwitched", |a| *a == Action::DbSwitched).await;
    let shown: HashSet<String> = h.fetch_all_pages().await.into_iter().collect();
    assert_eq!(shown, db3.zsets.iter().cloned().collect(), "db 3 zsets");
    h.state.keyspace_state.lock().rescan();
    assert_eq!(h.fetch_all_pages().await.len(), 7, "after a rescan");
    h.shutdown().await;
}

/// The page as the key table shows it: `▸ user: (3)` for a group, the
/// name for a key.
fn tree_labels(h: &Harness) -> Vec<String> {
    let ks = h.state.keyspace_state.lock();
    let mut keys = ks.page_keys().into_iter();
    ks.page_rows()
        .expect("tree mode")
        .into_iter()
        .map(|row| match row {
            PageRow::Group { prefix, count, .. } => format!("▸ {prefix} ({count})"),
            PageRow::Leaf { .. } => keys.next().expect("a key per leaf"),
        })
        .collect()
}

/// The tree groups seeded prefixes, unfolds, and stays a tree (with its
/// open groups) after a rescan and a DB switch.
#[tokio::test]
#[ignore = "needs Docker"]
async fn tree_groups_seeded_prefixes() {
    let server = support::start_redis().await;
    let names = ["user:1:name", "user:1:email", "user:2:name", "order:9"];
    for db in [0, 3] {
        let mut admin = server.admin(db).await;
        for name in names {
            let () = admin.set(name, "v").await.expect("SET");
        }
    }
    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().toggle_tree(":");
    h.fetch_page().await;
    assert_eq!(tree_labels(&h), ["▸ order: (1)", "▸ user: (3)"]);
    let opened = h.state.keyspace_state.lock().set_group(1, Some(true));
    assert_eq!(opened.as_deref(), Some("user:"));
    h.fetch_page().await;
    let unfolded = [
        "▸ order: (1)",
        "▸ user: (3)",
        "▸ user:1: (2)",
        "user:2:name",
    ];
    assert_eq!(tree_labels(&h), unfolded);

    h.state.keyspace_state.lock().rescan();
    h.fetch_page().await;
    assert_eq!(tree_labels(&h), unfolded, "after a rescan");

    h.send(RedisEvent::SelectDb(3));
    h.expect("DbSwitched", |a| *a == Action::DbSwitched).await;
    h.fetch_page().await;
    assert_eq!(tree_labels(&h), unfolded, "after SelectDb(3)");
    h.shutdown().await;
}

/// A name sort pages through the loaded keys in name order, whether it was
/// set before the sweep (batches merged into the order) or after a large
/// sweep (re-sorted off the keyspace lock).
#[tokio::test]
#[ignore = "needs Docker"]
async fn sorted_pages_list_names_in_order() {
    const KEYS: usize = 60_000;
    const PAGE: usize = 1_000;
    let by_name = Sort {
        key: SortKey::Name,
        descending: false,
    };
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("sorted");
    let mut pipe = redis::pipe();
    for keys in (0..KEYS).collect::<Vec<_>>().chunks(1_000) {
        let mut mset = redis::cmd("MSET");
        for i in keys {
            // Not in name order: `n:7` before `n:10`.
            mset.arg(format!("{p}n:{}", (i * 7_919) % KEYS)).arg("v");
        }
        pipe.add_command(mset).ignore();
    }
    let () = pipe.query_async(&mut admin).await.expect("seed MSET");
    let mut expected: Vec<String> = (0..KEYS).map(|i| format!("{p}n:{i}")).collect();
    expected.sort();

    let mut h = Harness::new(server.client(0), false).await;
    // Sorted from the start: every SCAN batch is merged into the order.
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_page_size(PAGE, 0);
        ks.set_sort(Some(by_name));
    }
    loop {
        h.fetch_page().await;
        let mut ks = h.state.keyspace_state.lock();
        if ks.sweep_done || !ks.next_page() {
            break;
        }
    }
    h.state.keyspace_state.lock().page = 0;
    assert_eq!(h.fetch_page().await, expected[..PAGE]);

    // Sorted after the sweep: 60k keys are above the inline limit.
    {
        let mut ks = h.state.keyspace_state.lock();
        assert!(ks.sweep_done);
        ks.set_sort(Some(Sort {
            descending: true,
            ..by_name
        }));
    }
    let mut descending = expected.clone();
    descending.reverse();
    assert_eq!(h.fetch_page().await, descending[..PAGE]);
    h.state.keyspace_state.lock().page = KEYS / PAGE - 1;
    assert_eq!(h.fetch_page().await, descending[KEYS - PAGE..]);
    h.shutdown().await;
}

/// Size ▼ over a loaded sweep: the backfill fetches every size (in batches
/// of 1,000 on its own connection) and the largest keys end up first.
#[tokio::test]
#[ignore = "needs Docker"]
async fn size_sort_backfills_every_key() {
    const FILLERS: usize = 3_000;
    // Doubling sizes, so allocator size classes can't make two equal.
    const BIG: usize = 8;
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("sized");
    support::seed_strings(&mut admin, &format!("{p}small:"), FILLERS).await;
    let mut pipe = redis::pipe();
    let mut big: Vec<String> = Vec::new();
    for i in 0..BIG {
        let key = format!("{p}big:{i:02}");
        pipe.set(&key, "x".repeat(1_000 << i)).ignore();
        big.push(key);
    }
    let () = pipe.query_async(&mut admin).await.expect("seed big keys");
    big.reverse(); // largest first

    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().set_page_size(500, 0);
    loop {
        h.fetch_page().await;
        let mut ks = h.state.keyspace_state.lock();
        if ks.sweep_done || !ks.next_page() {
            break;
        }
    }
    h.state.keyspace_state.lock().set_sort(Some(Sort {
        key: SortKey::Size,
        descending: true,
    }));
    h.state.keyspace_state.lock().set_page_size(BIG, 0);
    h.fetch_page().await;
    h.send(RedisEvent::BackfillMeta);
    let total = FILLERS + BIG;
    loop {
        h.expect("a backfilled batch", |a| *a == Action::KeyMetaBackfilled)
            .await;
        let progress = h.state.keyspace_state.lock().meta_progress();
        if progress == Some((total, total)) {
            break;
        }
    }
    let rows = h
        .state
        .keyspace_state
        .lock()
        .page_from_meta()
        .expect("the first page is known");
    let names: Vec<String> = rows.iter().map(|row| row.key.clone()).collect();
    assert_eq!(names, big);
    assert_eq!(h.fetch_page().await, big, "a fetched page agrees");

    // A rescan keeps the sort; the columns start over and fill again.
    h.state.keyspace_state.lock().rescan();
    h.fetch_page().await;
    assert_eq!(
        h.state
            .keyspace_state
            .lock()
            .meta_progress()
            .map(|(k, _)| k),
        Some(0)
    );
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

/// A DB switch that fails (no such DB) reloads the old DB's page: the App
/// moved the last page out of `state.keys`, so there's nothing to put back.
#[tokio::test]
#[ignore = "needs Docker"]
async fn failed_db_switch_reloads_the_old_page() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("baddb");
    support::seed_strings(&mut admin, &p, 30).await;

    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().set_page_size(100, 0);
    let before = h.fetch_page().await;
    assert_eq!(before.len(), 30);
    // What App::load_new_keys does with the page.
    assert!(h.state.keys.lock().take().is_some());

    h.send(RedisEvent::SelectDb(999));
    let error = h
        .expect("the switch error", |a| matches!(a, Action::Error(_)))
        .await;
    assert!(
        matches!(&error, Action::Error(msg) if msg.contains("db 999")),
        "{error:?}"
    );
    h.expect("the old page again", |a| {
        matches!(a, Action::LoadKeysIntoKeySpace)
    })
    .await;

    assert_eq!(*h.state.db.lock(), 0);
    let after: Vec<String> = h
        .state
        .keys
        .lock()
        .as_ref()
        .expect("the old page was reloaded")
        .iter()
        .map(|k| k.key.clone())
        .collect();
    assert_eq!(after, before);
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

/// The in-session toggle (`R`) moves the runner's guarantee: flipping the
/// shared flag refuses, then allows, the same write.
#[tokio::test]
#[ignore = "needs Docker"]
async fn read_only_toggle_reaches_the_runner() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let key = format!("{}k", support::prefix("rotoggle"));
    let _: () = admin.set(&key, "before").await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;
    let flag = h.read_only_flag();
    let write = || RedisEvent::SetString {
        key: key.clone(),
        value: "after".into(),
    };
    let answer = |a: &Action| matches!(a, Action::Error(_) | Action::RefreshSpace);

    flag.store(true, std::sync::atomic::Ordering::Release);
    h.send(write());
    match h.expect("the rejection", answer).await {
        Action::Error(msg) => assert!(msg.contains("Read-only"), "{msg}"),
        other => panic!("write went through while read-only: {other:?}"),
    }
    let value: String = admin.get(&key).await.expect("get");
    assert_eq!(value, "before");

    flag.store(false, std::sync::atomic::Ordering::Release);
    h.send(write());
    let action = h.expect("the write", answer).await;
    assert!(matches!(action, Action::RefreshSpace), "{action:?}");
    let value: String = admin.get(&key).await.expect("get");
    assert_eq!(value, "after");
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

/// AC1 (#48): a value that takes 3 s to load doesn't hold back a keys page
/// turn sent after it.
#[tokio::test]
#[ignore = "needs Docker"]
async fn slow_value_fetch_does_not_block_paging() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let keys = support::seed_strings(&mut admin, &support::prefix("slow"), 30).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.state.keyspace_state.lock().set_page_size(10, 0);
    assert_eq!(h.fetch_page().await.len(), 10);

    let _delay = ValueDelay::set(3000);
    h.state
        .value_state
        .lock()
        .select(&keys[0], RedisType::String);
    h.send(RedisEvent::FetchValue);
    let started = Instant::now();
    assert!(h.state.keyspace_state.lock().next_page());
    let page = h.fetch_page().await;
    let paged_in = started.elapsed();
    assert_eq!(page.len(), 10);
    assert!(
        paged_in < Duration::from_secs(1),
        "page turn waited for the value: {paged_in:?}"
    );

    h.expect("the slow value", |a| {
        *a == Action::LoadSelectedValueIntoView
    })
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(2900),
        "the value can't load before its delay"
    );
    h.shutdown().await;
}

/// AC2 (#48): moving through 20 keys quickly shows only the last one's value;
/// the earlier fetches are aborted and never land.
#[tokio::test]
#[ignore = "needs Docker"]
async fn fast_value_moves_show_only_the_last() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let prefix = support::prefix("moves");
    let keys: Vec<String> = (0..20).map(|i| format!("{prefix}{i}")).collect();
    let mut pipe = redis::pipe();
    for key in &keys {
        pipe.set(key, format!("value of {key}")).ignore();
    }
    let () = pipe.query_async(&mut admin).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;

    // Each fetch waits longer than the gap between two moves, so they overlap.
    let _delay = ValueDelay::set(100);
    for key in &keys {
        h.state.value_state.lock().select(key, RedisType::String);
        h.send(RedisEvent::FetchValue);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let actions = h.drain(Duration::from_secs(1)).await;
    let loads = actions
        .iter()
        .filter(|a| **a == Action::LoadSelectedValueIntoView)
        .count();
    assert_eq!(loads, 1, "only the last fetch lands: {actions:?}");
    let last = keys.last().expect("20 keys");
    match h.state.selected_value.lock().as_ref().map(|l| &l.value) {
        Some(KeyValue::String { value, .. }) => assert_eq!(value, &format!("value of {last}")),
        other => panic!("expected the last key's value, got {other:?}"),
    }
    h.shutdown().await;
}

/// Select the JSON `key` (`size` as `MEMORY USAGE` reported it, `cap` as
/// `keyspace.json_preview_bytes`) and load what the pane shows.
async fn load_json(h: &mut Harness, key: &str, size: Option<u64>, cap: usize) -> KeyValue {
    {
        let mut value_state = h.state.value_state.lock();
        value_state.select(key, RedisType::Json);
        value_state.size = size;
        value_state.json_cap = cap;
    }
    fetch_shown_value(h).await
}

/// `FetchValue` for the current `value_state`; the value once it lands.
async fn fetch_shown_value(h: &mut Harness) -> KeyValue {
    h.send(RedisEvent::FetchValue);
    let action = h
        .expect("the value or a failure", |a| {
            matches!(
                a,
                Action::LoadSelectedValueIntoView | Action::ValueLoadFailed | Action::Error(_)
            )
        })
        .await;
    assert_eq!(action, Action::LoadSelectedValueIntoView, "FetchValue");
    h.state
        .selected_value
        .lock()
        .as_ref()
        .map(|loaded| loaded.value.clone())
        .expect("a loaded value")
}

async fn json_set(con: &mut redis::aio::ConnectionManager, key: &str, path: &str, json: &str) {
    let () = redis::cmd("JSON.SET")
        .arg(key)
        .arg(path)
        .arg(json)
        .query_async(con)
        .await
        .expect("JSON.SET");
}

fn object_fields(value: &KeyValue) -> (usize, Vec<(String, String)>) {
    match value {
        KeyValue::JsonPreview(JsonPreview {
            kind: JsonPreviewKind::Object { len, fields },
            ..
        }) => (
            *len,
            fields
                .iter()
                .map(|f| (f.name.clone(), f.json_type.clone()))
                .collect(),
        ),
        other => panic!("expected an object preview, got {other:?}"),
    }
}

/// #49 / AC4: under the cap a document is read whole; above it the pane
/// shows its fields (paged), Enter opens one (the cap applies again), an
/// array shows its first 100 items, a scalar only its size.
#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)] // one scenario, step by step
async fn json_preview_pages_and_opens_fields() {
    let server = support::start_redis_stack().await;
    let mut admin = server.admin(0).await;
    let prefix = support::prefix("json");
    let doc = format!("{prefix}doc");
    let mut fields = serde_json::Map::new();
    fields.insert("a".into(), serde_json::json!({ "x": 1, "y": [true, null] }));
    fields.insert("b".into(), serde_json::json!((0..300).collect::<Vec<_>>()));
    fields.insert("we\"ird key".into(), serde_json::json!("quoted"));
    for i in 0..147 {
        fields.insert(format!("f{i:03}"), serde_json::json!(i));
    }
    json_set(
        &mut admin,
        &doc,
        "$",
        &serde_json::Value::Object(fields).to_string(),
    )
    .await;
    let small = format!("{prefix}small");
    json_set(&mut admin, &small, "$", r#"{"k":"v"}"#).await;
    let scalar = format!("{prefix}scalar");
    json_set(
        &mut admin,
        &scalar,
        "$",
        &format!("\"{}\"", "s".repeat(4096)),
    )
    .await;
    let mut h = Harness::new(server.client(0), false).await;

    // Under the cap: the whole document.
    match load_json(&mut h, &small, None, 1 << 20).await {
        KeyValue::Json(json) => assert_eq!(json, serde_json::json!({ "k": "v" })),
        other => panic!("small doc: {other:?}"),
    }

    // Above a 1 KiB cap, size from JSON.DEBUG MEMORY: page 1 of the fields.
    let cap = 1024;
    let preview = load_json(&mut h, &doc, None, cap).await;
    let (len, page1) = object_fields(&preview);
    assert_eq!(len, 150);
    assert_eq!(page1.len(), 100);
    assert_eq!(page1[0], ("a".into(), "object".into()));
    assert_eq!(page1[1], ("b".into(), "array".into()));
    assert_eq!(page1[2], ("we\"ird key".into(), "string".into()));
    assert_eq!(page1[3].1, "integer");

    // n: page 2 reuses the field names (no second OBJKEYS needed).
    assert!(h.state.value_state.lock().next_page());
    let (_, page2) = object_fields(&fetch_shown_value(&mut h).await);
    assert_eq!(page2.len(), 50);
    assert!(page2
        .iter()
        .all(|(name, _)| !page1.iter().any(|(n, _)| n == name)));

    // Enter on `a` (under the cap): its JSON. A quoted name works too.
    h.state.value_state.lock().page = 0;
    h.state.value_state.lock().open_json_field("a".into());
    match fetch_shown_value(&mut h).await {
        KeyValue::Json(json) => assert_eq!(json, serde_json::json!({ "x": 1, "y": [true, null] })),
        other => panic!("field a: {other:?}"),
    }
    h.state.value_state.lock().close_json_field();
    h.state
        .value_state
        .lock()
        .open_json_field("we\"ird key".into());
    match fetch_shown_value(&mut h).await {
        KeyValue::Json(json) => assert_eq!(json, serde_json::json!("quoted")),
        other => panic!("quoted field: {other:?}"),
    }
    h.state.value_state.lock().close_json_field();

    // Enter on `b` (above the cap again): an array preview, 100 of 300.
    h.state.value_state.lock().open_json_field("b".into());
    match fetch_shown_value(&mut h).await {
        KeyValue::JsonPreview(JsonPreview {
            kind: JsonPreviewKind::Array { len, items },
            ..
        }) => {
            assert_eq!(len, 300);
            assert_eq!(items, serde_json::json!((0..100).collect::<Vec<_>>()));
        }
        other => panic!("field b: {other:?}"),
    }

    // A scalar above the cap: only its type and size.
    match load_json(&mut h, &scalar, None, cap).await {
        KeyValue::JsonPreview(JsonPreview {
            bytes,
            kind: JsonPreviewKind::TooLarge { json_type },
        }) => {
            assert_eq!(json_type.as_deref(), Some("string"));
            assert!(bytes > 4096, "size {bytes}");
        }
        other => panic!("scalar: {other:?}"),
    }
    h.shutdown().await;
}

/// AC3 (#49): selecting a 50 MB `RedisJSON` document shows its preview in
/// under 500 ms, and the heap grows by less than 20 MB (the whole document
/// is never read).
#[tokio::test]
#[ignore = "needs Docker"]
async fn json_50mb_preview_is_fast_and_small() {
    let server = support::start_redis_stack().await;
    let mut admin = server.admin(0).await;
    let key = format!("{}big", support::prefix("json50"));
    json_set(&mut admin, &key, "$", "{}").await;
    let chunk = format!("\"{}\"", "x".repeat(1000));
    for batch in 0..50 {
        let mut pipe = redis::pipe();
        for i in 0..1000 {
            pipe.cmd("JSON.SET")
                .arg(&key)
                .arg(format!("$.f{batch:02}_{i:03}"))
                .arg(&chunk)
                .ignore();
        }
        let () = pipe.query_async(&mut admin).await.expect("seed 1 MB");
    }
    let size: u64 = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg(&key)
        .query_async(&mut admin)
        .await
        .expect("MEMORY USAGE");
    assert!(size >= 50_000_000, "seeded only {size} bytes");
    let mut h = Harness::new(server.client(0), false).await;

    // As the app does it: size from the key's MEMORY USAGE, default cap.
    for (size, how) in [(Some(size), "MEMORY USAGE"), (None, "JSON.DEBUG MEMORY")] {
        let before = reset_peak();
        let started = Instant::now();
        let value = load_json(&mut h, &key, size, 1 << 20).await;
        let took = started.elapsed();
        let grown = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);

        let (len, fields) = object_fields(&value);
        assert_eq!(len, 50_000, "{how}");
        assert_eq!(fields.len(), 100, "{how}");
        assert!(
            took < Duration::from_millis(500),
            "{how}: preview took {took:?}"
        );
        assert!(
            grown < 20 << 20,
            "{how}: heap grew by {grown} bytes during the preview"
        );
        println!("{how}: preview in {took:?}, heap peak +{} KiB", grown >> 10);
    }
    h.shutdown().await;
}

// ---- --probe ----------------------------------------------------------------

#[tokio::test]
#[ignore = "needs Docker"]
async fn probe_reports_resp3_on_redis_7() {
    let server = support::start_redis().await;
    let report = redis_rover::internals::probe_report(&server.client(0), "test")
        .await
        .expect("probe");
    eprintln!("{report}");
    assert!(report.contains("PING      PONG"), "{report}");
    assert!(report.contains("server    Redis 7."), "{report}");
    assert!(report.contains("role      master"), "{report}");
    assert!(report.contains("cluster   no"), "{report}");
    assert!(report.contains("RESP3     yes"), "{report}");
    assert!(report.contains("live      tracking"), "{report}");
}

/// A pre-6.0 server has no `HELLO`: the probe says so and still succeeds.
#[tokio::test]
#[ignore = "needs Docker"]
async fn probe_reports_no_resp3_on_redis_5() {
    let server = support::start_redis_tag("5-alpine").await;
    let report = redis_rover::internals::probe_report(&server.client(0), "test")
        .await
        .expect("probe succeeds on Redis 5");
    eprintln!("{report}");
    assert!(report.contains("server    Redis 5."), "{report}");
    assert!(report.contains("RESP3     no"), "{report}");
    assert!(
        report.contains("live      polling 2s (no RESP3)"),
        "{report}"
    );
}

/// The keyspace's key names as a set, asserting the sweep holds no
/// duplicates.
fn swept_set(h: &Harness) -> HashSet<String> {
    let state = h.state.keyspace_state.lock();
    let set: HashSet<String> = state.scanned.iter().cloned().collect();
    assert_eq!(set.len(), state.scanned.len(), "duplicates in the sweep");
    set
}

/// `LoadAll` sweeps to the end on its own connection while a page fetch
/// races it; every key is loaded once.
#[tokio::test]
#[ignore = "needs Docker"]
async fn load_all_reaches_the_end_of_the_sweep() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("load_all");
    let keys = support::seed_strings(&mut admin, &p, 20_000).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.fetch_page().await;

    h.send(RedisEvent::LoadAll { until: None });
    h.state.keyspace_state.lock().page = 300;
    h.send(RedisEvent::FetchKeys);
    let mut done = None;
    let mut page = false;
    while done.is_none() || !page {
        match h
            .expect("LoadAllDone and the page", |a| {
                matches!(
                    a,
                    Action::LoadAllDone { .. }
                        | Action::LoadKeysIntoKeySpace
                        | Action::KeysLoadAborted
                        | Action::Error(_)
                )
            })
            .await
        {
            Action::LoadAllDone { complete } => done = Some(complete),
            Action::LoadKeysIntoKeySpace => page = true,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(done, Some(true));
    assert!(h.state.keyspace_state.lock().sweep_done);
    let expected: HashSet<String> = keys.into_iter().collect();
    assert_eq!(swept_set(&h), expected);
    h.shutdown().await;
}

/// Stopping a load leaves a sweep that later page fetches and loads
/// continue without gaps or duplicates.
#[tokio::test]
#[ignore = "needs Docker"]
async fn stopped_load_all_leaves_a_consistent_sweep() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("load_stop");
    let keys = support::seed_strings(&mut admin, &p, 30_000).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.fetch_page().await;

    h.send(RedisEvent::LoadAll { until: None });
    h.send(RedisEvent::StopLoadAll);
    let after = h.drain(Duration::from_millis(300)).await;
    assert!(
        !after
            .iter()
            .any(|a| matches!(a, Action::LoadAllDone { .. })),
        "a stopped load sends no Done: {after:?}"
    );
    let partial = swept_set(&h);
    assert!(partial.len() < keys.len(), "stopped before the end");

    let loaded = partial.len();
    h.state.keyspace_state.lock().page = loaded / 10 + 2;
    h.fetch_page().await;
    h.send(RedisEvent::LoadAll { until: None });
    let done = h
        .expect("LoadAllDone", |a| matches!(a, Action::LoadAllDone { .. }))
        .await;
    assert_eq!(done, Action::LoadAllDone { complete: true });
    let expected: HashSet<String> = keys.into_iter().collect();
    assert_eq!(swept_set(&h), expected);
    h.shutdown().await;
}

/// `g o`'s search stops once the key is loaded, and ends `complete` for a
/// key that doesn't exist.
#[tokio::test]
#[ignore = "needs Docker"]
async fn load_until_a_key_stops_there() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("seek");
    let keys = support::seed_strings(&mut admin, &p, 20_000).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.fetch_page().await;

    let target = keys[12_345].clone();
    h.send(RedisEvent::LoadAll {
        until: Some(target.clone()),
    });
    let done = h
        .expect("LoadAllDone", |a| matches!(a, Action::LoadAllDone { .. }))
        .await;
    assert_eq!(done, Action::LoadAllDone { complete: false });
    assert!(h.state.keyspace_state.lock().index_of(&target).is_some());

    h.send(RedisEvent::LoadAll {
        until: Some(format!("{p}missing")),
    });
    let done = h
        .expect("LoadAllDone", |a| matches!(a, Action::LoadAllDone { .. }))
        .await;
    assert_eq!(done, Action::LoadAllDone { complete: true });
    assert_eq!(swept_set(&h).len(), keys.len());
    h.shutdown().await;
}

// --- Live updates of the open key -----------------------------------------

/// Open `key` the way `App` does: select it, load its value, track it. The
/// `first` key waits until the live task reports it armed (`Live`); a later
/// one, with the status already `Live`, gives the arming a moment.
async fn open_live(
    h: &mut Harness,
    key: &str,
    r_type: RedisType,
    db: i64,
    first: bool,
) -> KeyValue {
    h.state.value_state.lock().select(key, r_type);
    let value = fetch_shown_value(h).await;
    h.live(LiveCmd::Track {
        key: key.to_owned(),
        db,
    });
    if first {
        h.expect("live", |a| *a == Action::LiveStatus(Liveness::Live))
            .await;
    } else {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    value
}

/// Wait for `OpenKeyChanged` on `key`, then refetch in place as `App` does.
async fn follow_change(h: &mut Harness, key: &str) -> (KeyChange, KeyValue) {
    let action = h
        .expect(
            "OpenKeyChanged",
            |a| matches!(a, Action::OpenKeyChanged { key: k, .. } if k == key),
        )
        .await;
    let Action::OpenKeyChanged { change, .. } = action else {
        unreachable!()
    };
    h.state.value_state.lock().rescan();
    (change, fetch_shown_value(h).await)
}

fn hash_value(value: &KeyValue, field: &str) -> Option<String> {
    match value {
        KeyValue::Hash(entries) => entries
            .iter()
            .find(|(f, _)| f == field)
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

/// An `HSET` from another client shows up without a keypress, fast.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_tracks_open_hash() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let key = format!("{}user:1", support::prefix("live_tracks_open_hash"));
    let () = admin.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    let value = open_live(&mut h, &key, RedisType::Hash, 0, true).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Ann"));

    let start = Instant::now();
    let () = admin.hset(&key, "name", "Bob").await.unwrap();
    let (change, value) = follow_change(&mut h, &key).await;
    let took = start.elapsed();
    assert_eq!(
        change,
        KeyChange::Changed {
            r_type: "hash".into()
        }
    );
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Bob"));
    assert!(took < Duration::from_millis(200), "took {took:?}");
    h.shutdown().await;
}

/// Each invalidation re-arms the key: every change is seen, and a burst
/// ends on the last value.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_rearms_after_each_change() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let key = format!("{}user:1", support::prefix("live_rearms"));
    let () = admin.hset(&key, "n", 0).await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_live(&mut h, &key, RedisType::Hash, 0, true).await;

    for n in 1..=3 {
        let () = admin.hset(&key, "n", n).await.unwrap();
        let (_, value) = follow_change(&mut h, &key).await;
        assert_eq!(hash_value(&value, "n"), Some(n.to_string()), "change {n}");
    }

    // A burst: however the pushes group, the last refetch shows the end.
    for n in 4..=6 {
        let () = admin.hset(&key, "n", n).await.unwrap();
    }
    let deadline = Instant::now() + WAIT;
    loop {
        let (_, value) = follow_change(&mut h, &key).await;
        if hash_value(&value, "n").as_deref() == Some("6") {
            break;
        }
        assert!(Instant::now() < deadline, "never showed the last change");
    }
    h.shutdown().await;
}

/// Deleting the open key says so; recreating it is seen too.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_reports_deleted_and_expired() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let prefix = support::prefix("live_deleted");
    let key = format!("{prefix}user:1");
    let () = admin.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_live(&mut h, &key, RedisType::Hash, 0, true).await;

    let () = admin.del(&key).await.unwrap();
    let (change, _) = follow_change(&mut h, &key).await;
    assert_eq!(change, KeyChange::Deleted);

    let () = admin.set(&key, "back").await.unwrap();
    let action = h
        .expect("recreated", |a| matches!(a, Action::OpenKeyChanged { .. }))
        .await;
    assert_eq!(
        action,
        Action::OpenKeyChanged {
            key: key.clone(),
            change: KeyChange::Changed {
                r_type: "string".into()
            }
        },
        "tracked while gone"
    );

    let short = format!("{prefix}short");
    let () = admin.set_ex(&short, "v", 2).await.unwrap();
    open_live(&mut h, &short, RedisType::String, 0, false).await;
    let (change, _) = follow_change(&mut h, &short).await;
    assert_eq!(change, KeyChange::Expired);
    h.shutdown().await;
}

/// The id of the live task's connection, from `CLIENT LIST`.
async fn live_client_id(admin: &mut redis::aio::ConnectionManager) -> Option<i64> {
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(admin)
        .await
        .unwrap();
    list.lines().find_map(|line| {
        let mut id = None;
        let mut named = false;
        for field in line.split(' ') {
            match field.split_once('=') {
                Some(("id", v)) => id = v.parse().ok(),
                Some(("name", "rrover-live")) => named = true,
                _ => {}
            }
        }
        id.filter(|_| named)
    })
}

/// A lost connection says `reconnecting`, comes back live, and refetches
/// once: a change made in the gap is shown.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_reconnects_and_refetches() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let key = format!("{}user:1", support::prefix("live_reconnects"));
    let () = admin.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_live(&mut h, &key, RedisType::Hash, 0, true).await;

    let id = live_client_id(&mut admin)
        .await
        .expect("the live connection");
    let () = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(id)
        .query_async::<i64>(&mut admin)
        .await
        .map(|_| ())
        .unwrap();
    h.expect("reconnecting", |a| {
        *a == Action::LiveStatus(Liveness::NotLive("reconnecting".into()))
    })
    .await;
    // In the gap: no connection tracks the key.
    let () = admin.hset(&key, "name", "Gap").await.unwrap();
    h.expect("live again", |a| *a == Action::LiveStatus(Liveness::Live))
        .await;
    let (_, value) = follow_change(&mut h, &key).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Gap"));

    // Armed again after the reconnect.
    let () = admin.hset(&key, "name", "After").await.unwrap();
    let (_, value) = follow_change(&mut h, &key).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("After"));
    h.shutdown().await;
}

/// After opening another key, changes to the first one trigger nothing.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_other_key_untracked() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let prefix = support::prefix("live_other_key");
    let (one, two) = (format!("{prefix}user:1"), format!("{prefix}user:2"));
    let () = admin.hset(&one, "name", "Ann").await.unwrap();
    let () = admin.hset(&two, "name", "Eve").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_live(&mut h, &one, RedisType::Hash, 0, true).await;
    h.state.value_state.lock().select(&two, RedisType::Hash);
    fetch_shown_value(&mut h).await;
    h.live(LiveCmd::Track {
        key: two.clone(),
        db: 0,
    });

    for n in 0..3 {
        let () = admin.hset(&one, "name", n).await.unwrap();
    }
    let seen = h.drain(Duration::from_millis(500)).await;
    assert!(
        !seen
            .iter()
            .any(|a| matches!(a, Action::OpenKeyChanged { .. })),
        "{seen:?}"
    );

    let () = admin.hset(&two, "name", "Bob").await.unwrap();
    let (_, value) = follow_change(&mut h, &two).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Bob"));
    h.shutdown().await;
}

/// After a DB switch the key is armed on the new DB.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_db_switch_rearms() {
    let server = support::start_redis().await;
    let mut db3 = server.admin(3).await;
    let key = format!("{}user:1", support::prefix("live_db_switch"));
    let () = db3.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    h.send(RedisEvent::SelectDb(3));
    h.expect("switched", |a| *a == Action::DbSwitched).await;
    open_live(&mut h, &key, RedisType::Hash, 3, true).await;

    let () = db3.hset(&key, "name", "Bob").await.unwrap();
    let (_, value) = follow_change(&mut h, &key).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Bob"));
    h.shutdown().await;
}
