use crate::shell::sidebar::sidebar_tokens::ResolvedTokenKind;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use shepr_protocol::{ClientShellAgent, ClientShellPane, PublicPaneId};

use crate::shell::presentation::status::{status_glyph, status_text};
use crate::shell::sidebar::sidebar_tokens::{
    AgentTokenContext, ResolvedToken, TokenStyles, resolved_token_spans, sidebar_agent_rows,
};
use crate::shell::state::{ClientShellConfig, ShellHitMap};
use shepr_protocol::{ClientShellSnapshot, ClientShellWorkspace};
use std::collections::HashMap;

pub(in crate::shell) struct AgentRow {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) status: shepr_protocol::AgentStatus,
    pub(in crate::shell) focused: bool,
    pub(in crate::shell) rows: Vec<Vec<ResolvedToken>>,
    pub(in crate::shell) state_change_seq: u64,
}

pub(in crate::shell) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(config.palette.surface_dim),
    );
    if area.height < 2 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " agents",
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = match config.agent_panel_sort {
        shepr_config::AgentPanelSortConfig::Spaces => "grouped",
        shepr_config::AgentPanelSortConfig::Priority => "priority",
    };
    let sort_width =
        u16::try_from(display_width(sort_label).min(usize::from(area.width))).unwrap_or(u16::MAX);
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    hits.agent_sort_toggle = if config.mouse_capture {
        sort_rect
    } else {
        Rect::default()
    };
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    true
}

pub(in crate::shell) fn render_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[T],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    row_lines: impl Fn(&T) -> usize,
    mut render_row: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || rows.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let row_heights = rows
        .iter()
        .map(|row| u16::try_from(row_lines(row).max(1)).unwrap_or(u16::MAX))
        .collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let metrics = crate::shell::navigation::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *agent_scroll,
    );
    hits.agent_max_scroll = metrics.max_start();
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics.start();
    let show_scrollbar = metrics.max_start() > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*agent_scroll) {
        let height = row_heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        render_row(buffer, rect, row, hits);
        y = y
            .saturating_add(height)
            .saturating_add(if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            });
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        crate::shell::navigation::scroll::render_list_scrollbar(
            buffer,
            track,
            metrics,
            &config.palette,
        );
    }
}

pub(in crate::shell) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    if snapshot.agents.is_empty() {
        return Vec::new();
    }
    let index = AgentRowIndex::new(snapshot);
    index
        .agents
        .iter()
        .filter_map(|agent| index.agent_row(agent, config, machine))
        .collect()
}

/// Snapshot-local joins used while building the shared agent panel model.
struct AgentRowIndex<'a> {
    agents: &'a [ClientShellAgent],
    workspaces: HashMap<shepr_protocol::WorkspaceId, &'a ClientShellWorkspace>,
    panes: HashMap<PublicPaneId, &'a ClientShellPane>,
    focused_pane_id: Option<&'a PublicPaneId>,
}

impl<'a> AgentRowIndex<'a> {
    fn new(snapshot: &'a ClientShellSnapshot) -> Self {
        let workspaces = snapshot
            .workspaces
            .iter()
            .map(|workspace| (workspace.workspace_id, workspace))
            .collect();
        let panes = snapshot
            .panes
            .iter()
            .map(|pane| (pane.pane_id, pane))
            .collect();
        Self {
            agents: &snapshot.agents,
            workspaces,
            panes,
            focused_pane_id: snapshot.focused_pane_id.as_ref(),
        }
    }

    fn workspace(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<&'a ClientShellWorkspace> {
        self.workspaces.get(workspace_id).copied()
    }

    fn pane(&self, pane_id: &PublicPaneId) -> Option<&'a ClientShellPane> {
        self.panes.get(pane_id).copied()
    }

    fn agent_row(
        &self,
        agent: &'a ClientShellAgent,
        config: &ClientShellConfig,
        machine: Option<&str>,
    ) -> Option<AgentRow> {
        let workspace = self.workspace(agent.pane_id.workspace_id())?;
        let pane = self.pane(&agent.pane_id);
        let agent_label = agent.agent.map(shepr_config::ConfigAgent::label);
        let state_text = status_text(agent.agent_status);
        let canonical_agent = agent.agent;
        let rows = sidebar_agent_rows(
            &config.agents,
            &AgentTokenContext {
                machine,
                workspace: &workspace.label,
                pane: pane.and_then(|pane| pane.label.as_deref()),
                agent_label,
                terminal_title: agent.terminal_title.as_deref(),
                terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
                canonical_agent,
            },
            state_text,
        );
        Some(AgentRow {
            pane_id: agent.pane_id,
            status: agent.agent_status,
            focused: self.focused_pane_id == Some(&agent.pane_id),
            rows,
            state_change_seq: agent.state_change_seq,
        })
    }
}

pub(in crate::shell) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    focused: bool,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
    let row_style = if focused {
        Style::default().bg(palette.active_row_bg)
    } else {
        Style::default()
    };
    let name_style = if focused {
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD)
    };
    let glyph = status_glyph(row.status, config.status_indicators, palette, false);
    let status_style = glyph.style;
    let secondary = Style::default().fg(palette.overlay0);
    let fallback = [vec![ResolvedToken {
        kind: ResolvedTokenKind::StateIcon,
        style: Default::default(),
    }]];
    let rows = if row.rows.is_empty() {
        &fallback[..]
    } else {
        &row.rows
    };
    for (index, tokens) in rows.iter().take(rect.height as usize).enumerate() {
        let indent = if index == 0 { 1 } else { 3 };
        let mut spans = vec![ratatui::text::Span::raw(" ".repeat(indent))];
        spans.extend(resolved_token_spans(
            tokens,
            glyph,
            TokenStyles {
                state_text: status_style,
                primary: name_style,
                secondary,
                terminal_title: secondary,
            },
            palette,
            usize::from(
                rect.width
                    .saturating_sub(u16::try_from(indent).unwrap_or(u16::MAX)),
            ),
        ));
        crate::shell::presentation::render::put_spans(
            buffer,
            Rect::new(
                rect.x,
                rect.y + u16::try_from(index).unwrap_or(u16::MAX),
                rect.width,
                1,
            ),
            &spans,
            row_style,
        );
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    crate::shell::presentation::render::put_text(buffer, x, y, width, text, style);
}

fn display_width(text: &str) -> usize {
    crate::shell::presentation::render::rendered_text_width(text)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use crate::shell::presentation::render::{display_width, put_text};

    #[test]
    fn put_text_advances_by_display_width_and_clips_wide_characters() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));

        put_text(&mut buffer, 0, 0, 4, "界ab", Style::default());

        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_eq!(buffer[(2, 0)].symbol(), "a");
        assert_eq!(buffer[(3, 0)].symbol(), "b");
        assert_eq!(buffer[(4, 0)].symbol(), " ");

        let mut narrow = Buffer::empty(Rect::new(0, 0, 3, 1));
        put_text(&mut narrow, 0, 0, 2, "界x", Style::default());
        assert_eq!(narrow[(0, 0)].symbol(), "界");
        assert_eq!(narrow[(2, 0)].symbol(), " ");
    }

    #[test]
    fn put_text_attaches_combining_marks_to_the_previous_cell() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));

        // "e" + U+0301 (combining acute accent), then "x"; a leading mark has no cell
        // to join and is dropped, as is a control character.
        put_text(
            &mut buffer,
            0,
            0,
            4,
            "\u{301}e\u{301}\u{7}x",
            Style::default(),
        );

        assert_eq!(buffer[(0, 0)].symbol(), "e\u{301}");
        assert_eq!(buffer[(1, 0)].symbol(), "x");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }

    #[test]
    fn put_text_gives_halfwidth_voiced_marks_their_terminal_column() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));

        put_text(&mut buffer, 0, 0, 3, "x\u{ff9e}y", Style::default());

        assert_eq!(buffer[(0, 0)].symbol(), "x\u{ff9e}");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "y");
    }

    #[test]
    fn emoji_title_width_matches_outer_terminal_graphemes() {
        let title = "\u{2764}\u{fe0f}agent";
        assert_eq!(display_width(title), 7);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        put_text(&mut buffer, 0, 0, 3, "\u{2764}\u{fe0f}x", Style::default());
        assert_eq!(buffer[(0, 0)].symbol(), "\u{2764}\u{fe0f}");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "x");
    }

    #[test]
    fn display_width_matches_outer_terminal_emoji_and_voiced_marks() {
        assert_eq!(display_width("\u{263a}\u{fe0f}"), 2);
        assert_eq!(
            display_width("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            2
        );
        assert_eq!(display_width("ｶﾞx"), 3);
    }
}
