//! The App's confirm popup: a write's command preview, the typed confirm on
//! Prod, and lifting read-only. Drawn above every mode, bordered in the
//! environment's colour.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Stylize},
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap},
};
use ratatui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use crate::{config, target::Environment, utils::group_thousands, widgets::keyspace::format_bytes};

/// The typed confirm's one-line input; `expected` is its placeholder.
#[must_use]
pub fn typed_input(expected: &str) -> TextArea<'static> {
    let title = if expected == "delete" {
        "Type delete"
    } else {
        "Type the key name"
    };
    typed_input_titled(title, expected)
}

/// [`typed_input`] with its own title (`Type the count`).
#[must_use]
pub fn typed_input_titled(title: &str, expected: &str) -> TextArea<'static> {
    crate::widgets::keyspace::build_input_text_area(title, expected, None)
}

/// The words of a typed confirm: `Type {expected} to {action} {object}.`
/// and `Enter: {action} (type {what} first)`.
pub struct TypedText<'t> {
    pub expected: &'t str,
    pub action: &'t str,
    pub object: &'t str,
    pub what: &'t str,
}

/// Narrowest popup content, so short questions don't look cramped.
const MIN_CONTENT_WIDTH: usize = 30;

pub struct ConfirmPopup<'a> {
    title: String,
    body: Vec<Line<'a>>,
    hint: Line<'a>,
    color: Color,
    /// The typed confirm's one-line input, between body and hint.
    input: Option<&'a TextArea<'static>>,
    /// Content width the popup never goes below, so it doesn't jump when
    /// the hint changes.
    min_width: usize,
}

impl<'a> ConfirmPopup<'a> {
    /// The commands a write sends; `y` runs them.
    #[must_use]
    pub fn write_preview(lines: &'a [String], env: Environment) -> Self {
        let cfg = config::get();
        Self {
            title: format!(" Run on {}? ", env.label()),
            body: lines
                .iter()
                .map(|l| Line::from(l.as_str()).fg(cfg.colors.base05))
                .collect(),
            hint: Line::from("y: run   n/Esc: cancel").fg(cfg.colors.base04),
            color: cfg.colors.env(env),
            input: None,
            min_width: MIN_CONTENT_WIDTH,
        }
    }

    /// A warning above the commands (a rename / copy that replaces a key,
    /// `EXPIRE … 0`), in the environment colour.
    #[must_use]
    pub fn warning(mut self, warning: Option<&str>) -> Self {
        if let Some(warning) = warning {
            let line = Line::from(format!("⚠ {warning}")).fg(self.color).bold();
            self.body.splice(0..0, [line, Line::from("")]);
        }
        self
    }

    /// Notes under the commands: what they keep, how they degrade.
    #[must_use]
    pub fn notes(mut self, notes: &[String]) -> Self {
        if notes.is_empty() {
            return self;
        }
        let cfg = config::get();
        self.body.push(Line::from(""));
        self.body.extend(
            notes
                .iter()
                .map(|n| Line::from(n.clone()).fg(cfg.colors.base04)),
        );
        self
    }

    /// The typed confirm on Prod: the command, then an input that must
    /// hold `typed.expected` before Enter runs it.
    #[must_use]
    pub fn typed_confirm(
        self,
        typed: &TypedText<'_>,
        input: &'a TextArea<'static>,
        matches: bool,
    ) -> Self {
        let cfg = config::get();
        let TypedText {
            expected,
            action,
            object,
            what,
        } = typed;
        let mut popup = self;
        popup.body.push(Line::from(""));
        popup.body.push(
            Line::from(format!("Type {expected} to {action} {object}.")).fg(cfg.colors.base05),
        );
        let waiting = Line::from(format!("Enter: {action} (type {what} first)   Esc: cancel"));
        popup.min_width = waiting.width();
        popup.hint = if matches {
            Line::from(format!("Enter: {action}   Esc: cancel")).fg(cfg.colors.base04)
        } else {
            waiting.fg(cfg.colors.base03)
        };
        popup.input = Some(input);
        popup
    }

    /// [`Self::typed_confirm`] for deleting one key.
    #[must_use]
    pub fn typed_delete(
        lines: &'a [String],
        env: Environment,
        expected: &str,
        input: &'a TextArea<'static>,
        matches: bool,
    ) -> Self {
        let typed = TypedText {
            expected,
            action: "delete",
            object: "it",
            what: "the name",
        };
        Self::write_preview(lines, env).typed_confirm(&typed, input, matches)
    }

    /// Asked before writes are allowed on Prod.
    #[must_use]
    pub fn lift_read_only(env: Environment) -> Self {
        let cfg = config::get();
        Self {
            title: " Lift read-only ".to_owned(),
            body: vec![Line::from(format!("Allow writes to {}?", env.label()))],
            hint: Line::from("y: allow writes   n/Esc: cancel").fg(cfg.colors.base04),
            color: cfg.colors.env(env),
            input: None,
            min_width: MIN_CONTENT_WIDTH,
        }
    }

    /// `Shift-a` on a DB above 2M keys: how many, and the memory they take.
    #[must_use]
    pub fn load_all(db: i64, keys: u64, bytes: u64, env: Environment) -> Self {
        let cfg = config::get();
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        let keys = group_thousands(usize::try_from(keys).unwrap_or(usize::MAX));
        Self {
            title: " Load all keys? ".to_owned(),
            body: vec![
                Line::from(format!("DB {db} holds {keys} keys.")),
                Line::from(format!(
                    "Loading them all takes about {} of memory.",
                    format_bytes(bytes)
                )),
                Line::from("Esc stops the load at any time.").fg(cfg.colors.base04),
            ],
            hint: Line::from("y: load   n/Esc: cancel").fg(cfg.colors.base04),
            color: cfg.colors.env(env),
            input: None,
            min_width: MIN_CONTENT_WIDTH,
        }
    }

    /// Before MONITOR starts: what it costs the server.
    #[must_use]
    pub fn start_monitor(env: Environment) -> Self {
        let cfg = config::get();
        Self {
            title: format!(" Start MONITOR on {}? ", env.label()),
            body: vec![
                Line::from("MONITOR makes the server send every command here."),
                Line::from("It costs server CPU, and more the busier it is."),
                Line::from(if cfg.monitor.keep_running {
                    "It keeps running after you leave (monitor.keep_running)."
                } else {
                    "It stops when you leave this screen."
                })
                .fg(cfg.colors.base04),
            ],
            hint: Line::from("y: start   n/Esc: cancel").fg(cfg.colors.base04),
            color: cfg.colors.env(env),
            input: None,
            min_width: MIN_CONTENT_WIDTH,
        }
    }

    /// `g o` while the filter hides the open key.
    #[must_use]
    pub fn clear_filter_for(key: &str, env: Environment) -> Self {
        let cfg = config::get();
        Self {
            title: " Clear the filter? ".to_owned(),
            body: vec![
                Line::from(format!("The filter hides {key}.")),
                Line::from("Clear it and go to the key?"),
            ],
            hint: Line::from("y: clear and go   n/Esc: cancel").fg(cfg.colors.base04),
            color: cfg.colors.env(env),
            input: None,
            min_width: MIN_CONTENT_WIDTH,
        }
    }

    fn content_width(&self, area: Rect) -> u16 {
        let widest = self
            .body
            .iter()
            .map(Line::width)
            .chain([self.hint.width(), self.title.width()])
            .max()
            .unwrap_or(0)
            .max(self.min_width);
        // Borders and one column of padding on each side.
        let room = usize::from(area.width.saturating_sub(4)).max(1);
        u16::try_from(widest.min(room)).unwrap_or(u16::MAX)
    }
}

impl Widget for ConfirmPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        let width = self.content_width(area);
        let body = Paragraph::new(self.body).wrap(Wrap { trim: false });
        let body_rows = u16::try_from(body.line_count(width)).unwrap_or(u16::MAX);
        let input_rows = if self.input.is_some() { 3 } else { 0 };
        // Body, blank, input, hint, borders.
        let height = body_rows.saturating_add(input_rows + 4).min(area.height);

        let [_, row, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(area);
        let [_, popup, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(width + 4),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .areas(row);

        Clear.render(popup, buf);
        let block = Block::default()
            .title(self.title)
            .title_alignment(ratatui::layout::Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.color)
            .padding(Padding::horizontal(1))
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05);
        let inner = block.inner(popup);
        block.render(popup, buf);

        let [body_area, _, input_area, hint_area] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(input_rows),
            Constraint::Length(1),
        ])
        .areas(inner);
        body.render(body_area, buf);
        if let Some(input) = self.input {
            input.render(input_area, buf);
        }
        Paragraph::new(self.hint.centered()).render(hint_area, buf);
    }
}
