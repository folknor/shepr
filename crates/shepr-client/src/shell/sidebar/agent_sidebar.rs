use std::ops::Range;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};
use shepr_protocol::{ClientShellAgent, ClientShellPane, PublicPaneId, PublicTabId};

use super::*;

pub(super) struct AgentRow {
    pub(super) pane_id: shepr_protocol::PublicPaneId,
    pub(super) status: shepr_api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<ResolvedToken>>,
}

pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: shepr_config::AgentPanelSortConfig,
) -> Vec<shepr_protocol::PublicPaneId> {
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    sort_agent_refs(&mut agents, sort);
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

fn sort_agent_refs(agents: &mut [&ClientShellAgent], sort: shepr_config::AgentPanelSortConfig) {
    if sort == shepr_config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
}

pub(super) fn render_agent_panel_header(
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

pub(super) fn render_agent_list<T>(
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
    let metrics =
        super::scroll::list_scroll_metrics(&row_heights, &gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
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
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    if snapshot.agents.is_empty() {
        return Vec::new();
    }
    let index = AgentRowIndex::new(snapshot, config.agent_panel_sort);
    index.items[index.agents.clone()]
        .iter()
        .filter_map(|item| match item {
            AgentRowIndexItem::Agent { agent, .. } => index.agent_row(agent, config, machine),
            _ => None,
        })
        .collect()
}

/// Snapshot-local joins for the agent panel. Build the indexes once per list
/// render so each row resolves its related resources with keyed lookups.
struct AgentRowIndex<'a> {
    items: Vec<AgentRowIndexItem<'a>>,
    agents: Range<usize>,
    workspaces: Range<usize>,
    tabs: Range<usize>,
    panes: Range<usize>,
    tab_workspaces: Range<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AgentRowIndexKind {
    Agent,
    Workspace,
    Tab,
    Pane,
    TabWorkspace,
}

enum AgentRowIndexItem<'a> {
    Agent {
        agent: &'a ClientShellAgent,
        order: usize,
    },
    Workspace(&'a ClientShellWorkspace),
    Tab(&'a ClientShellTab),
    Pane(&'a ClientShellPane),
    TabWorkspace(&'a str),
}

impl AgentRowIndexItem<'_> {
    fn kind(&self) -> AgentRowIndexKind {
        match self {
            Self::Agent { .. } => AgentRowIndexKind::Agent,
            Self::Workspace(_) => AgentRowIndexKind::Workspace,
            Self::Tab(_) => AgentRowIndexKind::Tab,
            Self::Pane(_) => AgentRowIndexKind::Pane,
            Self::TabWorkspace(_) => AgentRowIndexKind::TabWorkspace,
        }
    }
}

impl<'a> AgentRowIndex<'a> {
    fn new(snapshot: &'a ClientShellSnapshot, sort: shepr_config::AgentPanelSortConfig) -> Self {
        let capacity = snapshot
            .agents
            .len()
            .saturating_add(snapshot.workspaces.len())
            .saturating_add(snapshot.tabs.len().saturating_mul(2))
            .saturating_add(snapshot.panes.len());
        let mut items = Vec::with_capacity(capacity);
        for (order, agent) in snapshot.agents.iter().enumerate() {
            items.push(AgentRowIndexItem::Agent { agent, order });
        }
        items.extend(snapshot.workspaces.iter().map(AgentRowIndexItem::Workspace));
        for tab in &snapshot.tabs {
            items.push(AgentRowIndexItem::Tab(tab));
            items.push(AgentRowIndexItem::TabWorkspace(&tab.workspace_id));
        }
        items.extend(snapshot.panes.iter().map(AgentRowIndexItem::Pane));
        items.sort_unstable_by(|left, right| {
            left.kind()
                .cmp(&right.kind())
                .then_with(|| match (left, right) {
                    (
                        AgentRowIndexItem::Agent {
                            agent: left,
                            order: left_order,
                        },
                        AgentRowIndexItem::Agent {
                            agent: right,
                            order: right_order,
                        },
                    ) if sort == shepr_config::AgentPanelSortConfig::Priority => (
                        std::cmp::Reverse(status_priority(left.agent_status)),
                        std::cmp::Reverse(left.state_change_seq),
                        left_order,
                    )
                        .cmp(&(
                            std::cmp::Reverse(status_priority(right.agent_status)),
                            std::cmp::Reverse(right.state_change_seq),
                            right_order,
                        )),
                    (
                        AgentRowIndexItem::Agent {
                            order: left_order, ..
                        },
                        AgentRowIndexItem::Agent {
                            order: right_order, ..
                        },
                    ) => left_order.cmp(right_order),
                    (AgentRowIndexItem::Workspace(left), AgentRowIndexItem::Workspace(right)) => {
                        left.workspace_id.cmp(&right.workspace_id)
                    }
                    (AgentRowIndexItem::Tab(left), AgentRowIndexItem::Tab(right)) => {
                        left.tab_id.cmp(&right.tab_id)
                    }
                    (AgentRowIndexItem::Pane(left), AgentRowIndexItem::Pane(right)) => {
                        left.pane_id.cmp(&right.pane_id)
                    }
                    (
                        AgentRowIndexItem::TabWorkspace(left),
                        AgentRowIndexItem::TabWorkspace(right),
                    ) => left.cmp(right),
                    _ => std::cmp::Ordering::Equal,
                })
        });
        let agents = Self::kind_range(&items, AgentRowIndexKind::Agent);
        let workspaces = Self::kind_range(&items, AgentRowIndexKind::Workspace);
        let tabs = Self::kind_range(&items, AgentRowIndexKind::Tab);
        let panes = Self::kind_range(&items, AgentRowIndexKind::Pane);
        let tab_workspaces = Self::kind_range(&items, AgentRowIndexKind::TabWorkspace);
        Self {
            agents,
            workspaces,
            tabs,
            panes,
            tab_workspaces,
            items,
        }
    }

    fn kind_range(items: &[AgentRowIndexItem<'_>], kind: AgentRowIndexKind) -> Range<usize> {
        let start = items.partition_point(|item| item.kind() < kind);
        let end = items.partition_point(|item| item.kind() <= kind);
        start..end
    }

    fn workspace(&self, workspace_id: &str) -> Option<&'a ClientShellWorkspace> {
        let items = &self.items[self.workspaces.clone()];
        let index = items
            .binary_search_by(|item| match item {
                AgentRowIndexItem::Workspace(workspace) => {
                    workspace.workspace_id.as_str().cmp(workspace_id)
                }
                _ => std::cmp::Ordering::Equal,
            })
            .ok()?;
        match items[index] {
            AgentRowIndexItem::Workspace(workspace) => Some(workspace),
            _ => None,
        }
    }

    fn tab(&self, tab_id: &PublicTabId) -> Option<&'a ClientShellTab> {
        let items = &self.items[self.tabs.clone()];
        let index = items
            .binary_search_by(|item| match item {
                AgentRowIndexItem::Tab(tab) => tab.tab_id.cmp(tab_id),
                _ => std::cmp::Ordering::Equal,
            })
            .ok()?;
        match items[index] {
            AgentRowIndexItem::Tab(tab) => Some(tab),
            _ => None,
        }
    }

    fn pane(&self, pane_id: &PublicPaneId) -> Option<&'a ClientShellPane> {
        let items = &self.items[self.panes.clone()];
        let index = items
            .binary_search_by(|item| match item {
                AgentRowIndexItem::Pane(pane) => pane.pane_id.cmp(pane_id),
                _ => std::cmp::Ordering::Equal,
            })
            .ok()?;
        match items[index] {
            AgentRowIndexItem::Pane(pane) => Some(pane),
            _ => None,
        }
    }

    fn tab_count(&self, workspace_id: &str) -> usize {
        let items = &self.items[self.tab_workspaces.clone()];
        let start = items.partition_point(|item| match item {
            AgentRowIndexItem::TabWorkspace(candidate) => *candidate < workspace_id,
            _ => false,
        });
        let end = items.partition_point(|item| match item {
            AgentRowIndexItem::TabWorkspace(candidate) => *candidate <= workspace_id,
            _ => false,
        });
        end - start
    }

    fn agent_row(
        &self,
        agent: &'a ClientShellAgent,
        config: &ClientShellConfig,
        machine: Option<&str>,
    ) -> Option<AgentRow> {
        let workspace = self.workspace(&agent.workspace_id)?;
        let tab = self.tab(&agent.tab_id);
        let pane = self.pane(&agent.pane_id);
        let tab_count = self.tab_count(&agent.workspace_id);
        let tab_label = tab
            .filter(|tab| tab_count > 1 || tab.custom_label)
            .map(|tab| tab.label.as_str());
        let agent_label = agent.name.as_deref().or(agent.agent.as_deref());
        let state_text = status_text(agent.agent_status);
        let canonical_agent = agent
            .agent
            .as_deref()
            .and_then(shepr_agent::detect::parse_agent_label);
        let rows = sidebar_agent_rows(
            &config.agents,
            &AgentTokenContext {
                machine,
                workspace: &workspace.label,
                tab: tab_label,
                pane: pane.and_then(|pane| pane.label.as_deref()),
                agent_label,
                terminal_title: agent.terminal_title.as_deref(),
                terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
                canonical_agent,
            },
            state_text,
        );
        Some(AgentRow {
            pane_id: agent.pane_id.clone(),
            status: agent.agent_status,
            focused: agent.focused,
            rows,
        })
    }
}

pub(super) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
    let row_style = if row.focused {
        Style::default().bg(palette.active_row_bg)
    } else {
        Style::default()
    };
    let name_style = if row.focused {
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
    let rows = if row.rows.is_empty() {
        vec![vec![ResolvedToken {
            kind: ResolvedTokenKind::StateIcon,
            style: Default::default(),
        }]]
    } else {
        row.rows.clone()
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
        Paragraph::new(Line::from(spans)).style(row_style).render(
            Rect::new(
                rect.x,
                rect.y + u16::try_from(index).unwrap_or(u16::MAX),
                rect.width,
                1,
            ),
            buffer,
        );
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let mut offset = 0usize;
    // The cell holding the last drawn character, so zero-width characters
    // (combining marks, joiners, variation selectors) join that cell.
    let mut last_column = None;
    for (unit, char_width) in shepr_vt::unicode_display_units(text) {
        let char_width = usize::from(char_width);
        if char_width == 0 {
            if !unit.chars().all(char::is_control)
                && let Some(cell) = last_column.and_then(|column| buffer.cell_mut((column, y)))
            {
                let mut symbol = cell.symbol().to_owned();
                symbol.push_str(unit);
                cell.set_symbol(&symbol);
            }
            continue;
        }
        if offset.saturating_add(char_width) > usize::from(width) {
            break;
        }
        let Ok(column_offset) = u16::try_from(offset) else {
            break;
        };
        let Some(column) = x.checked_add(column_offset) else {
            break;
        };
        if let Some(cell) = buffer.cell_mut((column, y)) {
            cell.set_symbol(unit).set_style(style);
            last_column = Some(column);
        } else {
            last_column = None;
        }
        offset = offset.saturating_add(char_width);
    }
}

fn display_width(text: &str) -> usize {
    shepr_vt::unicode_text_width(text)
}

#[cfg(test)]
mod tests {
    use super::{display_width, put_text};
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

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

        assert_eq!(buffer[(0, 0)].symbol(), "x");
        assert_eq!(buffer[(1, 0)].symbol(), "\u{ff9e}");
        assert_eq!(buffer[(2, 0)].symbol(), "y");
    }

    #[test]
    fn display_width_matches_terminal_codepoints_and_voiced_marks() {
        assert_eq!(display_width("\u{263a}\u{fe0f}"), 1);
        assert_eq!(
            display_width("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            6
        );
        assert_eq!(display_width("ｶﾞx"), 3);
    }
}
