//! Scrollable text viewer shared by the keyspace (STRING / JSON values) and
//! the Pub/Sub message detail: JSON is pretty-printed and colored, `w` wraps,
//! arrows pan when wrap is off.

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Stylize,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::{config, redis_client::types::KeyValue, utils::group_thousands};

/// Scroll state of a text in the viewer (#30). The limits are measured by
/// the last render.
#[derive(Debug, Clone, Copy)]
pub struct TextView {
    /// First shown row.
    pub(crate) top: usize,
    /// First shown column (only when `wrap` is off).
    pub(crate) left: usize,
    pub(crate) wrap: bool,
    /// Rows the text area showed on the last render.
    pub(crate) height: usize,
    pub(crate) max_top: usize,
    pub(crate) max_left: usize,
}

impl Default for TextView {
    fn default() -> Self {
        Self {
            top: 0,
            left: 0,
            wrap: true,
            height: 0,
            max_top: 0,
            max_left: 0,
        }
    }
}

/// Columns one `←` / `→` press pans a text.
pub(crate) const TEXT_PAN_STEP: usize = 8;

impl TextView {
    /// Scroll `step` rows down (or up).
    pub(crate) fn scroll(&mut self, down: bool, step: usize) {
        self.top = if down {
            (self.top + step).min(self.max_top)
        } else {
            self.top.saturating_sub(step)
        };
    }

    /// Scroll by one screen.
    pub(crate) fn scroll_page(&mut self, down: bool) {
        let step = self.height.saturating_sub(1).max(1);
        self.scroll(down, step);
    }

    /// Pan sideways. Only with wrap off.
    pub(crate) fn pan(&mut self, right: bool) {
        if self.wrap {
            return;
        }
        self.left = if right {
            (self.left + TEXT_PAN_STEP).min(self.max_left)
        } else {
            self.left.saturating_sub(TEXT_PAN_STEP)
        };
    }

    /// Turn line wrapping on / off, back at the top-left.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.top = 0;
        self.left = 0;
    }
}

/// A text as display lines, built once per load.
pub struct TextCache {
    pub(crate) lines: Vec<Line<'static>>,
    /// The text is JSON (a `RedisJSON` document, or text that parses).
    pub(crate) json: bool,
}

impl TextCache {
    /// Lines for a keyspace value; `None` for collections / unsupported types.
    pub(crate) fn build(value: &KeyValue) -> Option<Self> {
        match value {
            KeyValue::String { value: text, .. } => {
                Some(Self::from_text(text, !value.is_truncated()))
            }
            KeyValue::Json(json) => Some(Self {
                lines: json_lines(json),
                json: true,
            }),
            KeyValue::List(_)
            | KeyValue::Set(_)
            | KeyValue::Hash(_)
            | KeyValue::Zset(_)
            | KeyValue::Unknown => None,
        }
    }

    /// Lines for `text`: pretty JSON when `try_json` and it parses (a cut
    /// text never does), plain lines otherwise.
    #[must_use]
    pub fn from_text(text: &str, try_json: bool) -> Self {
        match try_json.then(|| parse_json_text(text)).flatten() {
            Some(json) => Self {
                lines: json_lines(&json),
                json: true,
            },
            None => Self {
                lines: plain_lines(text),
                json: false,
            },
        }
    }

    /// Ready-made plain lines (e.g. a hex dump).
    pub(crate) fn from_lines(lines: Vec<String>) -> Self {
        let color = config::get().colors.base05;
        let mut lines: Vec<Line<'static>> =
            lines.into_iter().map(|l| Line::from(l).fg(color)).collect();
        if lines.is_empty() {
            lines.push(Line::default());
        }
        Self { lines, json: false }
    }
}

/// Parse a text as JSON if it looks like an object or array.
fn parse_json_text(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// A plain text, one display line per text line.
fn plain_lines(text: &str) -> Vec<Line<'static>> {
    let color = config::get().colors.base05;
    let mut lines: Vec<Line<'static>> = text
        .lines()
        .map(|line| Line::from(line.to_owned()).fg(color))
        .collect();
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Pretty-print `value` with 2-space indents and syntax colors.
fn json_lines(value: &serde_json::Value) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    push_json(value, 0, Vec::new(), "", &mut out);
    out
}

/// Append the lines of `value` at `depth`. `prefix` goes before the value on
/// its first line (an object key), `comma` after it on its last line.
fn push_json(
    value: &serde_json::Value,
    depth: usize,
    prefix: Vec<Span<'static>>,
    comma: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    use serde_json::Value;

    let colors = &config::get().colors;
    let punct = |text: &'static str| Span::from(text).fg(colors.base05);
    let line = |spans: Vec<Span<'static>>| {
        let mut all = vec![Span::raw("  ".repeat(depth))];
        all.extend(spans);
        Line::from(all)
    };
    let (open, close) = match value {
        Value::Array(items) if !items.is_empty() => ("[", "]"),
        Value::Object(map) if !map.is_empty() => ("{", "}"),
        scalar => {
            let mut spans = prefix;
            spans.push(json_scalar(scalar));
            spans.push(punct(comma));
            out.push(line(spans));
            return;
        }
    };

    let mut first = prefix;
    first.push(punct(open));
    out.push(line(first));
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let comma = if i + 1 < items.len() { "," } else { "" };
                push_json(item, depth + 1, Vec::new(), comma, out);
            }
        }
        Value::Object(map) => {
            for (i, (key, item)) in map.iter().enumerate() {
                let comma = if i + 1 < map.len() { "," } else { "" };
                let key = vec![
                    Span::from(Value::String(key.clone()).to_string()).fg(colors.base0d),
                    punct(": "),
                ];
                push_json(item, depth + 1, key, comma, out);
            }
        }
        _ => {}
    }
    out.push(line(vec![punct(close), punct(comma)]));
}

/// One JSON scalar (or empty array / object), colored by kind.
fn json_scalar(value: &serde_json::Value) -> Span<'static> {
    use serde_json::Value;

    let colors = &config::get().colors;
    let color = match value {
        Value::String(_) => colors.base0b,
        Value::Number(_) => colors.base09,
        Value::Bool(_) | Value::Null => colors.base0e,
        Value::Array(_) | Value::Object(_) => colors.base05,
    };
    Span::from(value.to_string()).fg(color)
}

/// How the viewer is labelled and which keys its footer advertises.
pub struct TextLabels<'a> {
    /// Header, e.g. `Value` (` · JSON` is added for JSON).
    pub header: &'a str,
    /// `n/p page` works (the keyspace value viewer).
    pub paging: bool,
    /// Extra key hint at the end of the focused footer, e.g. `Esc close`.
    pub close: Option<&'a str>,
}

/// A header, the scrollable text, and a footer with the shown line range,
/// a note (`note`, e.g. truncation) and key hints.
pub fn render(
    cache: &TextCache,
    labels: &TextLabels<'_>,
    note: Option<String>,
    focused: bool,
    area: Rect,
    buf: &mut Buffer,
    view: &mut TextView,
) {
    let cfg = config::get();
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let header = if cache.json {
        format!("{} · JSON", labels.header)
    } else {
        labels.header.to_owned()
    };
    Paragraph::new(Line::from(header).fg(cfg.colors.base03).bold()).render(header_area, buf);

    let height = usize::from(body_area.height);
    let width = usize::from(body_area.width);
    let paragraph = if view.wrap {
        Paragraph::new(cache.lines.clone()).wrap(Wrap { trim: false })
    } else {
        Paragraph::new(cache.lines.clone())
    };
    let total = if view.wrap {
        paragraph.line_count(body_area.width)
    } else {
        cache.lines.len()
    };
    view.height = height;
    view.max_top = total.saturating_sub(height);
    view.max_left = if view.wrap {
        0
    } else {
        cache
            .lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or(0)
            .saturating_sub(width)
    };
    view.top = view.top.min(view.max_top);
    view.left = view.left.min(view.max_left);

    let to_u16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    if view.wrap {
        paragraph
            .scroll((to_u16(view.top), 0))
            .render(body_area, buf);
    } else {
        // Slice the rows first: `scroll` takes a `u16` offset.
        let rows: Vec<Line<'static>> = cache
            .lines
            .iter()
            .skip(view.top)
            .take(height)
            .cloned()
            .collect();
        Paragraph::new(rows)
            .scroll((0, to_u16(view.left)))
            .render(body_area, buf);
    }

    let footer = text_footer(note, labels, view, total, focused);
    Paragraph::new(Line::from(footer).fg(cfg.colors.base03))
        .alignment(Alignment::Right)
        .render(footer_area, buf);
}

/// Footer under a text: note, the shown line range, and the keys that work
/// (`Enter to scroll` when there's more than fits and it isn't focused).
fn text_footer(
    note: Option<String>,
    labels: &TextLabels<'_>,
    view: &TextView,
    total: usize,
    focused: bool,
) -> String {
    let mut parts: Vec<String> = note.into_iter().collect();
    if total > view.height {
        let last = (view.top + view.height).min(total);
        parts.push(format!(
            "lines {}–{} of {}",
            group_thousands(view.top + 1),
            group_thousands(last),
            group_thousands(total)
        ));
    }
    if focused {
        let page = if labels.paging { " · n/p page" } else { "" };
        let pan = if view.wrap { "" } else { " · ←/→ pan" };
        let wrap = if view.wrap { "w no-wrap" } else { "w wrap" };
        let close = labels.close.map(|c| format!(" · {c}")).unwrap_or_default();
        parts.push(format!("j/k scroll{page}{pan} · {wrap}{close}"));
    } else if total > view.height || view.max_left > 0 {
        parts.push("Enter to scroll".to_owned());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn json_is_pretty_printed_in_order() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"z":1,"b":[true,null],"c":{},"s":"x\"y"}"#).unwrap();
        let lines: Vec<String> = json_lines(&value).iter().map(line_text).collect();
        assert_eq!(
            lines,
            vec![
                "{",
                "  \"z\": 1,",
                "  \"b\": [",
                "    true,",
                "    null",
                "  ],",
                "  \"c\": {},",
                "  \"s\": \"x\\\"y\"",
                "}",
            ]
        );
    }

    #[test]
    fn string_json_detection() {
        assert!(parse_json_text("  {\"a\": 1}").is_some());
        assert!(parse_json_text("[1, 2]").is_some());
        assert!(parse_json_text("42").is_none(), "only objects / arrays");
        assert!(parse_json_text("{not json").is_none());

        let value = |text: &str, total_len: usize| KeyValue::String {
            value: text.into(),
            total_len,
        };
        let json = TextCache::build(&value("{\"a\":1}", 7)).unwrap();
        assert!(json.json);
        assert_eq!(json.lines.len(), 3);

        assert!(!TextCache::build(&value("{\"a\":1}", 100)).unwrap().json);

        let plain = TextCache::build(&value("a\nb", 3)).unwrap();
        assert!(!plain.json);
        assert_eq!(plain.lines.len(), 2);
        assert!(TextCache::build(&KeyValue::List(Vec::new())).is_none());

        assert!(TextCache::from_text("{\"a\":1}", true).json);
        assert!(!TextCache::from_text("{\"a\":1}", false).json);
        assert_eq!(TextCache::from_lines(Vec::new()).lines.len(), 1);
    }

    #[test]
    fn footer_hints_follow_the_labels() {
        let view = TextView {
            height: 10,
            ..TextView::default()
        };
        let keyspace = TextLabels {
            header: "Value",
            paging: true,
            close: None,
        };
        assert_eq!(
            text_footer(None, &keyspace, &view, 5, true),
            "j/k scroll · n/p page · w no-wrap"
        );
        let detail = TextLabels {
            header: "Payload",
            paging: false,
            close: Some("Esc close"),
        };
        assert_eq!(
            text_footer(Some("note".into()), &detail, &view, 5, true),
            "note · j/k scroll · w no-wrap · Esc close"
        );
    }
}
