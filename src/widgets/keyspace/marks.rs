//! Marked keys (`Space`, `Shift-m`, `Shift-u`): what `d` deletes in one go.
//! Kept by name, so they survive page turns, sorting and filters; cleared
//! on a DB switch and after the bulk delete.

use super::KeySpace;

impl KeySpace {
    /// Mark or unmark the selected key (a tree group row has no key).
    /// Returns whether a key was there to mark.
    pub fn toggle_mark(&mut self) -> bool {
        let Some(row) = self.table.selected() else {
            return false;
        };
        let Some(name) = self.leaf_name(row) else {
            return false;
        };
        if !self.marks.remove(&name) {
            self.marks.insert(name);
        }
        true
    }

    /// Mark every key on the page; if they all are already, unmark them.
    pub fn mark_all_visible(&mut self) {
        let names: Vec<String> = (0..self.keys.len())
            .filter_map(|row| self.leaf_name(row))
            .collect();
        if names.iter().all(|name| self.marks.contains(name)) {
            for name in &names {
                self.marks.remove(name);
            }
        } else {
            self.marks.extend(names);
        }
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
    }

    /// How many keys are marked.
    #[must_use]
    pub fn marked_count(&self) -> usize {
        self.marks.len()
    }

    /// The marked keys, sorted (the order the preview lists them in).
    #[must_use]
    pub fn marked_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.marks.iter().cloned().collect();
        keys.sort_unstable();
        keys
    }

    #[must_use]
    pub fn is_marked(&self, name: &str) -> bool {
        self.marks.contains(name)
    }

    /// The key on `row`, unless it is a tree group.
    fn leaf_name(&self, row: usize) -> Option<String> {
        self.key_at(row).map(|meta| meta.key.clone())
    }
}
