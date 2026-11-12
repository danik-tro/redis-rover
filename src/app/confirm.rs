//! Yes / no questions the App asks itself, drawn above every mode: lifting
//! read-only on Prod.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap},
};
use unicode_width::UnicodeWidthStr;

use super::App;
use crate::{action::Action, config, target::Environment};

/// A modal question; `y` accepts, `n` / Esc cancels.
#[derive(Debug)]
pub(super) enum PendingConfirm {
    /// Allow writes on Prod.
    LiftReadOnly,
}

impl App {
    /// `R`: turn read-only on at once; lifting it on Prod asks first.
    pub(super) fn toggle_read_only(&mut self, now: Instant) {
        if !self.is_read_only() {
            self.set_read_only(true);
            self.toast
                .show("Read-only on: writes are disabled", false, now);
        } else if self.environment == Environment::Prod {
            self.pending = Some(PendingConfirm::LiftReadOnly);
        } else {
            self.lift_read_only(now);
        }
    }

    fn lift_read_only(&mut self, now: Instant) {
        self.set_read_only(false);
        self.toast
            .show("Read-only off: writes are enabled", false, now);
    }

    /// Keys while a question is open. Popup-local like Enter / Esc in the
    /// other popups: `y` accepts, `n` / Esc cancels, the quit binding quits.
    pub(super) fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<Action> {
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match key.code {
            KeyCode::Char('y') if plain => {
                self.accept_confirm();
                None
            }
            KeyCode::Char('n') if plain => {
                self.pending = None;
                None
            }
            KeyCode::Esc => {
                self.pending = None;
                None
            }
            _ if Self::is_quit_key(key) => Some(Action::Quit),
            _ => None,
        }
    }

    fn accept_confirm(&mut self) {
        let now = self.clock.now();
        match self.pending.take() {
            Some(PendingConfirm::LiftReadOnly) => self.lift_read_only(now),
            None => {}
        }
    }

    /// Draw the open question, centred in `area`.
    pub(super) fn render_confirm(&self, area: Rect, buf: &mut Buffer) {
        let Some(pending) = &self.pending else {
            return;
        };
        let cfg = config::get();
        let env = self.environment;
        let (title, body, hint) = match pending {
            PendingConfirm::LiftReadOnly => (
                " Lift read-only ".to_owned(),
                vec![Line::from(format!("Allow writes to {}?", env.label()))],
                "y: allow writes   n/Esc: cancel",
            ),
        };
        let mut lines = body;
        lines.push(Line::from(""));
        lines.push(Line::styled(hint, cfg.colors.base04).alignment(Alignment::Center));

        let text_w = lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or(0)
            .max(title.width());
        let popup = centered(area, text_w, lines.len());
        Clear.render(popup, buf);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(title)
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.env(env))
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05),
            )
            .render(popup, buf);
    }
}

/// A box for `text_w` columns and `rows` lines (plus borders and padding),
/// centred and clamped to `area`.
pub(super) fn centered(area: Rect, text_w: usize, rows: usize) -> Rect {
    let w = u16::try_from(text_w.saturating_add(4))
        .unwrap_or(u16::MAX)
        .max(30)
        .min(area.width);
    let h = u16::try_from(rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let [_, row, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(h),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(w),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(row);
    popup
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::PendingConfirm;
    use crate::{
        action::Action,
        app::tests::{app, key},
        target::Environment,
    };

    #[test]
    fn toggle_flips_read_only_and_the_hint() {
        let mut app = app();
        let writable_hint = app.summary.hint.clone();
        app.update(&Action::ToggleReadOnly);
        assert!(app.is_read_only());
        assert!(app.summary.read_only);
        assert_ne!(app.summary.hint, writable_hint, "write keys leave the hint");
        app.update(&Action::ToggleReadOnly);
        assert!(!app.is_read_only());
        assert_eq!(app.summary.hint, writable_hint);
    }

    #[test]
    fn lifting_read_only_on_prod_asks_first() {
        let mut app = app().environment(Environment::Prod).read_only(true);
        app.update(&Action::ToggleReadOnly);
        assert!(app.is_read_only(), "not before the answer");
        assert!(matches!(app.pending, Some(PendingConfirm::LiftReadOnly)));

        // `n` keeps it read-only.
        assert_eq!(app.handle_key_event(key(KeyCode::Char('n'))), None);
        assert!(app.pending.is_none());
        assert!(app.is_read_only());

        // Other keys are swallowed; `y` lifts it.
        app.update(&Action::ToggleReadOnly);
        assert_eq!(app.handle_key_event(key(KeyCode::Char('d'))), None);
        assert!(app.is_read_only());
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(!app.is_read_only());

        // Turning it back on never asks.
        app.update(&Action::ToggleReadOnly);
        assert!(app.pending.is_none());
        assert!(app.is_read_only());
    }

    #[test]
    fn toggle_moves_the_shared_flag() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut app = app().read_only_flag(flag.clone());
        app.update(&Action::ToggleReadOnly);
        assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
    }
}
