//! Scrollable text viewer shared by the keyspace (STRING / JSON values) and
//! the Pub/Sub message detail: JSON is pretty-printed and colored, `w` wraps,
//! arrows pan when wrap is off.

use std::cell::RefCell;

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::{
    config,
    redis_client::types::{JsonPreviewKind, KeyValue},
    utils::group_thousands,
    widgets::keyspace::format_bytes,
};

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
            self.top.saturating_add(step).min(self.max_top)
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
    /// What a frame needs to know about every line, measured once (#79).
    measure: RefCell<Measure>,
}

/// Sizes of a [`TextCache`]'s lines, so a frame handles only the lines it
/// shows. Filled by the first render; the wrap part again when the width
/// changes.
#[derive(Default)]
struct Measure {
    /// Display width of each line.
    widths: Vec<usize>,
    widest: usize,
    /// The body width the text was last wrapped at, with the first wrapped
    /// row of each line and, last, the row count of the whole text.
    wrap: Option<(u16, Vec<usize>)>,
}

/// Rows `line` takes when wrapped at `width` columns, as `Paragraph` wraps
/// it. Only a line wider than that is wrapped to find out.
fn wrapped_rows(line: &Line<'_>, line_width: usize, width: u16) -> usize {
    if line_width <= usize::from(width) {
        return 1;
    }
    let spans: Vec<Span<'_>> = line
        .spans
        .iter()
        .map(|span| Span::styled(span.content.as_ref(), span.style))
        .collect();
    Paragraph::new(Line::from(spans))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

impl TextCache {
    fn new(lines: Vec<Line<'static>>, json: bool) -> Self {
        Self {
            lines,
            json,
            measure: RefCell::default(),
        }
    }

    /// The line sizes, measured if they aren't yet; with `wrap_width` also
    /// the wrapped rows at that width.
    fn measure(&self, wrap_width: Option<u16>) -> std::cell::RefMut<'_, Measure> {
        let mut measure = self.measure.borrow_mut();
        if measure.widths.len() != self.lines.len() {
            measure.widths = self.lines.iter().map(Line::width).collect();
            measure.widest = measure.widths.iter().copied().max().unwrap_or(0);
            measure.wrap = None;
        }
        if let Some(width) = wrap_width {
            if measure.wrap.as_ref().is_none_or(|(at, _)| *at != width) {
                let mut starts = Vec::with_capacity(self.lines.len() + 1);
                let mut row = 0;
                for (line, &line_width) in self.lines.iter().zip(&measure.widths) {
                    starts.push(row);
                    row += wrapped_rows(line, line_width, width);
                }
                starts.push(row);
                measure.wrap = Some((width, starts));
            }
        }
        measure
    }

    /// Lines for a keyspace value; `None` for collections / unsupported types.
    pub(crate) fn build(value: &KeyValue) -> Option<Self> {
        match value {
            KeyValue::String { value: text, .. } => {
                Some(Self::from_text(text, !value.is_truncated()))
            }
            KeyValue::Json(json) => Some(Self::new(json_lines(json), true)),
            KeyValue::JsonPreview(preview) => match &preview.kind {
                JsonPreviewKind::Array { items, .. } => Some(Self::new(json_lines(items), true)),
                JsonPreviewKind::TooLarge { json_type } => Some(Self::from_lines(too_large_lines(
                    preview.bytes,
                    json_type.as_deref(),
                ))),
                // A table of its fields.
                JsonPreviewKind::Object { .. } => None,
            },
            KeyValue::List(_)
            | KeyValue::Set(_)
            | KeyValue::Hash(_)
            | KeyValue::Zset(_)
            | KeyValue::Stream(_)
            | KeyValue::Unknown => None,
        }
    }

    /// Lines for `text`: pretty JSON when `try_json` and it parses (a cut
    /// text never does), plain lines otherwise.
    #[must_use]
    pub fn from_text(text: &str, try_json: bool) -> Self {
        match try_json.then(|| parse_json_text(text)).flatten() {
            Some(json) => Self::new(json_lines(&json), true),
            None => Self::new(plain_lines(text), false),
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
        Self::new(lines, false)
    }
}

/// What a JSON preview says when there is nothing it can show.
fn too_large_lines(bytes: u64, json_type: Option<&str>) -> Vec<String> {
    let size = format_bytes(usize::try_from(bytes).unwrap_or(usize::MAX));
    match json_type {
        Some(json_type) => vec![
            format!("This {json_type} is {size}, above keyspace.json_preview_bytes."),
            "Only objects and arrays have a preview. y copies it whole.".into(),
        ],
        None => vec![
            format!("This document is {size}, above keyspace.json_preview_bytes."),
            "A preview needs RedisJSON 2 or later ($ paths). y copies it whole.".into(),
        ],
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
#[derive(Default)]
pub struct TextLabels<'a> {
    /// Header, e.g. `Value` (` · JSON` is added for JSON).
    pub header: &'a str,
    /// `n/p page` works (the keyspace value viewer).
    pub paging: bool,
    /// Extra key hint at the end of the focused footer, e.g. `Esc close`.
    pub close: Option<&'a str>,
    /// Line numbers to highlight (sorted), drawn in `highlight`: lines a
    /// live refetch changed.
    pub changed: &'a [usize],
    pub highlight: Style,
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
    let mark = |i: usize, line: &Line<'static>| -> Line<'static> {
        if labels.changed.binary_search(&i).is_err() {
            return line.clone();
        }
        let mut line = line.clone();
        for span in &mut line.spans {
            span.style = span.style.patch(labels.highlight);
        }
        line.patch_style(labels.highlight)
    };
    // Nothing here walks the whole text: the sizes are measured once
    // (`TextCache::measure`) and only the lines in view are drawn. That
    // also keeps `scroll`'s `u16` offset small however long the text is.
    let wrap = view.wrap && width > 0;
    let measure = cache.measure(wrap.then_some(body_area.width));
    // The rows of an earlier wrap stay cached while wrap is off.
    let starts = measure
        .wrap
        .as_ref()
        .filter(|_| wrap)
        .map(|(_, starts)| starts.as_slice());
    let total = match starts {
        Some(starts) => starts.last().copied().unwrap_or(0),
        None => cache.lines.len(),
    };
    view.height = height;
    view.max_top = total.saturating_sub(height);
    view.max_left = if view.wrap {
        0
    } else {
        measure.widest.saturating_sub(width)
    };
    view.top = view.top.min(view.max_top);
    view.left = view.left.min(view.max_left);

    let to_u16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    if let Some(starts) = starts {
        // The line that holds row `top`, and how far into it the row is.
        let first = starts
            .partition_point(|&start| start <= view.top)
            .saturating_sub(1)
            .min(cache.lines.len().saturating_sub(1));
        let offset = view.top - starts[first];
        let rows: Vec<Line<'static>> = cache
            .lines
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|&(i, _)| starts[i] < view.top + height)
            .map(|(i, line)| mark(i, line))
            .collect();
        Paragraph::new(rows)
            .wrap(Wrap { trim: false })
            .scroll((to_u16(offset), 0))
            .render(body_area, buf);
    } else {
        let rows: Vec<Line<'static>> = cache
            .lines
            .iter()
            .enumerate()
            .skip(view.top)
            .take(height)
            .map(|(i, line)| mark(i, line))
            .collect();
        Paragraph::new(rows)
            .scroll((0, to_u16(view.left)))
            .render(body_area, buf);
    }
    drop(measure);

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
            lossy: false,
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

    /// The text rows `render` draws for `cache` at `top`.
    fn drawn(cache: &TextCache, view: &mut TextView, area: Rect) -> Vec<String> {
        let mut buf = Buffer::empty(area);
        render(
            cache,
            &TextLabels::default(),
            None,
            true,
            area,
            &mut buf,
            view,
        );
        (1..area.height - 1)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    /// #79: only the lines in view are wrapped, and the result is what
    /// wrapping the whole text and scrolling it shows.
    #[test]
    fn wrapped_text_is_drawn_like_the_whole_paragraph() {
        let text: String = (0..60)
            .map(|i| match i % 4 {
                0 => format!("{i} short\n"),
                1 => format!("{i} {}\n", "word ".repeat(30)),
                2 => format!("{i} {}\n", "x".repeat(95)),
                _ => format!("{i} 日本語のテキスト {}\n\n", "é ".repeat(40)),
            })
            .collect();
        let cache = TextCache::from_text(&text, false);
        for width in [7, 20, 33] {
            let area = Rect::new(0, 0, width, 12);
            let body = Rect::new(0, 0, width, 10);
            let whole = Paragraph::new(cache.lines.clone()).wrap(Wrap { trim: false });
            let total = whole.line_count(width);
            let mut view = TextView::default();
            for top in (0..total).step_by(3) {
                view.top = top;
                let rows = drawn(&cache, &mut view, area);
                let mut buf = Buffer::empty(body);
                whole
                    .clone()
                    .scroll((u16::try_from(view.top).unwrap(), 0))
                    .render(body, &mut buf);
                let expected: Vec<String> = (0..body.height)
                    .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
                    .collect();
                assert_eq!(rows, expected, "width {width}, top {top}");
            }
            assert_eq!(view.max_top, total - 10, "width {width}");
        }
    }

    #[test]
    fn a_long_wrapped_text_scrolls_past_65535_rows() {
        let lines: Vec<String> = (0..40_000).map(|i| format!("line {i} aaaa bbbb")).collect();
        let text = lines.join("\n");
        let cache = TextCache::from_text(&text, false);
        let area = Rect::new(0, 0, 10, 6);
        let mut view = TextView::default();
        drawn(&cache, &mut view, area);
        // Every line takes two rows at this width.
        assert_eq!(view.max_top, 80_000 - 4);
        view.top = 70_000;
        let rows = drawn(&cache, &mut view, area);
        assert_eq!(rows[0].trim_end(), "line 35000");
        assert_eq!(rows[1].trim_end(), "aaaa bbbb");
        view.scroll(true, usize::MAX);
        let rows = drawn(&cache, &mut view, area);
        assert_eq!(rows[3].trim_end(), "aaaa bbbb");
        assert_eq!(rows[2].trim_end(), "line 39999");

        // No wrap: the widest line sets how far the text pans.
        view.toggle_wrap();
        drawn(&cache, &mut view, area);
        assert_eq!(view.max_left, "line 39999 aaaa bbbb".len() - 10);
        assert_eq!(view.max_top, 40_000 - 4);
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
            ..TextLabels::default()
        };
        assert_eq!(
            text_footer(None, &keyspace, &view, 5, true),
            "j/k scroll · n/p page · w no-wrap"
        );
        let detail = TextLabels {
            header: "Payload",
            paging: false,
            close: Some("Esc close"),
            ..TextLabels::default()
        };
        assert_eq!(
            text_footer(Some("note".into()), &detail, &view, 5, true),
            "note · j/k scroll · w no-wrap · Esc close"
        );
    }
}
