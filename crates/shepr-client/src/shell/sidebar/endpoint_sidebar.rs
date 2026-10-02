use crate::endpoint::ClientEndpointStatus;
use crate::shell::presentation::render::{
    ShellRenderState, display_width, put_right_text, put_text,
};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::shell::endpoints::{ClientShellEndpoint, MachineHit, endpoint_status_presentation};
use crate::shell::state::{ClientShellConfig, ShellHitMap, WorkspaceHit};
use shepr_protocol::ClientShellSnapshot;

use ratatui::layout::Rect;
use shepr_config::theme::Palette;

use crate::shell::presentation::status::status_glyph;
use crate::shell::sidebar::sidebar_tokens::{
    expanded_sidebar_sections, sidebar_section_divider_rect,
};

use crate::limits::WORKSPACE_HEADER_ROWS;

pub(in crate::shell) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let single_endpoint = state.endpoints.len() == 1;
    crate::shell::presentation::render::render_sidebar_background(buffer, area, palette);
    let (workspace_area, divider_y, detail_area) =
        crate::shell::sidebar::collapsed_sidebar_sections(area);
    hits.workspace_body = workspace_area;
    let mut total_rows = 0usize;
    let mut selected_row = None;
    let reveal = std::mem::take(state.reveal_navigation_workspace);
    let reveal_focus = std::mem::take(state.reveal_focused_workspace);
    for endpoint in state.endpoints {
        total_rows += usize::from(!single_endpoint);
        if state.collapsed_endpoints.contains(&endpoint.endpoint_id) {
            continue;
        }
        if let Some(snapshot) = endpoint.snapshot.as_deref() {
            if reveal || reveal_focus {
                let candidate = snapshot
                    .workspaces
                    .iter()
                    .position(|workspace| {
                        if reveal {
                            state.selected_workspace_id.is_some_and(|target| {
                                target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
                            })
                        } else {
                            &endpoint.endpoint_id == state.active_endpoint_id && workspace.focused
                        }
                    })
                    .map(|index| total_rows + index);
                selected_row = candidate.or(selected_row);
            }
            total_rows += snapshot.workspaces.len();
        }
    }
    let height = usize::from(workspace_area.height);
    let max_scroll = total_rows.saturating_sub(height);
    *state.workspace_scroll = (*state.workspace_scroll).min(max_scroll);
    if let Some(row) = selected_row.filter(|_| height > 0) {
        let row_heights = vec![1; total_rows];
        let gaps = vec![0; total_rows];
        *state.workspace_scroll = crate::shell::navigation::scroll::list_scroll_start_to_reveal(
            &row_heights,
            &gaps,
            workspace_area.height,
            *state.workspace_scroll,
            row,
        );
    }
    hits.workspace_max_scroll = max_scroll;
    let mut skip = *state.workspace_scroll;
    let mut y = workspace_area.y;
    let mut machine_number = 0usize;
    for endpoint in state.endpoints {
        if y >= workspace_area.bottom() {
            break;
        }
        if !endpoint.endpoint_id.is_local() {
            machine_number += 1;
        }
        let active = &endpoint.endpoint_id == state.active_endpoint_id;
        let collapsed = state.collapsed_endpoints.contains(&endpoint.endpoint_id);
        if !single_endpoint && skip > 0 {
            skip -= 1;
        } else if !single_endpoint {
            let rect = Rect::new(workspace_area.x, y, workspace_area.width, 1);
            if active && collapsed {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let label = if endpoint.endpoint_id.is_local() {
                "L".to_owned()
            } else {
                machine_number.to_string()
            };
            let marker = if collapsed { "▸" } else { "▾" };
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width.saturating_sub(1),
                &format!("{marker}{label}"),
                Style::default().fg(if endpoint.status == ClientEndpointStatus::Online {
                    palette.text
                } else {
                    palette.overlay0
                }),
            );
            let mut status_badge = Rect::default();
            if !endpoint.endpoint_id.is_local() {
                let (glyph, _, color) = endpoint_status_presentation(endpoint.status, palette);
                let width = display_width(glyph).min(rect.width);
                status_badge = Rect::new(rect.right().saturating_sub(width), rect.y, width, 1);
                put_right_text(
                    buffer,
                    rect,
                    rect.y,
                    glyph,
                    state.machine_diagnostics.badge_style(
                        endpoint,
                        palette,
                        Style::default().fg(color),
                    ),
                );
            }
            hits.machines.push(MachineHit {
                rect,
                status_badge,
                collapse_toggle: Rect::new(rect.x, rect.y, u16::from(rect.width > 1), 1),
                endpoint_id: endpoint.endpoint_id.clone(),
            });
            y = y.saturating_add(1);
        }
        if collapsed {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for workspace in &snapshot.workspaces {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if y >= workspace_area.bottom() {
                break;
            }
            let rect = Rect::new(workspace_area.x, y, workspace_area.width, 1);
            let focused = active && workspace.focused;
            let selected = state.selected_workspace_id.is_some_and(|target| {
                target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
            });
            let selection_background =
                crate::shell::sidebar::workspace_selection_background(palette);
            if selected {
                buffer.set_style(rect, Style::default().bg(selection_background));
            } else if focused {
                buffer.set_style(
                    rect,
                    Style::default().bg(crate::shell::sidebar::workspace_active_background(
                        palette,
                        state.selected_workspace_id.is_some(),
                    )),
                );
            }
            let stale = endpoint.status != ClientEndpointStatus::Online;
            let glyph = status_glyph(
                workspace.agent_status,
                config.status_indicators,
                palette,
                stale,
            );
            let number = if single_endpoint {
                format!("{:<2}", workspace.number)
            } else {
                format!(" {}", workspace.number)
            };
            let number_width =
                crate::shell::presentation::render::display_width(&number).min(rect.width);
            let dim = if stale {
                Modifier::DIM
            } else {
                Modifier::empty()
            };
            put_text(
                buffer,
                rect.x,
                rect.y,
                number_width,
                &number,
                Style::default()
                    .fg(if focused && !stale {
                        palette.text
                    } else {
                        palette.overlay0
                    })
                    .add_modifier(dim),
            );
            put_text(
                buffer,
                rect.x.saturating_add(number_width),
                rect.y,
                rect.width.saturating_sub(number_width),
                glyph.text,
                glyph.style,
            );
            hits.workspaces.push(WorkspaceHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace.workspace_id.clone(),
            });
            y = y.saturating_add(1);
        }
    }
    if let Some(divider_y) = divider_y {
        put_text(
            buffer,
            workspace_area.x,
            divider_y,
            workspace_area.width,
            &"─".repeat(workspace_area.width as usize),
            Style::default().fg(palette.surface_dim),
        );
    }
    crate::shell::sidebar::endpoint_agents::render_collapsed(
        buffer,
        detail_area,
        state.active_endpoint_id,
        single_endpoint,
        config,
        state.agent_panel_model,
        hits,
    );
    hits.sidebar_toggle = if area.is_empty() || workspace_area.width == 0 {
        Rect::default()
    } else {
        Rect::new(
            workspace_area.x + workspace_area.width / 2,
            area.bottom().saturating_sub(1),
            1,
            1,
        )
    };
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "»",
        Style::default().fg(palette.overlay0),
    );
}

pub(in crate::shell) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    active_snapshot: Option<&ClientShellSnapshot>,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let single_endpoint = state.endpoints.len() == 1;
    crate::shell::presentation::render::render_sidebar_background(buffer, area, palette);
    hits.sidebar_divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    let (workspace_area, detail_area) =
        expanded_sidebar_sections(area, state.sidebar_section_split);
    hits.sidebar_section_divider = sidebar_section_divider_rect(area, state.sidebar_section_split);
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        if single_endpoint {
            " spaces"
        } else {
            " machines"
        },
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );

    enum Row {
        Endpoint(usize),
        Workspace { endpoint: usize, entry: usize },
    }
    let mut rows = Vec::new();
    for (endpoint_index, endpoint) in state.endpoints.iter().enumerate() {
        if !single_endpoint {
            rows.push(Row::Endpoint(endpoint_index));
        }
        if state.collapsed_endpoints.contains(&endpoint.endpoint_id) {
            continue;
        }
        if let Some(snapshot) = endpoint.snapshot.as_deref() {
            rows.extend(
                snapshot
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(entry, _)| Row::Workspace {
                        endpoint: endpoint_index,
                        entry,
                    }),
            );
        }
    }
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    hits.workspace_body = body;
    let row_heights = rows
        .iter()
        .map(|row| match row {
            Row::Endpoint(_) => 1,
            Row::Workspace { endpoint, entry } => {
                let endpoint = &state.endpoints[*endpoint];
                endpoint
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        let workspace = snapshot.workspaces.get(*entry)?;
                        let len = crate::shell::sidebar::workspace_rows(
                            workspace,
                            workspace.agent_status,
                            &config.spaces,
                        )
                        .len()
                        .max(1);
                        Some(u16::try_from(len).unwrap_or(u16::MAX))
                    })
                    .unwrap_or(1)
            }
        })
        .collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, row)| match (row, rows.get(index + 1)) {
            (
                Row::Workspace { endpoint, .. },
                Some(Row::Workspace {
                    endpoint: next_endpoint,
                    ..
                }),
            ) if endpoint == next_endpoint => config.spaces.row_gap,
            _ => 0,
        })
        .collect::<Vec<_>>();
    let reveal_navigation = !body.is_empty() && std::mem::take(state.reveal_navigation_workspace);
    let reveal_focus = !body.is_empty() && std::mem::take(state.reveal_focused_workspace);
    if reveal_navigation || reveal_focus {
        let selected_row = rows.iter().position(|row| match row {
            Row::Workspace { endpoint, entry } => {
                let endpoint = &state.endpoints[*endpoint];
                endpoint
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.workspaces.get(*entry))
                    .is_some_and(|workspace| {
                        if reveal_navigation {
                            state.selected_workspace_id.is_some_and(|target| {
                                target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
                            })
                        } else {
                            &endpoint.endpoint_id == state.active_endpoint_id
                                && active_snapshot.is_some_and(|snapshot| {
                                    snapshot.focused_workspace_id.as_deref()
                                        == Some(workspace.workspace_id.as_str())
                                })
                        }
                    })
            }
            Row::Endpoint(_) => false,
        });
        if let Some(selected_row) = selected_row {
            *state.workspace_scroll = crate::shell::navigation::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                selected_row,
            );
        }
    }
    let metrics = crate::shell::navigation::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *state.workspace_scroll,
    );
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (row_index, row) in rows.iter().enumerate().skip(*state.workspace_scroll) {
        match row {
            Row::Endpoint(index) => {
                if y >= body.bottom() {
                    break;
                }
                let endpoint = &state.endpoints[*index];
                let rect = Rect::new(body.x, y, content_width, 1);
                let collapsed = state.collapsed_endpoints.contains(&endpoint.endpoint_id);
                let marker = if collapsed { "▸" } else { "▾" };
                let status_badge = render_endpoint_row(
                    buffer,
                    rect,
                    marker,
                    endpoint,
                    collapsed && &endpoint.endpoint_id == state.active_endpoint_id,
                    state.machine_diagnostics,
                    palette,
                );
                hits.machines.push(MachineHit {
                    rect,
                    status_badge,
                    collapse_toggle: Rect::new(
                        rect.x.saturating_add(1),
                        rect.y,
                        u16::from(rect.width > 1),
                        1,
                    ),
                    endpoint_id: endpoint.endpoint_id.clone(),
                });
                y = y
                    .saturating_add(1)
                    .saturating_add(gaps.get(row_index).copied().unwrap_or(0));
            }
            Row::Workspace { endpoint, entry } => {
                let endpoint = &state.endpoints[*endpoint];
                let Some(snapshot) = endpoint.snapshot.as_deref() else {
                    continue;
                };
                let Some(workspace) = snapshot.workspaces.get(*entry) else {
                    continue;
                };
                let status = workspace.agent_status;
                let tokens =
                    crate::shell::sidebar::workspace_rows(workspace, status, &config.spaces);
                let height = u16::try_from(tokens.len().max(1))
                    .unwrap_or(u16::MAX)
                    .min(body.height);
                if y.saturating_add(height) > body.bottom() {
                    break;
                }
                let rect = Rect::new(body.x, y, content_width, height);
                let nested = if single_endpoint {
                    rect
                } else {
                    Rect::new(
                        rect.x.saturating_add(2),
                        rect.y,
                        rect.width.saturating_sub(2),
                        rect.height,
                    )
                };
                let endpoint_active = &endpoint.endpoint_id == state.active_endpoint_id;
                let selected = state.selected_workspace_id.is_some_and(|target| {
                    target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
                });
                // Drag-reordering moves workspaces of the active machine only.
                let dragged = endpoint_active
                    && state
                        .dragged_workspace_id
                        .is_some_and(|id| id.as_str() == workspace.workspace_id.as_str());
                crate::shell::sidebar::render_workspace_rows(
                    buffer,
                    nested,
                    workspace.number,
                    status,
                    config.status_indicators,
                    &tokens,
                    endpoint_active && workspace.focused,
                    selected,
                    state.selected_workspace_id.is_some(),
                    dragged,
                    palette,
                );
                if endpoint.status != ClientEndpointStatus::Online {
                    buffer.set_style(
                        rect,
                        Style::default()
                            .fg(palette.overlay0)
                            .add_modifier(Modifier::DIM),
                    );
                }
                hits.workspaces.push(WorkspaceHit {
                    rect,
                    endpoint_id: endpoint.endpoint_id.clone(),
                    workspace_id: workspace.workspace_id.clone(),
                });
                y = y
                    .saturating_add(height)
                    .saturating_add(gaps.get(row_index).copied().unwrap_or(0));
            }
        }
    }
    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        crate::shell::navigation::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }

    // Same drop marker as the single-machine sidebar draws while a workspace is dragged.
    if let Some(row) = state.workspace_drop_indicator_row.filter(|row| {
        *row >= workspace_area.y.saturating_add(1)
            && *row < workspace_area.bottom().saturating_sub(1)
    }) {
        put_text(
            buffer,
            body.x,
            row,
            body.width,
            &"─".repeat(body.width as usize),
            Style::default().fg(palette.accent),
        );
    }

    let footer_y = workspace_area.bottom().saturating_sub(1);
    if config.mouse_capture {
        let label = if single_endpoint {
            " new".to_owned()
        } else {
            format!(" new · {}", active_endpoint_label(state))
        };
        hits.new_workspace = Rect::new(
            workspace_area.x,
            footer_y,
            display_width(&label).min(workspace_area.width),
            u16::from(workspace_area.height > 0),
        );
        put_text(
            buffer,
            workspace_area.x,
            footer_y,
            workspace_area.width,
            &label,
            Style::default().fg(palette.overlay0),
        );
        let width = 6.min(workspace_area.width);
        hits.global_launcher = Rect::new(
            workspace_area.right().saturating_sub(width),
            footer_y,
            width,
            1,
        );
        put_right_text(
            buffer,
            workspace_area,
            footer_y,
            "menu",
            Style::default().fg(palette.overlay0),
        );
    }
    crate::shell::sidebar::endpoint_agents::render_expanded(
        buffer,
        detail_area,
        state.active_endpoint_id,
        single_endpoint,
        config,
        state.agent_panel_model,
        state.agent_scroll,
        hits,
    );
    hits.sidebar_toggle = Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        u16::from(area.height > 0),
    );
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "«",
        Style::default().fg(palette.overlay0),
    );
}

fn active_endpoint_label<'a>(state: &'a ShellRenderState<'_>) -> &'a str {
    state
        .endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == state.active_endpoint_id)
        .map_or(state.active_endpoint_id.display_label(), |endpoint| {
            endpoint.endpoint_id.display_label()
        })
}

fn render_endpoint_row(
    buffer: &mut Buffer,
    rect: Rect,
    marker: &str,
    endpoint: &ClientShellEndpoint,
    highlighted: bool,
    auth: &crate::shell::overlays::machine_diagnostics::MachineDiagnostics,
    palette: &Palette,
) -> Rect {
    if highlighted {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    }
    let (glyph, state, color) = endpoint_status_presentation(endpoint.status, palette);
    let state = if endpoint.status == ClientEndpointStatus::Online {
        ""
    } else {
        state
    };
    let signal = if auth.required_for(endpoint) {
        "! auth".to_owned()
    } else if endpoint.status == ClientEndpointStatus::Attention {
        "! error".to_owned()
    } else if endpoint.endpoint_id.is_local() {
        String::new()
    } else if state.is_empty() {
        glyph.to_owned()
    } else {
        format!("{glyph} {state}")
    };
    let signal_width = display_width(&signal).min(rect.width);
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width.saturating_sub(signal_width.saturating_add(1)),
        &format!(" {marker} {}", endpoint.endpoint_id.display_label()),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    put_right_text(
        buffer,
        rect,
        rect.y,
        &signal,
        auth.badge_style(endpoint, palette, Style::default().fg(color)),
    );
    Rect::new(
        rect.right().saturating_sub(signal_width),
        rect.y,
        signal_width,
        1,
    )
}
