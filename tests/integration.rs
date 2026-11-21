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

mod cluster_support;
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
        diff, text_from_bytes, Action, ConnStatus, Diff, EntryKind, ItemDelete, ItemEdit,
        JsonPreview, JsonPreviewKind, KeyChange, KeyItem, KeyValue, LiveCmd, Liveness, MemberKind,
        Monitor, MonitorCmd, MonitorStatus, NewKeySpec, PageRow, PubSubCmd, RedisEvent, RedisType,
        Sort, SortKey, Target, Ttl, TypeFilter, ValueEdit,
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
/// `KeysChanged` on success, `Error` on failure.
async fn write(h: &mut Harness, event: RedisEvent) -> Action {
    h.send(event);
    h.expect("the write's result", |a| {
        matches!(
            a,
            Action::RequestSelectedValue | Action::KeysChanged | Action::Error(_)
        )
    })
    .await
}

/// Like [`write`], but waits for the last action of a write
/// (`KeysChanged`), so the next write's answer isn't mistaken for this one.
async fn write_done(h: &mut Harness, event: RedisEvent) -> Action {
    h.send(event);
    h.expect("the write's result", |a| {
        matches!(a, Action::KeysChanged | Action::Error(_))
    })
    .await
}

/// #80: a page that needs a long sweep is scanned in slices. A write sent
/// while it runs is done between two slices, and the fetch still gets its
/// one answer with every match.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_long_scan_lets_a_write_through() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("slices");
    // One key in ten matches, so no SCAN step comes back empty (the COUNT
    // hint stays at 100) and a 10,000-row page takes the whole sweep:
    // about 600 steps, twelve slices.
    let keys = support::seed_strings(&mut admin, &format!("{p}k:"), 60_000).await;
    let matching = keys.iter().filter(|k| k.ends_with('7')).count();

    let mut h = Harness::new(server.client(0), false).await;
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_page_size(10_000, 0);
        ks.set_pattern(Some(format!("{p}k:*7")));
    }
    h.send(RedisEvent::FetchKeys);
    h.send(RedisEvent::CreateKey {
        key: format!("{p}new"),
        spec: NewKeySpec::String("v".into()),
    });

    let mut seen = Vec::new();
    loop {
        let action = h
            .expect("the page", |a| {
                matches!(
                    a,
                    Action::KeysScanning(_)
                        | Action::KeysChanged
                        | Action::LoadKeysIntoKeySpace
                        | Action::KeysLoadAborted
                        | Action::Error(_)
                )
            })
            .await;
        let done = action == Action::LoadKeysIntoKeySpace;
        seen.push(action);
        if done {
            break;
        }
    }
    let position = |wanted: &dyn Fn(&Action) -> bool| seen.iter().position(wanted);
    let scanning = position(&|a| matches!(a, Action::KeysScanning(_)));
    let written = position(&|a| *a == Action::KeysChanged);
    assert!(scanning.is_some(), "no progress shown: {seen:?}");
    assert!(
        written.is_some_and(|at| at + 1 < seen.len()),
        "the write waited for the page: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|a| matches!(a, Action::Error(_) | Action::KeysLoadAborted)),
        "{seen:?}"
    );
    let checked: Vec<u64> = seen
        .iter()
        .filter_map(|a| match a {
            Action::KeysScanning(checked) => Some(*checked),
            _ => None,
        })
        .collect();
    assert!(checked.windows(2).all(|w| w[0] < w[1]), "{checked:?}");

    {
        let ks = h.state.keyspace_state.lock();
        assert!(ks.sweep_done);
        assert_eq!(ks.scanned.len(), matching);
    }
    assert!(h.drain(Duration::from_millis(200)).await.is_empty());
    h.shutdown().await;
}

/// #76: the app's own writes change one key of the sweep. Nothing is
/// scanned again, so the place in a long list (and what is loaded) stays.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_write_updates_the_sweep_without_a_rescan() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("inplace");
    support::seed_strings(&mut admin, &format!("{p}k:"), 2_000).await;
    let set = format!("{p}set");
    let _: () = admin.sadd(&set, "only").await.expect("SADD");

    let mut h = Harness::new(server.client(0), false).await;
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_page_size(100, 0);
        ks.next_page();
    }
    let page = h.fetch_page().await;
    let (cursor, loaded) = {
        let ks = h.state.keyspace_state.lock();
        (ks.sweep_cursor, ks.scanned.len())
    };
    assert!(cursor != 0 && loaded >= 200, "the sweep is part way");
    // The sweep as it would be after a rescan: restarted, cursor 0.
    let sweep = |h: &Harness| {
        let ks = h.state.keyspace_state.lock();
        (ks.sweep_cursor, ks.scanned.len(), ks.page)
    };

    // Delete a key of the page: one key less, the rest as it was.
    let victim = page[3].clone();
    let done = write_done(
        &mut h,
        RedisEvent::DeleteKey {
            key: victim.clone(),
        },
    )
    .await;
    assert_eq!(done, Action::KeysChanged);
    assert_eq!(sweep(&h), (cursor, loaded - 1, 1));
    let after = h.fetch_page().await;
    assert!(!after.contains(&victim));
    assert_eq!(after[..3], page[..3], "the rows before it didn't move");
    assert_eq!(after[3..99], page[4..], "the rows after it moved up");
    // Filling the page's last row scans on from where the sweep was, but
    // only if the delete left it short of two full pages: SCAN batch sizes
    // vary, so the sweep may have held a spare key.
    let (held, held_cursor) = (loaded - 1, cursor);
    let (cursor, loaded) = {
        let ks = h.state.keyspace_state.lock();
        (ks.sweep_cursor, ks.scanned.len())
    };
    assert_ne!(cursor, 0, "the sweep is still part way");
    if held >= 200 {
        assert_eq!((cursor, loaded), (held_cursor, held), "no SCAN was needed");
    } else {
        assert!(loaded >= 200, "the sweep scanned on to fill the page");
    }

    // Create: the new key joins the loaded ones.
    let fresh = format!("{p}fresh");
    let done = write_done(
        &mut h,
        RedisEvent::CreateKey {
            key: fresh.clone(),
            spec: NewKeySpec::String("v".into()),
        },
    )
    .await;
    assert_eq!(done, Action::KeysChanged);
    assert_eq!(sweep(&h), (cursor, loaded + 1, 1));
    // The next fetch shows the page the new key is on.
    assert!(h.fetch_page().await.contains(&fresh));
    assert!(h.state.keyspace_state.lock().previous_page());
    let loaded = loaded + 1;
    let page_now = h.state.keyspace_state.lock().page;

    // A TTL on a loaded key changes nothing in the sweep; the page shows it.
    let timed = after[0].clone();
    let done = write_done(
        &mut h,
        RedisEvent::SetTtl {
            key: timed.clone(),
            ttl: Ttl::Expire(500),
        },
    )
    .await;
    assert_eq!(done, Action::KeysChanged);
    assert_eq!(sweep(&h), (cursor, loaded, page_now));
    h.state.keyspace_state.lock().page = 1;
    h.fetch_page().await;
    let ttl = h
        .state
        .keys
        .lock()
        .as_ref()
        .and_then(|keys| keys.iter().find(|k| k.key == timed).map(|k| k.ttl));
    assert!(matches!(ttl, Some(1..=500)), "{ttl:?}");

    // Removing a set's last member deletes the key: it leaves the sweep
    // (if the sweep had reached it).
    let held = h.state.keyspace_state.lock().index_of(&set).is_some();
    let done = write_done(
        &mut h,
        RedisEvent::DeleteItem {
            key: set.clone(),
            delete: ItemDelete::SetMember("only".into()),
        },
    )
    .await;
    assert_eq!(done, Action::KeysChanged);
    assert_eq!(h.state.keyspace_state.lock().index_of(&set), None);
    assert_eq!(sweep(&h), (cursor, loaded - usize::from(held), 1));

    // The rest of the sweep still finds every key exactly once.
    h.state.keyspace_state.lock().set_page_size(5_000, 0);
    let all = h.fetch_page().await;
    let unique: HashSet<&String> = all.iter().collect();
    assert_eq!(unique.len(), all.len());
    assert_eq!(all.len(), 2_000, "2,000 - 1 deleted + 1 created");
    h.shutdown().await;
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
        // Wait for this FetchKeys' own answer: a failure is `Error`, then
        // `KeysLoadAborted`. Stopping at the `Error` would leave the other
        // in the channel, and every later answer would be read one late.
        let action = h
            .expect("keys or a failure", |a| {
                matches!(a, Action::LoadKeysIntoKeySpace | Action::KeysLoadAborted)
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
#[allow(clippy::too_many_lines)] // one event per write variant
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
            ttl: Ttl::Expire(5),
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
        RedisEvent::SetTtl {
            key: hash.clone(),
            ttl: Ttl::Expire(0),
        },
        RedisEvent::ShiftTtl {
            key: zset.clone(),
            delta_secs: 60,
        },
        RedisEvent::RenameKey {
            from: string.clone(),
            to: format!("{p}renamed"),
            overwrite: true,
        },
        RedisEvent::CopyKey {
            from: hash.clone(),
            to: format!("{p}copy"),
            db: Some(1),
            overwrite: true,
        },
        RedisEvent::RenameField {
            key: hash.clone(),
            from: "f1".into(),
            to: "g1".into(),
        },
        RedisEvent::RenameMember {
            key: zset.clone(),
            kind: MemberKind::Zset,
            from: "a".into(),
            to: "z".into(),
        },
        RedisEvent::DeleteKeys { keys: keys.clone() },
        RedisEvent::ApplyValueEdit {
            key: list.clone(),
            edit: ValueEdit::List {
                items: vec!["x".into()],
            },
        },
        RedisEvent::SlowlogReset,
        RedisEvent::SPublish {
            channel: format!("{p}shard"),
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
                        | Action::KeysChanged
                        | Action::RequestSelectedValue
                        | Action::Published { .. }
                        | Action::TargetExists { .. }
                        | Action::KeysDeleted(_)
                        | Action::Notice(_)
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
            Action::KeysChanged | Action::RequestSelectedValue | Action::Published { .. }
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
    let answer = |a: &Action| matches!(a, Action::Error(_) | Action::KeysChanged);

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
    assert!(matches!(action, Action::KeysChanged), "{action:?}");
    let value: String = admin.get(&key).await.expect("get");
    assert_eq!(value, "after");
    h.shutdown().await;
}

/// #63: more events than the old 50-slot broadcast channel held, sent
/// before the runner can take one. None may be lost and the runner must go
/// on answering (it used to stop for the rest of the session).
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_burst_of_events_never_stalls_the_runner() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("burst");
    let keys = support::seed_strings(&mut admin, &p, 5).await;
    let mut h = Harness::new(server.client(0), false).await;
    h.state
        .keyspace_state
        .lock()
        .set_pattern(Some(format!("{p}*")));
    h.state
        .value_state
        .lock()
        .select(&keys[0], RedisType::String);

    for _ in 0..200 {
        h.send(RedisEvent::FetchValue);
        h.send(RedisEvent::FetchKeys);
    }
    // A write queued behind the burst still runs.
    h.send(RedisEvent::SetTtl {
        key: keys[0].clone(),
        ttl: Ttl::Expire(100),
    });
    h.expect("the write queued behind the burst", |a| {
        *a == Action::KeysChanged
    })
    .await;
    let ttl: i64 = admin.ttl(&keys[0]).await.expect("TTL");
    assert!(ttl > 0, "the queued EXPIRE ran");

    // And the runner still answers afterwards.
    let page = h.fetch_page().await;
    assert_eq!(page.len(), 5);
    h.shutdown().await;
}

/// #68: a LIST edit or delete by index is refused once another client has
/// moved the elements, a SET member replace runs as one transaction, and a
/// new STRING never overwrites a key created meanwhile.
#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)] // one scenario per write shape
async fn list_writes_check_the_element_shown() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("stale");
    let list = format!("{p}list");
    let () = admin.rpush(&list, &["a", "b", "c"]).await.expect("RPUSH");
    let mut h = Harness::new(server.client(0), false).await;

    // Another client pushes in front: index 1 now holds "a", not "b".
    let () = admin.lpush(&list, "new").await.expect("LPUSH");
    for event in [
        RedisEvent::EditItem {
            key: list.clone(),
            edit: ItemEdit::ListSet {
                index: 1,
                expected: "b".into(),
                value: "B".into(),
            },
        },
        RedisEvent::DeleteItem {
            key: list.clone(),
            delete: ItemDelete::ListIndex {
                index: 1,
                expected: "b".into(),
            },
        },
    ] {
        match write_done(&mut h, event).await {
            Action::Error(text) => assert!(text.contains("the list changed"), "{text}"),
            other => panic!("a stale index must be refused, got {other:?}"),
        }
    }
    let items: Vec<String> = admin.lrange(&list, 0, -1).await.expect("LRANGE");
    assert_eq!(items, ["new", "a", "b", "c"]);

    // With the value shown now, both go through.
    let ok = write_done(
        &mut h,
        RedisEvent::EditItem {
            key: list.clone(),
            edit: ItemEdit::ListSet {
                index: 2,
                expected: "b".into(),
                value: "B".into(),
            },
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "edit: {ok:?}");
    let ok = write_done(
        &mut h,
        RedisEvent::DeleteItem {
            key: list.clone(),
            delete: ItemDelete::ListIndex {
                index: 0,
                expected: "new".into(),
            },
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "delete: {ok:?}");
    let items: Vec<String> = admin.lrange(&list, 0, -1).await.expect("LRANGE");
    assert_eq!(items, ["a", "B", "c"]);

    // A user without scripting: the same check, without `EVAL`.
    let () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("noscript")
        .arg("on")
        .arg(">pw")
        .arg("~*")
        .arg("&*")
        .arg("+@all")
        .arg("-@scripting")
        .query_async(&mut admin)
        .await
        .expect("ACL SETUSER");
    let url = server
        .url(0)
        .replacen("redis://", "redis://noscript:pw@", 1);
    let mut limited = Harness::new(redis::Client::open(url).expect("client"), false).await;
    let stale = write_done(
        &mut limited,
        RedisEvent::DeleteItem {
            key: list.clone(),
            delete: ItemDelete::ListIndex {
                index: 0,
                expected: "zzz".into(),
            },
        },
    )
    .await;
    assert!(
        matches!(&stale, Action::Error(text) if text.contains("the list changed")),
        "{stale:?}"
    );
    let ok = write_done(
        &mut limited,
        RedisEvent::DeleteItem {
            key: list.clone(),
            delete: ItemDelete::ListIndex {
                index: 0,
                expected: "a".into(),
            },
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "unscripted delete: {ok:?}");
    let items: Vec<String> = admin.lrange(&list, 0, -1).await.expect("LRANGE");
    assert_eq!(items, ["B", "c"]);

    // SET member replace: SREM + SADD in one MULTI.
    let set = format!("{p}set");
    let () = admin.sadd(&set, &["a", "b"]).await.expect("SADD");
    let ok = write_done(
        &mut h,
        RedisEvent::EditItem {
            key: set.clone(),
            edit: ItemEdit::SetReplace {
                old: "a".into(),
                new: "z".into(),
            },
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "set: {ok:?}");
    let mut members: Vec<String> = admin.smembers(&set).await.expect("SMEMBERS");
    members.sort();
    assert_eq!(members, ["b", "z"]);

    // A new STRING over an existing key is refused and changes nothing.
    let taken = format!("{p}taken");
    let () = admin.set(&taken, "theirs").await.expect("SET");
    let refused = write_done(
        &mut h,
        RedisEvent::CreateKey {
            key: taken.clone(),
            spec: NewKeySpec::String("ours".into()),
        },
    )
    .await;
    assert!(matches!(refused, Action::Error(_)), "{refused:?}");
    let value: String = admin.get(&taken).await.expect("GET");
    assert_eq!(value, "theirs");
    let fresh = format!("{p}fresh");
    let ok = write_done(
        &mut h,
        RedisEvent::CreateKey {
            key: fresh.clone(),
            spec: NewKeySpec::String("ours".into()),
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "create: {ok:?}");
    let value: String = admin.get(&fresh).await.expect("GET");
    assert_eq!(value, "ours");
}

/// #69: a user whose ACL refuses `MEMORY USAGE` still gets the key list,
/// with unknown sizes, instead of "Failed to fetch keys".
#[tokio::test]
#[ignore = "needs Docker"]
async fn keys_list_without_memory_usage() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("nomem");
    let keys = support::seed_strings(&mut admin, &p, 5).await;
    let () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("limited")
        .arg("on")
        .arg(">pw")
        .arg("~*")
        .arg("&*")
        .arg("+@all")
        .arg("-memory")
        .query_async(&mut admin)
        .await
        .expect("ACL SETUSER");
    let url = server.url(0).replacen("redis://", "redis://limited:pw@", 1);
    let client = redis::Client::open(url).expect("client");
    let mut h = Harness::new(client, false).await;
    h.state
        .keyspace_state
        .lock()
        .set_pattern(Some(format!("{p}*")));

    // Twice: the second page fetch no longer sends `MEMORY USAGE`.
    for _ in 0..2 {
        let page: HashSet<String> = h.fetch_page().await.into_iter().collect();
        assert_eq!(page, keys.iter().cloned().collect::<HashSet<_>>());
        let metas = h.state.keys.lock().clone().expect("the page");
        assert!(metas.iter().all(|m| m.size == 0), "sizes unknown");
        assert!(metas.iter().all(|m| m.r_type == RedisType::String));
    }
    h.shutdown().await;
}

/// #65: a key name and collection members that aren't UTF-8 neither fail
/// the page nor the value, and the key can be opened, edited and deleted
/// under its exact bytes.
#[tokio::test]
#[ignore = "needs Docker"]
async fn binary_key_names_and_members() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("bin");
    let plain = support::seed_strings(&mut admin, &p, 3).await;
    let raw_string: Vec<u8> = [p.as_bytes(), b"\xff\xfe"].concat();
    let raw_hash: Vec<u8> = [p.as_bytes(), b"hash\x80"].concat();
    let raw_list: Vec<u8> = [p.as_bytes(), b"list"].concat();
    let () = admin.set(&raw_string, "x").await.expect("SET");
    let () = admin
        .hset(&raw_hash, b"f\xc0".as_slice(), b"\x93\x01\x02".as_slice())
        .await
        .expect("HSET");
    let () = admin
        .rpush(&raw_list, &[b"a".as_slice(), b"\xfe\xed".as_slice()])
        .await
        .expect("RPUSH");

    let mut h = Harness::new(server.client(0), false).await;
    h.state
        .keyspace_state
        .lock()
        .set_pattern(Some(format!("{p}*")));
    let page = h.fetch_page().await;
    assert_eq!(page.len(), plain.len() + 3, "{page:?}");
    // The names as the app keeps them: exact bytes, as text.
    let string_key = text_from_bytes(&raw_string);
    let hash_key = text_from_bytes(&raw_hash);
    assert!(page.contains(&string_key), "{page:?}");
    assert!(page.contains(&hash_key), "{page:?}");
    let metas = h.state.keys.lock().clone().expect("the page");
    let type_of = |key: &str| {
        metas
            .iter()
            .find(|m| m.key == key)
            .map(|m| m.r_type.clone())
            .expect("meta")
    };
    assert_eq!(type_of(&string_key), RedisType::String);
    assert_eq!(type_of(&hash_key), RedisType::Hash);

    // Values with binary members load.
    match load_value(&mut h, &hash_key, RedisType::Hash).await {
        KeyValue::Hash(entries) => assert_eq!(entries.len(), 1),
        other => panic!("hash: {other:?}"),
    }
    let list_key = format!("{p}list");
    let items = match load_value(&mut h, &list_key, RedisType::List).await {
        KeyValue::List(items) => items,
        other => panic!("list: {other:?}"),
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], "a");

    // Deleting the binary element targets its exact bytes.
    let ok = write_done(
        &mut h,
        RedisEvent::DeleteItem {
            key: list_key.clone(),
            delete: ItemDelete::ListIndex {
                index: 1,
                expected: items[1].clone(),
            },
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "delete item: {ok:?}");
    let left: Vec<Vec<u8>> = admin.lrange(&raw_list, 0, -1).await.expect("LRANGE");
    assert_eq!(left, [b"a".to_vec()]);

    // TTL and delete on the binary key name.
    let ok = write_done(
        &mut h,
        RedisEvent::SetTtl {
            key: string_key.clone(),
            ttl: Ttl::Expire(500),
        },
    )
    .await;
    assert!(!matches!(ok, Action::Error(_)), "ttl: {ok:?}");
    let ttl: i64 = admin.ttl(&raw_string).await.expect("TTL");
    assert!(ttl > 0, "{ttl}");
    let ok = write_done(&mut h, RedisEvent::DeleteKey { key: string_key }).await;
    assert!(!matches!(ok, Action::Error(_)), "delete: {ok:?}");
    let exists: bool = admin.exists(&raw_string).await.expect("EXISTS");
    assert!(!exists, "the binary key is deleted");
    h.shutdown().await;
}

/// #66: editing a STRING keeps its TTL (`SET … KEEPTTL`), also on a server
/// without `KEEPTTL` (`PTTL`, then `SET … PX`), and a key without a TTL
/// gets none.
async fn string_edit_keeps_ttl(server: &support::Server) {
    let mut admin = server.admin(0).await;
    let p = support::prefix("keepttl");
    let with_ttl = format!("{p}session");
    let without = format!("{p}plain");
    let () = admin.set_ex(&with_ttl, "old", 1000).await.expect("SETEX");
    let () = admin.set(&without, "old").await.expect("SET");
    let mut h = Harness::new(server.client(0), false).await;

    for key in [&with_ttl, &without] {
        h.send(RedisEvent::SetString {
            key: key.clone(),
            value: "new".into(),
        });
        // `KeysChanged` is the last action of a successful SetString.
        let ok = h
            .expect("the edit's result", |a| {
                matches!(a, Action::KeysChanged | Action::Error(_))
            })
            .await;
        assert_eq!(ok, Action::KeysChanged, "SetString {key}");
        let value: String = admin.get(key).await.expect("GET");
        assert_eq!(value, "new");
    }
    let ttl: i64 = admin.ttl(&with_ttl).await.expect("TTL");
    assert!((900..=1000).contains(&ttl), "TTL kept, got {ttl}");
    let ttl: i64 = admin.ttl(&without).await.expect("TTL");
    assert_eq!(ttl, -1, "no TTL invented");
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn string_edit_keeps_ttl_on_redis_7() {
    string_edit_keeps_ttl(&support::start_redis().await).await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn string_edit_keeps_ttl_on_redis_5() {
    string_edit_keeps_ttl(&support::start_redis_tag("5-alpine").await).await;
}

/// #66: a STRING that isn't UTF-8 is flagged, so the editor refuses it
/// instead of saving `�` over the stored bytes.
#[tokio::test]
#[ignore = "needs Docker"]
async fn binary_string_is_flagged_lossy() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("binary");
    let binary = format!("{p}blob");
    let text = format!("{p}text");
    let () = admin
        .set(&binary, &[0x00u8, 0xff, 0xfe, 0x61][..])
        .await
        .expect("SET");
    let () = admin.set(&text, "héllo").await.expect("SET");
    let mut h = Harness::new(server.client(0), false).await;

    let value = load_value(&mut h, &binary, RedisType::String).await;
    assert!(value.is_lossy(), "{value:?}");
    let value = load_value(&mut h, &text, RedisType::String).await;
    assert!(!value.is_lossy(), "{value:?}");
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
        expected: "b".into(),
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

/// Open `key` on a server without tracking: the live task polls. Waits for
/// `Polling` and gives the first check (the baseline) a moment.
async fn open_polled(h: &mut Harness, key: &str, r_type: RedisType, size: Option<u64>) {
    {
        let mut value_state = h.state.value_state.lock();
        value_state.select(key, r_type);
        value_state.size = size;
    }
    fetch_shown_value(h).await;
    h.live(LiveCmd::Track {
        key: key.to_owned(),
        db: 0,
    });
    h.expect("polling", |a| {
        *a == Action::LiveStatus(Liveness::Polling { every_ms: 2000 })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Redis 5 has no RESP3: rrover connects, polls every 2 s, and a change
/// shows up within about that.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_polls_on_redis_5() {
    let server = support::start_redis_tag("5-alpine").await;
    let mut admin = server.admin(0).await;
    let key = format!("{}user:1", support::prefix("live_polls"));
    let () = admin.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_polled(&mut h, &key, RedisType::Hash, Some(100)).await;

    // Nothing changed: no notice.
    let quiet = h.drain(Duration::from_millis(2500)).await;
    assert!(
        !quiet
            .iter()
            .any(|a| matches!(a, Action::OpenKeyChanged { .. })),
        "{quiet:?}"
    );

    let start = Instant::now();
    let () = admin.hset(&key, "name", "Bob").await.unwrap();
    let (change, value) = follow_change(&mut h, &key).await;
    assert_eq!(
        change,
        KeyChange::Changed {
            r_type: "hash".into()
        }
    );
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Bob"));
    assert!(
        start.elapsed() < Duration::from_millis(2600),
        "{:?}",
        start.elapsed()
    );

    let () = admin.del(&key).await.unwrap();
    let (change, _) = follow_change(&mut h, &key).await;
    assert_eq!(change, KeyChange::Deleted);
    h.shutdown().await;
}

/// A key above the `DUMP` limit is fingerprinted by the page on screen: a
/// change on that page is seen.
#[tokio::test]
#[ignore = "needs Docker"]
async fn live_polls_a_large_key_by_its_page() {
    let server = support::start_redis_tag("5-alpine").await;
    let mut admin = server.admin(0).await;
    let key = format!("{}big", support::prefix("live_polls_large"));
    let () = admin.hset(&key, "name", "Ann").await.unwrap();
    let mut h = Harness::new(server.client(0), false).await;
    open_polled(&mut h, &key, RedisType::Hash, Some(1 << 20)).await;

    let () = admin.hset(&key, "name", "Bob").await.unwrap();
    let (_, value) = follow_change(&mut h, &key).await;
    assert_eq!(hash_value(&value, "name").as_deref(), Some("Bob"));
    h.shutdown().await;
}

// ---- Sol 08: power edits ------------------------------------------------

/// A page large enough to hold every key of a test, filtered to its prefix.
async fn page_of(h: &mut Harness, prefix: &str) -> Vec<String> {
    {
        let mut ks = h.state.keyspace_state.lock();
        ks.set_page_size(1_000, 0);
        ks.set_pattern(Some(format!("{prefix}*")));
    }
    h.fetch_page().await
}

/// What a rename / copy answers: done (`KeysChanged`, or a `Notice` for a
/// copy to another DB), `TargetExists`, or an error.
async fn moved(h: &mut Harness, event: RedisEvent) -> Action {
    h.send(event);
    h.expect("the rename / copy result", |a| {
        matches!(
            a,
            Action::KeysChanged
                | Action::Error(_)
                | Action::TargetExists { .. }
                | Action::Notice(_)
        )
    })
    .await
}

/// AC1 / AC2: `RENAMENX` keeps the TTL and puts the new name in the sweep;
/// a taken name changes nothing until the overwrite is sent.
#[tokio::test]
#[ignore = "needs Docker"]
async fn rename_keeps_the_ttl_and_never_overwrites_unasked() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("rename");
    let (user1, user9) = (format!("{p}user:1"), format!("{p}user:9"));
    let _: () = admin.set_ex(&user1, "ada", 600).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;
    assert_eq!(page_of(&mut h, &p).await, std::slice::from_ref(&user1));

    let rename = |overwrite| RedisEvent::RenameKey {
        from: user1.clone(),
        to: user9.clone(),
        overwrite,
    };
    assert_eq!(moved(&mut h, rename(false)).await, Action::KeysChanged);
    let ttl: i64 = admin.ttl(&user9).await.expect("TTL");
    assert!((590..=600).contains(&ttl), "TTL kept: {ttl}");
    assert!(!admin.exists::<_, bool>(&user1).await.expect("EXISTS"));
    assert_eq!(
        h.fetch_page().await,
        std::slice::from_ref(&user9),
        "patched, no rescan"
    );

    // AC2: user:9 is taken now.
    let _: () = admin.set(&user1, "bob").await.expect("seed");
    match moved(&mut h, rename(false)).await {
        Action::TargetExists { from, to, db, copy } => {
            assert_eq!(
                (from, to, db, copy),
                (user1.clone(), user9.clone(), None, false)
            );
        }
        other => panic!("expected TargetExists, got {other:?}"),
    }
    let kept: String = admin.get(&user9).await.expect("GET");
    assert_eq!(kept, "ada", "nothing replaced without the second confirm");

    assert_eq!(moved(&mut h, rename(true)).await, Action::KeysChanged);
    let replaced: String = admin.get(&user9).await.expect("GET");
    assert_eq!(replaced, "bob");
    assert_eq!(h.fetch_page().await, [user9]);
    h.shutdown().await;
}

/// AC3: a copy to another DB keeps the TTL; `tag` picks the server (7.x
/// has `COPY`, 6.0 goes through `DUMP` + `RESTORE`).
async fn copy_to_db_2(tag: &str) {
    let server = support::start_redis_tag(tag).await;
    let mut admin = server.admin(0).await;
    let mut db2 = server.admin(2).await;
    let p = support::prefix("copy");
    let cart = format!("{p}cart:1");
    let _: () = admin
        .hset_multiple(&cart, &[("apple", "2"), ("pear", "1")])
        .await
        .expect("seed");
    let _: () = admin.expire(&cart, 900).await.expect("EXPIRE");
    let mut h = Harness::new(server.client(0), false).await;

    let copy = |overwrite| RedisEvent::CopyKey {
        from: cart.clone(),
        to: cart.clone(),
        db: Some(2),
        overwrite,
    };
    match moved(&mut h, copy(false)).await {
        Action::Notice(msg) => assert!(msg.contains("DB 2"), "{msg}"),
        other => panic!("{tag}: expected the copy notice, got {other:?}"),
    }
    let copied: Vec<(String, String)> = db2.hgetall(&cart).await.expect("HGETALL");
    assert_eq!(copied.len(), 2, "{tag}");
    let ttl: i64 = db2.ttl(&cart).await.expect("TTL");
    assert!((890..=900).contains(&ttl), "{tag}: TTL kept: {ttl}");

    // Taken in DB 2: asked first, then replaced.
    let _: () = db2.del(&cart).await.expect("DEL");
    let _: () = db2.set(&cart, "other").await.expect("SET");
    match moved(&mut h, copy(false)).await {
        Action::TargetExists { db, copy, .. } => assert_eq!((db, copy), (Some(2), true)),
        other => panic!("{tag}: expected TargetExists, got {other:?}"),
    }
    assert!(matches!(moved(&mut h, copy(true)).await, Action::Notice(_)));
    let kind: String = redis::cmd("TYPE")
        .arg(&cart)
        .query_async(&mut db2)
        .await
        .expect("TYPE");
    assert_eq!(kind, "hash", "{tag}");

    // In the same DB the copy shows up in the sweep.
    let twin = format!("{p}cart:2");
    let in_place = RedisEvent::CopyKey {
        from: cart.clone(),
        to: twin.clone(),
        db: None,
        overwrite: false,
    };
    assert_eq!(moved(&mut h, in_place).await, Action::KeysChanged);
    let mut page = page_of(&mut h, &p).await;
    page.sort();
    assert_eq!(page, [cart, twin]);
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn copy_to_another_db_on_redis_7() {
    copy_to_db_2(support::REDIS_TAG).await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn copy_to_another_db_on_redis_6_0_uses_dump_restore() {
    copy_to_db_2("6.0-alpine").await;
}

/// AC4: relative shifts, absolute durations, persist, and `EXPIRE 0`.
#[tokio::test]
#[ignore = "needs Docker"]
async fn ttl_durations_shift_set_and_persist() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("ttl");
    let key = format!("{p}session");
    let _: () = admin.set_ex(&key, "x", 3600).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;
    let ttl = |admin: &mut redis::aio::ConnectionManager| {
        let mut admin = admin.clone();
        let key = key.clone();
        async move { admin.ttl::<_, i64>(&key).await.expect("TTL") }
    };

    let shift = |delta_secs| RedisEvent::ShiftTtl {
        key: key.clone(),
        delta_secs,
    };
    assert_eq!(write_done(&mut h, shift(1800)).await, Action::KeysChanged);
    let now = ttl(&mut admin).await;
    assert!((5390..=5400).contains(&now), "+30m: {now}");

    let set = |ttl| RedisEvent::SetTtl {
        key: key.clone(),
        ttl,
    };
    assert_eq!(
        write_done(&mut h, set(Ttl::Expire(9000))).await,
        Action::KeysChanged
    );
    assert_eq!(ttl(&mut admin).await, 9000, "2h30m");

    // Shifting past zero is refused, not a silent delete.
    match write_done(&mut h, shift(-10_000)).await {
        Action::Error(msg) => assert!(msg.contains("enter 0"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        write_done(&mut h, set(Ttl::Persist)).await,
        Action::KeysChanged
    );
    assert_eq!(ttl(&mut admin).await, -1, "persist");
    match write_done(&mut h, shift(600)).await {
        Action::Error(msg) => assert!(msg.contains("no TTL"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        write_done(&mut h, set(Ttl::Expire(0))).await,
        Action::KeysChanged
    );
    assert!(
        !admin.exists::<_, bool>(&key).await.expect("EXISTS"),
        "0 deletes"
    );
    h.shutdown().await;
}

/// AC5 / AC6: a field rename never overwrites and keeps the value; a zset
/// member keeps its score; a set member moves.
#[tokio::test]
#[ignore = "needs Docker"]
async fn rename_field_and_member() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("renitem");
    let (hash, zset, set) = (format!("{p}user"), format!("{p}board"), format!("{p}tags"));
    let _: () = admin
        .hset_multiple(&hash, &[("name", "Ada"), ("email", "ada@example.com")])
        .await
        .expect("seed");
    let _: () = admin.zadd(&zset, "ada", 42).await.expect("seed");
    let _: () = admin.sadd(&set, &["a", "b"]).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;

    let field = |to: &str| RedisEvent::RenameField {
        key: hash.clone(),
        from: "name".into(),
        to: to.into(),
    };
    match write_done(&mut h, field("email")).await {
        Action::Error(msg) => assert!(msg.contains("already exists"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let email: String = admin.hget(&hash, "email").await.expect("HGET");
    assert_eq!(email, "ada@example.com");
    assert_eq!(
        write_done(&mut h, field("full_name")).await,
        Action::KeysChanged
    );
    let all: std::collections::HashMap<String, String> =
        admin.hgetall(&hash).await.expect("HGETALL");
    assert_eq!(all.get("full_name").map(String::as_str), Some("Ada"));
    assert!(!all.contains_key("name"));

    let member = RedisEvent::RenameMember {
        key: zset.clone(),
        kind: MemberKind::Zset,
        from: "ada".into(),
        to: "ada_l".into(),
    };
    assert_eq!(write_done(&mut h, member).await, Action::KeysChanged);
    let score: Option<f64> = admin.zscore(&zset, "ada_l").await.expect("ZSCORE");
    assert_eq!(score, Some(42.0));
    let old: Option<f64> = admin.zscore(&zset, "ada").await.expect("ZSCORE");
    assert_eq!(old, None);

    let taken = RedisEvent::RenameMember {
        key: set.clone(),
        kind: MemberKind::Set,
        from: "a".into(),
        to: "b".into(),
    };
    assert!(matches!(write_done(&mut h, taken).await, Action::Error(_)));
    let moved_member = RedisEvent::RenameMember {
        key: set.clone(),
        kind: MemberKind::Set,
        from: "a".into(),
        to: "c".into(),
    };
    assert_eq!(write_done(&mut h, moved_member).await, Action::KeysChanged);
    let mut members: Vec<String> = admin.smembers(&set).await.expect("SMEMBERS");
    members.sort();
    assert_eq!(members, ["b", "c"]);
    h.shutdown().await;
}

/// AC7: 50 marked keys go in one `UNLINK` and leave the sweep.
#[tokio::test]
#[ignore = "needs Docker"]
async fn bulk_delete_unlinks_and_patches_the_list() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("bulk");
    let keys = support::seed_strings(&mut admin, &format!("{p}k:"), 60).await;
    let mut h = Harness::new(server.client(0), false).await;
    assert_eq!(page_of(&mut h, &p).await.len(), 60);

    let marked: Vec<String> = keys[..50].to_vec();
    h.send(RedisEvent::DeleteKeys {
        keys: marked.clone(),
    });
    let deleted = h
        .expect("the delete", |a| {
            matches!(a, Action::KeysDeleted(_) | Action::Error(_))
        })
        .await;
    assert_eq!(deleted, Action::KeysDeleted(50));
    h.expect("the refresh", |a| *a == Action::KeysChanged).await;
    let left: u64 = redis::cmd("EXISTS")
        .arg(&marked)
        .query_async(&mut admin)
        .await
        .expect("EXISTS");
    assert_eq!(left, 0);
    let mut page = h.fetch_page().await;
    page.sort();
    let mut rest = keys[50..].to_vec();
    rest.sort();
    assert_eq!(page, rest);
    h.shutdown().await;
}

/// A server that refuses `EVAL` (an ACL without `@scripting`): renames fall
/// back to plain commands, say so, and still never overwrite.
#[tokio::test]
#[ignore = "needs Docker"]
async fn renames_fall_back_when_scripting_is_refused() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("noscript");
    let (hash, zset) = (format!("{p}user"), format!("{p}board"));
    let _: () = admin
        .hset_multiple(&hash, &[("name", "Ada"), ("email", "a@x")])
        .await
        .expect("seed");
    let _: () = admin.zadd(&zset, "ada", 42).await.expect("seed");
    let () = redis::cmd("ACL")
        .arg(&[
            "SETUSER",
            "noscript",
            "on",
            ">pw",
            "~*",
            "&*",
            "+@all",
            "-@scripting",
        ])
        .query_async(&mut admin)
        .await
        .expect("ACL SETUSER");
    let url = format!("redis://noscript:pw@{}:{}/0", server.host, server.port);
    let mut h = Harness::new(redis::Client::open(url).expect("url"), false).await;

    h.send(RedisEvent::RenameField {
        key: hash.clone(),
        from: "name".into(),
        to: "full_name".into(),
    });
    match h
        .expect("the warning", |a| {
            matches!(a, Action::Notice(_) | Action::Error(_))
        })
        .await
    {
        Action::Notice(msg) => assert!(msg.contains("not atomic"), "{msg}"),
        other => panic!("expected the fallback notice, got {other:?}"),
    }
    h.expect("the refresh", |a| *a == Action::KeysChanged).await;
    let value: Option<String> = admin.hget(&hash, "full_name").await.expect("HGET");
    assert_eq!(value.as_deref(), Some("Ada"));

    let taken = RedisEvent::RenameField {
        key: hash.clone(),
        from: "full_name".into(),
        to: "email".into(),
    };
    match write_done(&mut h, taken).await {
        Action::Error(msg) => assert!(msg.contains("already exists"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let email: String = admin.hget(&hash, "email").await.expect("HGET");
    assert_eq!(email, "a@x");

    let member = RedisEvent::RenameMember {
        key: zset.clone(),
        kind: MemberKind::Zset,
        from: "ada".into(),
        to: "ada_l".into(),
    };
    assert_eq!(write_done(&mut h, member).await, Action::KeysChanged);
    let score: Option<f64> = admin.zscore(&zset, "ada_l").await.expect("ZSCORE");
    assert_eq!(score, Some(42.0));
    h.shutdown().await;
}

/// `Shift-e` on a whole collection: the document `y` copies, edited and
/// diffed, is written in one transaction; only what changed moves, and a
/// list keeps its TTL.
#[tokio::test]
#[ignore = "needs Docker"]
async fn whole_values_edited_as_json_round_trip() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let p = support::prefix("wholeedit");
    let (hash, list, set, zset) = (
        format!("{p}h"),
        format!("{p}l"),
        format!("{p}s"),
        format!("{p}z"),
    );
    let _: () = admin
        .hset_multiple(&hash, &[("name", "Ada"), ("email", "a@x")])
        .await
        .expect("seed");
    let _: () = admin.rpush(&list, &["a", "b"]).await.expect("seed");
    let _: () = admin.expire(&list, 600).await.expect("seed");
    let _: () = admin.sadd(&set, &["a", "b"]).await.expect("seed");
    let _: () = admin.zadd(&zset, "ada", 42).await.expect("seed");
    let mut h = Harness::new(server.client(0), false).await;

    let fetched = |key: &str, r_type: RedisType| RedisEvent::FetchFullValue {
        key: key.to_owned(),
        r_type,
    };
    for (key, r_type, edited) in [
        (
            &hash,
            RedisType::Hash,
            r#"{"name": "Ada L", "city": "London"}"#,
        ),
        (&list, RedisType::List, r#"["b", "a", "c"]"#),
        (&set, RedisType::Set, r#"["b", "c"]"#),
        (
            &zset,
            RedisType::Zset,
            r#"[{"member": "ada", "score": 43}, {"member": "bob", "score": "inf"}]"#,
        ),
    ] {
        h.send(fetched(key, r_type.clone()));
        h.expect("the whole value", |a| *a == Action::YankReady)
            .await;
        let (_, text) = h.state.yank.lock().take().expect("the value");
        let edit = match diff(&r_type, &text, edited).expect("diffs") {
            Diff::Changed { edit, .. } => edit,
            Diff::Same => panic!("{r_type:?}: no change found"),
        };
        let done = write_done(
            &mut h,
            RedisEvent::ApplyValueEdit {
                key: key.clone(),
                edit,
            },
        )
        .await;
        assert_eq!(done, Action::KeysChanged, "{r_type:?}");
    }

    let fields: std::collections::HashMap<String, String> =
        admin.hgetall(&hash).await.expect("HGETALL");
    assert_eq!(
        fields,
        [("name", "Ada L"), ("city", "London")]
            .map(|(f, v)| (f.to_owned(), v.to_owned()))
            .into()
    );
    let items: Vec<String> = admin.lrange(&list, 0, -1).await.expect("LRANGE");
    assert_eq!(items, ["b", "a", "c"]);
    let ttl: i64 = admin.ttl(&list).await.expect("TTL");
    assert!((590..=600).contains(&ttl), "the list kept its TTL: {ttl}");
    let mut members: Vec<String> = admin.smembers(&set).await.expect("SMEMBERS");
    members.sort();
    assert_eq!(members, ["b", "c"]);
    let scores: Vec<(String, f64)> = admin.zrange_withscores(&zset, 0, -1).await.expect("ZRANGE");
    assert_eq!(
        scores,
        [("ada".to_owned(), 43.0), ("bob".to_owned(), f64::INFINITY)]
    );
    h.shutdown().await;
}

// ---- MONITOR ---------------------------------------------------------------

/// Poll `check` until it holds, or fail with `what` after `WAIT`.
async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `id` of every client in MONITOR mode (flag `O` in `CLIENT LIST`).
async fn monitor_clients(admin: &mut redis::aio::ConnectionManager) -> Vec<String> {
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(admin)
        .await
        .expect("CLIENT LIST");
    list.lines()
        .filter(|line| {
            line.split(' ')
                .any(|f| f.strip_prefix("flags=").is_some_and(|v| v.contains('O')))
        })
        .filter_map(|line| {
            line.split(' ')
                .find_map(|f| f.strip_prefix("id=").map(str::to_owned))
        })
        .collect()
}

/// Whether the store holds `SET key value`.
fn holds_set(h: &Harness, key: &str, value: &str) -> bool {
    h.state
        .monitor
        .lock()
        .iter()
        .any(|l| l.line.cmd.eq_ignore_ascii_case("SET") && l.line.args == [key, value])
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn monitor_start_line_and_stop_closes_the_connection() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let h = Harness::new(server.client(0), false).await;
    assert!(
        monitor_clients(&mut admin).await.is_empty(),
        "idle until started"
    );

    h.monitor(MonitorCmd::Start);
    let store = h.state.monitor.clone();
    wait_until("running", || {
        *store.lock().status() == MonitorStatus::Running
    })
    .await;
    assert_eq!(monitor_clients(&mut admin).await.len(), 1);

    let () = admin.set("a", "1").await.expect("SET");
    wait_until("line", || holds_set(&h, "a", "1")).await;

    h.monitor(MonitorCmd::Stop);
    wait_until("stopped", || {
        *store.lock().status() == MonitorStatus::Stopped
    })
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !monitor_clients(&mut admin).await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the monitor client is still connected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn monitor_reconnects_after_a_kill() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let h = Harness::new(server.client(0), false).await;
    h.monitor(MonitorCmd::Start);
    let store = h.state.monitor.clone();
    wait_until("running", || {
        *store.lock().status() == MonitorStatus::Running
    })
    .await;

    let ids = monitor_clients(&mut admin).await;
    assert_eq!(ids.len(), 1);
    let _: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(&ids[0])
        .query_async(&mut admin)
        .await
        .expect("CLIENT KILL");
    wait_until("noticed", || {
        matches!(
            store.lock().status(),
            MonitorStatus::Reconnecting { attempt: 1, .. }
        )
    })
    .await;
    wait_until("back", || *store.lock().status() == MonitorStatus::Running).await;

    let () = admin.set("after", "1").await.expect("SET");
    wait_until("line after reconnect", || holds_set(&h, "after", "1")).await;
    h.shutdown().await;
}

/// A server that refuses MONITOR is reported once and never retried.
#[tokio::test]
#[ignore = "needs Docker"]
async fn monitor_refused_by_an_acl_is_final() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("limited")
        .arg("on")
        .arg(">pw")
        .arg("~*")
        .arg("+@all")
        .arg("-monitor")
        .query_async(&mut admin)
        .await
        .expect("ACL SETUSER");
    let url = format!("redis://limited:pw@{}:{}/0", server.host, server.port);
    let h = Harness::new(redis::Client::open(url).expect("url"), false).await;

    h.monitor(MonitorCmd::Start);
    let store = h.state.monitor.clone();
    wait_until("refused", || {
        matches!(store.lock().status(), MonitorStatus::Failed(_))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        matches!(store.lock().status(), MonitorStatus::Failed(_)),
        "no retry"
    );
    assert!(monitor_clients(&mut admin).await.is_empty());
    h.shutdown().await;
}

/// `total_commands_processed` from `INFO stats`.
async fn commands_processed(admin: &mut redis::aio::ConnectionManager) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("stats")
        .query_async(admin)
        .await
        .expect("INFO");
    info.lines()
        .find_map(|l| l.strip_prefix("total_commands_processed:"))
        .and_then(|n| n.trim().parse().ok())
        .expect("total_commands_processed")
}

/// At 50k commands a second the UI still answers within a frame, the screen
/// shows it is sampling, and the heap stays within the buffer limit + 20 MiB.
/// Timings mean something in a release build:
/// `cargo test --release --test integration -- --ignored --nocapture monitor_under_load`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker"]
async fn monitor_under_load_stays_responsive_and_bounded() {
    const LOAD: Duration = Duration::from_secs(6);
    let server = support::start_redis().await;
    let h = Harness::new(server.client(0), false).await;
    let before = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(before, Ordering::Relaxed);

    h.monitor(MonitorCmd::Start);
    let store = h.state.monitor.clone();
    wait_until("running", || {
        *store.lock().status() == MonitorStatus::Running
    })
    .await;
    let limit = store.lock().limits().bytes;
    let seen_before = store.lock().seen();
    let mut admin = server.admin(0).await;
    let commands_before = commands_processed(&mut admin).await;

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..4 {
        let client = server.client(0);
        let stop = stop.clone();
        writers.push(tokio::spawn(async move {
            let mut con = client
                .get_multiplexed_async_connection()
                .await
                .expect("load connection");
            while !stop.load(Ordering::Relaxed) {
                let mut pipe = redis::pipe();
                for i in 0..500 {
                    pipe.set(format!("load:{w}:{i}"), "value").ignore();
                }
                let () = pipe.query_async(&mut con).await.expect("load pipeline");
            }
        }));
    }

    let mut monitor = Monitor::new(store.clone());
    let area = ratatui::layout::Rect::new(0, 0, 120, 40);
    let started = Instant::now();
    let mut frames = Vec::new();
    let mut sampled = false;
    let mut down = false;
    while started.elapsed() < LOAD {
        let at = Instant::now();
        monitor.move_selection(down);
        down = !down;
        let view = monitor.build_view(area, std::time::SystemTime::now());
        std::hint::black_box(view);
        frames.push(at.elapsed());
        sampled |= store.lock().sample_every() > 1;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.await.expect("load task");
    }

    let commands = commands_processed(&mut admin).await - commands_before;
    // The stream may still be draining: wait for it, then compare.
    let deadline = Instant::now() + WAIT;
    while store.lock().seen() - seen_before < commands * 9 / 10 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let seen = store.lock().seen() - seen_before;
    assert!(
        seen <= commands + 16,
        "counted {seen} lines for {commands} commands"
    );
    let rate = commands / elapsed.as_secs().max(1);
    frames.sort();
    let p99 = frames[frames.len() * 99 / 100];
    let max = frames[frames.len() - 1];
    let grown = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(before);
    eprintln!(
        "load: {rate} commands/s by the server, {seen} of {commands} lines read, sampling 1/{}, frame p99 {p99:?} max {max:?}, heap +{} MiB",
        store.lock().sample_every(),
        grown >> 20
    );
    assert!(rate >= 50_000, "the load only reached {rate} lines/s");
    assert!(sampled, "never sampled");
    assert!(store.lock().bytes() <= limit);
    assert!(grown <= limit + (20 << 20), "heap grew {grown} bytes");
    if !cfg!(debug_assertions) {
        assert!(p99 < Duration::from_millis(16), "frame p99 {p99:?}");
    }
    h.monitor(MonitorCmd::Stop);
    h.shutdown().await;
}

// ---- Slowlog ---------------------------------------------------------------

async fn config_set(admin: &mut redis::aio::ConnectionManager, name: &str, value: &str) {
    let () = redis::cmd("CONFIG")
        .arg("SET")
        .arg(name)
        .arg(value)
        .query_async(admin)
        .await
        .expect("CONFIG SET");
}

async fn slowlog_len(admin: &mut redis::aio::ConnectionManager) -> i64 {
    redis::cmd("SLOWLOG")
        .arg("LEN")
        .query_async(admin)
        .await
        .expect("SLOWLOG LEN")
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn slowlog_lists_with_durations_and_resets() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    config_set(&mut admin, "slowlog-log-slower-than", "0").await;
    let () = admin.set("slow:key", "value").await.expect("SET");

    let mut h = Harness::new(server.client(0), false).await;
    h.send(RedisEvent::FetchSlowlog);
    h.expect("loaded", |a| *a == Action::SlowlogLoaded).await;
    let data = h.state.slowlog.lock().take().expect("data");
    assert_eq!(data.error, None);
    assert_eq!(data.slower_than, Some(0));
    assert_eq!(data.max_len, Some(128));
    let set = data
        .entries
        .iter()
        .find(|e| e.args.len() == 3 && e.args[1] == "slow:key")
        .expect("the SET is listed");
    assert!(set.args[0].eq_ignore_ascii_case("SET"));
    assert_eq!(set.args[2], "value");
    assert!(!set.client.is_empty());

    config_set(&mut admin, "slowlog-log-slower-than", "-1").await;
    h.send(RedisEvent::SlowlogReset);
    h.expect("reloaded", |a| *a == Action::SlowlogLoaded).await;
    assert_eq!(slowlog_len(&mut admin).await, 0);
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn slowlog_reset_is_refused_under_read_only() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    config_set(&mut admin, "slowlog-log-slower-than", "0").await;
    let () = admin.set("a", "1").await.expect("SET");
    config_set(&mut admin, "slowlog-log-slower-than", "-1").await;
    let before = slowlog_len(&mut admin).await;
    assert!(before > 0);

    let mut h = Harness::new(server.client(0), true).await;
    h.send(RedisEvent::SlowlogReset);
    h.expect(
        "refused",
        |a| matches!(a, Action::Error(m) if m.contains("Read-only")),
    )
    .await;
    assert_eq!(slowlog_len(&mut admin).await, before);
    h.shutdown().await;
}

/// A server that refuses `CONFIG` still lists; one that refuses `SLOWLOG`
/// reports it in the data, with no error popup.
#[tokio::test]
#[ignore = "needs Docker"]
async fn slowlog_refusals_stay_inline() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    for (user, denied) in [("nocfg", "-config"), ("noslow", "-slowlog")] {
        let () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg(user)
            .arg("on")
            .arg(">pw")
            .arg("~*")
            .arg("+@all")
            .arg(denied)
            .query_async(&mut admin)
            .await
            .expect("ACL SETUSER");
    }
    let client = |user: &str| {
        let url = format!("redis://{user}:pw@{}:{}/0", server.host, server.port);
        redis::Client::open(url).expect("url")
    };

    let mut h = Harness::new(client("nocfg"), false).await;
    h.send(RedisEvent::FetchSlowlog);
    h.expect("loaded", |a| *a == Action::SlowlogLoaded).await;
    let data = h.state.slowlog.lock().take().expect("data");
    assert_eq!(
        (data.slower_than, data.max_len, data.error),
        (None, None, None)
    );
    h.shutdown().await;

    let mut h = Harness::new(client("noslow"), false).await;
    h.send(RedisEvent::FetchSlowlog);
    let seen = h.expect("loaded", |a| *a == Action::SlowlogLoaded).await;
    assert_eq!(seen, Action::SlowlogLoaded);
    let data = h.state.slowlog.lock().take().expect("data");
    let error = data.error.expect("refused");
    assert!(error.to_lowercase().contains("permission"), "{error}");
    h.shutdown().await;
}

// ---- Sharded Pub/Sub -------------------------------------------------------

/// `SPUBLISH channel payload` until someone receives it (the `SSUBSCRIBE`
/// may still be in flight).
async fn spublish_until_heard(admin: &mut redis::aio::ConnectionManager, channel: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let heard: i64 = redis::cmd("SPUBLISH")
            .arg(channel)
            .arg("hi")
            .query_async(admin)
            .await
            .expect("SPUBLISH");
        if heard > 0 {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "never subscribed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn shard_round_trip_and_unsubscribe() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let mut h = Harness::new(server.client(0), false).await;
    let target = Target::Shard("orders".into());
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target.clone()));

    spublish_until_heard(&mut admin, "orders").await;
    let store = h.state.pubsub.clone();
    wait_until("message", || store.lock().subs[0].stats.total >= 1).await;
    {
        let guard = store.lock();
        let buffer = &guard.subs[0].buffer;
        let message = buffer.range(0, buffer.len()).next().expect("a message");
        assert_eq!(message.channel, "orders");
        assert_eq!(message.payload.copy_text(), "hi");
    }

    // Our own SPUBLISH goes through the write path.
    h.send(RedisEvent::SPublish {
        channel: "orders".into(),
        payload: "second".into(),
    });
    let published = h
        .expect("published", |a| matches!(a, Action::Published { .. }))
        .await;
    assert!(matches!(published, Action::Published { receivers: 1, .. }));

    store.lock().remove(&target);
    h.pubsub(PubSubCmd::Unsubscribe(target));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let heard: i64 = redis::cmd("SPUBLISH")
            .arg("orders")
            .arg("x")
            .query_async(&mut admin)
            .await
            .expect("SPUBLISH");
        if heard == 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "still subscribed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn shard_resubscribes_after_a_lost_connection() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let h = Harness::new(server.client(0), false).await;
    let target = Target::Shard("orders".into());
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target));
    spublish_until_heard(&mut admin, "orders").await;

    let _: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(&mut admin)
        .await
        .expect("CLIENT KILL");
    let store = h.state.pubsub.clone();
    wait_until("noticed", || {
        matches!(store.lock().status, ConnStatus::Reconnecting { .. })
    })
    .await;
    wait_until("back", || store.lock().status == ConnStatus::Connected).await;
    let before = store.lock().subs[0].stats.total;
    spublish_until_heard(&mut admin, "orders").await;
    wait_until("message after reconnect", || {
        store.lock().subs[0].stats.total > before
    })
    .await;
    h.shutdown().await;
}

/// Redis 6.0 has no sharded Pub/Sub: `INFO` says so, and a shard target
/// that is tried anyway is refused without taking the task down.
#[tokio::test]
#[ignore = "needs Docker"]
async fn shard_is_unavailable_on_redis_6() {
    let server = support::start_redis_tag("6.0-alpine").await;
    let mut h = Harness::new(server.client(0), false).await;
    let info = h.state.info.clone();
    wait_until("INFO", || info.lock().is_some()).await;
    assert!(!info.lock().as_ref().expect("info").supports_shards());

    let target = Target::Shard("orders".into());
    h.state.pubsub.lock().add(target.clone());
    h.pubsub(PubSubCmd::Subscribe(target));
    h.expect("refused", |a| matches!(a, Action::Error(_))).await;
    assert!(h.state.pubsub.lock().subs.is_empty());
    h.shutdown().await;
}

/// On a cluster each shard subscription goes to the node owning its slot,
/// and keeps receiving after that node fails over.
#[tokio::test]
#[ignore = "needs Docker"]
async fn shard_subscribes_on_the_slot_owner_in_a_cluster() {
    let fixture = cluster_support::ClusterFixture::start();
    let cluster = fixture.client();
    let mut admin = cluster.get_async_connection().await.expect("connect");
    let mut h = Harness::cluster(fixture.seed(), fixture.clients()).await;
    h.fetch_page().await;

    // These hash to slots on three different masters.
    let channels = ["foo", "bar", "c"];
    for channel in channels {
        let target = Target::Shard(channel.into());
        h.state.pubsub.lock().add(target.clone());
        h.pubsub(PubSubCmd::Subscribe(target));
    }
    let store = h.state.pubsub.clone();
    for (index, channel) in channels.into_iter().enumerate() {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let heard: i64 = redis::cmd("SPUBLISH")
                .arg(channel)
                .arg("hi")
                .query_async(&mut admin)
                .await
                .expect("SPUBLISH");
            if heard > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{channel}: no owner"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        wait_until(channel, || store.lock().subs[index].stats.total >= 1).await;
        let guard = store.lock();
        let buffer = &guard.subs[index].buffer;
        let message = buffer.range(0, buffer.len()).next().expect("a message");
        assert_eq!(message.channel, channel);
        assert_eq!(message.payload.copy_text(), "hi");
    }

    // `bar` is on 7100: kill it, the subscription follows the new master.
    let before = store.lock().subs[1].stats.total;
    fixture.kill_node(7100);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        // One attempt may wait on the dead node: the deadline still holds.
        let mut publish = redis::cmd("SPUBLISH");
        publish.arg("bar").arg("after");
        let heard = tokio::time::timeout(
            Duration::from_secs(2),
            publish.query_async::<i64>(&mut admin),
        )
        .await;
        if matches!(heard, Ok(Ok(n)) if n > 0) && store.lock().subs[1].stats.total > before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no message after the failover"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    h.shutdown().await;
}

// ---- Streams ---------------------------------------------------------------

/// Open `key` with `page_size` rows and load the current page.
async fn load_stream(h: &mut Harness) -> redis_rover::internals::StreamValue {
    h.send(RedisEvent::FetchValue);
    h.expect("value", |a| *a == Action::LoadSelectedValueIntoView)
        .await;
    let loaded = h.state.selected_value.lock().clone().expect("a value");
    match loaded.value {
        KeyValue::Stream(stream) => stream,
        other => panic!("not a stream: {other:?}"),
    }
}

async fn xadd(admin: &mut redis::aio::ConnectionManager, key: &str, id: &str, value: &str) {
    let _: String = redis::cmd("XADD")
        .arg(key)
        .arg(id)
        .arg("field")
        .arg(value)
        .query_async(admin)
        .await
        .expect("XADD");
}

/// Pages cross a millisecond boundary (sequence 0), so the ID decrement is
/// exercised, on the oldest server image and the current one.
async fn stream_pages_on(tag: &str) {
    let server = support::start_redis_tag(tag).await;
    let mut admin = server.admin(0).await;
    for (i, id) in ["5-0", "5-1", "6-0", "6-1", "7-0", "7-1", "8-0"]
        .iter()
        .enumerate()
    {
        xadd(&mut admin, "s", id, &format!("v{i}")).await;
    }
    let mut h = Harness::new(server.client(0), false).await;
    {
        let mut value = h.state.value_state.lock();
        value.select("s", RedisType::Stream);
        value.page_size = 3;
    }
    let ids = |s: &redis_rover::internals::StreamValue| {
        s.entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
    };
    let first = load_stream(&mut h).await;
    assert_eq!(ids(&first), ["8-0", "7-1", "7-0"], "{tag}");
    assert_eq!((first.length, first.last_id.as_str()), (7, "8-0"));

    assert!(h.state.value_state.lock().next_page());
    assert_eq!(
        ids(&load_stream(&mut h).await),
        ["6-1", "6-0", "5-1"],
        "{tag}"
    );
    assert!(h.state.value_state.lock().next_page());
    assert_eq!(ids(&load_stream(&mut h).await), ["5-0"], "{tag}");
    assert!(!h.state.value_state.lock().next_page(), "the last page");

    // A new entry doesn't shift a page already reached.
    xadd(&mut admin, "s", "9-0", "new").await;
    assert!(h.state.value_state.lock().previous_page());
    assert_eq!(
        ids(&load_stream(&mut h).await),
        ["6-1", "6-0", "5-1"],
        "{tag}"
    );
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn stream_pages_across_three_pages() {
    stream_pages_on("7-alpine").await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn stream_pages_on_redis_6_0() {
    stream_pages_on("6.0-alpine").await;
}

async fn stream_groups_on(tag: &str, has_lag: bool) {
    let server = support::start_redis_tag(tag).await;
    let mut admin = server.admin(0).await;
    for id in ["1-0", "2-0", "3-0"] {
        xadd(&mut admin, "s", id, "v").await;
    }
    for group in ["billing", "audit"] {
        let () = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg("s")
            .arg(group)
            .arg("0")
            .query_async(&mut admin)
            .await
            .expect("XGROUP");
    }
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("billing")
        .arg("worker-1")
        .arg("COUNT")
        .arg(2)
        .arg("STREAMS")
        .arg("s")
        .arg(">")
        .query_async(&mut admin)
        .await
        .expect("XREADGROUP");

    let mut h = Harness::new(server.client(0), true).await;
    h.state.value_state.lock().select("s", RedisType::Stream);
    let stream = load_stream(&mut h).await;
    let groups = stream.groups.expect("groups");
    assert_eq!(groups.len(), 2, "{tag}");
    let billing = groups
        .iter()
        .find(|g| g.name == "billing")
        .expect("billing");
    assert_eq!((billing.consumers, billing.pending), (1, 2), "{tag}");
    assert_eq!(billing.last_delivered, "2-0", "{tag}");
    assert_eq!(billing.lag, has_lag.then_some(1), "{tag}");

    h.send(RedisEvent::FetchStreamGroup {
        key: "s".into(),
        group: "billing".into(),
    });
    h.expect("group", |a| *a == Action::StreamGroupLoaded).await;
    let detail = h.state.stream_group.lock().take().expect("detail");
    assert_eq!(detail.error, None);
    assert_eq!(detail.consumers.len(), 1);
    assert_eq!(
        (
            detail.consumers[0].name.as_str(),
            detail.consumers[0].pending
        ),
        ("worker-1", 2)
    );
    let pending: Vec<_> = detail.pending.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(pending, ["1-0", "2-0"]);
    assert_eq!(detail.pending[0].consumer, "worker-1");
    assert_eq!(detail.pending[0].deliveries, 1);
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn stream_groups_consumers_and_pending() {
    stream_groups_on("7-alpine", true).await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn stream_groups_on_redis_6_0() {
    stream_groups_on("6.0-alpine", false).await;
}

/// AC6: 10k entries and two groups: the newest page within 200 ms, and both
/// groups with their pending counts.
#[tokio::test]
#[ignore = "needs Docker"]
async fn stream_of_10k_opens_fast_with_both_groups() {
    let server = support::start_redis().await;
    let mut admin = server.admin(0).await;
    let mut pipe = redis::pipe();
    for i in 1..=10_000 {
        pipe.cmd("XADD")
            .arg("big")
            .arg(format!("{i}-0"))
            .arg("n")
            .arg(i)
            .arg("payload")
            .arg("some value")
            .ignore();
    }
    let () = pipe.query_async(&mut admin).await.expect("seed");
    for group in ["billing", "audit"] {
        let () = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg("big")
            .arg(group)
            .arg("0")
            .query_async(&mut admin)
            .await
            .expect("XGROUP");
    }
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("audit")
        .arg("w")
        .arg("COUNT")
        .arg(7)
        .arg("STREAMS")
        .arg("big")
        .arg(">")
        .query_async(&mut admin)
        .await
        .expect("XREADGROUP");

    let mut h = Harness::new(server.client(0), true).await;
    h.state.value_state.lock().select("big", RedisType::Stream);
    let started = Instant::now();
    let stream = load_stream(&mut h).await;
    let took = started.elapsed();
    eprintln!("newest page of 10k entries in {took:?}");
    assert!(took < Duration::from_millis(200), "took {took:?}");
    assert_eq!(stream.length, 10_000);
    assert_eq!(stream.entries[0].id, "10000-0");
    let groups = stream.groups.expect("groups");
    let pending = |name: &str| groups.iter().find(|g| g.name == name).map(|g| g.pending);
    assert_eq!((pending("billing"), pending("audit")), (Some(0), Some(7)));
    h.shutdown().await;
}
