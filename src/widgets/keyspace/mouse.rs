//! What the mouse needs from the key list and the value pane: where the last
//! frame drew their rows, and moves that don't go through a key press.

use ratatui::layout::{Position, Rect};

use super::{render::key_pane_percent, state::Focus, KeySpace};

/// The narrowest and widest key pane, in percent.
const SPLIT_MIN: u16 = 25;
const SPLIT_MAX: u16 = 75;
/// How far one key press moves the divider, in percent.
const SPLIT_STEP: u16 = 5;

/// Where the last frame drew the two panes and their table rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneAreas {
    pub keys: Rect,
    pub value: Rect,
    /// The key table's rows, below its header.
    pub(super) key_rows: Rect,
    /// The value table's rows, below its header; empty for a text value.
    pub(super) value_rows: Rect,
}

/// A table's rows: what is left under its header and the rule below it.
pub(super) fn below_header(table: Rect) -> Rect {
    Rect {
        y: table.y.saturating_add(2),
        height: table.height.saturating_sub(2),
        ..table
    }
}

/// The table row drawn at a terminal cell, if it holds one of `len` rows.
fn row_at(rows: Rect, offset: usize, len: usize, column: u16, row: u16) -> Option<usize> {
    rows.contains(Position::new(column, row))
        .then(|| offset + usize::from(row - rows.y))
        .filter(|index| *index < len)
}

impl KeySpace {
    #[must_use]
    pub fn pane_areas(&self) -> PaneAreas {
        self.panes
    }

    /// The key pane's width in percent, if the user chose one.
    #[must_use]
    pub fn split(&self) -> Option<u16> {
        self.split
    }

    /// Set the key pane's width (from a saved session), kept in range.
    pub fn set_split(&mut self, split: Option<u16>) {
        self.split = split.map(|percent| percent.clamp(SPLIT_MIN, SPLIT_MAX));
    }

    /// Move the divider one step. Nothing to move with one pane.
    pub fn move_split(&mut self, right: bool) {
        if self.single_pane {
            return;
        }
        let current = self
            .split
            .unwrap_or_else(|| key_pane_percent(self.drawn_width));
        let next = if right {
            current.saturating_add(SPLIT_STEP)
        } else {
            current.saturating_sub(SPLIT_STEP)
        };
        self.split = Some(next.clamp(SPLIT_MIN, SPLIT_MAX));
    }

    /// The last frame showed one pane at a time.
    #[must_use]
    pub fn is_single_pane(&self) -> bool {
        self.single_pane
    }

    /// One pane: show the selected key's details.
    pub fn open_detail(&mut self) {
        self.detail_open = true;
    }

    /// Back to the key list. Returns whether the details were open.
    pub fn close_detail(&mut self) -> bool {
        std::mem::take(&mut self.detail_open)
    }

    /// One pane: the value's side is the one to show.
    pub(super) fn shows_detail(&self) -> bool {
        self.focus != Focus::Keys || self.detail_open
    }

    /// An overlay, menu or form is open: the panes under it take no clicks.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        self.is_popup()
            || self.is_overlay_open()
            || self.is_sort_menu_open()
            || self.is_type_menu_open()
            || self.is_wizard_active()
            || self.is_add_item_active()
            || self.is_edit_item_active()
    }

    /// The key row at a terminal cell.
    #[must_use]
    pub fn key_row_at(&self, column: u16, row: u16) -> Option<usize> {
        row_at(
            self.panes.key_rows,
            self.table.offset(),
            self.keys.len(),
            column,
            row,
        )
    }

    /// The value row at a terminal cell.
    #[must_use]
    pub fn value_row_at(&self, column: u16, row: u16) -> Option<usize> {
        row_at(
            self.panes.value_rows,
            self.value_table.offset(),
            self.value_row_count(),
            column,
            row,
        )
    }

    /// Put the cursor on key `row` with focus on the list. Returns whether
    /// another row got selected, which means another value to load.
    pub fn click_key_row(&mut self, row: usize) -> bool {
        let changed = self.table.selected() != Some(row);
        if self.focus != Focus::Keys {
            self.exit_value_focus();
        }
        self.table.select(Some(row));
        changed
    }

    /// Put the value cursor on `row` and focus the value table.
    pub fn click_value_row(&mut self, row: usize) {
        if row < self.value_row_count() {
            self.focus = Focus::Value;
            self.value_table.select(Some(row));
        }
    }

    /// The selected key's value is on screen.
    #[must_use]
    pub fn has_loaded_value(&self) -> bool {
        self.selected_value.is_some()
    }

    /// The key cursor is on the page's last (`down`) or first row, where the
    /// wheel turns the page instead of wrapping around.
    #[must_use]
    pub fn at_key_page_edge(&self, down: bool) -> bool {
        self.table.selected().is_some_and(|row| {
            if down {
                row + 1 >= self.keys.len()
            } else {
                row == 0
            }
        })
    }

    /// The value cursor is at an end of a value that has no page that way,
    /// where a `j` / `k` would wrap around but the wheel should stop.
    #[must_use]
    pub fn at_value_end(&self, down: bool) -> bool {
        if self.is_stream_open() {
            return false;
        }
        let paged = !self.stream_blocks_paging()
            && self
                .selected_value
                .as_ref()
                .is_some_and(|loaded| loaded.page.is_paged());
        let len = self.value_row_count();
        !paged
            && self.value_table.selected().is_some_and(
                |row| {
                    if down {
                        row + 1 >= len
                    } else {
                        row == 0
                    }
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_counts_from_the_table_offset() {
        let rows = Rect::new(2, 10, 20, 5);

        assert_eq!(row_at(rows, 0, 3, 2, 10), Some(0));
        assert_eq!(row_at(rows, 7, 20, 5, 12), Some(9));
    }

    #[test]
    fn cells_without_a_row_are_nothing() {
        let rows = Rect::new(2, 10, 20, 5);

        assert_eq!(row_at(rows, 0, 3, 2, 9), None, "the header");
        assert_eq!(row_at(rows, 0, 3, 22, 10), None, "beside the table");
        assert_eq!(row_at(rows, 0, 3, 2, 13), None, "below the last row");
    }

    #[test]
    fn the_divider_moves_in_steps_and_stays_in_range() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.drawn_width = 120;

        keyspace.move_split(true);
        assert_eq!(keyspace.split(), Some(40));
        keyspace.move_split(false);
        keyspace.move_split(false);
        assert_eq!(keyspace.split(), Some(30));
        for _ in 0..20 {
            keyspace.move_split(false);
        }
        assert_eq!(keyspace.split(), Some(SPLIT_MIN));
        for _ in 0..20 {
            keyspace.move_split(true);
        }
        assert_eq!(keyspace.split(), Some(SPLIT_MAX));
    }

    #[test]
    fn a_saved_split_out_of_range_is_clamped() {
        let mut keyspace = KeySpace::new(Vec::new());

        keyspace.set_split(Some(5));
        assert_eq!(keyspace.split(), Some(SPLIT_MIN));
        keyspace.set_split(Some(500));
        assert_eq!(keyspace.split(), Some(SPLIT_MAX));
        keyspace.set_split(None);
        assert_eq!(keyspace.split(), None);
    }

    #[test]
    fn with_one_pane_the_divider_does_not_move() {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.single_pane = true;

        keyspace.move_split(true);

        assert_eq!(keyspace.split(), None);
    }

    #[test]
    fn below_header_survives_a_tiny_table() {
        assert!(below_header(Rect::new(0, 0, 10, 1)).is_empty());
    }
}
