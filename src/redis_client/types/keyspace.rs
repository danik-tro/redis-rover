use std::hash::BuildHasher;
use std::sync::LazyLock;

use hashbrown::HashTable;

/// Default number of keys shown per page.
pub const DEFAULT_PAGE_SIZE: usize = 10;

/// Default `SCAN ... COUNT` hint. Larger than the page size so a selective
/// `MATCH` pattern on a big keyspace needs fewer round trips per page.
pub const DEFAULT_SCAN_COUNT: usize = 100;

/// Keyspace browsing state: pattern plus client-side paging over a single
/// `SCAN` sweep.
///
/// Paging is done client-side rather than with a stack of raw SCAN cursors:
/// cursors aren't stable across writes (rehashing can move keys between
/// buckets), so walking back through remembered cursors could skip or repeat
/// keys, and a single SCAN step may return zero or duplicate keys. Instead the
/// sweep accumulates the (deduplicated) key names it has seen so far, and a page
/// is just a `page_size` slice of that list — back-navigation is exact and every
/// page but the last is full. The sweep only advances as far as the current page
/// needs, so memory is bounded by how far the user has paged.
#[derive(Debug, Clone)]
pub struct KeyspaceState {
    pub pattern: Option<String>,
    /// Keys shown per page.
    pub page_size: usize,
    /// `COUNT` hint sent with every `SCAN` step.
    pub scan_count: usize,
    /// Current page, 0-based.
    pub page: usize,
    /// Key names discovered so far, in SCAN order, deduplicated. Each name is
    /// stored once, here; indices into it stay stable until the next rescan.
    pub scanned: Vec<String>,
    /// Dedup index over `scanned` (#46): which names it already holds.
    seen: HashTable<SeenSlot>,
    /// Hashes a key name for `seen`. A field so tests can force collisions.
    hash: fn(&str) -> u32,
    /// Cursor for the next `SCAN` step of the sweep.
    pub sweep_cursor: usize,
    /// The sweep reached the end (SCAN returned cursor 0).
    pub sweep_done: bool,
    /// Bumped on every page / pattern / rescan change, so the background
    /// fetcher can drop results computed for a state the user already left.
    pub generation: u64,
    /// Bumped only when the sweep restarts (rescan, new pattern, DB switch).
    /// A SCAN result from the same epoch still extends the sweep even if the
    /// user turned the page meanwhile.
    sweep_epoch: u64,
}

/// One `KeyspaceState::seen` entry: where the name sits in `scanned`, plus
/// 32 bits of its hash. Keeping the hash means the table never re-reads the
/// names when it grows; a matching hash is still confirmed by comparing names.
#[derive(Debug, Clone, Copy)]
struct SeenSlot {
    index: u32,
    hash: u32,
}

/// 32-bit hash of a key name. Seeded once per process.
fn hash_name(name: &str) -> u32 {
    static STATE: LazyLock<hashbrown::DefaultHashBuilder> = LazyLock::new(Default::default);
    // Truncation is the point: the low 32 bits are all `SeenSlot` keeps.
    #[allow(clippy::cast_possible_truncation)]
    let hash = STATE.hash_one(name) as u32;
    hash
}

/// Spread a 32-bit hash over the 64 bits the table probes with.
fn table_hash(hash: u32) -> u64 {
    u64::from(hash).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// What the runner needs to advance the sweep, copied out under the lock so
/// the sweep itself is never cloned (#45).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepRequest {
    pub pattern: Option<String>,
    /// Cursor to continue the sweep from.
    pub cursor: usize,
    /// `COUNT` hint for every `SCAN` step.
    pub scan_count: usize,
    /// New keys the current page still needs. The runner can't dedup against
    /// keys it doesn't hold, so it stops once it has this many raw keys.
    pub need: usize,
    /// `KeyspaceState::generation` when the request was made.
    pub generation: u64,
    epoch: u64,
}

/// Raw keys from one or more `SCAN` steps, in order, and the cursor after the
/// last step. Duplicates are settled by [`KeyspaceState::apply_result`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub cursor: usize,
    pub batch: Vec<String>,
}

impl SweepResult {
    /// SCAN returned cursor 0: the sweep is over.
    #[must_use]
    pub fn done(&self) -> bool {
        self.cursor == 0
    }
}

impl KeyspaceState {
    /// Range of `scanned` covered by the current page (clamped to what's known).
    #[must_use]
    pub fn page_range(&self) -> std::ops::Range<usize> {
        let start = (self.page * self.page_size).min(self.scanned.len());
        let end = (start + self.page_size).min(self.scanned.len());
        start..end
    }

    /// Number of key names the sweep must have found to fill the current page.
    #[must_use]
    pub fn keys_needed(&self) -> usize {
        (self.page + 1) * self.page_size
    }

    /// The sweep still needs to `SCAN` further to fill the current page.
    #[must_use]
    pub fn needs_scan(&self) -> bool {
        !self.sweep_done && self.scanned.len() < self.keys_needed()
    }

    #[must_use]
    pub fn has_next_page(&self) -> bool {
        self.scanned.len() > self.page_range().end || !self.sweep_done
    }

    #[must_use]
    pub fn has_previous_page(&self) -> bool {
        self.page > 0
    }

    /// Advance one page. Returns `false` (and changes nothing) on the last page.
    pub fn next_page(&mut self) -> bool {
        if !self.has_next_page() {
            return false;
        }
        self.page += 1;
        self.generation += 1;
        true
    }

    /// Go back one page. Returns `false` (and changes nothing) on the first page.
    pub fn previous_page(&mut self) -> bool {
        if !self.has_previous_page() {
            return false;
        }
        self.page -= 1;
        self.generation += 1;
        true
    }

    /// Change how many keys a page shows (the table was resized). Lands on the
    /// page that holds key number `anchor` (0-based, in sweep order), so the
    /// user's place stays on screen. The sweep is reused; only the page's
    /// metadata must be fetched again (bumps `generation`). Returns `false`
    /// (and changes nothing) when the size is unchanged or zero.
    pub fn set_page_size(&mut self, page_size: usize, anchor: usize) -> bool {
        if page_size == 0 || page_size == self.page_size {
            return false;
        }
        self.page_size = page_size;
        self.page = anchor / page_size;
        self.generation += 1;
        true
    }

    /// Restart the sweep from scratch, keeping the current page, so writes
    /// (new / deleted keys) are picked up without jumping back to page 1.
    pub fn rescan(&mut self) {
        self.scanned.clear();
        self.seen.clear();
        self.sweep_cursor = 0;
        self.sweep_done = false;
        self.generation += 1;
        self.sweep_epoch += 1;
    }

    /// Start over after a DB switch: keep the filter, go back to page 1 and
    /// drop the old DB's sweep (bumps `generation`).
    pub fn reset_for_db(&mut self) {
        self.page = 0;
        self.rescan();
    }

    pub fn set_pattern(&mut self, pattern: Option<String>) {
        self.pattern = pattern;
        self.page = 0;
        self.rescan();
    }

    pub fn delete_pattern(&mut self) {
        self.set_pattern(None);
    }

    /// Merge one `SCAN` step into the sweep, skipping keys already seen (SCAN
    /// may return a key more than once). A returned cursor of 0 ends the sweep.
    pub fn merge_batch(&mut self, cursor: usize, keys: Vec<String>) {
        let mut full = false;
        for key in keys {
            let hash = (self.hash)(&key);
            let scanned = &self.scanned;
            let known = self.seen.find(table_hash(hash), |slot| {
                slot.hash == hash && scanned[slot.index as usize] == key
            });
            if known.is_some() {
                continue;
            }
            let Ok(index) = u32::try_from(self.scanned.len()) else {
                log::error!(
                    "Keyspace sweep stopped at {} keys: the index is full",
                    self.scanned.len()
                );
                full = true;
                break;
            };
            self.seen
                .insert_unique(table_hash(hash), SeenSlot { index, hash }, |slot| {
                    table_hash(slot.hash)
                });
            self.scanned.push(key);
        }
        self.sweep_cursor = cursor;
        self.sweep_done = cursor == 0 || full;
    }

    /// The next `SCAN` work the current page needs, or `None` when the sweep
    /// already covers it (or has ended).
    #[must_use]
    pub fn sweep_request(&self) -> Option<SweepRequest> {
        if !self.needs_scan() {
            return None;
        }
        Some(SweepRequest {
            pattern: self.pattern.clone(),
            cursor: self.sweep_cursor,
            scan_count: self.scan_count,
            need: self.keys_needed() - self.scanned.len(),
            generation: self.generation,
            epoch: self.sweep_epoch,
        })
    }

    /// Merge the runner's answer to `request`. Returns `false`, and changes
    /// nothing, when the sweep moved on meanwhile: it restarted (rescan, new
    /// pattern, DB switch), ended, or no longer sits at the request's cursor.
    /// A page turn alone doesn't make the result stale.
    pub fn apply_result(&mut self, request: &SweepRequest, result: SweepResult) -> bool {
        if request.epoch != self.sweep_epoch
            || self.sweep_done
            || request.cursor != self.sweep_cursor
        {
            return false;
        }
        self.merge_batch(result.cursor, result.batch);
        true
    }

    /// Copy of the current page's key names: at most `page_size` of them.
    #[must_use]
    pub fn page_keys(&self) -> Vec<String> {
        self.scanned[self.page_range()].to_vec()
    }

    /// If the finished sweep doesn't reach the current page (keys were deleted
    /// since the user paged there), fall back to the last non-empty page.
    pub fn clamp_page(&mut self) {
        if !self.sweep_done || self.page_size == 0 {
            return;
        }
        let last_page = self.scanned.len().saturating_sub(1) / self.page_size;
        self.page = self.page.min(last_page);
    }
}

impl Default for KeyspaceState {
    fn default() -> Self {
        Self {
            pattern: None,
            page_size: DEFAULT_PAGE_SIZE,
            scan_count: DEFAULT_SCAN_COUNT,
            page: 0,
            scanned: Vec::new(),
            seen: HashTable::new(),
            hash: hash_name,
            sweep_cursor: 0,
            sweep_done: false,
            generation: 0,
            sweep_epoch: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::redis_client::types::tests::keys;

    #[test]
    fn reset_for_db_keeps_pattern_and_goes_to_first_page() {
        let mut state = state_with(10);
        state.set_pattern(Some("user:*".into()));
        state.merge_batch(0, keys(0..30));
        state.next_page();
        let generation = state.generation;

        state.reset_for_db();
        assert_eq!(state.page, 0);
        assert_eq!(state.pattern.as_deref(), Some("user:*"));
        assert!(state.scanned.is_empty());
        assert!(state.generation > generation);
    }

    fn state_with(page_size: usize) -> KeyspaceState {
        KeyspaceState {
            page_size,
            ..KeyspaceState::default()
        }
    }

    #[test]
    fn page_range_slices_scanned_keys() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..25));

        assert_eq!(state.page_range(), 0..10);
        assert!(state.next_page());
        assert_eq!(state.page_range(), 10..20);
        assert!(state.next_page());
        assert_eq!(state.page_range(), 20..25);
        assert!(!state.has_next_page());
        assert!(!state.next_page());
        assert_eq!(state.page, 2);
    }

    #[test]
    fn previous_page_stops_at_first() {
        let mut state = state_with(10);
        assert!(!state.previous_page());
        state.merge_batch(5, keys(0..10));
        assert!(state.next_page());
        assert!(state.previous_page());
        assert_eq!(state.page, 0);
    }

    #[test]
    fn unfinished_sweep_has_next_page() {
        let mut state = state_with(10);
        state.merge_batch(42, keys(0..10));
        assert!(!state.needs_scan());
        assert!(state.has_next_page());
        assert!(state.next_page());
        assert!(state.needs_scan());
        assert_eq!(state.keys_needed(), 20);
    }

    #[test]
    fn merge_batch_dedups_and_tracks_cursor() {
        let mut state = state_with(10);
        state.merge_batch(7, vec!["a".into(), "b".into()]);
        state.merge_batch(0, vec!["b".into(), "c".into(), "a".into()]);

        assert_eq!(state.scanned, vec!["a", "b", "c"]);
        assert!(state.sweep_done);
        assert_eq!(state.sweep_cursor, 0);
    }

    #[test]
    fn rescan_keeps_page_and_set_pattern_resets_it() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..30));
        state.next_page();
        let generation = state.generation;

        state.rescan();
        assert_eq!(state.page, 1);
        assert!(state.scanned.is_empty());
        assert!(!state.sweep_done);
        assert!(state.generation > generation);

        state.set_pattern(Some("k1*".into()));
        assert_eq!(state.page, 0);
        assert_eq!(state.pattern.as_deref(), Some("k1*"));
    }

    fn result(cursor: usize, batch: Vec<String>) -> SweepResult {
        SweepResult { cursor, batch }
    }

    #[test]
    fn sweep_request_none_when_page_is_covered() {
        let mut state = state_with(10);
        state.merge_batch(5, keys(0..10));
        assert_eq!(state.sweep_request(), None, "page 1 is full");

        let mut done = state_with(10);
        done.merge_batch(0, keys(0..3));
        assert_eq!(done.sweep_request(), None, "sweep ended short");
    }

    #[test]
    fn sweep_request_needs_only_missing_keys() {
        let mut state = state_with(10);
        state.set_pattern(Some("k*".into()));
        state.merge_batch(42, keys(0..14));
        assert!(state.next_page());

        let request = state.sweep_request().expect("page 2 needs keys");
        assert_eq!(request.cursor, 42);
        assert_eq!(request.need, 6);
        assert_eq!(request.pattern.as_deref(), Some("k*"));
        assert_eq!(request.scan_count, DEFAULT_SCAN_COUNT);
        assert_eq!(request.generation, state.generation);
    }

    #[test]
    fn apply_result_merges_and_moves_cursor() {
        let mut state = state_with(10);
        let request = state.sweep_request().expect("empty sweep needs keys");
        assert!(state.apply_result(&request, result(17, keys(0..12))));
        assert_eq!(state.sweep_cursor, 17);
        assert!(!state.sweep_done);
        assert_eq!(state.page_range(), 0..10);
        assert_eq!(state.sweep_request(), None);

        // Applying the same answer twice is refused: the cursor moved.
        assert!(!state.apply_result(&request, result(99, keys(50..60))));
        assert_eq!(state.scanned.len(), 12);
    }

    #[test]
    fn apply_result_drops_stale_epoch() {
        let mut state = state_with(10);
        let request = state.sweep_request().expect("needs keys");
        // The user changes the filter while the SCAN is in flight.
        state.set_pattern(Some("user:*".into()));
        assert!(!state.apply_result(&request, result(5, keys(0..10))));
        assert!(state.scanned.is_empty());
        assert_eq!(state.sweep_cursor, 0);

        let mut state = state_with(10);
        let request = state.sweep_request().expect("needs keys");
        state.rescan();
        assert!(!state.apply_result(&request, result(5, keys(0..10))));
        assert!(state.scanned.is_empty());
    }

    #[test]
    fn apply_result_keeps_progress_when_only_page_changed() {
        let mut state = state_with(10);
        state.merge_batch(3, keys(0..10));
        assert!(state.next_page());
        let request = state.sweep_request().expect("page 2 needs keys");
        // The user presses `n` again before the SCAN answer lands.
        assert!(state.next_page());
        assert_ne!(state.generation, request.generation);

        assert!(state.apply_result(&request, result(8, keys(10..20))));
        assert_eq!(state.scanned.len(), 20);
        assert_eq!(state.sweep_cursor, 8);
        let next = state.sweep_request().expect("page 3 still needs keys");
        assert_eq!(next.cursor, 8);
        assert_eq!(next.need, 10);
    }

    #[test]
    fn duplicates_leave_page_short_then_request_again() {
        let mut state = state_with(10);
        state.merge_batch(4, keys(0..10));
        assert!(state.next_page());

        // 10 raw keys, but SCAN repeated six of page 1's.
        let request = state.sweep_request().expect("page 2 needs keys");
        assert_eq!(request.need, 10);
        let mut batch = keys(0..6);
        batch.extend(keys(10..14));
        assert!(state.apply_result(&request, result(9, batch)));
        assert_eq!(state.scanned.len(), 14, "repeats are merged once");

        let again = state.sweep_request().expect("page 2 is still short");
        assert_eq!(again.cursor, 9);
        assert_eq!(again.need, 6);
        assert!(state.apply_result(&again, result(0, keys(10..20))));
        assert_eq!(state.page_keys(), keys(10..20));
        assert_eq!(state.sweep_request(), None);
    }

    #[test]
    fn page_keys_copies_only_the_page() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..25));
        assert_eq!(state.page_keys(), keys(0..10));
        state.next_page();
        state.next_page();
        assert_eq!(state.page_keys(), keys(20..25));
        assert!(state_with(10).page_keys().is_empty());
    }

    #[test]
    fn merge_batch_keeps_both_keys_on_hash_collision() {
        // Every name hashes the same: only the name compare tells them apart.
        let mut state = KeyspaceState {
            hash: |_| 7,
            ..state_with(10)
        };
        state.merge_batch(3, vec!["a".into(), "b".into(), "a".into()]);
        state.merge_batch(0, vec!["c".into(), "b".into(), "d".into()]);
        assert_eq!(state.scanned, vec!["a", "b", "c", "d"]);

        // Some collide (same length), some don't.
        let mut state = KeyspaceState {
            #[allow(clippy::cast_possible_truncation)]
            hash: |name| name.len() as u32,
            ..state_with(10)
        };
        state.merge_batch(0, keys(0..20).into_iter().chain(keys(5..15)).collect());
        assert_eq!(state.scanned, keys(0..20));
    }

    #[test]
    fn rescan_clears_the_index() {
        let mut state = state_with(10);
        state.merge_batch(5, keys(0..10));
        state.rescan();
        state.merge_batch(0, keys(0..10));
        assert_eq!(state.scanned, keys(0..10), "old names don't count as seen");
    }

    #[test]
    fn index_survives_growth() {
        let mut state = state_with(10);
        for b in 0..100 {
            state.merge_batch(b + 1, keys(b * 100..(b + 1) * 100));
        }
        // Every name again, after many table resizes: all repeats.
        state.merge_batch(0, keys(0..10_000));
        assert_eq!(state.scanned.len(), 10_000);
        assert_eq!(state.scanned, keys(0..10_000));
    }

    #[test]
    fn clamp_page_falls_back_after_deletes() {
        let mut state = state_with(10);
        state.page = 3;
        state.merge_batch(0, keys(0..15));
        state.clamp_page();
        assert_eq!(state.page, 1);

        let mut empty = state_with(10);
        empty.page = 2;
        empty.merge_batch(0, Vec::new());
        empty.clamp_page();
        assert_eq!(empty.page, 0);
    }

    #[test]
    fn set_page_size_keeps_anchor_on_screen() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..100));
        state.next_page();
        state.next_page();
        // Key 23 is on page 2 (20..30).
        let generation = state.generation;

        assert!(state.set_page_size(7, 23));
        assert_eq!(state.page, 3);
        assert!(state.page_range().contains(&23));
        assert!(state.generation > generation);

        let generation = state.generation;
        assert!(!state.set_page_size(7, 23), "same size is a no-op");
        assert!(!state.set_page_size(0, 23), "zero is ignored");
        assert_eq!(state.generation, generation);

        assert!(state.set_page_size(40, 23));
        assert_eq!(state.page, 0);
        assert_eq!(state.page_range(), 0..40);
    }

    #[test]
    fn set_page_size_past_end_clamps_once_swept() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..15));
        assert!(state.set_page_size(4, 14));
        assert_eq!(state.page, 3);
        state.clamp_page();
        assert_eq!(state.page_range(), 12..15);
    }

    #[test]
    fn clamp_page_ignores_unfinished_sweep() {
        let mut state = state_with(10);
        state.page = 3;
        state.merge_batch(9, keys(0..15));
        state.clamp_page();
        assert_eq!(state.page, 3);
    }
}
