use std::collections::BTreeMap;

use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Flex, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState,
        StatefulWidget, Widget,
    },
};

use crate::config;

/// Popup width, in columns.
const WIDTH: u16 = 30;
/// Most rows shown at once; longer lists scroll.
const MAX_ROWS: u16 = 16;

/// The `s` popup for switching logical DBs. Lists every DB with its key count
/// from `INFO`; the current one is marked.
#[derive(Debug, Default)]
pub struct DbPicker {
    open: bool,
    /// `(db, key count)`, in order.
    rows: Vec<(i64, u64)>,
    current: i64,
    list: ListState,
    /// Digits typed so far, to jump to a DB by number.
    typed: String,
}

impl DbPicker {
    /// Open the picker on `current`. Lists `0..db_count`, plus any higher DB
    /// that `INFO` reports keys for.
    pub fn open(&mut self, db_count: usize, current: i64, keyspace: &BTreeMap<i64, u64>) {
        let count = i64::try_from(db_count).unwrap_or(i64::MAX);
        let mut dbs: Vec<i64> = (0..count).collect();
        dbs.extend(keyspace.keys().filter(|&&db| db >= count));
        if !dbs.contains(&current) {
            dbs.push(current);
            dbs.sort_unstable();
        }

        self.rows = dbs
            .into_iter()
            .map(|db| (db, keyspace.get(&db).copied().unwrap_or_default()))
            .collect();
        self.current = current;
        self.typed.clear();
        let index = self.rows.iter().position(|(db, _)| *db == current);
        self.list = ListState::default().with_selected(index.or(Some(0)));
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The highlighted DB.
    pub fn selected(&self) -> Option<i64> {
        self.list
            .selected()
            .and_then(|i| self.rows.get(i))
            .map(|(db, _)| *db)
    }

    /// `j` / `k` / arrows move, digits jump to a DB by number.
    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(true),
            KeyCode::Char('k') | KeyCode::Up => self.step(false),
            KeyCode::Char(c) if c.is_ascii_digit() => self.type_digit(c),
            _ => {}
        }
    }

    fn step(&mut self, down: bool) {
        self.typed.clear();
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let i = self.list.selected().unwrap_or(0);
        let next = if down {
            if i >= last {
                0
            } else {
                i + 1
            }
        } else if i == 0 {
            last
        } else {
            i - 1
        };
        self.list.select(Some(next));
    }

    /// Add a digit to the typed number and jump to that DB. If no DB has that
    /// number, start over from this digit (so `1`, `2` reaches db12, and `3`
    /// after it starts again at db3).
    fn type_digit(&mut self, c: char) {
        self.typed.push(c);
        if !self.jump_to_typed() {
            self.typed = c.to_string();
            self.jump_to_typed();
        }
    }

    fn jump_to_typed(&mut self) -> bool {
        let Ok(db) = self.typed.parse::<i64>() else {
            return false;
        };
        match self.rows.iter().position(|(row_db, _)| *row_db == db) {
            Some(i) => {
                self.list.select(Some(i));
                true
            }
            None => false,
        }
    }
}

pub struct DbPickerWidget;

impl StatefulWidget for DbPickerWidget {
    type State = DbPicker;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if !state.open {
            return;
        }
        let cfg = config::get();
        let rows = u16::try_from(state.rows.len())
            .unwrap_or(u16::MAX)
            .min(MAX_ROWS);
        // Rows + borders + footer line.
        let [popup] = Layout::vertical([Constraint::Length(rows + 3)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::horizontal([Constraint::Length(WIDTH)])
            .flex(Flex::Center)
            .areas(popup);
        Clear.render(popup, buf);

        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .fg(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .title(" Select database ")
            .title_bottom(Line::from(" Enter switch · Esc cancel ").centered());

        let items = state.rows.iter().map(|(db, keys)| {
            let marker = if *db == state.current { "●" } else { " " };
            let noun = if *keys == 1 { "key" } else { "keys" };
            let color = if *keys == 0 {
                cfg.colors.base03
            } else {
                cfg.colors.base05
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{marker} ")).fg(cfg.colors.base0b),
                Span::raw(format!("db{db:<4}")).fg(color).bold(),
                Span::raw(format!("{keys:>10} {noun}")).fg(color),
            ]))
        });

        let list = List::new(items)
            .block(block)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("▌")
            .highlight_style(
                Style::default()
                    .bg(cfg.colors.base02)
                    .add_modifier(Modifier::BOLD),
            );
        StatefulWidget::render(list, popup, buf, &mut state.list);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker(current: i64) -> DbPicker {
        let mut picker = DbPicker::default();
        picker.open(16, current, &BTreeMap::from([(0, 5), (3, 1)]));
        picker
    }

    #[test]
    fn opens_on_current_db() {
        let picker = picker(3);
        assert!(picker.is_open());
        assert_eq!(picker.selected(), Some(3));
        assert_eq!(picker.rows.len(), 16);
        assert_eq!(picker.rows[3], (3, 1));
        assert_eq!(picker.rows[4], (4, 0));
    }

    #[test]
    fn moves_and_wraps() {
        let mut picker = picker(0);
        picker.handle_key(key(KeyCode::Char('k')));
        assert_eq!(picker.selected(), Some(15));
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), Some(0));
        picker.handle_key(key(KeyCode::Char('j')));
        assert_eq!(picker.selected(), Some(1));
    }

    #[test]
    fn digits_jump_to_db() {
        let mut picker = picker(0);
        picker.handle_key(key(KeyCode::Char('1')));
        assert_eq!(picker.selected(), Some(1));
        picker.handle_key(key(KeyCode::Char('2')));
        assert_eq!(picker.selected(), Some(12));
        // db123 doesn't exist: start over at db3.
        picker.handle_key(key(KeyCode::Char('3')));
        assert_eq!(picker.selected(), Some(3));
    }

    #[test]
    fn lists_dbs_beyond_count_and_current() {
        let mut picker = DbPicker::default();
        picker.open(2, 7, &BTreeMap::from([(5, 9)]));
        let dbs: Vec<i64> = picker.rows.iter().map(|(db, _)| *db).collect();
        assert_eq!(dbs, vec![0, 1, 5, 7]);
        assert_eq!(picker.selected(), Some(7));
    }
}
