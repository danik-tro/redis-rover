//! The type filter checklist (`Shift-t`): which Redis types the key list
//! shows.

use crossterm::event::{KeyCode, KeyEvent};

use crate::redis_client::types::{TypeFilter, STANDARD_TYPES};

/// Open checklist: one row per type, the cursor and the ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMenu {
    /// Type names, the standard ones first.
    pub(super) items: Vec<String>,
    pub(super) checked: Vec<bool>,
    pub(super) cursor: usize,
}

/// What a key did in the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeMenuOutcome {
    /// Still open.
    Open,
    /// Enter: show these types (empty: all).
    Apply(TypeFilter),
}

impl TypeMenu {
    /// Rows for the standard types, then any other type in `seen` or in
    /// the active `filter` (module types such as `MBbloom--`), ticked as
    /// `filter` is.
    #[must_use]
    pub fn new<'a>(filter: &'a TypeFilter, seen: impl IntoIterator<Item = &'a str>) -> Self {
        let mut items: Vec<String> = STANDARD_TYPES.iter().map(|&name| name.to_owned()).collect();
        let extra = filter.names().iter().map(String::as_str);
        for name in seen.into_iter().chain(extra) {
            if !items.iter().any(|item| item == name) {
                items.push(name.to_owned());
            }
        }
        let checked = items
            .iter()
            .map(|item| filter.names().contains(item))
            .collect();
        Self {
            items,
            checked,
            cursor: 0,
        }
    }

    /// The ticked types.
    #[must_use]
    pub fn filter(&self) -> TypeFilter {
        TypeFilter::new(
            self.items
                .iter()
                .zip(&self.checked)
                .filter(|(_, &checked)| checked)
                .map(|(item, _)| item),
        )
    }

    /// `j`/`k` (or the arrows) move, Space ticks, `x` clears every tick,
    /// Enter applies. Esc is handled by the caller.
    pub fn handle_key(&mut self, key: KeyEvent) -> TypeMenuOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(checked) = self.checked.get_mut(self.cursor) {
                    *checked = !*checked;
                }
            }
            KeyCode::Char('x') => self.checked.fill(false),
            KeyCode::Enter => return TypeMenuOutcome::Apply(self.filter()),
            _ => {}
        }
        TypeMenuOutcome::Open
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn lists_standard_then_seen_types_ticked_as_the_filter() {
        let menu = TypeMenu::new(
            &TypeFilter::new(["hash", "MBbloom--"]),
            ["MBbloom--", "hash"],
        );
        assert_eq!(
            menu.items,
            [
                "string",
                "list",
                "set",
                "zset",
                "hash",
                "stream",
                "json",
                "MBbloom--"
            ]
        );
        assert_eq!(
            menu.checked,
            [false, false, false, false, true, false, false, true]
        );
    }

    #[test]
    fn space_ticks_x_clears_enter_applies() {
        let mut menu = TypeMenu::new(&TypeFilter::default(), []);
        menu.handle_key(key(KeyCode::Char('k')));
        assert_eq!(menu.cursor, 0, "stays on the first row");
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Char(' '),
        ] {
            assert_eq!(menu.handle_key(key(code)), TypeMenuOutcome::Open);
        }
        assert_eq!(
            menu.handle_key(key(KeyCode::Enter)),
            TypeMenuOutcome::Apply(TypeFilter::new(["list", "zset"]))
        );
        menu.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            menu.handle_key(key(KeyCode::Enter)),
            TypeMenuOutcome::Apply(TypeFilter::default())
        );
        for _ in 0..20 {
            menu.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(menu.cursor, menu.items.len() - 1, "stops on the last row");
    }
}
