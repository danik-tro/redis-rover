//! What the mouse needs from the key list and the value pane: where the last
//! frame drew their rows, and moves that don't go through a key press.

use ratatui::layout::{Position, Rect};

use super::{state::Focus, KeySpace};

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
    fn below_header_survives_a_tiny_table() {
        assert!(below_header(Rect::new(0, 0, 10, 1)).is_empty());
    }
}
