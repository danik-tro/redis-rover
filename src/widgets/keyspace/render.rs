use super::{
    format_bytes_u128, format_ttl, format_ttl_detail, truncate_to_width, value_footer, Focus,
    KeySpace, CLUSTER_BANNER,
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

use crate::{
    config,
    redis_client::types::{JsonField, JsonPreview, JsonPreviewKind, KeyValue, TYPE_BADGE_WIDTH},
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
        let selected = state
            .table
            .selected()
            .and_then(|index| state.keys.get(index));
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

        Self::render_value(&loaded.value, view_area, buf, &mut state.value_table);
    }

    fn render_value(value: &KeyValue, area: Rect, buf: &mut Buffer, table_state: &mut TableState) {
        match value {
            KeyValue::List(items) => {
                Self::render_single_column_table("Item", items, area, buf, table_state);
            }
            KeyValue::Set(members) => {
                Self::render_single_column_table("Member", members, area, buf, table_state);
            }
            KeyValue::Hash(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Self::render_two_column_table("Field", "Value", &pairs, area, buf, table_state);
            }
            KeyValue::Zset(entries) => {
                let pairs: Vec<(String, String)> = entries
                    .iter()
                    .map(|(member, score)| (member.clone(), score.to_string()))
                    .collect();
                Self::render_two_column_table("Member", "Score", &pairs, area, buf, table_state);
            }
            KeyValue::JsonPreview(JsonPreview {
                kind: JsonPreviewKind::Object { fields, .. },
                ..
            }) => Self::render_json_fields(fields, area, buf, table_state),
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
    fn render_json_fields(
        fields: &[JsonField],
        area: Rect,
        buf: &mut Buffer,
        table_state: &mut TableState,
    ) {
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
        let rows = fields.iter().map(|field| {
            Row::new([
                Cell::from(field.name.as_str()).fg(cfg.colors.base05),
                Cell::from(field.json_type.as_str()).fg(cfg.colors.base04),
                Cell::from(format_bytes_u128(u128::from(field.bytes))).fg(cfg.colors.base04),
            ])
        });
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
        render_header_rule(area, buf);
    }

    fn render_single_column_table(
        header_label: &str,
        rows_data: &[String],
        area: Rect,
        buf: &mut Buffer,
        table_state: &mut TableState,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(100)];
        let header = Row::new([Cell::from(header_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let rows = rows_data
            .iter()
            .map(|item| Row::new([Cell::from(item.as_str())]).fg(cfg.colors.base05));
        let table = Table::new(rows, widths)
            .header(header)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
        render_header_rule(area, buf);
    }

    fn render_two_column_table(
        left_label: &str,
        right_label: &str,
        rows_data: &[(String, String)],
        area: Rect,
        buf: &mut Buffer,
        table_state: &mut TableState,
    ) {
        let cfg = config::get();
        let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];
        let header = Row::new([Cell::from(left_label), Cell::from(right_label)])
            .bottom_margin(1)
            .fg(cfg.colors.base03)
            .bold();

        let rows = rows_data.iter().map(|(left, right)| {
            Row::new([
                Cell::from(left.as_str()).fg(cfg.colors.base05),
                Cell::from(right.as_str()).fg(cfg.colors.base04),
            ])
        });
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol(highlight_symbol())
            .row_highlight_style(selected_row_style())
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(table, area, buf, table_state);
        render_header_rule(area, buf);
    }
}

/// Width of the type column: fits the longest label (`json`, `hash`, …).
#[allow(clippy::cast_possible_truncation)]
const TYPE_COLUMN_WIDTH: u16 = TYPE_BADGE_WIDTH as u16;

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
            // Paging status lives on the bottom border.
            .title_bottom(
                Line::from(format!(" {page_summary} "))
                    .fg(cfg.colors.base05)
                    .left_aligned(),
            )
            // Raw SCAN cursor: secondary/debug detail, kept dim.
            .title_bottom(
                Line::from(format!(
                    " {}/page · cursor {} ",
                    state.page.page_size, state.page.scan_cursor
                ))
                .fg(cfg.colors.base03)
                .right_aligned(),
            );

        let table_area = space_block.inner(t_area);
        space_block.render(t_area, buf);

        Block::new()
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base04)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(values_border)
            .borders(Borders::all())
            .title(pane_title(
                if value_focused {
                    " Values · focused "
                } else {
                    " Values "
                },
                value_focused,
            ))
            .render(view_area, buf);

        let [filter_area, banner_area, rule_area, table_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(state.cluster_mode)),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .horizontal_margin(1)
        .areas(table_area);

        let pattern = match state.page.pattern.as_deref() {
            Some(p) => Span::from(p).fg(cfg.colors.base0d).bold(),
            None => Span::from("*").fg(cfg.colors.base03),
        };
        Paragraph::new(Line::from(vec![
            Span::from("filter ").fg(cfg.colors.base03),
            pattern,
        ]))
        .render(filter_area, buf);
        if state.cluster_mode {
            Paragraph::new(Line::from(CLUSTER_BANNER).fg(cfg.colors.base0b).bold())
                .render(banner_area, buf);
        }
        render_rule(rule_area, buf);
        // Header + the rule row under it take two lines.
        state.visible_rows = usize::from(table_area.height.saturating_sub(2)).max(1);

        let widths = [
            Constraint::Length(TYPE_COLUMN_WIDTH),
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Length(10),
        ];
        let header: Row<'_> = Row::new([
            Cell::from("Type"),
            Cell::from("Key"),
            Cell::from(Line::from("TTL").right_aligned()),
            Cell::from(Line::from("Size").right_aligned()),
        ])
        .bottom_margin(1)
        .fg(cfg.colors.base03)
        .bold();

        // The Key column is `Fill`: measure it the way the table will, so long
        // names can end in `…` instead of being clipped silently.
        let highlight_width = u16::try_from(highlight_symbol().width()).unwrap_or(u16::MAX);
        let key_width = usize::from(
            Layout::horizontal(widths).spacing(1).split(Rect {
                width: table_area.width.saturating_sub(highlight_width),
                ..table_area
            })[1]
                .width,
        );

        // Cell fg beats the row highlight, so the selected row needs a
        // brighter "dim" or `∞` / `—` / size vanish into its background.
        let selected = state.table.selected();
        let rows = state.keys.iter().enumerate().map(|(i, meta)| {
            let badge: ratatui::text::Text = (&meta.r_type).into();
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
            Row::new([
                Cell::from(badge),
                Cell::from(truncate_to_width(&meta.key, key_width)).fg(cfg.colors.base05),
                Cell::from(Line::from(format_ttl(meta.ttl)).right_aligned()).fg(ttl_color),
                Cell::from(
                    Line::from(format!(
                        "{:.2}",
                        Byte::from_u128(meta.size)
                            .unwrap_or_default()
                            .get_appropriate_unit(UnitType::Binary)
                    ))
                    .right_aligned(),
                )
                .fg(dim),
            ])
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

        if state.is_popup() {
            Self::render_confirm_popup(state, area, buf);
        }

        if state.is_overlay_open() {
            Self::render_action_overlay(state, area, buf);
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
