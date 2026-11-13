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
    crate::widgets::keyspace::build_input_text_area(title, expected, None)
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

    /// The typed confirm on Prod: the command, then an input that must
    /// hold `expected` before Enter deletes.
    #[must_use]
    pub fn typed_delete(
        lines: &'a [String],
        env: Environment,
        expected: &str,
        input: &'a TextArea<'static>,
        matches: bool,
    ) -> Self {
        let cfg = config::get();
        let mut popup = Self::write_preview(lines, env);
        popup.body.push(Line::from(""));
        popup
            .body
            .push(Line::from(format!("Type {expected} to delete it.")).fg(cfg.colors.base05));
        let waiting = Line::from("Enter: delete (type the name first)   Esc: cancel");
        popup.min_width = waiting.width();
        popup.hint = if matches {
            Line::from("Enter: delete   Esc: cancel").fg(cfg.colors.base04)
        } else {
            waiting.fg(cfg.colors.base03)
        };
        popup.input = Some(input);
        popup
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
