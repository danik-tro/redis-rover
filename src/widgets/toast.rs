//! Short non-modal messages ("Copied …", Pub/Sub notifications), owned by the
//! App and drawn above every mode and popup.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, StatefulWidget, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{config, widgets::keyspace::truncate_to_width};

/// How long a toast stays up.
pub const TOAST_TIMEOUT: Duration = Duration::from_millis(2500);
/// Background toasts waiting for the current one to go away.
const QUEUE_CAP: usize = 3;

#[derive(Debug, Default)]
pub struct Toast {
    /// The message shown and when it expires (`None` = until replaced).
    current: Option<(String, Option<Instant>)>,
    /// Background messages shown one after another once `current` expires.
    queue: VecDeque<String>,
}

impl Toast {
    /// Show `msg` now, replacing the current toast: user feedback. It stays
    /// for [`TOAST_TIMEOUT`] from `now`, or until replaced if `sticky`.
    pub fn show(&mut self, msg: impl Into<String>, sticky: bool, now: Instant) {
        let expires = (!sticky).then(|| now + TOAST_TIMEOUT);
        self.current = Some((msg.into(), expires));
    }

    /// A background message (Pub/Sub notification). It never replaces the
    /// current toast (a sticky "Copying …" in particular): it waits in a
    /// short queue, the oldest dropped when full. Returns whether it is
    /// shown now.
    pub fn notify(&mut self, msg: impl Into<String>, now: Instant) -> bool {
        if self.current.is_none() {
            self.show(msg, false, now);
            return true;
        }
        if self.queue.len() == QUEUE_CAP {
            self.queue.pop_front();
        }
        self.queue.push_back(msg.into());
        false
    }

    /// Drop the toast once it has expired, then show the next queued one.
    /// Returns whether what is shown changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self
            .current
            .as_ref()
            .is_some_and(|(_, expires)| expires.is_some_and(|at| now >= at))
        {
            self.current = None;
            changed = true;
        }
        if self.current.is_none() {
            if let Some(next) = self.queue.pop_front() {
                self.current = Some((next, Some(now + TOAST_TIMEOUT)));
                changed = true;
            }
        }
        changed
    }

    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.current.as_ref().map(|(msg, _)| msg.as_str())
    }
}

/// A one-line boxed message at the bottom-right of the area, cut with `…` to
/// fit.
pub struct ToastWidget;

impl StatefulWidget for ToastWidget {
    type State = Toast;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let Some(msg) = state.message() else {
            return;
        };
        // Borders and padding take 4 columns; keep a column of margin.
        let max_text = usize::from(area.width.saturating_sub(6));
        let text = truncate_to_width(msg, max_text);
        let width = u16::try_from(text.width())
            .unwrap_or(u16::MAX)
            .saturating_add(4);
        if width < 5 || area.height < 5 {
            return;
        }
        let toast = Rect {
            x: area.right() - width - 1,
            y: area.bottom() - 4,
            width,
            height: 3,
        };
        let cfg = config::get();
        Clear.render(toast, buf);
        Paragraph::new(Line::from(text.into_owned()).fg(cfg.colors.base05))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base0b)
                    .bg(cfg.colors.base00)
                    .padding(Padding::horizontal(1)),
            )
            .render(toast, buf);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn render(toast: &mut Toast, width: u16) -> String {
        let area = Rect::new(0, 0, width, 10);
        let mut buf = Buffer::empty(area);
        ToastWidget.render(area, &mut buf, toast);
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn toast_expires() {
        let mut toast = Toast::default();
        toast.show("Copied", false, Instant::now());
        assert!(render(&mut toast, 80).contains("Copied"));
        toast.expire(Instant::now());
        assert_eq!(toast.message(), Some("Copied"));
        toast.expire(Instant::now() + TOAST_TIMEOUT);
        assert_eq!(toast.message(), None);
        assert!(!render(&mut toast, 80).contains("Copied"));

        toast.show("Copying…", true, Instant::now());
        toast.expire(Instant::now() + TOAST_TIMEOUT * 10);
        assert_eq!(toast.message(), Some("Copying…"), "sticky until replaced");
    }

    #[test]
    fn notify_never_replaces_a_sticky_toast() {
        let mut toast = Toast::default();
        toast.show("Copying key…", true, Instant::now());
        toast.notify("chat: hi", Instant::now());
        toast.expire(Instant::now() + TOAST_TIMEOUT * 10);
        assert_eq!(toast.message(), Some("Copying key…"));

        // The copy finishes: its result shows, then the queued notice.
        toast.show("Copied", false, Instant::now());
        assert_eq!(toast.message(), Some("Copied"));
        let later = Instant::now() + TOAST_TIMEOUT;
        toast.expire(later);
        assert_eq!(toast.message(), Some("chat: hi"));
        toast.expire(later + TOAST_TIMEOUT);
        assert_eq!(toast.message(), None);
    }

    #[test]
    fn notify_shows_at_once_when_free_and_queue_is_capped() {
        let mut toast = Toast::default();
        toast.notify("a", Instant::now());
        assert_eq!(toast.message(), Some("a"));
        for msg in ["b", "c", "d", "e"] {
            toast.notify(msg, Instant::now());
        }
        let mut now = Instant::now();
        let mut seen = Vec::new();
        for _ in 0..5 {
            now += TOAST_TIMEOUT;
            toast.expire(now);
            seen.extend(toast.message().map(str::to_owned));
        }
        // "b" was dropped when the queue overflowed.
        assert_eq!(seen, vec!["c", "d", "e"]);
    }

    #[test]
    fn long_text_is_cut_to_fit() {
        let mut toast = Toast::default();
        toast.show(
            format!("orders:new: {}", "x".repeat(200)),
            false,
            Instant::now(),
        );
        let text = render(&mut toast, 40);
        assert!(text.contains("orders:new: xxx"), "{text}");
        assert!(text.contains('…'), "{text}");
    }
}
