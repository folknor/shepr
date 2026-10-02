use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    single_endpoint: bool,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    for (index, row) in rows.into_iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(
            area.x,
            area.y + u16::try_from(index).unwrap_or(u16::MAX),
            area.width,
            1,
        );
        if row.agent.focused {
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
                Style::default().fg(if row.agent.focused {
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
            hits.agents.push((rect, row.agent.pane_id));
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
                .push((rect, row.endpoint_id, row.agent.pane_id));
        }
    }
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    single_endpoint: bool,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !super::agent_sidebar::render_agent_panel_header(buffer, area, config, hits) {
        return;
    }
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        &rows,
        None,
        config,
        agent_scroll,
        hits,
        |row| row.agent.rows.len(),
        |buffer, rect, row, hits| {
            super::agent_sidebar::render_agent_row(buffer, rect, &row.agent, config);
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
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let rows = agent_rows(&self.endpoints, &self.active_endpoint_id, &self.config);
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
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}

struct EndpointAgentRow {
    endpoint_id: ClientEndpointId,
    machine_label: String,
    stale: bool,
    agent: super::agent_sidebar::AgentRow,
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                super::agent_sidebar::agent_rows(
                    snapshot,
                    config,
                    (endpoints.len() > 1).then_some(endpoint.endpoint_id.display_label()),
                )
                .into_iter()
                .map(|agent| {
                    (
                        (endpoint.endpoint_id.clone(), agent.pane_id.to_string()),
                        agent,
                    )
                })
                .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(endpoints, config.agent_panel_sort)
        .into_iter()
        .filter_map(|row| {
            let key = (
                row.endpoint.endpoint_id.clone(),
                row.agent.pane_id.to_string(),
            );
            let mut agent = rendered_rows.remove(&key)?;
            agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
            Some(EndpointAgentRow {
                endpoint_id: row.endpoint.endpoint_id.clone(),
                machine_label: row.endpoint.label.to_owned(),
                stale: row.endpoint.stale(),
                agent,
            })
        })
        .collect()
}
