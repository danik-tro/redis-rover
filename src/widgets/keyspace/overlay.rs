use super::{
    add_item_pair_titles, format_bytes_u128, ActiveField, KeySpace, KeySpaceWidget, NewKeyDraft,
    NewKeyStep, NewKind, OverlayKind, SORT_MENU,
};

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap},
};
use ratatui_textarea::TextArea;

use crate::config;

/// Height of the type-selector popup: 5 radio rows + 2 borders + blank + hint.
/// (`NewKind::ALL` has 5 entries.)
const NEW_KIND_POPUP_HEIGHT: u16 = 9;

impl KeySpaceWidget {
    pub(super) fn render_confirm_popup(state: &mut KeySpace, area: Rect, buf: &mut Buffer) {
        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Min(3),
            Constraint::Percentage(40),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Min(15),
            Constraint::Percentage(30),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        if let Some(ref text_area) = state.text_area {
            text_area.render(popup_area, buf);
        }

        // Live match count on the popup's bottom border.
        if let Some(preview) = state.filter_preview.as_ref() {
            if popup_area.height >= 3 && popup_area.width > 4 {
                let status = Rect {
                    x: popup_area.x + 2,
                    y: popup_area.bottom() - 1,
                    width: popup_area.width - 4,
                    height: 1,
                };
                Paragraph::new(
                    Line::from(format!(" {} ", preview.status(&state.filter_text())))
                        .fg(config::get().colors.base0b),
                )
                .alignment(Alignment::Right)
                .render(status, buf);
            }
        }
    }

    /// Render the active action overlay (delete-confirm, edit, TTL input, or a
    /// notice) as a centered modal box on top of the keyspace. Mirrors the
    /// centering approach of [`Self::render_confirm_popup`].
    pub(super) fn render_action_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(overlay) = state.action_overlay.as_ref() else {
            return;
        };
        let cfg = config::get();

        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Length(5),
            Constraint::Percentage(40),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Min(30),
            Constraint::Percentage(20),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        Clear.render(popup_area, buf);

        // Input overlays render their own bordered text-area; non-input overlays
        // render a bordered message with a key hint.
        if let Some(ref text_area) = overlay.text_area {
            text_area.render(popup_area, buf);
            return;
        }

        let delete_item_body;
        let yank_body;
        let (title, body, hint): (&str, &str, &str) = match &overlay.kind {
            OverlayKind::ConfirmYank { key, size, .. } => {
                yank_body = format!(
                    "{key} is ~{} (estimate). Copy it anyway?",
                    format_bytes_u128(*size)
                );
                (
                    " Copy large value ",
                    yank_body.as_str(),
                    "Enter: copy   Esc: cancel",
                )
            }
            OverlayKind::ConfirmDelete => (
                " Delete key ",
                "Delete the selected key?",
                "Enter: confirm   Esc: cancel",
            ),
            OverlayKind::ConfirmDeleteItem { label, .. } => {
                delete_item_body = format!("Delete item: {label}?");
                (
                    " Delete item ",
                    delete_item_body.as_str(),
                    "Enter: confirm   Esc: cancel",
                )
            }
            OverlayKind::Notice(msg) => (" Notice ", msg.as_str(), "Esc: dismiss"),
            // The input kinds are handled above; unreachable here.
            OverlayKind::EditValue | OverlayKind::SetTtl => return,
        };

        let block = Block::default()
            .title(title)
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(cfg.colors.base04)
            .bg(cfg.colors.base00)
            .fg(cfg.colors.base05);

        let lines = vec![
            Line::from(body).alignment(Alignment::Center),
            Line::from(""),
            Line::styled(hint, cfg.colors.base04).alignment(Alignment::Center),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(block)
            .render(popup_area, buf);
    }

    /// Center a popup of `height` rows in `area`, returning the inner Rect (after
    /// clearing it). Mirrors the centering used by the other overlays.
    /// The `o` menu: one row per choice, the active one marked with `›` and
    /// its direction; the letters pick, the active letter again reverses.
    pub(super) fn render_sort_menu(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        let current = state.page.sort;
        let width = 26.min(area.width);
        // One row per choice plus the borders.
        let height = 7.min(area.height);
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        Clear.render(popup, buf);
        let lines: Vec<Line> = SORT_MENU
            .iter()
            .map(|&(letter, key, label)| {
                let active = current.map(|sort| sort.key) == key;
                let arrow = match current {
                    Some(sort) if active => {
                        if sort.descending {
                            "▼"
                        } else {
                            "▲"
                        }
                    }
                    _ => " ",
                };
                let text = format!(
                    " {} {letter}  {label:<11}{arrow}",
                    if active { "›" } else { " " }
                );
                if active {
                    Line::from(text).fg(cfg.colors.base0d).bold()
                } else {
                    Line::from(text)
                }
            })
            .collect();
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Sort by ")
                    .title_bottom(Line::from(" again = reverse · Esc ").fg(cfg.colors.base04))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base04)
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05),
            )
            .render(popup, buf);
    }

    fn centered_popup(area: Rect, height: u16, buf: &mut Buffer) -> Rect {
        let [_, popup_area, _] = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Length(height),
            Constraint::Percentage(30),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);

        let [_, popup_area, _] = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Min(34),
            Constraint::Percentage(20),
        ])
        .flex(ratatui::layout::Flex::Center)
        .areas(popup_area);

        Clear.render(popup_area, buf);
        popup_area
    }

    /// Render the add-key wizard overlay for whichever step is active.
    pub(super) fn render_new_key_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.new_key.as_ref() else {
            return;
        };
        let cfg = config::get();

        match &draft.step {
            NewKeyStep::Type => {
                // 5 kinds + border (2) + blank + hint line.
                let popup = Self::centered_popup(area, NEW_KIND_POPUP_HEIGHT, buf);
                let block = Block::default()
                    .title(" Select type ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base04)
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05);

                let mut lines: Vec<Line> = NewKind::ALL
                    .iter()
                    .enumerate()
                    .map(|(idx, kind)| {
                        let marker = if idx == draft.kind_cursor {
                            "(●)"
                        } else {
                            "( )"
                        };
                        let line = Line::from(format!("  {marker} {}", kind.label()));
                        if idx == draft.kind_cursor {
                            line.style(cfg.colors.base04)
                        } else {
                            line
                        }
                    })
                    .collect();
                lines.push(Line::from(""));
                lines.push(
                    Line::styled("j/k move   Enter: next   Esc: cancel", cfg.colors.base03)
                        .alignment(Alignment::Center),
                );

                Paragraph::new(lines).block(block).render(popup, buf);
            }
            NewKeyStep::Name => {
                let popup = Self::centered_popup(area, 5, buf);
                let [input_area, hint_area] =
                    Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
                draft.input_a.render(input_area, buf);
                Self::render_wizard_hint(
                    &format!("New {} key   Enter: next   Esc: cancel", draft.kind.label()),
                    hint_area,
                    buf,
                );
            }
            NewKeyStep::Element => Self::render_element_inputs(draft, area, buf),
            NewKeyStep::Notice(msg) => {
                let popup = Self::centered_popup(area, 5, buf);
                let block = Block::default()
                    .title(" Notice ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(cfg.colors.base04)
                    .bg(cfg.colors.base00)
                    .fg(cfg.colors.base05);
                let lines = vec![
                    Line::from(msg.as_str()).alignment(Alignment::Center),
                    Line::from(""),
                    Line::styled("Enter / Esc: back", cfg.colors.base03)
                        .alignment(Alignment::Center),
                ];
                Paragraph::new(lines)
                    .wrap(Wrap { trim: true })
                    .block(block)
                    .render(popup, buf);
            }
        }
    }

    /// Render the (one or two) element inputs plus a running summary + hint for
    /// the wizard's repeatable element step.
    fn render_element_inputs(draft: &NewKeyDraft, area: Rect, buf: &mut Buffer) {
        let count = draft.item_count();
        let hint = if draft.kind.is_pair() {
            format!("{count} added   Tab: switch field   Enter: add   Ctrl+S: finish   Esc: cancel")
        } else if matches!(draft.kind, NewKind::String) {
            "Enter: create   Esc: cancel".to_string()
        } else {
            format!("{count} added   Enter: add   Ctrl+S: finish   Esc: cancel")
        };

        if let Some(input_b) = draft.input_b.as_ref() {
            let popup = Self::centered_popup(area, 7, buf);
            let [a_area, b_area, hint_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(popup);
            let (title_a, title_b) = draft.kind.pair_titles();
            let a_active = draft.active == ActiveField::A;
            render_focused_input(&draft.input_a, title_a, a_active, a_area, buf);
            render_focused_input(input_b, title_b, !a_active, b_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        } else {
            let popup = Self::centered_popup(area, 5, buf);
            let [a_area, hint_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
            draft.input_a.render(a_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        }
    }

    /// Render the add-item overlay for the focused collection.
    pub(super) fn render_add_item_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.add_item.as_ref() else {
            return;
        };
        let cfg = config::get();

        if let Some(notice) = draft.notice.as_ref() {
            let popup = Self::centered_popup(area, 5, buf);
            let block = Block::default()
                .title(" Notice ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(cfg.colors.base04)
                .bg(cfg.colors.base00)
                .fg(cfg.colors.base05);
            let lines = vec![
                Line::from(notice.as_str()).alignment(Alignment::Center),
                Line::from(""),
                Line::styled("Any key: back", cfg.colors.base03).alignment(Alignment::Center),
            ];
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(block)
                .render(popup, buf);
            return;
        }

        let hint = format!(
            "Add to {key}   {keys}Enter: add   Esc: cancel",
            key = draft.key,
            keys = if draft.input_b.is_some() {
                "Tab: switch field   "
            } else {
                ""
            },
        );

        if let Some(input_b) = draft.input_b.as_ref() {
            let popup = Self::centered_popup(area, 7, buf);
            let [a_area, b_area, hint_area] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(popup);
            let (title_a, title_b) = add_item_pair_titles(&draft.kind);
            let a_active = draft.active == ActiveField::A;
            render_focused_input(&draft.input_a, title_a, a_active, a_area, buf);
            render_focused_input(input_b, title_b, !a_active, b_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        } else {
            let popup = Self::centered_popup(area, 5, buf);
            let [a_area, hint_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
            draft.input_a.render(a_area, buf);
            Self::render_wizard_hint(&hint, hint_area, buf);
        }
    }

    /// Render the edit-item overlay for the focused collection element.
    pub(super) fn render_edit_item_overlay(state: &KeySpace, area: Rect, buf: &mut Buffer) {
        let Some(draft) = state.edit_item.as_ref() else {
            return;
        };
        let cfg = config::get();

        if let Some(notice) = draft.notice.as_ref() {
            let popup = Self::centered_popup(area, 5, buf);
            let block = Block::default()
                .title(" Notice ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(cfg.colors.base04)
                .bg(cfg.colors.base00)
                .fg(cfg.colors.base05);
            let lines = vec![
                Line::from(notice.as_str()).alignment(Alignment::Center),
                Line::from(""),
                Line::styled("Any key: back", cfg.colors.base03).alignment(Alignment::Center),
            ];
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(block)
                .render(popup, buf);
            return;
        }

        let popup = Self::centered_popup(area, 5, buf);
        let [input_area, hint_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(popup);
        draft.input.render(input_area, buf);
        Self::render_wizard_hint(
            &format!("Edit {key}   Enter: save   Esc: cancel", key = draft.key),
            hint_area,
            buf,
        );
    }

    fn render_wizard_hint(hint: &str, area: Rect, buf: &mut Buffer) {
        let cfg = config::get();
        Paragraph::new(Line::styled(hint, cfg.colors.base03))
            .alignment(Alignment::Center)
            .render(area, buf);
    }
}

/// Render one of two paired inputs (hash field/value, zset member/score) with a
/// border color that signals focus: the active field is drawn in an accent
/// color with a `▸ … ◂` title, the inactive one dimmed. Renders a styled clone
/// so the stored `TextArea` keeps its neutral default; `title` is supplied by
/// the caller (ratatui's `Block` exposes no title getter).
fn render_focused_input(
    text_area: &TextArea<'static>,
    title: &str,
    is_active: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let cfg = config::get();
    let mut clone = text_area.clone();

    let (border, title_line) = if is_active {
        (cfg.colors.base0a, format!("▸ {title} ◂"))
    } else {
        (cfg.colors.base03, title.to_string())
    };

    clone.set_block(
        Block::default()
            .border_style(border)
            .border_type(BorderType::Rounded)
            .borders(Borders::ALL)
            .title(title_line),
    );
    clone.render(area, buf);
}
