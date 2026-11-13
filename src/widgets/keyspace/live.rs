//! What the value pane shows about live updates: the liveness label in its
//! title, the rows a live refetch changed, and a key that is gone.

use std::{
    collections::HashSet,
    time::{Instant, SystemTime},
};

use ratatui::{
    style::{Style, Stylize},
    text::Line,
};

use super::{truncate_to_width, KeySpace};
use crate::{
    config,
    redis_client::{
        live::Liveness,
        types::{JsonPreview, JsonPreviewKind, KeyValue, LoadedValue},
    },
    widgets::{local_time, text_view::TextCache},
};

/// What the pane remembers between live refetches.
#[derive(Debug, Default)]
pub(super) struct LiveView {
    /// The live task's last status; `None` shows nothing (`live.mode =
    /// "off"`, or no status yet).
    pub(super) status: Option<Liveness>,
    /// Rows (or text lines) of the shown page that the last live refetch
    /// changed, sorted.
    pub(super) changed: Vec<usize>,
    /// When the highlight of `changed` ends.
    changed_until: Option<Instant>,
    /// The open key no longer exists.
    pub(super) gone: Option<Gone>,
}

/// A key deleted or expired while open.
#[derive(Debug)]
pub(super) struct Gone {
    pub(super) key: String,
    /// `key deleted · 14:22:07`.
    pub(super) label: String,
}

impl LiveView {
    pub(super) fn clear_changed(&mut self) {
        self.changed.clear();
        self.changed_until = None;
    }
}

impl KeySpace {
    /// The live task's status, shown in the value pane's title while a key
    /// is open.
    pub fn set_liveness(&mut self, status: Option<Liveness>) {
        self.live.status = status;
    }

    /// Show a value refetched because the key changed: same page and
    /// cursor, with the rows that differ from what was shown highlighted
    /// until `until`.
    pub fn show_live_value(&mut self, value: Option<LoadedValue>, until: Instant) {
        let old = self.selected_value.take();
        let old_text = self.text_cache.take();
        self.set_selected_value(value);
        let (Some(old), Some(new)) = (old.as_ref(), self.selected_value.as_ref()) else {
            return;
        };
        // A page turn in between: rows don't line up.
        if old.page.start != new.page.start {
            return;
        }
        self.live.changed = match (old_text.as_ref(), self.text_cache.as_ref()) {
            (Some(old), Some(new)) => changed_lines(old, new),
            _ => changed_rows(&old.value, &new.value),
        };
        if !self.live.changed.is_empty() {
            self.live.changed_until = Some(until);
        }
    }

    /// End the highlight once its time is up; true when that changed the
    /// screen.
    pub fn expire_changed(&mut self, now: Instant) -> bool {
        match self.live.changed_until {
            Some(until) if now >= until => {
                self.live.clear_changed();
                true
            }
            _ => false,
        }
    }

    /// `key` was deleted (or expired) at `wall`: its last value stays,
    /// dimmed, with the time in the pane's bottom border.
    pub fn mark_gone(&mut self, key: &str, expired: bool, wall: SystemTime) {
        let what = if expired { "expired" } else { "deleted" };
        self.live.clear_changed();
        self.live.gone = Some(Gone {
            key: key.to_owned(),
            label: format!(" key {what} · {} ", local_time::hms(wall)),
        });
    }

    /// The key exists again.
    pub fn clear_gone(&mut self) {
        self.live.gone = None;
    }

    /// The gone label, when it belongs to the selected key.
    pub(super) fn gone_label(&self) -> Option<&str> {
        let gone = self.live.gone.as_ref()?;
        let (key, _) = self.selected_key()?;
        (gone.key == key).then_some(gone.label.as_str())
    }

    #[must_use]
    pub fn is_gone(&self) -> bool {
        self.gone_label().is_some()
    }

    /// Rows highlighted as changed (tests).
    #[must_use]
    pub fn changed_rows(&self) -> &[usize] {
        &self.live.changed
    }
}

/// The title label for `status`, cut to `width` columns: `● live`,
/// `◌ polling 2s`, `✕ not live (reason)`. The symbol alone tells them
/// apart without colour.
pub(super) fn liveness_label(status: &Liveness, width: usize) -> Option<Line<'static>> {
    let cfg = config::get();
    let (text, color) = match status {
        Liveness::Live => ("● live".to_owned(), cfg.colors.base0b),
        Liveness::Polling { every_ms } => (
            format!("◌ polling {}", interval(*every_ms)),
            cfg.colors.base0a,
        ),
        Liveness::NotLive(reason) => (format!("✕ not live ({reason})"), cfg.colors.base08),
    };
    // One space either side, inside the border.
    let room = width.checked_sub(2).filter(|&room| room >= 2)?;
    let text = truncate_to_width(&text, room);
    Some(Line::from(format!(" {text} ")).fg(color).right_aligned())
}

/// `2s`, `1.5s`, `250ms`.
fn interval(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else if ms > 1000 {
        format!(
            "{:.1}s",
            f64::from(u32::try_from(ms).unwrap_or(u32::MAX)) / 1000.0
        )
    } else {
        format!("{ms}ms")
    }
}

/// The style of a changed row or line.
pub(super) fn changed_style() -> Style {
    let cfg = config::get();
    Style::new().bg(cfg.colors.base0a).fg(cfg.colors.base00)
}

/// Text lines that differ, by line number.
fn changed_lines(old: &TextCache, new: &TextCache) -> Vec<usize> {
    let text = |line: &Line<'_>| -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    };
    new.lines
        .iter()
        .enumerate()
        .filter(|(i, line)| old.lines.get(*i).is_none_or(|old| text(old) != text(line)))
        .map(|(i, _)| i)
        .collect()
}

/// Rows of `new` that `old` didn't show: a list by index, a hash by field
/// and value, a set by member, a sorted set by member and score, a JSON
/// object preview by field, type and size.
fn changed_rows(old: &KeyValue, new: &KeyValue) -> Vec<usize> {
    fn new_rows<T: Eq + std::hash::Hash>(
        old: impl IntoIterator<Item = T>,
        new: impl IntoIterator<Item = T>,
    ) -> Vec<usize> {
        let old: HashSet<T> = old.into_iter().collect();
        new.into_iter()
            .enumerate()
            .filter(|(_, row)| !old.contains(row))
            .map(|(i, _)| i)
            .collect()
    }
    match (old, new) {
        (KeyValue::List(old), KeyValue::List(new)) => new
            .iter()
            .enumerate()
            .filter(|(i, item)| old.get(*i) != Some(item))
            .map(|(i, _)| i)
            .collect(),
        (KeyValue::Set(old), KeyValue::Set(new)) => new_rows(old, new),
        (KeyValue::Hash(old), KeyValue::Hash(new)) => new_rows(old, new),
        (KeyValue::Zset(old), KeyValue::Zset(new)) => {
            let row = |(member, score): &(String, f64)| (member.clone(), score.to_bits());
            new_rows(old.iter().map(row), new.iter().map(row))
        }
        (
            KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields: old, .. },
                ..
            }),
            KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields: new, .. },
                ..
            }),
        ) => {
            let row = |f: &crate::redis_client::types::JsonField| {
                (f.name.clone(), f.json_type.clone(), f.bytes)
            };
            new_rows(old.iter().map(row), new.iter().map(row))
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::redis_client::types::ValuePageInfo;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|&(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    }

    #[test]
    fn rows_compare_by_index_field_member_and_score() {
        let list = |items| KeyValue::List(strings(items));
        assert_eq!(
            changed_rows(&list(&["a", "b", "c"]), &list(&["a", "x", "c", "d"])),
            [1, 3]
        );
        let hash = |items| KeyValue::Hash(pairs(items));
        assert_eq!(
            changed_rows(
                &hash(&[("name", "Ann"), ("plan", "pro")]),
                &hash(&[("plan", "pro"), ("name", "Bob")])
            ),
            [1],
            "a hash compares by field, not by row"
        );
        let set = |items| KeyValue::Set(strings(items));
        assert_eq!(changed_rows(&set(&["a", "b"]), &set(&["b", "c", "a"])), [1]);
        let zset = |items: &[(&str, f64)]| {
            KeyValue::Zset(items.iter().map(|&(m, s)| (m.to_owned(), s)).collect())
        };
        assert_eq!(
            changed_rows(
                &zset(&[("a", 1.0), ("b", 2.0)]),
                &zset(&[("a", 1.0), ("b", 3.0)])
            ),
            [1]
        );
        assert!(changed_rows(&list(&["a"]), &set(&["a"])).is_empty());
    }

    #[test]
    fn text_compares_by_line() {
        let text = |s| TextCache::from_text(s, false);
        assert_eq!(changed_lines(&text("a\nb\nc"), &text("a\nB\nc\nd")), [1, 3]);
    }

    fn value(value: KeyValue) -> LoadedValue {
        LoadedValue {
            value,
            page: ValuePageInfo::default(),
        }
    }

    #[test]
    fn a_live_refetch_highlights_until_its_time() {
        let hash = |items| Some(value(KeyValue::Hash(pairs(items))));
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.set_selected_value(hash(&[("name", "Ann")]));
        let now = Instant::now();
        let until = now + std::time::Duration::from_millis(1500);
        keyspace.show_live_value(hash(&[("name", "Bob"), ("plan", "pro")]), until);
        assert_eq!(keyspace.changed_rows(), [0, 1]);
        assert!(!keyspace.expire_changed(now));
        assert!(keyspace.expire_changed(until));
        assert!(keyspace.changed_rows().is_empty());
        assert!(!keyspace.expire_changed(until), "nothing left to clear");

        // A plain load (page turn, another key) drops the highlight.
        keyspace.show_live_value(hash(&[("name", "Cy")]), until);
        assert_eq!(keyspace.changed_rows(), [0]);
        keyspace.set_selected_value(hash(&[("name", "Cy")]));
        assert!(keyspace.changed_rows().is_empty());
    }

    #[test]
    fn labels_fit_their_room() {
        let text =
            |status: &Liveness, width| liveness_label(status, width).map(|line| line.to_string());
        assert_eq!(text(&Liveness::Live, 40).as_deref(), Some(" ● live "));
        assert_eq!(
            text(&Liveness::Polling { every_ms: 2000 }, 40).as_deref(),
            Some(" ◌ polling 2s ")
        );
        assert_eq!(
            text(&Liveness::Polling { every_ms: 1500 }, 40).as_deref(),
            Some(" ◌ polling 1.5s ")
        );
        assert_eq!(
            text(&Liveness::NotLive("reconnecting".into()), 40).as_deref(),
            Some(" ✕ not live (reconnecting) ")
        );
        let cut = text(&Liveness::NotLive("reconnecting".into()), 14).unwrap();
        assert!(cut.starts_with(" ✕ not"), "{cut}");
        assert!(
            unicode_width::UnicodeWidthStr::width(cut.as_str()) <= 14,
            "{cut}"
        );
        assert_eq!(text(&Liveness::Live, 3), None);
    }
}
