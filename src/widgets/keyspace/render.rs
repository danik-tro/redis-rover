use super::{
    format::format_count, format_bytes_u128, format_ttl, format_ttl_detail, live,
    truncate_to_width, value_footer, Focus, KeySpace, CLUSTER_BANNER,
};

use byte_unit::{Byte, UnitType};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, HighlightSpacing, Paragraph, Row, StatefulWidget, Table, TableState,
        Widget, Wrap,
    },
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config,
    redis_client::types::{
        JsonField, JsonPreview, JsonPreviewKind, KeyValue, PageRow, SortKey, TYPE_BADGE_WIDTH,
    },
    widgets::text_view::{self, TextLabels},
};

pub struct KeySpaceWidget;

impl KeySpaceWidget {
    /// Placeholder for an empty key table (#40): distinguishes "still loading",
    /// "pattern matched nothing" and "empty database", with the keys to recover.
    fn render_empty_keys(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let lines = if !state.loaded || state.loading {
            vec![Line::from("Loading keys…")]
        } else if let Some(pattern) = state.page.pattern.as_deref() {
            vec![
                Line::from(format!("No keys match `{pattern}`")).bold(),
                Line::from("l clear filter · f change filter"),
            ]
        } else {
            vec![
                Line::from("No keys in this database").bold(),
                Line::from("a add key · r refresh"),
            ]
        };
        // No "next page" hint: paging slices one deduplicated SCAN sweep, so a
        // page is only empty once the sweep has ended with no keys at all.
        Self::render_placeholder(lines, area, buf);
    }

    /// Centered dim placeholder line(s), as used by the empty states.
    fn render_placeholder(lines: Vec<Line<'_>>, area: Rect, buf: &mut Buffer) {
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let [centered] = Layout::vertical([Constraint::Length(height)])
            .flex(ratatui::layout::Flex::Center)
            .areas(area);
        Paragraph::new(lines)
            .fg(config::get().colors.base03)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .render(centered, buf);
    }

    fn render_key_view(state: &mut KeySpace, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        // Inside the Values pane's border.
        let inner = Block::new().borders(Borders::ALL).inner(area);
        if let Some((prefix, expanded)) = state.selected_group() {
            let hint = if expanded {
                "Enter or ← to close"
            } else {
                "Enter or → to open"
            };
            Self::render_placeholder(vec![Line::from(format!("{prefix} · {hint}"))], inner, buf);
            return;
        }
        let selected = state.table.selected().and_then(|row| state.key_at(row));
        let Some(key) = selected else {
            Self::render_placeholder(vec![Line::from("Select a key · Enter to open")], inner, buf);
            return;
        };

        let [key_meta_area, rule_area, view_area] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .horizontal_margin(1)
        .areas(inner);

        let field = |label: &'static str, value: Span<'static>| {
            Line::from(vec![
                Span::from(format!("{label:<5} ")).fg(cfg.colors.base03),
                value,
            ])
        };
        let badge: ratatui::text::Text = (&key.r_type).into();
        let badge = badge
            .lines
            .into_iter()
            .next()
            .and_then(|line| line.spans.into_iter().next())
            .unwrap_or_default();
        // The detail pane has room for the full name (badges cut long ones).
        let badge = Span::styled(key.r_type.name().to_owned(), badge.style);
        let size = Byte::from_u128(key.size)
            .unwrap_or_default()
            .get_appropriate_unit(UnitType::Binary);
        let key_info = vec![
            field(
                "Key",
                Span::from(key.key.clone()).fg(cfg.colors.base05).bold(),
            ),
            field("Type", badge),
            field(
                "TTL",
                Span::from(format_ttl_detail(key.ttl)).fg(cfg.colors.base05),
            ),
            field(
                "Size",
                Span::from(format!("{size:.2}")).fg(cfg.colors.base05),
            ),
        ];

        Paragraph::new(key_info)
            .wrap(Wrap { trim: true })
            .render(key_meta_area, buf);
        render_rule(rule_area, buf);

        let Some(loaded) = state.selected_value.as_ref() else {
            Self::render_placeholder(vec![Line::from("Loading…")], view_area, buf);
            return;
        };

        if let Some(cache) = state.text_cache.as_ref() {
            let footer = value_footer(loaded, state.value_loading, &state.json_path);
            text_view::render(
                cache,
                &TextLabels {
                    header: "Value",
                    paging: true,
                    close: None,
                    changed: &state.live.changed,
                    highlight: live::changed_style(),
                },
                footer,
                state.focus == Focus::Text,
                view_area,
                buf,
                &mut state.text_view,
            );
            return;
        }

        let view_area = match value_footer(loaded, state.value_loading, &state.json_path) {
            Some(footer) => {
                let [value_area, footer_area] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(view_area);
                Paragraph::new(Line::from(footer).fg(cfg.colors.base03))
                    .alignment(Alignment::Right)
                    .render(footer_area, buf);
                value_area
            }
            None => view_area,
        };

        let mut rows = Rows {
            table: &mut state.value_table,
            changed: &state.live.changed,
        };
        Self::render_value(&loaded.value, view_area, buf, &mut rows);
    }

    fn render_value(value: &KeyValue, area: Rect, buf: &mut Buffer, rows: &mut Rows<'_>) {
        match value {
            KeyValue::List(items) => {
                Self::render_single_column_table("Item", items, area, buf, rows);
            }
            KeyValue::Set(members) => {
                Self::render_single_column_table("Member", members, area, buf, rows);
            }
            KeyValue::Hash(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Self::render_two_column_table("Field", "Value", &pairs, area, buf, rows);
            }
            KeyValue::Zset(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(member, score)| (member.clone(), score.to_string()))
                    .collect();
                Self::render_two_column_table("Member", "Score", &pairs, area, buf, rows);
            }
            KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields, .. },
                ..
            }) => Self::render_json_fields(fields, area, buf, rows),
            // STRING / JSON go through `text_view::render`.
            KeyValue::String { .. }
            | KeyValue::Json(_)
            | KeyValue::JsonPreview(_)
            | KeyValue::Unknown => {
                Self::render_placeholder(
                    vec![Line::from("Viewer for this type isn't supported yet")],
                    area,
                    buf,
                );
            }
        }
    }

    /// The fields of a JSON object preview: name, type and size.
    fn render_json_fields(fields: &[JsonField], area: Rect, buf: &mut Buffer, rows: &mut Rows<'_>) {
        let cfg = config::get();
        let widths = [
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(11),
        ];
        let header = Row::new([Cell::from("Field"), Cell::from("Type"), Cell::from("Size")])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();
        let table_rows = fields.iter().enumerate().map(|(i, field)| {
            rows.row(
                i,
                [
                    Cell::from(field.name.as_str()).fg(cfg.colors.base05),
                    Cell::from(field.json_type.as_str()).fg(cfg.colors.base04),
                    Cell::from(format_bytes_u128(u128::from(field.bytes))).fg(cfg.colors.base04),
                ],
            )
        });
        let table = Table::new(table_rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, rows.table);
        render_header_rule(area, buf);
    }

    fn render_single_column_table(
        header_label: &str,
        rows_data: &[String],
        area: Rect,
        buf: &mut Buffer,
        rows: &mut Rows<'_>,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(100)];
        let header = Row::new([Cell::from(header_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let table_rows = rows_data
            .iter()
            .enumerate()
            .map(|(i, item)| rows.row(i, [Cell::from(item.as_str()).fg(cfg.colors.base05)]));
        let table = Table::new(table_rows, widths)
            .header(header)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, rows.table);
        render_header_rule(area, buf);
    }

    fn render_two_column_table(
        left_label: &str,
        right_label: &str,
        rows_data: &[(String, String)],
        area: Rect,
        buf: &mut Buffer,
        rows: &mut Rows<'_>,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];
        let header = Row::new([Cell::from(left_label), Cell::from(right_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let table_rows = rows_data.iter().enumerate().map(|(i, (left, right))| {
            rows.row(
                i,
                [
                    Cell::from(left.as_str()).fg(cfg.colors.base05),
                    Cell::from(right.as_str()).fg(cfg.colors.base04),
                ],
            )
        });
        let table = Table::new(table_rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, rows.table);
        render_header_rule(area, buf);
    }
}

/// A value table's cursor and the rows a live refetch changed.
struct Rows<'a> {
    table: &'a mut TableState,
    changed: &'a [usize],
}

impl Rows<'_> {
    /// Row `i` of the table, highlighted when it changed (the cells'
    /// own colours give way to the highlight).
    fn row<'c, const N: usize>(&self, i: usize, cells: [Cell<'c>; N]) -> Row<'c> {
        if self.changed.binary_search(&i).is_err() {
            return Row::new(cells);
        }
        let style = live::changed_style();
        Row::new(cells.map(|cell| cell.style(style))).style(style)
    }
}

/// Width of the type column: fits the longest label (`json`, `hash`, …).
#[allow(clippy::cast_possible_truncation)]
const TYPE_COLUMN_WIDTH: u16 = TYPE_BADGE_WIDTH as u16;

/// A column of the key table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Type,
    Key,
    Ttl,
    Size,
}

impl Column {
    fn width(self) -> Constraint {
        match self {
            Self::Type => Constraint::Length(TYPE_COLUMN_WIDTH),
            Self::Key => Constraint::Fill(1),
            Self::Ttl => Constraint::Length(7),
            Self::Size => Constraint::Length(10),
        }
    }
}

/// Selection marker shared by the key and value tables.
fn highlight_symbol() -> Span<'static> {
    Span::from("▌ ").fg(config::get().colors.base0d)
}

fn selected_row_style() -> Style {
    Style::new().bg(config::get().colors.base02).bold()
}

/// Dim horizontal rule across `area`'s first row.
fn render_rule(area: Rect, buf: &mut Buffer) {
    Paragraph::new("─".repeat(usize::from(area.width)))
        .fg(config::get().colors.base02)
        .render(area, buf);
}

/// Draws the rule into the blank row a table header leaves via
/// `bottom_margin(1)`, underlining the header.
fn render_header_rule(table_area: Rect, buf: &mut Buffer) {
    if table_area.height < 2 {
        return;
    }
    render_rule(
        Rect {
            y: table_area.y + 1,
            height: 1,
            ..table_area
        },
        buf,
    );
}

impl StatefulWidget for KeySpaceWidget {
    type State = KeySpace;

    #[allow(clippy::too_many_lines)]
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let cfg = config::get();
        let [t_area, view_area] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)])
                .flex(ratatui::layout::Flex::Center)
                .areas(area);

        // Highlight whichever pane currently has keyboard focus so it's obvious
        // where `j`/`k` will move after `EnterValue`.
        let value_focused = state.focus != Focus::Keys;
        let focused = Style::new().fg(cfg.colors.base0d);
        let unfocused = Style::new().fg(cfg.colors.base02);
        let (keys_border, values_border) = if value_focused {
            (unfocused, focused)
        } else {
            (focused, unfocused)
        };
        let pane_title = |text: &'static str, has_focus: bool| {
            if has_focus {
                Line::from(text).fg(cfg.colors.base05).bold()
            } else {
                Line::from(text).fg(cfg.colors.base03)
            }
        };

        let page_summary = if state.loaded {
            state.page.summary()
        } else {
            format!("Page {} · loading…", state.page.page + 1)
        };
        // The page summary sits bottom left, a running load (else the raw
        // SCAN cursor, a dim debug detail) bottom right. A load that doesn't
        // fit beside the summary takes the whole border.
        let mut summary = Some(
            Line::from(format!(" {page_summary} "))
                .fg(cfg.colors.base05)
                .left_aligned(),
        );
        let room = usize::from(t_area.width.saturating_sub(2));
        let right = match state.load_progress {
            Some(progress) => {
                let used = summary.as_ref().map_or(0, Line::width) + 1;
                let mut label = progress.label(room.saturating_sub(used));
                if label.chars().count() + used > room {
                    summary = None;
                    label = progress.label(room);
                }
                Line::from(label).fg(cfg.colors.base0a).right_aligned()
            }
            None => Line::from(format!(
                " {}/page · cursor {} ",
                state.page.page_size, state.page.scan_cursor
            ))
            .fg(cfg.colors.base03)
            .right_aligned(),
        };
        let space_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(keys_border)
            .borders(Borders::all())
            .title(pane_title(
                if state.loading {
                    " Keys · loading… "
                } else {
                    " Keys "
                },
                !value_focused,
            ))
            .title_bottom(right);
        let space_block = match summary {
            Some(summary) => space_block.title_bottom(summary),
            None => space_block,
        };

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        let values_title = if value_focused {
            " Values · focused "
        } else {
            " Values "
        };
        let mut values_block = Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(values_border)
            .borders(Borders::all())
            .title(pane_title(values_title, value_focused));
        // Liveness belongs to the open key: shown only while one is.
        let key_shown = state.table.selected().and_then(|row| state.key_at(row));
        if let (Some(status), Some(_)) = (state.live.status.as_ref(), key_shown) {
            // Inside the corners, one column apart from the pane's title.
            let room = usize::from(view_area.width).saturating_sub(2 + values_title.width() + 1);
            if let Some(label) = live::liveness_label(status, room) {
                values_block = values_block.title(label);
            }
        }
        let gone = state.gone_label().map(str::to_owned);
        if let Some(label) = &gone {
            values_block = values_block.title_bottom(
                Line::from(label.clone())
                    .fg(cfg.colors.base08)
                    .bold()
                    .left_aligned(),
            );
        }
        values_block.render(view_area, buf);

        let [filter_area, banner_area, rule_area, table_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(state.cluster_mode)),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .horizontal_margin(1)
        .areas(table_area);

        // While a type / TTL / size sort still lacks metadata, the right end
        // says how much is known (`size 41k/120k`); in tree mode, how many
        // keys the tree groups.
        let note = state.page.note();
        let note_width = note
            .as_deref()
            .map_or(0, |n| u16::try_from(n.width()).unwrap_or(u16::MAX));
        let [pattern_area, note_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(note_width)])
                .spacing(u16::from(note_width > 0))
                .areas(filter_area);
        let label = "filter ";
        // The type filter (` · hash,zset`) keeps its room; the pattern is cut.
        let types = (!state.page.types.is_empty()).then(|| format!(" · {}", state.page.types));
        let room = usize::from(pattern_area.width)
            .saturating_sub(label.len())
            .saturating_sub(types.as_deref().map_or(0, UnicodeWidthStr::width));
        let pattern = match state.page.pattern.as_deref() {
            Some(p) => Span::from(truncate_to_width(p, room).into_owned())
                .fg(cfg.colors.base0d)
                .bold(),
            None => Span::from("*").fg(cfg.colors.base03),
        };
        let mut spans = vec![Span::from(label).fg(cfg.colors.base03), pattern];
        if let Some(types) = types {
            spans.push(Span::from(types).fg(cfg.colors.base0e).bold());
        }
        Paragraph::new(Line::from(spans)).render(pattern_area, buf);
        if let Some(note) = note {
            Paragraph::new(Line::from(note).fg(cfg.colors.base03))
                .alignment(Alignment::Right)
                .render(note_area, buf);
        }
        if state.cluster_mode {
            Paragraph::new(Line::from(CLUSTER_BANNER).fg(cfg.colors.base0b).bold())
                .render(banner_area, buf);
        }
        render_rule(rule_area, buf);
        // Header + the rule row under it take two lines.
        state.visible_rows = usize::from(table_area.height.saturating_sub(2)).max(1);

        // The flat list shows every column. The tree needs room for its
        // names, so a narrow pane drops the type, then TTL and size.
        let columns: &[Column] = match (&state.tree_rows, table_area.width) {
            (None, _) | (Some(_), 60..) => &[Column::Type, Column::Key, Column::Ttl, Column::Size],
            (Some(_), 36..) => &[Column::Key, Column::Size],
            (Some(_), _) => &[Column::Key],
        };
        let widths: Vec<Constraint> = columns.iter().map(|c| c.width()).collect();
        // The sorted column carries its direction: `Key ▲`, `Size ▼`.
        let title = |name: &'static str, key: SortKey| -> String {
            match state.page.sort {
                Some(sort) if sort.key == key => {
                    format!("{name} {}", if sort.descending { '▼' } else { '▲' })
                }
                _ => name.to_owned(),
            }
        };
        let header: Row<'_> = Row::new(columns.iter().map(|column| match column {
            Column::Type => Cell::from(title("Type", SortKey::Type)),
            Column::Key => Cell::from(title("Key", SortKey::Name)),
            Column::Ttl => Cell::from(Line::from(title("TTL", SortKey::Ttl)).right_aligned()),
            Column::Size => Cell::from(Line::from(title("Size", SortKey::Size)).right_aligned()),
        }))
        .bottom_margin(1)
        .fg(cfg.colors.base03)
        .bold();

        // The Key column is `Fill`: measure it the way the table will, so long
        // names can end in `…` instead of being clipped silently.
        let highlight_width = u16::try_from(highlight_symbol().width()).unwrap_or(u16::MAX);
        let key_column = columns.iter().position(|c| *c == Column::Key).unwrap_or(0);
        let key_width = usize::from(
            Layout::horizontal(widths.iter().copied())
                .spacing(1)
                .split(Rect {
                    width: table_area.width.saturating_sub(highlight_width),
                    ..table_area
                })[key_column]
                .width,
        );

        // Cell fg beats the row highlight, so the selected row needs a
        // brighter "dim" or `∞` / `—` / size vanish into its background.
        let selected = state.table.selected();
        let rows = state.keys.iter().enumerate().map(|(i, meta)| {
            let tree_row = state.tree_rows.as_ref().and_then(|rows| rows.get(i));
            if let Some(PageRow::Group {
                count,
                depth,
                expanded,
                ..
            }) = tree_row
            {
                let label = format!(
                    "{}{} {} ({})",
                    "  ".repeat(usize::from(*depth)),
                    if *expanded { '▾' } else { '▸' },
                    meta.key,
                    format_count(*count),
                );
                return Row::new(columns.iter().map(|column| {
                    match column {
                        Column::Key => {
                            Cell::from(truncate_to_width(&label, key_width).into_owned())
                                .fg(cfg.colors.base0d)
                                .bold()
                        }
                        _ => Cell::default(),
                    }
                }));
            }
            let indent = match tree_row {
                Some(PageRow::Leaf { depth }) => "  ".repeat(usize::from(*depth) + 1),
                _ => String::new(),
            };
            let dim = if selected == Some(i) {
                cfg.colors.base05
            } else {
                cfg.colors.base03
            };
            // Keys with a real expiry stand out; `∞` / `—` stay dim.
            let ttl_color = if meta.ttl >= 0 {
                cfg.colors.base0b
            } else {
                dim
            };
            Row::new(columns.iter().map(|column| {
                match column {
                    Column::Type => {
                        let badge: ratatui::text::Text = (&meta.r_type).into();
                        Cell::from(badge)
                    }
                    Column::Key => Cell::from(
                        truncate_to_width(&format!("{indent}{}", meta.key), key_width).into_owned(),
                    )
                    .fg(cfg.colors.base05),
                    Column::Ttl => {
                        Cell::from(Line::from(format_ttl(meta.ttl)).right_aligned()).fg(ttl_color)
                    }
                    Column::Size => Cell::from(
                        Line::from(format!(
                            "{:.2}",
                            Byte::from_u128(meta.size)
                                .unwrap_or_default()
                                .get_appropriate_unit(UnitType::Binary)
                        ))
                        .right_aligned(),
                    )
                    .fg(dim),
                }
            }))
        });
        let table: Table<'_> = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        if state.keys.is_empty() {
            Self::render_empty_keys(state, table_area, buf);
        } else {
            StatefulWidget::render(table, table_area, buf, &mut state.table);
            render_header_rule(table_area, buf);
        }

        Self::render_key_view(state, view_area, buf);
        if gone.is_some() {
            // The last value stays readable, dimmed.
            let inner = Block::new().borders(Borders::ALL).inner(view_area);
            buf.set_style(inner, Style::new().fg(cfg.colors.base03));
        }

        if state.is_popup() {
            Self::render_confirm_popup(state, area, buf);
        }

        if state.is_overlay_open() {
            Self::render_action_overlay(state, area, buf);
        }

        if state.is_sort_menu_open() {
            Self::render_sort_menu(state, area, buf);
        }
        if let Some(menu) = state.type_menu.as_ref() {
            Self::render_type_menu(menu, area, buf);
        }

        if state.is_wizard_active() {
            Self::render_new_key_overlay(state, area, buf);
        }

        if state.is_add_item_active() {
            Self::render_add_item_overlay(state, area, buf);
        }

        if state.is_edit_item_active() {
            Self::render_edit_item_overlay(state, area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::{KeyMeta, KeyValue, LoadedValue, RedisType, ValuePageInfo};
    use crate::widgets::keyspace::format::TTL_NO_EXPIRY;

    use crate::widgets::keyspace::tests::{render_text, with_value};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

    #[test]
    fn redis_json_value_renders_pretty() {
        let doc = serde_json::json!({ "name": "Ann", "tags": ["a"] });
        let loaded = LoadedValue {
            value: KeyValue::Json(doc),
            page: ValuePageInfo::default(),
        };
        let mut keyspace = with_value(RedisType::Json, loaded);
        let screen = render_text(&mut keyspace);
        assert!(screen.contains("Value · JSON"));
        assert!(screen.contains("\"name\": \"Ann\","));
    }

    #[test]
    fn visible_rows_follow_table_height() {
        let mut keyspace = KeySpace::new(Vec::new());
        assert_eq!(keyspace.visible_rows(), 0);
        render_text(&mut keyspace);
        // 20 rows - 2 borders - filter - rule - header - header rule.
        assert_eq!(keyspace.visible_rows(), 14);
        keyspace.cluster_mode = true;
        render_text(&mut keyspace);
        assert_eq!(keyspace.visible_rows(), 13);
    }

    /// One key per row; the first is selected.
    fn keys_pane(keys: &[(&str, isize)]) -> (KeySpace, Buffer) {
        let mut keyspace = KeySpace::new(Vec::new());
        keyspace.set_keys(
            keys.iter()
                .map(|&(key, ttl)| KeyMeta {
                    key: key.into(),
                    r_type: RedisType::String,
                    size: 148,
                    ttl,
                    value: KeyValue::Unknown,
                })
                .collect(),
        );
        keyspace.table.select(Some(0));
        let area = Rect::new(0, 0, 120, 20);
        let mut buf = Buffer::empty(area);
        StatefulWidget::render(super::KeySpaceWidget, area, &mut buf, &mut keyspace);
        (keyspace, buf)
    }

    /// Text of the Keys pane (left 35%) on row `y`.
    fn keys_pane_row(buf: &Buffer, y: u16) -> String {
        (0..42).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn long_key_names_end_in_ellipsis() {
        let long = "app:config:feature_flags:with:a:very:long:suffix";
        let (_, buf) = keys_pane(&[(long, -1)]);
        let rows: Vec<String> = (0..20).map(|y| keys_pane_row(&buf, y)).collect();
        let row = rows
            .iter()
            .find(|r| r.contains("app:"))
            .expect("key row rendered");
        assert!(row.contains('…'), "no ellipsis in {row:?}");
        assert!(!row.contains(long));
    }

    #[test]
    fn selected_row_dim_cells_stay_readable() {
        let cfg = crate::config::get();
        let (_, buf) = keys_pane(&[("user:1043", TTL_NO_EXPIRY), ("user:1044", -1)]);
        let infinity_fgs: Vec<_> = (0..20)
            .flat_map(|y| (0..42).map(move |x| (x, y)))
            .filter(|&pos| buf[pos].symbol() == "∞")
            .map(|pos| buf[pos].fg)
            .collect();
        // First row is selected (readable), second is not (dim).
        assert_eq!(infinity_fgs, vec![cfg.colors.base05, cfg.colors.base03]);
    }
}
