//! The order the key list shows the loaded part of the sweep in.
//!
//! By default that is SCAN order and nothing is stored. Once the user sorts,
//! `order` holds indices into `KeyspaceState::scanned` in display order.
//! New SCAN batches are merged into it as sorted runs. A full rebuild (after
//! a sort change) runs inline when the sweep is small, otherwise off the
//! keyspace lock on a copy of the names ([`SortJob`]).
//!
//! Sorting by type, TTL or size needs each key's metadata. It is kept in
//! [`MetaColumns`], parallel to `scanned`, filled by the runner's backfill.
//! Keys whose metadata isn't known yet sort last, in SCAN order.

use std::cmp::Ordering;

use super::{tree::Tree, KeyMeta, KeyValue, RedisType};

/// What the key list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Type,
    Ttl,
    Size,
}

impl SortKey {
    /// Sorting by it needs `TYPE` / `TTL` / `MEMORY USAGE` of every key.
    #[must_use]
    pub fn needs_meta(self) -> bool {
        self != Self::Name
    }

    /// Lowercase column name, as the header's progress note shows it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "key",
            Self::Type => "type",
            Self::Ttl => "ttl",
            Self::Size => "size",
        }
    }
}

/// A sort column and its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Sort {
    /// The sort after choosing `key` in the sort menu (`None` = SCAN order).
    /// A new column starts in its natural direction: size ▼ (largest first),
    /// the others ▲ (TTL: soonest expiry first). The active column again
    /// reverses it.
    #[must_use]
    pub fn pick(current: Option<Self>, key: Option<SortKey>) -> Option<Self> {
        let key = key?;
        Some(match current {
            Some(sort) if sort.key == key => sort.reversed(),
            _ => Self {
                key,
                descending: key == SortKey::Size,
            },
        })
    }

    /// The same column, the other way round.
    #[must_use]
    pub fn reversed(self) -> Self {
        Self {
            descending: !self.descending,
            ..self
        }
    }
}

const TYPE_UNKNOWN: u8 = 0;
/// Code for type names past the 254th distinct one (never seen in practice).
const TYPE_OTHER: u8 = u8::MAX;
const TTL_UNKNOWN: i64 = i64::MIN;
const SIZE_UNKNOWN: u32 = u32::MAX;

/// Type, TTL and size of each key in the sweep, by sweep index. About 13
/// bytes a key; only allocated once the user sorts by one of them.
#[derive(Debug, Clone, Default)]
pub struct MetaColumns {
    /// 0 = unknown, else 1 + an index into `type_names`.
    types: Vec<u8>,
    /// `TTL` reply: seconds, -1 = no expiry, -2 = gone.
    ttl: Vec<i64>,
    /// `MEMORY USAGE` in bytes, capped just below `SIZE_UNKNOWN`.
    size: Vec<u32>,
    /// Type names seen so far (`hash`, `ReJSON-RL` as `json`, module types).
    type_names: Vec<String>,
    /// How many keys have their metadata.
    known: usize,
}

impl MetaColumns {
    /// Make room for `len` keys; the new ones are unknown.
    pub fn grow(&mut self, len: usize) {
        self.types.resize(len, TYPE_UNKNOWN);
        self.ttl.resize(len, TTL_UNKNOWN);
        self.size.resize(len, SIZE_UNKNOWN);
    }

    /// Forget every key (the sweep restarted).
    pub fn clear(&mut self) {
        self.types.clear();
        self.ttl.clear();
        self.size.clear();
        self.known = 0;
    }

    #[must_use]
    pub fn known(&self) -> usize {
        self.known
    }

    #[must_use]
    pub fn is_known(&self, index: u32) -> bool {
        self.types
            .get(index as usize)
            .is_some_and(|&code| code != TYPE_UNKNOWN)
    }

    /// Store `meta` for key `index`. Returns `true` if it was unknown before.
    pub fn set(&mut self, index: u32, meta: &KeyMeta) -> bool {
        let i = index as usize;
        if i >= self.types.len() {
            return false;
        }
        let was_unknown = self.types[i] == TYPE_UNKNOWN;
        self.types[i] = self.type_code(meta.r_type.name());
        self.ttl[i] = i64::try_from(meta.ttl).unwrap_or(-1);
        self.size[i] =
            u32::try_from(meta.size).map_or(SIZE_UNKNOWN - 1, |size| size.min(SIZE_UNKNOWN - 1));
        if was_unknown {
            self.known += 1;
        }
        was_unknown
    }

    fn type_code(&mut self, name: &str) -> u8 {
        if let Some(i) = self.type_names.iter().position(|n| n == name) {
            return u8::try_from(i + 1).unwrap_or(TYPE_OTHER);
        }
        if self.type_names.len() + 1 >= usize::from(TYPE_OTHER) {
            return TYPE_OTHER;
        }
        self.type_names.push(name.to_owned());
        u8::try_from(self.type_names.len()).unwrap_or(TYPE_OTHER)
    }

    #[must_use]
    pub fn type_name(&self, index: u32) -> Option<&str> {
        match *self.types.get(index as usize)? {
            TYPE_UNKNOWN => None,
            TYPE_OTHER => Some("?"),
            code => self
                .type_names
                .get(usize::from(code) - 1)
                .map(String::as_str),
        }
    }

    #[must_use]
    pub fn ttl(&self, index: u32) -> Option<i64> {
        self.ttl
            .get(index as usize)
            .copied()
            .filter(|&ttl| ttl != TTL_UNKNOWN)
    }

    #[must_use]
    pub fn size(&self, index: u32) -> Option<u32> {
        self.size
            .get(index as usize)
            .copied()
            .filter(|&size| size != SIZE_UNKNOWN)
    }

    /// Key `index` as a table row, if its metadata is known.
    #[must_use]
    pub fn key_meta(&self, index: u32, name: &str) -> Option<KeyMeta> {
        Some(KeyMeta {
            key: name.to_owned(),
            r_type: RedisType::from(self.type_name(index)?),
            size: u128::from(self.size(index)?),
            ttl: isize::try_from(self.ttl(index)?).unwrap_or(-1),
            value: KeyValue::Unknown,
        })
    }
}

/// TTL as a sort rank: soonest expiry first, then keys without one, then
/// keys that were gone.
fn ttl_rank(ttl: i64) -> i64 {
    match ttl {
        t if t >= 0 => t,
        -1 => i64::MAX - 1,
        _ => i64::MAX,
    }
}

/// Up to this many loaded keys a full re-sort runs inline (under the
/// keyspace lock); above it, in `spawn_blocking` on a [`SortJob`]. From the
/// `sort_names` bench: 100k names take about 9 ms, so 50k stay well inside
/// one 16 ms frame.
pub const SYNC_SORT_MAX: usize = 50_000;

/// What a sort reads, by sweep index: names and, for a type / TTL / size
/// sort, the metadata columns.
pub trait SortInput {
    fn name(&self, index: u32) -> &str;
    fn meta(&self) -> Option<&MetaColumns> {
        None
    }
}

impl SortInput for [String] {
    fn name(&self, index: u32) -> &str {
        &self[index as usize]
    }
}

/// The live sweep: names plus the columns, if allocated.
pub struct Swept<'a> {
    pub names: &'a [String],
    pub meta: Option<&'a MetaColumns>,
}

impl SortInput for Swept<'_> {
    fn name(&self, index: u32) -> &str {
        &self.names[index as usize]
    }
    fn meta(&self) -> Option<&MetaColumns> {
        self.meta
    }
}

/// Compare two sweep indices under `sort`. A type / TTL / size sort puts
/// keys with unknown metadata last, in SCAN order, and breaks ties by name
/// (ascending, whatever the direction). Last resort is the index, so the
/// order is total and deterministic.
pub fn compare<I: SortInput + ?Sized>(input: &I, sort: Sort, a: u32, b: u32) -> Ordering {
    let by_key = match sort.key {
        SortKey::Name => input.name(a).cmp(input.name(b)),
        key => {
            let Some(meta) = input.meta() else {
                return a.cmp(&b);
            };
            let ordering = match key {
                SortKey::Type => meta
                    .type_name(a)
                    .zip(meta.type_name(b))
                    .map(|(x, y)| x.cmp(y)),
                SortKey::Ttl => meta
                    .ttl(a)
                    .zip(meta.ttl(b))
                    .map(|(x, y)| ttl_rank(x).cmp(&ttl_rank(y))),
                SortKey::Size | SortKey::Name => {
                    meta.size(a).zip(meta.size(b)).map(|(x, y)| x.cmp(&y))
                }
            };
            match ordering {
                Some(ordering) => {
                    let ordering = if sort.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    return ordering
                        .then_with(|| input.name(a).cmp(input.name(b)))
                        .then(a.cmp(&b));
                }
                // Known before unknown, whatever the direction.
                None => {
                    return match (meta.is_known(a), meta.is_known(b)) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => a.cmp(&b),
                    }
                }
            }
        }
    };
    let by_key = if sort.descending {
        by_key.reverse()
    } else {
        by_key
    };
    by_key.then(a.cmp(&b))
}

/// Indices `0..len`, sorted under `sort`.
#[must_use]
pub fn sort_indices<I: SortInput + ?Sized>(names: &I, len: u32, sort: Sort) -> Vec<u32> {
    let mut order: Vec<u32> = (0..len).collect();
    // Names are unique in the sweep, so a name sort needs no tie-break, and
    // the plain comparison is what keeps a 1M-key re-sort fast.
    match (sort.key, sort.descending) {
        (SortKey::Name, false) => order.sort_unstable_by(|&a, &b| names.name(a).cmp(names.name(b))),
        (SortKey::Name, true) => order.sort_unstable_by(|&a, &b| names.name(b).cmp(names.name(a))),
        _ => order.sort_unstable_by(|&a, &b| compare(names, sort, a, b)),
    }
    order
}

/// Merge `run` into the sorted `order`. `run` is sorted first. Works in
/// place from the back, so only the elements after the first insertion point
/// move, and `order` grows like any `Vec`.
pub fn merge_run(order: &mut Vec<u32>, mut run: Vec<u32>, cmp: impl Fn(u32, u32) -> Ordering) {
    if run.is_empty() {
        return;
    }
    run.sort_unstable_by(|&a, &b| cmp(a, b));
    let old = order.len();
    // Common case: every new key sorts after the current last one.
    if order
        .last()
        .is_none_or(|&last| cmp(last, run[0]) != Ordering::Greater)
    {
        order.extend(run);
        return;
    }
    order.resize(old + run.len(), 0);
    let (mut i, mut j, mut k) = (old, run.len(), old + run.len());
    while j > 0 {
        if i > 0 && cmp(order[i - 1], run[j - 1]) == Ordering::Greater {
            order[k - 1] = order[i - 1];
            i -= 1;
        } else {
            order[k - 1] = run[j - 1];
            j -= 1;
        }
        k -= 1;
    }
}

/// Sort and display-order state of the key list.
#[derive(Debug, Clone, Default)]
pub struct KeyView {
    /// The sort the user picked. In tree mode it orders the keys inside
    /// each group; the order itself is by name.
    sort: Option<Sort>,
    /// Sweep indices sorted by [`Self::order_sort`]. `None` with such a
    /// sort means it must be rebuilt; `None` without one means SCAN order.
    order: Option<Vec<u32>>,
    /// Bumped on every change of the order's sort, so a rebuild for an
    /// older one is dropped.
    sort_generation: u64,
    /// Prefix tree mode, built over the name order.
    tree: Option<Tree>,
}

const BY_NAME: Sort = Sort {
    key: SortKey::Name,
    descending: false,
};

impl KeyView {
    #[must_use]
    pub fn sort(&self) -> Option<Sort> {
        self.sort
    }

    /// What `order` is sorted by: by name in tree mode, else the user's
    /// sort.
    fn order_sort(&self) -> Option<Sort> {
        if self.tree.is_some() {
            Some(BY_NAME)
        } else {
            self.sort
        }
    }

    #[must_use]
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn tree_mut(&mut self) -> Option<&mut Tree> {
        self.tree.as_mut()
    }

    /// Turn tree mode on (with this separator) or off. The order is
    /// rebuilt by the next page fetch if its sort changes.
    pub fn set_tree(&mut self, tree: Option<Tree>) {
        let before = self.order_sort();
        self.tree = tree;
        if self.order_sort() != before {
            self.order = None;
            self.sort_generation += 1;
        }
    }

    /// Rebuild the tree's visible rows (nothing outside tree mode; no rows
    /// while the order waits for its rebuild).
    pub fn refresh_tree<I: SortInput + ?Sized>(&mut self, input: &I) {
        let sort = self.sort;
        let (Some(tree), order) = (self.tree.as_mut(), self.order.as_deref()) else {
            return;
        };
        match order {
            Some(by_name) => tree.rebuild(input, by_name, sort),
            None => tree.clear_rows(),
        }
    }

    /// The display order, when a sort is set and the order is built.
    #[must_use]
    pub fn order(&self) -> Option<&[u32]> {
        self.order.as_deref()
    }

    /// A sort is set but its order isn't built yet.
    #[must_use]
    pub fn needs_rebuild(&self) -> bool {
        self.order_sort().is_some() && self.order.is_none()
    }

    /// Change the sort. The order is rebuilt by the next page fetch (in
    /// tree mode it stays by name; only the rows change).
    pub fn set_sort(&mut self, sort: Option<Sort>) {
        if self.sort == sort {
            return;
        }
        let before = self.order_sort();
        self.sort = sort;
        if self.order_sort() != before {
            self.order = None;
            self.sort_generation += 1;
        }
    }

    /// The sweep restarted: keep the sort, drop the order.
    pub fn reset(&mut self) {
        self.order = None;
        self.sort_generation += 1;
    }

    /// Merge the new sweep indices `from..to` into the built order.
    pub fn merge_new<I: SortInput + ?Sized>(&mut self, names: &I, from: u32, to: u32) {
        let (Some(sort), Some(order)) = (self.order_sort(), self.order.as_mut()) else {
            return;
        };
        merge_run(order, (from..to).collect(), |a, b| {
            compare(names, sort, a, b)
        });
    }

    /// Keys `indices` got their metadata: move them to their place in a
    /// type / TTL / size order (they sat at the end as unknown).
    pub fn reposition<I: SortInput + ?Sized>(&mut self, input: &I, indices: &[u32]) {
        let (Some(sort), Some(order)) = (self.order_sort(), self.order.as_mut()) else {
            return;
        };
        if !sort.key.needs_meta() || indices.is_empty() {
            return;
        }
        let mut moved = vec![false; order.len()];
        for &index in indices {
            if let Some(flag) = moved.get_mut(index as usize) {
                *flag = true;
            }
        }
        order.retain(|&index| !moved.get(index as usize).copied().unwrap_or(false));
        merge_run(order, indices.to_vec(), |a, b| compare(input, sort, a, b));
    }

    /// Build the order inline over `len` names.
    pub fn rebuild<I: SortInput + ?Sized>(&mut self, names: &I, len: u32) {
        if let Some(sort) = self.order_sort() {
            self.order = Some(sort_indices(names, len, sort));
        }
    }

    /// Copy what a rebuild over `names[..len]` needs, to sort it off the
    /// lock. `None` without a sort.
    #[must_use]
    pub fn job(&self, names: &[String], meta: Option<&MetaColumns>, epoch: u64) -> Option<SortJob> {
        let sort = self.order_sort()?;
        Some(SortJob {
            names: NameArena::copy(names),
            meta: meta.filter(|_| sort.key.needs_meta()).cloned(),
            sort,
            epoch,
            sort_generation: self.sort_generation,
        })
    }

    /// Install a finished [`SortJob`]'s order, merging in the indices
    /// `sorted.len..len` that arrived meanwhile. Returns `false`, and changes
    /// nothing, if the sort changed or the order was built meanwhile (the
    /// caller checks the sweep epoch).
    pub fn install<I: SortInput + ?Sized>(
        &mut self,
        sorted: SortedOrder,
        names: &I,
        len: u32,
    ) -> bool {
        if sorted.sort_generation != self.sort_generation || self.order.is_some() {
            return false;
        }
        let covered = u32::try_from(sorted.order.len()).unwrap_or(u32::MAX);
        self.order = Some(sorted.order);
        self.merge_new(names, covered, len);
        true
    }
}

/// Key names packed into one buffer: one allocation instead of one per name.
#[derive(Debug, Clone, Default)]
pub struct NameArena {
    bytes: String,
    /// End of each name in `bytes`.
    ends: Vec<usize>,
}

impl NameArena {
    #[must_use]
    pub fn copy(names: &[String]) -> Self {
        let total = names.iter().map(String::len).sum();
        let mut arena = Self {
            bytes: String::with_capacity(total),
            ends: Vec::with_capacity(names.len()),
        };
        for name in names {
            arena.bytes.push_str(name);
            arena.ends.push(arena.bytes.len());
        }
        arena
    }

    fn len(&self) -> usize {
        self.ends.len()
    }
}

impl SortInput for NameArena {
    fn name(&self, index: u32) -> &str {
        let i = index as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.bytes[start..self.ends[i]]
    }
}

/// A full re-sort, copied out of the keyspace lock.
#[derive(Debug)]
pub struct SortJob {
    names: NameArena,
    /// A copy of the columns, for a type / TTL / size sort.
    meta: Option<MetaColumns>,
    sort: Sort,
    /// `KeyspaceState::sweep_epoch` when the job was made.
    pub(super) epoch: u64,
    sort_generation: u64,
}

impl SortInput for SortJob {
    fn name(&self, index: u32) -> &str {
        self.names.name(index)
    }
    fn meta(&self) -> Option<&MetaColumns> {
        self.meta.as_ref()
    }
}

impl SortJob {
    /// Sort. Pure CPU work; meant for `spawn_blocking`.
    #[must_use]
    pub fn run(self) -> SortedOrder {
        let len = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        SortedOrder {
            order: sort_indices(&self, len, self.sort),
            epoch: self.epoch,
            sort_generation: self.sort_generation,
        }
    }
}

/// The result of a [`SortJob`].
#[derive(Debug)]
pub struct SortedOrder {
    order: Vec<u32>,
    pub(super) epoch: u64,
    sort_generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const BY_NAME: Sort = Sort {
        key: SortKey::Name,
        descending: false,
    };

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn shown(names: &[String], order: &[u32]) -> Vec<String> {
        order.iter().map(|&i| names[i as usize].clone()).collect()
    }

    #[test]
    fn sorts_by_name_both_ways() {
        let names = names(&["b", "c", "a"]);
        let up = sort_indices(names.as_slice(), 3, BY_NAME);
        assert_eq!(shown(&names, &up), ["a", "b", "c"]);
        let down = Sort {
            descending: true,
            ..BY_NAME
        };
        let down = sort_indices(names.as_slice(), 3, down);
        assert_eq!(shown(&names, &down), ["c", "b", "a"]);
    }

    #[test]
    fn merge_matches_a_full_sort() {
        // Pseudo-random names, merged in uneven runs.
        let all: Vec<String> = (0..500u32)
            .map(|i| format!("k{}", i.wrapping_mul(2_654_435_761) % 1000))
            .collect();
        let mut order = Vec::new();
        let mut from = 0;
        for size in [1, 7, 0, 50, 13, 200, 229] {
            let to = from + size;
            merge_run(&mut order, (from..to).collect(), |a, b| {
                compare(all.as_slice(), BY_NAME, a, b)
            });
            from = to;
        }
        assert_eq!(from, 500);
        assert_eq!(order, sort_indices(all.as_slice(), 500, BY_NAME));
    }

    #[test]
    fn merge_appends_a_run_past_the_end() {
        let names = names(&["a", "b", "c", "d"]);
        let mut order = vec![0, 1];
        merge_run(&mut order, vec![3, 2], |a, b| {
            compare(names.as_slice(), BY_NAME, a, b)
        });
        assert_eq!(order, [0, 1, 2, 3]);
    }

    #[test]
    fn arena_reads_back_every_name() {
        let names = names(&["", "user:1", "é", ""]);
        let arena = NameArena::copy(&names);
        assert_eq!(arena.len(), 4);
        for (i, name) in names.iter().enumerate() {
            assert_eq!(arena.name(u32::try_from(i).unwrap()), name);
        }
    }

    #[test]
    fn job_result_installs_and_merges_late_keys() {
        let mut all = names(&["d", "a", "c"]);
        let mut view = KeyView::default();
        view.set_sort(Some(BY_NAME));
        assert!(view.needs_rebuild());
        let job = view.job(&all, None, 0).expect("sort set");
        // A batch lands while the job runs.
        all.push("b".into());
        assert!(view.install(job.run(), all.as_slice(), 4));
        assert_eq!(shown(&all, view.order().unwrap()), ["a", "b", "c", "d"]);
    }

    #[test]
    fn job_for_an_older_sort_is_dropped() {
        let all = names(&["b", "a"]);
        let mut view = KeyView::default();
        view.set_sort(Some(BY_NAME));
        let job = view.job(&all, None, 0).expect("sort set");
        view.set_sort(Some(Sort {
            descending: true,
            ..BY_NAME
        }));
        assert!(!view.install(job.run(), all.as_slice(), 2));
        assert!(view.needs_rebuild());
    }

    #[test]
    fn no_sort_means_no_order() {
        let mut view = KeyView::default();
        assert!(!view.needs_rebuild());
        view.rebuild(names(&["a"]).as_slice(), 1);
        assert!(view.order().is_none());
        assert!(view.job(&names(&["a"]), None, 0).is_none());
    }

    fn row(r_type: &str, ttl: isize, size: u128) -> KeyMeta {
        KeyMeta {
            key: String::new(),
            r_type: RedisType::from(r_type),
            size,
            ttl,
            value: KeyValue::Unknown,
        }
    }

    #[test]
    fn picking_a_column_starts_it_and_picking_it_again_reverses() {
        let pick = |current, key| Sort::pick(current, key).map(|s| (s.key, s.descending));
        assert_eq!(
            pick(None, Some(SortKey::Name)),
            Some((SortKey::Name, false))
        );
        assert_eq!(pick(None, Some(SortKey::Size)), Some((SortKey::Size, true)));
        assert_eq!(
            pick(Some(BY_NAME), Some(SortKey::Ttl)),
            Some((SortKey::Ttl, false))
        );
        assert_eq!(
            pick(Some(BY_NAME), Some(SortKey::Name)),
            Some((SortKey::Name, true))
        );
        assert_eq!(
            pick(Some(BY_NAME.reversed()), Some(SortKey::Name)),
            Some((SortKey::Name, false))
        );
        assert_eq!(pick(Some(BY_NAME), None), None, "x: back to SCAN order");
    }

    /// Five keys; metadata known for `known` of them.
    fn swept(known: &[(u32, KeyMeta)]) -> (Vec<String>, MetaColumns) {
        let names = names(&["e", "d", "c", "b", "a"]);
        let mut meta = MetaColumns::default();
        meta.grow(names.len());
        for (index, row) in known {
            meta.set(*index, row);
        }
        (names, meta)
    }

    fn sorted(names: &[String], meta: &MetaColumns, key: SortKey, descending: bool) -> Vec<String> {
        let input = Swept {
            names,
            meta: Some(meta),
        };
        let order = sort_indices(&input, 5, Sort { key, descending });
        shown(names, &order)
    }

    #[test]
    fn unknown_metadata_sorts_last_in_scan_order() {
        let (names, meta) = swept(&[(3, row("hash", -1, 10)), (1, row("string", 5, 300))]);
        assert_eq!(meta.known(), 2);
        // Both directions keep unknown keys (e, c, a) last, in SCAN order.
        assert_eq!(
            sorted(&names, &meta, SortKey::Size, true),
            ["d", "b", "e", "c", "a"]
        );
        assert_eq!(
            sorted(&names, &meta, SortKey::Size, false),
            ["b", "d", "e", "c", "a"]
        );
        assert_eq!(
            sorted(&names, &meta, SortKey::Type, false),
            ["b", "d", "e", "c", "a"]
        );
    }

    #[test]
    fn ttl_puts_soonest_first_then_no_expiry_then_gone() {
        let (names, meta) = swept(&[
            (0, row("string", -1, 1)),
            (1, row("string", 30, 1)),
            (2, row("none", -2, 0)),
            (3, row("string", 5, 1)),
            (4, row("string", -1, 1)),
        ]);
        // Equal TTLs (`a`, `e`: no expiry) fall back to the name.
        assert_eq!(
            sorted(&names, &meta, SortKey::Ttl, false),
            ["b", "d", "a", "e", "c"]
        );
    }

    #[test]
    fn reposition_moves_newly_known_keys_into_place() {
        let (names, mut meta) = swept(&[(0, row("string", -1, 50))]);
        let sort = Sort {
            key: SortKey::Size,
            descending: true,
        };
        let mut view = KeyView::default();
        view.set_sort(Some(sort));
        view.rebuild(
            &Swept {
                names: &names,
                meta: Some(&meta),
            },
            5,
        );
        assert_eq!(
            shown(&names, view.order().unwrap()),
            ["e", "d", "c", "b", "a"]
        );

        meta.set(4, &row("string", -1, 900));
        meta.set(2, &row("string", -1, 10));
        let input = Swept {
            names: &names,
            meta: Some(&meta),
        };
        view.reposition(&input, &[4, 2]);
        assert_eq!(
            shown(&names, view.order().unwrap()),
            ["a", "e", "c", "d", "b"]
        );
        assert_eq!(view.order().unwrap().len(), 5);
    }

    #[test]
    fn columns_round_trip_a_row() {
        let mut meta = MetaColumns::default();
        meta.grow(2);
        assert!(meta.key_meta(0, "k").is_none());
        assert!(meta.set(0, &row("ReJSON-RL", 60, u128::from(u64::MAX))));
        assert!(!meta.set(0, &row("ReJSON-RL", 60, 7)), "already known");
        let back = meta.key_meta(0, "k").expect("known");
        assert_eq!(back.r_type, RedisType::Json);
        assert_eq!(back.ttl, 60);
        assert_eq!(back.size, 7);
        meta.set(1, &row("MBbloom--", -1, u128::from(u64::MAX)));
        let big = meta.key_meta(1, "b").expect("known");
        assert_eq!(big.size, u128::from(u32::MAX - 1), "size saturates");
        assert_eq!(big.r_type, RedisType::Unknown("MBbloom--".into()));
        meta.clear();
        assert_eq!(meta.known(), 0);
    }
}
