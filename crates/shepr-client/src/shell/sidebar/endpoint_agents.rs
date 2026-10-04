use crate::shell::presentation::status::status_glyph;
use crate::shell::presentation::text::put_text;
use crate::shell::sidebar::layout::{AgentPanelView, AgentSlot, SidebarInputs};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

/// Draws the collapsed sidebar's agent column, one glyph row per slot.
pub(in crate::shell) fn draw_collapsed(
    buffer: &mut Buffer,
    slots: &[AgentSlot],
    inputs: &SidebarInputs<'_>,
) {
    let config = inputs.config;
    for slot in slots {
        let Some(row) = inputs.model.rows.get(slot.row) else {
            continue;
        };
        let rect = slot.hit.rect;
        let focused = row.agent.focused && &row.endpoint_id == inputs.presented;
        if focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let glyph = status_glyph(
            row.agent.status,
            config.status_indicators,
            &config.palette,
            row.stale,
        );
        if inputs.single_endpoint() {
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width.min(2),
                &format!("{:<2}", slot.row + 1),
                Style::default().fg(if focused {
                    config.palette.text
                } else {
                    config.palette.overlay0
                }),
            );
            put_text(
                buffer,
                rect.x.saturating_add(2),
                rect.y,
                rect.width.saturating_sub(2),
                glyph.text,
                glyph.style,
            );
        } else {
            let initial = row.machine_label.chars().next().unwrap_or('?');
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!("{initial}{}", glyph.text),
                glyph.style,
            );
        }
    }
}

/// Draws the expanded sidebar's agent section: its divider, header and rows.
pub(in crate::shell) fn draw_agent_panel(
    buffer: &mut Buffer,
    panel: &AgentPanelView,
    inputs: &SidebarInputs<'_>,
) {
    let config = inputs.config;
    crate::shell::sidebar::agent_sidebar::draw_agent_panel_header(
        buffer,
        panel,
        config,
        inputs.agent_panel_sort,
    );
    let Some(list) = &panel.list else {
        return;
    };
    for slot in &list.slots {
        let Some(row) = inputs.model.rows.get(slot.row) else {
            continue;
        };
        let rect = slot.hit.rect;
        let focused = row.agent.focused && &row.endpoint_id == inputs.presented;
        crate::shell::sidebar::agent_sidebar::render_agent_row(
            buffer, rect, &row.agent, focused, config,
        );
        if row.stale {
            buffer.set_style(
                rect,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
    }
    if let Some(track) = list.scrollbar {
        crate::shell::view::list::render_list_scrollbar(
            buffer,
            track,
            list.scroll,
            &config.palette,
        );
    }
}
