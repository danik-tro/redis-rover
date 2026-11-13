//! The order the key list shows the loaded part of the sweep in.
//!
//! By default that is SCAN order and nothing is stored. Once the user sorts,
//! `order` holds indices into `KeyspaceState::scanned` in display order.
//! New SCAN batches are merged into it as sorted runs. A full rebuild (after
//! a sort change) runs inline when the sweep is small, otherwise off the
//! keyspace lock on a copy of the names ([`SortJob`]).

use std::cmp::Ordering;

/// What the key list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
}

/// A sort column and its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// Up to this many loaded keys a full re-sort runs inline (under the
/// keyspace lock); above it, in `spawn_blocking` on a [`SortJob`]. From the
/// `sort_names` bench: 100k names take about 9 ms, so 50k stay well inside
/// one 16 ms frame.
pub const SYNC_SORT_MAX: usize = 50_000;

/// Read access to key names by sweep index.
pub trait Names {
    fn name(&self, index: u32) -> &str;
}

impl Names for [String] {
    fn name(&self, index: u32) -> &str {
        &self[index as usize]
    }
}

/// Compare two sweep indices under `sort`. Ties fall back to the index, so
/// the order is total and deterministic.
pub fn compare<N: Names + ?Sized>(names: &N, sort: Sort, a: u32, b: u32) -> Ordering {
    let by_key = match sort.key {
        SortKey::Name => names.name(a).cmp(names.name(b)),
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
pub fn sort_indices<N: Names + ?Sized>(names: &N, len: u32, sort: Sort) -> Vec<u32> {
    let mut order: Vec<u32> = (0..len).collect();
    order.sort_unstable_by(|&a, &b| compare(names, sort, a, b));
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
    sort: Option<Sort>,
    /// Sweep indices in display order. `None` with a sort set means it must
    /// be rebuilt; `None` without one means SCAN order.
    order: Option<Vec<u32>>,
    /// Bumped on every sort change, so a rebuild for an older sort is dropped.
    sort_generation: u64,
}

impl KeyView {
    #[must_use]
    pub fn sort(&self) -> Option<Sort> {
        self.sort
    }

    /// The display order, when a sort is set and the order is built.
    #[must_use]
    pub fn order(&self) -> Option<&[u32]> {
        self.order.as_deref()
    }

    /// A sort is set but its order isn't built yet.
    #[must_use]
    pub fn needs_rebuild(&self) -> bool {
        self.sort.is_some() && self.order.is_none()
    }

    /// Change the sort. The order is rebuilt by the next page fetch.
    pub fn set_sort(&mut self, sort: Option<Sort>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.order = None;
        self.sort_generation += 1;
    }

    /// The sweep restarted: keep the sort, drop the order.
    pub fn reset(&mut self) {
        self.order = None;
        self.sort_generation += 1;
    }

    /// Merge the new sweep indices `from..to` into the built order.
    pub fn merge_new<N: Names + ?Sized>(&mut self, names: &N, from: u32, to: u32) {
        let (Some(sort), Some(order)) = (self.sort, self.order.as_mut()) else {
            return;
        };
        merge_run(order, (from..to).collect(), |a, b| {
            compare(names, sort, a, b)
        });
    }

    /// Build the order inline over `len` names.
    pub fn rebuild<N: Names + ?Sized>(&mut self, names: &N, len: u32) {
        if let Some(sort) = self.sort {
            self.order = Some(sort_indices(names, len, sort));
        }
    }

    /// Copy what a rebuild over `names[..len]` needs, to sort it off the
    /// lock. `None` without a sort.
    #[must_use]
    pub fn job(&self, names: &[String], epoch: u64) -> Option<SortJob> {
        let sort = self.sort?;
        Some(SortJob {
            names: NameArena::copy(names),
            sort,
            epoch,
            sort_generation: self.sort_generation,
        })
    }

    /// Install a finished [`SortJob`]'s order, merging in the indices
    /// `sorted.len..len` that arrived meanwhile. Returns `false`, and changes
    /// nothing, if the sort changed or the order was built meanwhile (the
    /// caller checks the sweep epoch).
    pub fn install<N: Names + ?Sized>(&mut self, sorted: SortedOrder, names: &N, len: u32) -> bool {
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

impl Names for NameArena {
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
    sort: Sort,
    /// `KeyspaceState::sweep_epoch` when the job was made.
    pub(super) epoch: u64,
    sort_generation: u64,
}

impl SortJob {
    /// Sort. Pure CPU work; meant for `spawn_blocking`.
    #[must_use]
    pub fn run(self) -> SortedOrder {
        let len = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        SortedOrder {
            order: sort_indices(&self.names, len, self.sort),
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
        let job = view.job(&all, 0).expect("sort set");
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
        let job = view.job(&all, 0).expect("sort set");
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
        assert!(view.job(&names(&["a"]), 0).is_none());
    }
}
