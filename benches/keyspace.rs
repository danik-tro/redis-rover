//! Keyspace sweep benches: merging SCAN batches and a deep page turn.
//! Synthetic key names, no Redis.
//!
//! 10M keys runs only with `RROVER_BENCH_LARGE=1` (slow, about 2 GB of RAM).

mod support;

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BatchSize, Criterion, Throughput};
use redis_rover::internals::{KeyspaceState, SweepResult};

#[global_allocator]
static ALLOC: support::Counting = support::Counting;

/// Key names like `session:42:1000042`, about 20 bytes each.
fn key_names(n: usize) -> Vec<String> {
    const PREFIXES: [&str; 4] = ["user", "session", "order", "cache"];
    (0..n)
        .map(|i| format!("{}:{}:{i}", PREFIXES[i % PREFIXES.len()], i / 1000))
        .collect()
}

/// `n` names cut into SCAN-sized batches (`scan_count` from the defaults).
fn batches(n: usize) -> Vec<Vec<String>> {
    let scan_count = KeyspaceState::default().scan_count;
    let mut names = key_names(n).into_iter();
    let mut out = Vec::with_capacity(n.div_ceil(scan_count));
    loop {
        let batch: Vec<String> = names.by_ref().take(scan_count).collect();
        if batch.is_empty() {
            return out;
        }
        out.push(batch);
    }
}

/// Merge every batch into a fresh sweep. The last batch returns cursor 0 when
/// `finish` is set, so the sweep ends; otherwise the sweep stays open.
fn merge_all(batches: Vec<Vec<String>>, finish: bool) -> KeyspaceState {
    let mut state = KeyspaceState::default();
    let last = batches.len();
    for (i, batch) in batches.into_iter().enumerate() {
        let cursor = if finish && i + 1 == last { 0 } else { i + 1 };
        state.merge_batch(cursor, batch);
    }
    state
}

/// An `n`-key sweep that is not finished yet, on its last full page, the way
/// it is after paging far into a big keyspace. Pages are as big as a SCAN
/// batch, so every page turn from here needs exactly one batch.
fn deep_sweep(n: usize) -> KeyspaceState {
    let mut state = merge_all(batches(n), false);
    state.page_size = state.scan_count;
    state.page = (n / state.page_size).saturating_sub(1);
    state
}

/// `count` fresh SCAN batches of key names the sweep hasn't seen yet.
fn fresh_batches(state: &KeyspaceState, count: usize) -> Vec<Vec<String>> {
    let first = state.scanned.len();
    (0..count)
        .map(|b| {
            let from = first + b * state.scan_count;
            (from..from + state.scan_count)
                .map(|i| format!("fresh:{}:{i}", i / 1000))
                .collect()
        })
        .collect()
}

/// What the app and the runner do under the keyspace lock for one `n`:
/// `next_page`, take a `SweepRequest`, merge the SCAN answer, copy the page's
/// names for `fetch_meta`. The SCAN and `fetch_meta` I/O themselves are not
/// included.
fn page_turn(state: &mut KeyspaceState, batch: Vec<String>) -> Vec<String> {
    assert!(state.next_page());
    if let Some(request) = state.sweep_request() {
        let cursor = request.cursor + 1;
        assert!(state.apply_result(&request, SweepResult { cursor, batch }));
    }
    state.page_keys()
}

fn sizes() -> Vec<(usize, &'static str)> {
    let mut sizes = vec![(100_000, "100k"), (1_000_000, "1M")];
    if std::env::var_os("RROVER_BENCH_LARGE").is_some_and(|v| v == "1") {
        sizes.push((10_000_000, "10M"));
    }
    sizes
}

fn bench_merge_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_batch");
    group.sample_size(10);
    for (n, label) in sizes() {
        if support::is_bench_run() {
            // Generating the names is part of the window, so `retained` is
            // exactly what the finished sweep keeps (the batches are consumed).
            let (state, mem) = support::measure(|| merge_all(batches(n), true));
            assert_eq!(state.scanned.len(), n);
            drop(state);
            support::record(
                &format!("merge_batch/{label}"),
                mem,
                "retained = whole sweep",
            );
        }
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(label, |b| {
            b.iter_batched(
                || batches(n),
                |batches| merge_all(batches, true),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// A page turn deep into an `n`-key sweep. Each sample starts from a fresh
/// clone of the base sweep and turns `iters` pages forward; the clone and the
/// fresh key names are made outside the timing. Hash-table growth stays in,
/// as the real sweep pays it too.
fn bench_page_turn_deep(c: &mut Criterion) {
    let mut group = c.benchmark_group("page_turn_deep");
    group.sample_size(10);
    for (n, label) in sizes() {
        let mut base = deep_sweep(n);
        if support::is_bench_run() {
            // On the sweep as built, not a clone: a clone has no spare
            // capacity, so its first push would reallocate the whole list.
            let batch = fresh_batches(&base, 1).remove(0);
            let (page, mem) = support::measure(|| page_turn(&mut base, batch));
            assert_eq!(page.len(), base.page_size);
            drop(page);
            support::record(
                &format!("page_turn_deep/{label}"),
                mem,
                "allocated = one page turn",
            );
        }
        group.bench_function(label, |b| {
            b.iter_custom(|iters| {
                let mut state = base.clone();
                let count = usize::try_from(iters).expect("iters fits in usize");
                let batches = fresh_batches(&state, count);
                // Give the clone the spare room the real sweep has, so the
                // timing doesn't include copying the whole list once.
                state.scanned.reserve(count * state.scan_count);
                let mut elapsed = Duration::ZERO;
                for batch in batches {
                    let started = Instant::now();
                    let page = page_turn(black_box(&mut state), batch);
                    elapsed += started.elapsed();
                    drop(black_box(page));
                }
                elapsed
            });
        });
    }
    group.finish();
}

fn main() {
    support::use_default_config();
    let mut c = Criterion::default().configure_from_args();
    bench_merge_batch(&mut c);
    bench_page_turn_deep(&mut c);
    c.final_summary();
}
