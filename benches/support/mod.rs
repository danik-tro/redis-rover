//! Shared bench helpers: a counting allocator for memory numbers, and setup
//! that keeps the user's config out of the measurements.
//!
//! Memory is measured once per bench, outside criterion's timing loop, and
//! only in a real bench run (`cargo bench`), not when the bench binaries run in
//! test mode (`cargo test --benches`, nextest).

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};

/// Wraps the system allocator and counts live, peak and total bytes.
pub struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

fn on_alloc(size: usize) {
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
    TOTAL.fetch_add(size, Relaxed);
}

fn on_dealloc(size: usize) {
    LIVE.fetch_sub(size, Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; only counters are
// updated around it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            on_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        on_dealloc(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            on_dealloc(layout.size());
            on_alloc(new_size);
        }
        new_ptr
    }
}

/// Heap use of one run of a closure.
#[derive(Debug, Clone, Copy)]
pub struct Mem {
    /// Bytes still allocated afterwards (what the result keeps alive).
    /// Negative when the closure freed more than it kept.
    pub retained: i128,
    /// Highest heap use above the starting point while it ran.
    pub peak: usize,
    /// All bytes allocated while it ran, freed or not.
    pub allocated: usize,
}

/// Run `f` once and count its heap use. The result is returned, so what it
/// holds counts as retained.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Mem) {
    let live_before = LIVE.load(Relaxed);
    let total_before = TOTAL.load(Relaxed);
    PEAK.store(live_before, Relaxed);
    let out = f();
    let live_after = LIVE.load(Relaxed);
    let mem = Mem {
        retained: i128::try_from(live_after).unwrap_or(i128::MAX)
            - i128::try_from(live_before).unwrap_or(i128::MAX),
        peak: PEAK.load(Relaxed).saturating_sub(live_before),
        allocated: TOTAL.load(Relaxed).saturating_sub(total_before),
    };
    (out, mem)
}

/// True in `cargo bench` (criterion gets `--bench`), false in test mode.
pub fn is_bench_run() -> bool {
    std::env::args().any(|arg| arg == "--bench")
}

/// Save a memory result for `scripts/bench.sh`, as one small JSON file per
/// bench id under `target/criterion/rrover-memory/`.
pub fn record(id: &str, mem: Mem, note: &str) {
    let dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| PathBuf::from("target"), PathBuf::from)
        .join("criterion")
        .join("rrover-memory");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("memory: can't create {}: {err}", dir.display());
        return;
    }
    let mut json = String::new();
    let _ = write!(
        json,
        r#"{{"id":"{id}","retained":{},"peak":{},"allocated":{},"note":"{note}"}}"#,
        mem.retained, mem.peak, mem.allocated
    );
    let path = dir.join(format!("{}.json", id.replace('/', "__")));
    if let Err(err) = std::fs::write(&path, json) {
        eprintln!("memory: can't write {}: {err}", path.display());
    }
    eprintln!(
        "memory {id}: retained {} B, peak {} B, allocated {} B",
        mem.retained, mem.peak, mem.allocated
    );
}

/// Point the app's config lookup at an empty directory, so benches always run
/// with the bundled defaults, whatever the user has configured. Must run
/// before the first `config::get()`.
pub fn use_default_config() {
    let empty = std::env::temp_dir().join("rrover-bench-no-config");
    // The variable name comes from the crate name: `REDIS_ROVER_` for the
    // library, `RROVER_` for code that was built into the binary before.
    std::env::set_var("REDIS_ROVER_CONFIG", &empty);
    std::env::set_var("RROVER_CONFIG", &empty);
}
