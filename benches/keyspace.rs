//! Keyspace sweep benches: merging SCAN batches, a deep page turn, and
//! applying a sweep. Synthetic key names, no Redis.
//!
//! 10M keys runs only with `RROVER_BENCH_LARGE=1` (slow, about 2 GB of RAM).

mod support;

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput};
use redis_rover::internals::KeyspaceState;

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

/// A 1M-key sweep that is not finished yet, sitting on a deep page (the
/// second to last), the way it is after paging far into a big keyspace.
fn deep_sweep(n: usize) -> KeyspaceState {
    let mut state = merge_all(batches(n), false);
    state.page = (n / state.page_size).saturating_sub(2);
    state
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

/// What `RedisEvent::FetchKeys` does today before any I/O:
/// `keyspace_state.lock().clone()`, then the page moves on. The clone is
/// dropped inside the timing, as the real snapshot is.
fn bench_page_turn_deep(c: &mut Criterion) {
    let base = deep_sweep(1_000_000);
    if support::is_bench_run() {
        let (snapshot, mem) = support::measure(|| {
            let mut snapshot = base.clone();
            snapshot.next_page();
            snapshot
        });
        drop(snapshot);
        support::record("page_turn_deep/1M", mem, "allocated = one snapshot clone");
    }
    let mut group = c.benchmark_group("page_turn_deep");
    group.sample_size(10);
    group.bench_function("1M", |b| {
        b.iter(|| {
            let mut snapshot = black_box(&base).clone();
            black_box(snapshot.next_page());
            snapshot
        });
    });
    group.finish();
}

/// The runner writing a finished snapshot back: `apply_sweep` replaces the
/// 1M-key sweep held in shared state with the one the fetch advanced.
fn bench_apply_sweep(c: &mut Criterion) {
    let base = deep_sweep(1_000_000);
    if support::is_bench_run() {
        let (mut current, next) = (base.clone(), base.clone());
        let ((), mem) = support::measure(|| current.apply_sweep(next));
        drop(current);
        support::record(
            "apply_sweep/1M",
            mem,
            "retained < 0: the old sweep is freed",
        );
    }
    let mut group = c.benchmark_group("apply_sweep");
    group.sample_size(10);
    group.bench_function("1M", |b| {
        b.iter_batched(
            || (base.clone(), base.clone()),
            |(mut current, next)| {
                current.apply_sweep(next);
                current
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn main() {
    support::use_default_config();
    let mut c = Criterion::default().configure_from_args();
    bench_merge_batch(&mut c);
    bench_page_turn_deep(&mut c);
    bench_apply_sweep(&mut c);
    c.final_summary();
}
