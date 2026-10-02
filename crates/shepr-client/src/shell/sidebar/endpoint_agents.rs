use crate::endpoint::ClientEndpointId;
use crate::shell::presentation::render::put_text;
use crate::shell::state::{ClientShellConfig, ClientShellState, ShellHitMap};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::shell::presentation::status::status_glyph;
use ratatui::layout::Rect;

pub(in crate::shell) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    active_endpoint_id: &ClientEndpointId,
    single_endpoint: bool,
    config: &ClientShellConfig,
    model: &crate::shell::navigation::aggregate_navigation::AgentPanelModel,
    hits: &mut ShellHitMap,
) {
    for (index, row) in model.rows.iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(
            area.x,
            area.y + u16::try_from(index).unwrap_or(u16::MAX),
            area.width,
            1,
        );
        let focused = row.agent.focused && &row.endpoint_id == active_endpoint_id;
        if focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let glyph = status_glyph(
            row.agent.status,
            config.status_indicators,
            &config.palette,
            row.stale,
        );
        if single_endpoint {
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width.min(2),
                &format!("{:<2}", index + 1),
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
            hits.agents.push((rect, row.agent.pane_id.clone()));
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
            hits.endpoint_agents
                .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
        }
    }
}

pub(in crate::shell) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    active_endpoint_id: &ClientEndpointId,
    single_endpoint: bool,
    config: &ClientShellConfig,
    model: &crate::shell::navigation::aggregate_navigation::AgentPanelModel,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !crate::shell::sidebar::agent_sidebar::render_agent_panel_header(buffer, area, config, hits)
    {
        return;
    }
    crate::shell::sidebar::agent_sidebar::render_agent_list(
        buffer,
        area,
        &model.rows,
        None,
        config,
        agent_scroll,
        hits,
        |row| row.agent.rows.len(),
        |buffer, rect, row, hits| {
            let focused = row.agent.focused && &row.endpoint_id == active_endpoint_id;
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
            if single_endpoint {
                hits.agents.push((rect, row.agent.pane_id.clone()));
            } else {
                hits.endpoint_agents.push((
                    rect,
                    row.endpoint_id.clone(),
                    row.agent.pane_id.clone(),
                ));
            }
        },
    );
}

impl ClientShellState {
    pub(in crate::shell) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let rows = &self.agent_panel_model.rows;
        let Some(target) = rows.iter().position(|row| {
            &row.endpoint_id == endpoint_id && row.agent.pane_id.as_str() == pane_id
        }) else {
            return;
        };
        let heights = rows
            .iter()
            .map(|row| u16::try_from(row.agent.rows.len().max(1)).unwrap_or(u16::MAX))
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; rows.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = crate::shell::navigation::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}
