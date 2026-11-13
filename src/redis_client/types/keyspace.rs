use std::hash::BuildHasher;
use std::sync::LazyLock;

use hashbrown::HashTable;

use super::{
    tree::{PageRow, Tree, TreeRow},
    type_filter::{TypeFilter, TypeScan},
    view::{KeyView, MetaColumns, Sort, SortJob, SortedOrder, Swept, SYNC_SORT_MAX},
    KeyMeta,
};

/// Default number of keys shown per page.
pub const DEFAULT_PAGE_SIZE: usize = 10;

/// Default `SCAN ... COUNT` hint. Larger than the page size so a selective
/// `MATCH` pattern on a big keyspace needs fewer round trips per page.
pub const DEFAULT_SCAN_COUNT: usize = 100;

/// `COUNT` of each SCAN step a load-all takes (at least `scan_count`).
pub const LOAD_ALL_COUNT: usize = 1000;

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
    /// Display order of the loaded keys (SCAN order unless sorted).
    view: KeyView,
    /// Type / TTL / size per key, parallel to `scanned`. Allocated the first
    /// time the user sorts by one of them.
    meta: Option<MetaColumns>,
    /// The metadata backfill waits while the user is on another screen.
    backfill_paused: bool,
    /// Only keys of these types enter the sweep (empty: all).
    type_filter: TypeFilter,
    /// The server accepts `SCAN … TYPE` (until it rejects it once).
    server_type_scan: bool,
    /// Tree mode: raw keys to load. The tree groups what is loaded and
    /// never scans just to fill a page with groups; `n` past the last row
    /// raises it by a page.
    tree_target: usize,
    /// A key to bring on screen once the order is built
    /// ([`Self::reveal`]).
    reveal: Option<String>,
}

/// Keys whose metadata the backfill fetches next, copied out under the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillRequest {
    pub indices: Vec<u32>,
    pub names: Vec<String>,
    epoch: u64,
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
    /// How the SCAN steps apply the type filter.
    pub types: TypeScan,
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

/// What [`KeyspaceState::apply_result`] did with a SCAN answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepApply {
    /// Merged into the sweep.
    Applied,
    /// The sweep restarted (rescan, new pattern, DB switch): dropped.
    Stale,
    /// The same sweep moved on without this answer (another SCAN advanced
    /// it first): dropped, ask again from the new cursor.
    Raced,
}

impl SweepResult {
    /// SCAN returned cursor 0: the sweep is over.
    #[must_use]
    pub fn done(&self) -> bool {
        self.cursor == 0
    }
}

impl KeyspaceState {
    /// Number of keys the list shows over the loaded part of the sweep.
    #[must_use]
    pub fn view_len(&self) -> usize {
        match self.view.tree() {
            Some(tree) if self.view.order().is_some() => tree.rows().len(),
            _ => self.scanned.len(),
        }
    }

    /// What the sweep's progress is measured in: raw keys in tree mode,
    /// rows otherwise.
    fn loaded(&self) -> usize {
        if self.view.tree().is_some() {
            self.scanned.len()
        } else {
            self.view_len()
        }
    }

    /// Positions in the list (display order) covered by the current page,
    /// clamped to what's loaded. In SCAN order these are indices into
    /// `scanned`.
    #[must_use]
    pub fn page_range(&self) -> std::ops::Range<usize> {
        let start = (self.page * self.page_size).min(self.view_len());
        let end = (start + self.page_size).min(self.view_len());
        start..end
    }

    /// Number of key names the sweep must have found to fill the current page.
    #[must_use]
    pub fn keys_needed(&self) -> usize {
        if self.view.tree().is_some() {
            self.tree_target
        } else {
            (self.page + 1) * self.page_size
        }
    }

    /// The sweep still needs to `SCAN` further to fill the current page.
    #[must_use]
    pub fn needs_scan(&self) -> bool {
        !self.sweep_done && self.loaded() < self.keys_needed()
    }

    #[must_use]
    pub fn has_next_page(&self) -> bool {
        self.view_len() > self.page_range().end || !self.sweep_done
    }

    #[must_use]
    pub fn has_previous_page(&self) -> bool {
        self.page > 0
    }

    /// Advance one page. Returns `false` (and changes nothing) on the last page.
    /// In tree mode, past the last row it loads another page of raw keys
    /// instead (the page stays).
    pub fn next_page(&mut self) -> bool {
        if !self.has_next_page() {
            return false;
        }
        if self.view.tree().is_some() && self.view_len() <= self.page_range().end {
            self.tree_target = self.scanned.len() + self.page_size;
        } else {
            self.page += 1;
        }
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
        self.view.reset();
        self.refresh_tree();
        if let Some(meta) = self.meta.as_mut() {
            meta.clear();
        }
        self.tree_target = self.tree_target.max(self.page_size);
    }

    /// Back to page 1 of a new sweep (pattern, type filter, DB).
    fn restart_from_top(&mut self) {
        self.page = 0;
        self.tree_target = self.page_size;
        self.rescan();
    }

    /// Start over after a DB switch: keep the filter, go back to page 1 and
    /// drop the old DB's sweep (bumps `generation`).
    pub fn reset_for_db(&mut self) {
        self.restart_from_top();
    }

    pub fn set_pattern(&mut self, pattern: Option<String>) {
        self.pattern = pattern;
        self.restart_from_top();
    }

    pub fn delete_pattern(&mut self) {
        self.set_pattern(None);
    }

    #[must_use]
    pub fn type_filter(&self) -> &TypeFilter {
        &self.type_filter
    }

    /// Show only keys of these types (empty: all). Restarts the sweep from
    /// page 1, like a new pattern. Returns `false` when nothing changed.
    pub fn set_type_filter(&mut self, filter: TypeFilter) -> bool {
        if self.type_filter == filter {
            return false;
        }
        self.type_filter = filter;
        self.restart_from_top();
        true
    }

    /// The server rejected `SCAN … TYPE` (before Redis 6): filter on the
    /// client from now on.
    pub fn disable_server_type_scan(&mut self) {
        self.server_type_scan = false;
    }

    fn type_scan(&self) -> TypeScan {
        if self.type_filter.is_empty() {
            return TypeScan::All;
        }
        match self.type_filter.scan_type() {
            Some(scan_type) if self.server_type_scan => TypeScan::Server(scan_type.to_owned()),
            _ => TypeScan::Client(self.type_filter.clone()),
        }
    }

    /// Merge one `SCAN` step into the sweep, skipping keys already seen (SCAN
    /// may return a key more than once). A returned cursor of 0 ends the sweep.
    pub fn merge_batch(&mut self, cursor: usize, keys: Vec<String>) {
        let mut full = false;
        let first_new = self.scanned.len();
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
        // Both fit: every index went through `u32::try_from` above.
        if let Some(meta) = self.meta.as_mut() {
            meta.grow(self.scanned.len());
        }
        let input = Swept {
            names: &self.scanned,
            meta: self.meta.as_ref(),
        };
        #[allow(clippy::cast_possible_truncation)]
        self.view
            .merge_new(&input, first_new as u32, self.scanned.len() as u32);
        self.sweep_cursor = cursor;
        self.sweep_done = cursor == 0 || full;
        self.refresh_tree();
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
            need: self.keys_needed() - self.loaded(),
            types: self.type_scan(),
            generation: self.generation,
            epoch: self.sweep_epoch,
        })
    }

    /// Merge the runner's answer to `request`. Changes nothing unless it
    /// returns [`SweepApply::Applied`]: the sweep restarted (rescan, new
    /// pattern, DB switch) is `Stale`; the same sweep ended or moved past the
    /// request's cursor is `Raced`. A page turn alone doesn't make the result
    /// stale.
    pub fn apply_result(&mut self, request: &SweepRequest, result: SweepResult) -> SweepApply {
        if request.epoch != self.sweep_epoch {
            return SweepApply::Stale;
        }
        if self.sweep_done || request.cursor != self.sweep_cursor {
            return SweepApply::Raced;
        }
        self.merge_batch(result.cursor, result.batch);
        SweepApply::Applied
    }

    /// The next SCAN step of a load-all, from the sweep's cursor whatever
    /// the page needs. `None` once the sweep has ended.
    #[must_use]
    pub fn load_request(&self) -> Option<SweepRequest> {
        if self.sweep_done {
            return None;
        }
        Some(SweepRequest {
            pattern: self.pattern.clone(),
            cursor: self.sweep_cursor,
            scan_count: self.scan_count.max(LOAD_ALL_COUNT),
            need: 1,
            types: self.type_scan(),
            generation: self.generation,
            epoch: self.sweep_epoch,
        })
    }

    /// `name`'s index in `scanned`, if the sweep has loaded it.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<u32> {
        let hash = (self.hash)(name);
        let scanned = &self.scanned;
        self.seen
            .find(table_hash(hash), |slot| {
                slot.hash == hash && scanned[slot.index as usize] == name
            })
            .map(|slot| slot.index)
    }

    /// Bring `name` on screen: once the order is built
    /// ([`Self::apply_reveal`], run by the runner), go to the page that
    /// shows it, opening the tree groups it sits in. Bumps `generation`.
    pub fn reveal(&mut self, name: String) {
        self.reveal = Some(name);
        self.generation += 1;
    }

    /// Go to the page of the key [`Self::reveal`] asked for. Needs the order
    /// built; a key no longer loaded leaves the page as it is.
    pub fn apply_reveal(&mut self) {
        let Some(name) = self.reveal.take() else {
            return;
        };
        let Some(index) = self.index_of(&name) else {
            return;
        };
        if let Some(tree) = self.view.tree_mut() {
            tree.expand_path(&name);
            self.refresh_tree();
        }
        let position = match (self.view.tree(), self.view.order()) {
            (Some(tree), Some(_)) => tree
                .rows()
                .iter()
                .position(|row| matches!(*row, TreeRow::Leaf { index: i, .. } if i == index)),
            (None, Some(order)) => order.iter().position(|&i| i == index),
            (_, None) => Some(index as usize),
        };
        if let Some(position) = position {
            self.page = position / self.page_size.max(1);
        }
    }

    /// Average length of the loaded names (from the first 1,000), for
    /// a load-all memory estimate. `None` before any key is loaded.
    #[must_use]
    pub fn average_name_len(&self) -> Option<usize> {
        let sample = &self.scanned[..self.scanned.len().min(1000)];
        (!sample.is_empty()).then(|| sample.iter().map(String::len).sum::<usize>() / sample.len())
    }

    /// Copy of the current page's key names, in display order: at most
    /// `page_size` of them. An order still waiting for its rebuild falls back
    /// to SCAN order.
    #[must_use]
    pub fn page_keys(&self) -> Vec<String> {
        let range = self.page_range();
        if let (Some(tree), Some(_)) = (self.view.tree(), self.view.order()) {
            return tree.rows()[range]
                .iter()
                .filter_map(|row| match *row {
                    TreeRow::Leaf { index, .. } => Some(self.scanned[index as usize].clone()),
                    TreeRow::Group { .. } => None,
                })
                .collect();
        }
        match self.view.order() {
            Some(order) => order[range]
                .iter()
                .map(|&index| self.scanned[index as usize].clone())
                .collect(),
            None => self.scanned[range].to_vec(),
        }
    }

    /// The active sort, if any (`None`: SCAN order).
    #[must_use]
    pub fn sort(&self) -> Option<Sort> {
        self.view.sort()
    }

    /// Sort the list (or go back to SCAN order with `None`), from page 1.
    /// The order is built by the next page fetch (bumps `generation`).
    pub fn set_sort(&mut self, sort: Option<Sort>) {
        if self.view.sort() == sort {
            return;
        }
        if sort.is_some_and(|sort| sort.key.needs_meta()) && self.meta.is_none() {
            let mut meta = MetaColumns::default();
            meta.grow(self.scanned.len());
            self.meta = Some(meta);
        }
        self.view.set_sort(sort);
        self.page = 0;
        self.generation += 1;
        self.refresh_tree();
    }

    /// Tree mode is on.
    #[must_use]
    pub fn is_tree(&self) -> bool {
        self.view.tree().is_some()
    }

    /// Switch between the flat list and the prefix tree (keys grouped by
    /// `separator`), from page 1. The tree groups the keys loaded so far.
    pub fn toggle_tree(&mut self, separator: &str) {
        let tree = (!self.is_tree()).then(|| Tree::new(separator));
        self.view.set_tree(tree);
        self.page = 0;
        self.tree_target = self.scanned.len().max(self.page_size);
        self.generation += 1;
        self.refresh_tree();
    }

    /// The current page's rows in tree mode (`None` in the flat list):
    /// groups, and a `Leaf` per key of [`Self::page_keys`].
    #[must_use]
    pub fn page_rows(&self) -> Option<Vec<PageRow>> {
        let tree = self.view.tree()?;
        self.view.order()?;
        Some(
            tree.rows()[self.page_range()]
                .iter()
                .map(|row| match *row {
                    TreeRow::Group {
                        start, end, depth, ..
                    } => {
                        let prefix = self.group_prefix(row).unwrap_or_default().to_owned();
                        PageRow::Group {
                            expanded: tree.is_expanded(&prefix),
                            prefix,
                            count: (end - start) as usize,
                            depth,
                        }
                    }
                    TreeRow::Leaf { depth, .. } => PageRow::Leaf { depth },
                })
                .collect(),
        )
    }

    fn group_prefix(&self, row: &TreeRow) -> Option<&str> {
        let TreeRow::Group {
            start, prefix_len, ..
        } = *row
        else {
            return None;
        };
        let index = self.view.order()?[start as usize];
        Some(&self.scanned[index as usize][..prefix_len as usize])
    }

    /// Open (`Some(true)`), close (`Some(false)`) or toggle (`None`) the
    /// group on row `position` of the list. Returns its prefix if it
    /// changed, so the caller can keep the cursor on it.
    pub fn set_group(&mut self, position: usize, open: Option<bool>) -> Option<String> {
        let row = *self.view.tree()?.rows().get(position)?;
        let prefix = self.group_prefix(&row)?.to_owned();
        let tree = self.view.tree_mut()?;
        let open = open.unwrap_or(!tree.is_expanded(&prefix));
        if !tree.set_expanded(&prefix, open) {
            return None;
        }
        self.refresh_tree();
        self.generation += 1;
        Some(prefix)
    }

    /// `Left` in the tree: close the group on row `position`, or if it is
    /// a key or a closed group, the group it sits in. Moves to the page
    /// that shows the closed group and returns its prefix.
    pub fn fold(&mut self, position: usize) -> Option<String> {
        let rows = self.view.tree()?.rows();
        let row = *rows.get(position)?;
        let open_group = self
            .group_prefix(&row)
            .is_some_and(|prefix| self.view.tree().is_some_and(|t| t.is_expanded(prefix)));
        let target = if open_group {
            position
        } else {
            let depth = match row {
                TreeRow::Group { depth, .. } | TreeRow::Leaf { depth, .. } => depth,
            };
            let parent = depth.checked_sub(1)?;
            rows[..position]
                .iter()
                .rposition(|r| matches!(*r, TreeRow::Group { depth, .. } if depth == parent))?
        };
        let prefix = self.set_group(target, Some(false))?;
        self.page = target / self.page_size.max(1);
        Some(prefix)
    }

    /// The active sort needs metadata the backfill hasn't fetched yet:
    /// `(known, loaded)`. `None` for SCAN order or a name sort.
    #[must_use]
    pub fn meta_progress(&self) -> Option<(usize, usize)> {
        if !self.view.sort()?.key.needs_meta() {
            return None;
        }
        let known = self.meta.as_ref().map_or(0, MetaColumns::known);
        Some((known, self.scanned.len()))
    }

    /// Pause / resume the metadata backfill (the user left / came back).
    pub fn set_backfill_paused(&mut self, paused: bool) {
        self.backfill_paused = paused;
    }

    /// Up to `max` keys without metadata for the backfill, nearest to the
    /// current page first (outwards from it, in display order). `None` when
    /// the sort doesn't need metadata, everything is known, the order is
    /// still being built, or the backfill is paused.
    #[must_use]
    pub fn backfill_request(&self, max: usize) -> Option<BackfillRequest> {
        let (known, loaded) = self.meta_progress()?;
        if self.backfill_paused || known >= loaded || self.view.needs_rebuild() {
            return None;
        }
        let meta = self.meta.as_ref()?;
        let order = self.view.order()?;
        // Tree rows aren't order positions: start from the page's first
        // group (its keys are next to it in the name order).
        let start = match self.view.tree() {
            Some(tree) => tree.rows()[self.page_range()]
                .iter()
                .find_map(|row| match *row {
                    TreeRow::Group { start, .. } => Some(start as usize),
                    TreeRow::Leaf { .. } => None,
                })
                .unwrap_or(0),
            None => self.page_range().start,
        }
        .min(order.len());
        let mut indices = Vec::with_capacity(max.min(loaded - known));
        let (mut down, mut up) = (start, start);
        while indices.len() < max && (up < order.len() || down > 0) {
            for pos in [Some(up).filter(|&p| p < order.len()), down.checked_sub(1)]
                .into_iter()
                .flatten()
            {
                if indices.len() < max && !meta.is_known(order[pos]) {
                    indices.push(order[pos]);
                }
            }
            up += 1;
            down = down.saturating_sub(1);
        }
        let names = indices
            .iter()
            .map(|&index| self.scanned[index as usize].clone())
            .collect();
        Some(BackfillRequest {
            indices,
            names,
            epoch: self.sweep_epoch,
        })
    }

    /// Store the backfill's answer to `request` (`metas` in its order) and
    /// move those keys to their place. Returns `false`, and changes nothing,
    /// when the sweep restarted meanwhile.
    pub fn record_meta(&mut self, request: &BackfillRequest, metas: &[KeyMeta]) -> bool {
        if request.epoch != self.sweep_epoch {
            return false;
        }
        let Some(meta) = self.meta.as_mut() else {
            return false;
        };
        let newly_known: Vec<u32> = request
            .indices
            .iter()
            .zip(metas)
            .filter(|(&index, row)| meta.set(index, row))
            .map(|(&index, _)| index)
            .collect();
        let input = Swept {
            names: &self.scanned,
            meta: self.meta.as_ref(),
        };
        self.view.reposition(&input, &newly_known);
        self.refresh_tree();
        true
    }

    /// The current page as table rows from the columns, if every key on it
    /// has its metadata (so the page can be redrawn without a round trip).
    #[must_use]
    pub fn page_from_meta(&self) -> Option<Vec<KeyMeta>> {
        let meta = self.meta.as_ref()?;
        let order = self.view.order()?;
        let row = |index: u32| meta.key_meta(index, &self.scanned[index as usize]);
        match self.view.tree() {
            Some(tree) => tree.rows()[self.page_range()]
                .iter()
                .filter_map(|row| match *row {
                    TreeRow::Leaf { index, .. } => Some(index),
                    TreeRow::Group { .. } => None,
                })
                .map(row)
                .collect(),
            None => order[self.page_range()].iter().map(|&i| row(i)).collect(),
        }
    }

    /// Build a pending sort order. Small sweeps (up to [`SYNC_SORT_MAX`]
    /// keys) are sorted here; for a larger one this returns a [`SortJob`] to
    /// run off the lock and hand back to [`Self::install_sorted`].
    pub fn rebuild_view(&mut self) -> Option<SortJob> {
        if !self.view.needs_rebuild() {
            return None;
        }
        if self.scanned.len() <= SYNC_SORT_MAX {
            let len = self.scanned_len_u32();
            let input = Swept {
                names: &self.scanned,
                meta: self.meta.as_ref(),
            };
            self.view.rebuild(&input, len);
            self.refresh_tree();
            return None;
        }
        self.sort_job()
    }

    fn refresh_tree(&mut self) {
        let input = Swept {
            names: &self.scanned,
            meta: self.meta.as_ref(),
        };
        self.view.refresh_tree(&input);
    }

    /// A copy of what a full re-sort needs, whatever the sweep's size.
    /// `None` unless the order is waiting for a rebuild.
    #[must_use]
    pub fn sort_job(&self) -> Option<SortJob> {
        if !self.view.needs_rebuild() {
            return None;
        }
        self.view
            .job(&self.scanned, self.meta.as_ref(), self.sweep_epoch)
    }

    /// Install a finished [`SortJob`]. Returns `false`, and changes nothing,
    /// when the sweep restarted or the sort changed meanwhile. Keys merged
    /// since the job was made are merged into the order.
    pub fn install_sorted(&mut self, sorted: SortedOrder) -> bool {
        if sorted.epoch != self.sweep_epoch {
            return false;
        }
        let len = self.scanned_len_u32();
        let input = Swept {
            names: &self.scanned,
            meta: self.meta.as_ref(),
        };
        let installed = self.view.install(sorted, &input, len);
        if installed {
            self.refresh_tree();
        }
        installed
    }

    // `merge_batch` never lets `scanned` outgrow `u32`.
    #[allow(clippy::cast_possible_truncation)]
    fn scanned_len_u32(&self) -> u32 {
        self.scanned.len() as u32
    }

    /// If the finished sweep doesn't reach the current page (keys were deleted
    /// since the user paged there), fall back to the last non-empty page.
    pub fn clamp_page(&mut self) {
        if !self.sweep_done || self.page_size == 0 {
            return;
        }
        let last_page = self.view_len().saturating_sub(1) / self.page_size;
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
            view: KeyView::default(),
            meta: None,
            backfill_paused: false,
            type_filter: TypeFilter::default(),
            server_type_scan: true,
            tree_target: DEFAULT_PAGE_SIZE,
            reveal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::redis_client::types::{tests::keys, SortKey};

    #[test]
    fn type_filter_restarts_the_sweep_and_picks_how_to_scan() {
        let mut state = state_with(10);
        state.merge_batch(5, keys(0..15));
        state.next_page();
        assert_eq!(state.sweep_request().unwrap().types, TypeScan::All);

        assert!(state.set_type_filter(TypeFilter::new(["hash"])));
        assert_eq!(state.page, 0);
        assert!(state.scanned.is_empty(), "the sweep restarts");
        assert!(
            !state.set_type_filter(TypeFilter::new(["hash"])),
            "unchanged"
        );
        assert_eq!(
            state.sweep_request().unwrap().types,
            TypeScan::Server("hash".into())
        );

        state.set_type_filter(TypeFilter::new(["hash", "zset"]));
        assert_eq!(
            state.sweep_request().unwrap().types,
            TypeScan::Client(TypeFilter::new(["hash", "zset"]))
        );

        state.set_type_filter(TypeFilter::new(["json"]));
        state.disable_server_type_scan();
        assert_eq!(
            state.sweep_request().unwrap().types,
            TypeScan::Client(TypeFilter::new(["json"])),
            "after the server rejected SCAN TYPE"
        );
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|&name| name.to_owned()).collect()
    }

    fn row_labels(state: &KeyspaceState) -> Vec<String> {
        let mut keys = state.page_keys().into_iter();
        state
            .page_rows()
            .unwrap()
            .into_iter()
            .map(|row| match row {
                PageRow::Group {
                    prefix,
                    count,
                    expanded,
                    ..
                } => format!("{} {prefix} ({count})", if expanded { '▾' } else { '▸' }),
                PageRow::Leaf { .. } => keys.next().unwrap(),
            })
            .collect()
    }

    #[test]
    fn tree_folds_unfolds_and_pages_over_rows() {
        let mut state = state_with(10);
        state.merge_batch(
            0,
            names(&[
                "user:1:name",
                "order:9",
                "user:2:name",
                "user:1:email",
                "solo",
            ]),
        );
        state.toggle_tree(":");
        assert!(state.is_tree());
        assert!(state.rebuild_view().is_none(), "small: sorted inline");
        assert_eq!(row_labels(&state), ["▸ order: (1)", "▸ user: (3)", "solo"]);
        assert_eq!(state.view_len(), 3);

        assert_eq!(state.set_group(1, None).as_deref(), Some("user:"));
        assert_eq!(
            row_labels(&state),
            [
                "▸ order: (1)",
                "▾ user: (3)",
                "▸ user:1: (2)",
                "user:2:name",
                "solo"
            ]
        );
        assert_eq!(state.set_group(3, Some(true)), None, "a key is no group");
        state.set_group(2, Some(true));
        assert_eq!(
            row_labels(&state)[2..5],
            ["▾ user:1: (2)", "user:1:email", "user:1:name"]
        );

        // ← on a key closes the group it is in; on an open group, that one.
        assert_eq!(state.fold(4).as_deref(), Some("user:1:"));
        assert_eq!(state.fold(1).as_deref(), Some("user:"));
        assert_eq!(row_labels(&state), ["▸ order: (1)", "▸ user: (3)", "solo"]);
        assert_eq!(state.fold(0), None, "nothing above the top level");

        // Back to the flat list: SCAN order, every key.
        state.toggle_tree(":");
        assert!(state.page_rows().is_none());
        assert_eq!(state.view_len(), 5);
        assert_eq!(state.page_keys()[0], "user:1:name");
    }

    #[test]
    fn tree_loads_raw_keys_not_rows() {
        let mut state = state_with(3);
        state.toggle_tree(":");
        assert_eq!(state.sweep_request().unwrap().need, 3);
        state.merge_batch(7, names(&["a:1", "a:2", "a:3"]));
        state.rebuild_view();
        assert_eq!(row_labels(&state), ["▸ a: (3)"]);
        assert!(
            !state.needs_scan(),
            "one group fills no page, but 3 keys are loaded"
        );

        // `n` past the last row loads another page of keys, same page.
        assert!(state.next_page());
        assert_eq!(state.page, 0);
        assert_eq!(state.sweep_request().unwrap().need, 3);
    }

    #[test]
    fn tree_keeps_the_sort_inside_groups_and_survives_rescan() {
        let mut state = state_with(10);
        state.toggle_tree(":");
        state.set_sort(Some(Sort {
            key: SortKey::Name,
            descending: true,
        }));
        state.merge_batch(0, names(&["b", "a", "g:1"]));
        state.rebuild_view();
        assert_eq!(row_labels(&state), ["▸ g: (1)", "b", "a"]);
        state.set_group(0, Some(true));

        state.rescan();
        state.merge_batch(0, names(&["g:1", "g:2"]));
        state.rebuild_view();
        assert_eq!(row_labels(&state), ["▾ g: (2)", "g:2", "g:1"], "kept open");
        state.reset_for_db();
        assert!(state.is_tree());
    }

    #[test]
    fn load_request_runs_to_the_end_whatever_the_page_needs() {
        let mut state = state_with(10);
        state.merge_batch(5, names(&["a", "b", "c"]));
        let request = state.load_request().expect("sweep not done");
        assert_eq!(request.cursor, 5);
        assert_eq!(request.scan_count, LOAD_ALL_COUNT);
        assert_eq!(request.need, 1);
        // A page fetch moved the sweep first: the load asks again from there.
        state.merge_batch(9, names(&["d"]));
        assert_eq!(
            state.apply_result(&request, result(0, names(&["x"]))),
            SweepApply::Raced
        );
        let request = state.load_request().unwrap();
        assert_eq!(
            state.apply_result(&request, result(0, names(&["e"]))),
            SweepApply::Applied
        );
        assert!(state.load_request().is_none(), "the sweep ended");
        assert_eq!(state.index_of("e"), Some(4));
        assert_eq!(state.index_of("x"), None);
    }

    #[test]
    fn reveal_goes_to_the_page_of_the_key() {
        let mut state = state_with(2);
        state.merge_batch(0, names(&["d", "c", "b", "a", "e"]));
        state.reveal("a".into());
        state.apply_reveal();
        assert_eq!(state.page, 1, "SCAN order: a is 4th");

        state.set_sort(Some(Sort {
            key: SortKey::Name,
            descending: false,
        }));
        state.rebuild_view();
        state.reveal("e".into());
        state.apply_reveal();
        assert_eq!(state.page, 2, "by name: e is 5th");

        state.reveal("gone".into());
        state.apply_reveal();
        assert_eq!(state.page, 2, "an unknown key leaves the page");
    }

    #[test]
    fn reveal_opens_the_groups_of_the_key() {
        let mut state = state_with(3);
        state.merge_batch(0, names(&["user:1:name", "user:1:email", "order:9", "a"]));
        state.toggle_tree(":");
        state.rebuild_view();
        state.reveal("user:1:name".into());
        state.apply_reveal();
        // Rows: order:, user:, user:1:, its two keys, a.
        assert_eq!(state.page, 1);
        assert_eq!(row_labels(&state), ["user:1:email", "user:1:name", "a"]);
    }

    #[test]
    fn type_filter_survives_rescan_and_db_switch() {
        let mut state = state_with(10);
        state.set_type_filter(TypeFilter::new(["hash"]));
        state.rescan();
        state.reset_for_db();
        assert_eq!(state.type_filter(), &TypeFilter::new(["hash"]));
        assert_eq!(
            state.sweep_request().unwrap().types,
            TypeScan::Server("hash".into())
        );
    }

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
        assert_eq!(
            SweepApply::Applied,
            state.apply_result(&request, result(17, keys(0..12)))
        );
        assert_eq!(state.sweep_cursor, 17);
        assert!(!state.sweep_done);
        assert_eq!(state.page_range(), 0..10);
        assert_eq!(state.sweep_request(), None);

        // Applying the same answer twice is refused: the cursor moved.
        assert_eq!(
            SweepApply::Raced,
            state.apply_result(&request, result(99, keys(50..60)))
        );
        assert_eq!(state.scanned.len(), 12);
    }

    #[test]
    fn apply_result_drops_stale_epoch() {
        let mut state = state_with(10);
        let request = state.sweep_request().expect("needs keys");
        // The user changes the filter while the SCAN is in flight.
        state.set_pattern(Some("user:*".into()));
        assert_eq!(
            SweepApply::Stale,
            state.apply_result(&request, result(5, keys(0..10)))
        );
        assert!(state.scanned.is_empty());
        assert_eq!(state.sweep_cursor, 0);

        let mut state = state_with(10);
        let request = state.sweep_request().expect("needs keys");
        state.rescan();
        assert_eq!(
            SweepApply::Stale,
            state.apply_result(&request, result(5, keys(0..10)))
        );
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

        assert_eq!(
            SweepApply::Applied,
            state.apply_result(&request, result(8, keys(10..20)))
        );
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
        assert_eq!(
            SweepApply::Applied,
            state.apply_result(&request, result(9, batch))
        );
        assert_eq!(state.scanned.len(), 14, "repeats are merged once");

        let again = state.sweep_request().expect("page 2 is still short");
        assert_eq!(again.cursor, 9);
        assert_eq!(again.need, 6);
        assert_eq!(
            SweepApply::Applied,
            state.apply_result(&again, result(0, keys(10..20)))
        );
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

    const BY_NAME: Sort = Sort {
        key: super::super::view::SortKey::Name,
        descending: false,
    };

    #[test]
    fn sorted_pages_follow_the_order_and_merge_new_batches() {
        let mut state = state_with(3);
        state.merge_batch(7, ["d", "b", "f"].map(String::from).to_vec());
        state.set_sort(Some(BY_NAME));
        assert_eq!(state.page_keys(), ["d", "b", "f"], "SCAN order until built");
        assert!(state.rebuild_view().is_none(), "small sweeps sort inline");
        assert_eq!(state.page_keys(), ["b", "d", "f"]);

        // A later batch is merged into the built order.
        state.merge_batch(0, ["a", "e", "c"].map(String::from).to_vec());
        assert_eq!(state.page_keys(), ["a", "b", "c"]);
        assert!(state.next_page());
        assert_eq!(state.page_keys(), ["d", "e", "f"]);
    }

    #[test]
    fn set_sort_goes_to_page_one_and_bumps_generation() {
        let mut state = state_with(10);
        state.merge_batch(0, keys(0..30));
        state.next_page();
        let generation = state.generation;
        state.set_sort(Some(BY_NAME));
        assert_eq!(state.page, 0);
        assert!(state.generation > generation);

        let generation = state.generation;
        state.set_sort(Some(BY_NAME));
        assert_eq!(state.generation, generation, "same sort is a no-op");
    }

    #[test]
    fn rescan_keeps_the_sort_and_rebuilds() {
        let mut state = state_with(10);
        state.set_sort(Some(BY_NAME));
        state.merge_batch(5, ["b", "a"].map(String::from).to_vec());
        assert!(state.rebuild_view().is_none());
        state.rescan();
        assert_eq!(state.sort(), Some(BY_NAME));
        state.merge_batch(0, ["z", "y"].map(String::from).to_vec());
        assert!(state.rebuild_view().is_none());
        assert_eq!(state.page_keys(), ["y", "z"]);
    }

    #[test]
    fn off_lock_sort_installs_with_late_keys() {
        let mut state = state_with(4);
        state.set_sort(Some(BY_NAME));
        state.merge_batch(5, ["c", "a"].map(String::from).to_vec());
        let job = state.sort_job().expect("order pending");
        // The page fetch merges another batch while the job runs.
        state.merge_batch(0, ["d", "b"].map(String::from).to_vec());
        assert!(state.install_sorted(job.run()));
        assert_eq!(state.page_keys(), ["a", "b", "c", "d"]);
        assert!(state.sort_job().is_none(), "built");
    }

    #[test]
    fn off_lock_sort_from_an_old_sweep_is_dropped() {
        let mut state = state_with(4);
        state.set_sort(Some(BY_NAME));
        state.merge_batch(5, ["c", "a"].map(String::from).to_vec());
        let job = state.sort_job().expect("order pending");
        state.rescan();
        state.merge_batch(0, ["x"].map(String::from).to_vec());
        assert!(!state.install_sorted(job.run()));
        assert!(state.sort_job().is_some(), "still pending");
    }

    fn by_size() -> Sort {
        Sort {
            key: super::super::view::SortKey::Size,
            descending: true,
        }
    }

    fn sized(name: &str, size: u128) -> KeyMeta {
        KeyMeta {
            key: name.into(),
            r_type: super::super::RedisType::String,
            size,
            ttl: -1,
            value: super::super::KeyValue::Unknown,
        }
    }

    /// Answer `request` with sizes taken from the key number (`k7` → 7).
    fn answer(request: &BackfillRequest) -> Vec<KeyMeta> {
        request
            .names
            .iter()
            .map(|name| sized(name, name[1..].parse().unwrap()))
            .collect()
    }

    #[test]
    fn backfill_fills_the_page_first_then_sorts_it() {
        let mut state = state_with(3);
        state.merge_batch(0, keys(0..10));
        assert_eq!(state.backfill_request(5), None, "no sort");
        state.set_sort(Some(by_size()));
        assert_eq!(state.meta_progress(), Some((0, 10)));
        assert_eq!(state.backfill_request(5), None, "order not built yet");
        assert!(state.rebuild_view().is_none());

        state.page = 2; // positions 6..9
        let request = state.backfill_request(3).expect("sizes missing");
        assert_eq!(request.indices, [6, 5, 7], "outwards from the page");
        assert!(state.record_meta(&request, &answer(&request)));
        assert_eq!(state.meta_progress(), Some((3, 10)));
        // Known sizes moved to the front, largest first.
        state.page = 0;
        assert_eq!(state.page_keys(), ["k7", "k6", "k5"]);
        assert_eq!(
            state
                .page_from_meta()
                .unwrap()
                .iter()
                .map(|m| m.size)
                .collect::<Vec<_>>(),
            [7, 6, 5]
        );
        state.page = 1;
        assert_eq!(
            state.page_from_meta().map(|rows| rows.len()),
            None,
            "unknown rows"
        );

        while let Some(request) = state.backfill_request(4) {
            assert!(state.record_meta(&request, &answer(&request)));
        }
        assert_eq!(state.meta_progress(), Some((10, 10)));
        state.page = 0;
        assert_eq!(state.page_keys(), ["k9", "k8", "k7"]);
    }

    #[test]
    fn backfill_waits_while_paused_and_drops_old_sweeps() {
        let mut state = state_with(3);
        state.merge_batch(0, keys(0..4));
        state.set_sort(Some(by_size()));
        assert!(state.rebuild_view().is_none());
        state.set_backfill_paused(true);
        assert_eq!(state.backfill_request(10), None);
        state.set_backfill_paused(false);
        let request = state.backfill_request(10).expect("sizes missing");

        state.rescan();
        state.merge_batch(0, keys(0..4));
        assert!(!state.record_meta(&request, &answer(&request)));
        assert_eq!(state.meta_progress(), Some((0, 4)), "columns restarted too");
    }

    #[test]
    fn name_sort_needs_no_metadata() {
        let mut state = state_with(3);
        state.merge_batch(0, keys(0..4));
        state.set_sort(Some(BY_NAME));
        assert_eq!(state.meta_progress(), None);
        assert!(state.rebuild_view().is_none());
        assert_eq!(state.backfill_request(10), None);
    }
}
