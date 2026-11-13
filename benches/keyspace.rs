//! Keyspace sweep benches: merging SCAN batches, a deep page turn, and
//! sorting the loaded keys. Synthetic key names, no Redis.
//!
//! 10M keys runs only with `RROVER_BENCH_LARGE=1` (slow, about 2 GB of RAM).

mod support;

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BatchSize, Criterion, Throughput};
use redis_rover::internals::{KeyspaceState, Sort, SortKey, SweepApply, SweepResult};

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
        let applied = state.apply_result(&request, SweepResult { cursor, batch });
        assert_eq!(applied, SweepApply::Applied);
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

const BY_NAME: Sort = Sort {
    key: SortKey::Name,
    descending: false,
};

/// A finished `n`-key sweep with a name sort set but not built yet.
fn unsorted(n: usize) -> KeyspaceState {
    let mut state = merge_all(batches(n), true);
    state.set_sort(Some(BY_NAME));
    state
}

/// A full re-sort by name of an `n`-key sweep, as the runner does it: copy
/// the names under the lock (`sort_job`), sort off it, install the order.
fn bench_sort_names(c: &mut Criterion) {
    let mut group = c.benchmark_group("sort_names");
    group.sample_size(10);
    for (n, label) in sizes() {
        let base = unsorted(n);
        if support::is_bench_run() {
            let mut state = base.clone();
            let ((), mem) = support::measure(|| {
                let job = state.sort_job().expect("sort pending");
                assert!(state.install_sorted(job.run()));
            });
            support::record(&format!("sort_names/{label}"), mem, "retained = the order");
        }
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(label, |b| {
            b.iter_batched(
                || base.clone(),
                |mut state| {
                    let job = state.sort_job().expect("sort pending");
                    assert!(state.install_sorted(job.run()));
                    state
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// The part of a re-sort that holds the keyspace lock: copying the names.
fn bench_sort_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("sort_snapshot");
    group.sample_size(10);
    for (n, label) in sizes() {
        let base = unsorted(n);
        group.bench_function(label, |b| {
            b.iter(|| black_box(base.sort_job()));
        });
    }
    group.finish();
}

/// One 1,000-key SCAN answer merged into a name-sorted `n`-key sweep: dedup,
/// then the sorted run merged into the order. Each sample starts from a clone
/// of the sorted base and merges `iters` fresh batches.
fn bench_merge_sorted(c: &mut Criterion) {
    const BATCH: usize = 1000;
    let mut group = c.benchmark_group("merge_sorted_1000");
    group.sample_size(10);
    for (n, label) in sizes() {
        let mut base = unsorted(n);
        let job = base.sort_job().expect("sort pending");
        assert!(base.install_sorted(job.run()));
        group.bench_function(label, |b| {
            b.iter_custom(|iters| {
                let mut state = base.clone();
                let count = usize::try_from(iters).expect("iters fits in usize");
                let first = state.scanned.len();
                let batches: Vec<Vec<String>> = (0..count)
                    .map(|b| {
                        let from = first + b * BATCH;
                        (from..from + BATCH)
                            .map(|i| format!("order:{}:x{i}", i % 997))
                            .collect()
                    })
                    .collect();
                state.scanned.reserve(count * BATCH);
                let mut elapsed = Duration::ZERO;
                for batch in batches {
                    let started = Instant::now();
                    black_box(&mut state).merge_batch(1, batch);
                    elapsed += started.elapsed();
                }
                elapsed
            });
        });
    }
    group.finish();
}

/// Building the tree's visible rows over an `n`-key name order, as a fold
/// does: open the first top-level group (a quarter of the keys, 1,000-key
/// subgroups), then close it again. Two rebuilds per iteration.
fn bench_tree_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_build");
    group.sample_size(10);
    for (n, label) in sizes() {
        let mut state = unsorted(n);
        state.set_sort(None);
        state.toggle_tree(":");
        let job = state.sort_job().expect("name order pending");
        assert!(state.install_sorted(job.run()));
        group.bench_function(label, |b| {
            b.iter(|| {
                assert!(state.set_group(0, Some(true)).is_some());
                assert!(state.set_group(0, Some(false)).is_some());
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
    bench_sort_names(&mut c);
    bench_sort_snapshot(&mut c);
    bench_merge_sorted(&mut c);
    bench_tree_build(&mut c);
    c.final_summary();
}
