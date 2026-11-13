//! The prefix tree view of the loaded keys.
//!
//! Keys are grouped by a separator (`:` by default): `user:1:name` sits in
//! `user:` → `user:1:`. The rows are built from the name-sorted order, where
//! every key under a prefix is contiguous, so a group is just a range of
//! that order: its end is found with a binary search, and a folded group
//! costs O(log n) whatever its size. Only the visible rows are stored.
//!
//! At each level the groups come first (by name), then the keys directly in
//! it, in the active sort (by name if none). Below the top level a group
//! holding a single key shows as that key; the top level always shows its
//! prefixes.

use std::collections::HashSet;

use super::view::{compare, Sort, SortInput, SortKey};

/// Default key separator.
pub const DEFAULT_SEPARATOR: &str = ":";

/// One visible row of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    /// Keys `start..end` of the name order share the first `prefix_len`
    /// bytes of their name (the separator included).
    Group {
        start: u32,
        end: u32,
        prefix_len: u32,
        depth: u16,
    },
    /// The key at this sweep index.
    Leaf { index: u32, depth: u16 },
}

/// A row of the current page, as the key table draws it. Leaves are in
/// the same order as the page's keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRow {
    Group {
        /// `user:1:`, the separator included.
        prefix: String,
        /// Keys loaded under it.
        count: usize,
        depth: u16,
        expanded: bool,
    },
    Leaf {
        depth: u16,
    },
}

/// Tree mode: the separator, which groups are open, and the visible rows.
#[derive(Debug, Clone)]
pub struct Tree {
    separator: String,
    expanded: HashSet<String>,
    rows: Vec<TreeRow>,
}

impl Tree {
    /// An empty separator falls back to [`DEFAULT_SEPARATOR`].
    #[must_use]
    pub fn new(separator: &str) -> Self {
        let separator = if separator.is_empty() {
            DEFAULT_SEPARATOR
        } else {
            separator
        };
        Self {
            separator: separator.to_owned(),
            expanded: HashSet::new(),
            rows: Vec::new(),
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    pub fn clear_rows(&mut self) {
        self.rows.clear();
    }

    #[must_use]
    pub fn is_expanded(&self, prefix: &str) -> bool {
        self.expanded.contains(prefix)
    }

    /// Open or close the group `prefix`. Returns `true` if it changed.
    pub fn set_expanded(&mut self, prefix: &str, open: bool) -> bool {
        if open {
            self.expanded.insert(prefix.to_owned())
        } else {
            self.expanded.remove(prefix)
        }
    }

    /// Rebuild the visible rows over `by_name` (sweep indices sorted by
    /// name, ascending). Keys directly in a group follow `sort`.
    pub fn rebuild<I: SortInput + ?Sized>(
        &mut self,
        input: &I,
        by_name: &[u32],
        sort: Option<Sort>,
    ) {
        let mut rows = std::mem::take(&mut self.rows);
        rows.clear();
        let level = Level {
            input,
            by_name,
            separator: &self.separator,
            expanded: &self.expanded,
            sort,
        };
        level.build(0, by_name.len(), 0, 0, &mut rows);
        self.rows = rows;
    }
}

struct Level<'a, I: ?Sized> {
    input: &'a I,
    by_name: &'a [u32],
    separator: &'a str,
    expanded: &'a HashSet<String>,
    sort: Option<Sort>,
}

impl<I: SortInput + ?Sized> Level<'_, I> {
    /// Rows for the keys `start..end` of the name order, which all share
    /// the first `prefix_len` bytes.
    fn build(
        &self,
        start: usize,
        end: usize,
        prefix_len: usize,
        depth: u16,
        rows: &mut Vec<TreeRow>,
    ) {
        let mut leaves = Vec::new();
        let mut i = start;
        while i < end {
            let name = self.input.name(self.by_name[i]);
            let Some(at) = name[prefix_len..].find(self.separator) else {
                leaves.push(self.by_name[i]);
                i += 1;
                continue;
            };
            let prefix = &name[..prefix_len + at + self.separator.len()];
            // Sorted by name: every key under `prefix` follows this one.
            let group_end = i + self.by_name[i..end]
                .partition_point(|&index| self.input.name(index).starts_with(prefix));
            // A group of one key below the top level is just that key.
            if depth > 0 && group_end - i == 1 {
                leaves.push(self.by_name[i]);
                i += 1;
                continue;
            }
            rows.push(TreeRow::Group {
                start: to_u32(i),
                end: to_u32(group_end),
                prefix_len: to_u32(prefix.len()),
                depth,
            });
            if self.expanded.contains(prefix) {
                self.build(i, group_end, prefix.len(), depth + 1, rows);
            }
            i = group_end;
        }
        match self.sort {
            Some(sort) if sort.key != SortKey::Name || sort.descending => {
                leaves.sort_unstable_by(|&a, &b| compare(self.input, sort, a, b));
            }
            _ => {}
        }
        rows.extend(
            leaves
                .into_iter()
                .map(|index| TreeRow::Leaf { index, depth }),
        );
    }
}

// Positions and lengths stay below `u32::MAX`: the sweep never outgrows it.
#[allow(clippy::cast_possible_truncation)]
fn to_u32(n: usize) -> u32 {
    n as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::view::sort_indices;

    const BY_NAME: Sort = Sort {
        key: SortKey::Name,
        descending: false,
    };

    fn labels(tree: &Tree, names: &[String], by_name: &[u32]) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|row| match *row {
                TreeRow::Group {
                    start,
                    end,
                    prefix_len,
                    depth,
                } => {
                    let name = &names[by_name[start as usize] as usize];
                    format!(
                        "{}▸ {} ({})",
                        "  ".repeat(depth.into()),
                        &name[..prefix_len as usize],
                        end - start
                    )
                }
                TreeRow::Leaf { index, depth } => {
                    format!("{}{}", "  ".repeat(depth.into()), names[index as usize])
                }
            })
            .collect()
    }

    fn setup(list: &[&str]) -> (Vec<String>, Vec<u32>) {
        let names: Vec<String> = list.iter().map(|&n| n.to_owned()).collect();
        let by_name = sort_indices(names.as_slice(), to_u32(names.len()), BY_NAME);
        (names, by_name)
    }

    #[test]
    fn groups_fold_and_unfold() {
        let (names, by_name) = setup(&["user:1:name", "order:9", "user:2:name", "user:1:email"]);
        let mut tree = Tree::new(":");
        tree.rebuild(names.as_slice(), &by_name, None);
        assert_eq!(
            labels(&tree, &names, &by_name),
            ["▸ order: (1)", "▸ user: (3)"]
        );

        tree.set_expanded("user:", true);
        tree.rebuild(names.as_slice(), &by_name, None);
        assert_eq!(
            labels(&tree, &names, &by_name),
            [
                "▸ order: (1)",
                "▸ user: (3)",
                "  ▸ user:1: (2)",
                "  user:2:name"
            ]
        );

        tree.set_expanded("user:1:", true);
        tree.rebuild(names.as_slice(), &by_name, None);
        assert_eq!(
            labels(&tree, &names, &by_name)[2..],
            [
                "  ▸ user:1: (2)",
                "    user:1:email",
                "    user:1:name",
                "  user:2:name"
            ]
        );
    }

    #[test]
    fn groups_come_before_keys_and_keys_follow_the_sort() {
        let (names, by_name) = setup(&["b", "a:x", "c", "a", "z:1"]);
        let mut tree = Tree::new(":");
        let descending = Sort {
            descending: true,
            ..BY_NAME
        };
        tree.rebuild(names.as_slice(), &by_name, Some(descending));
        assert_eq!(
            labels(&tree, &names, &by_name),
            ["▸ a: (1)", "▸ z: (1)", "c", "b", "a"]
        );
    }

    #[test]
    fn multi_char_and_trailing_separators() {
        let (names, by_name) = setup(&["a::b", "a::", "a:b"]);
        let mut tree = Tree::new("::");
        tree.set_expanded("a::", true);
        tree.rebuild(names.as_slice(), &by_name, None);
        assert_eq!(
            labels(&tree, &names, &by_name),
            ["▸ a:: (2)", "  a::", "  a::b", "a:b"]
        );
        assert_eq!(Tree::new("").separator, ":");
    }
}
