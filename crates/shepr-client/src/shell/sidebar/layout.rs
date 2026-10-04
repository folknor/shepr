//! Sidebar layout: the pure resolution of what the sidebar shows, from the shell's
//! endpoints, the stored scroll state and the area it is given. Drawing reads the
//! resulting view and writes only to the buffer; nothing here mutates the shell.

use std::collections::HashSet;

use ratatui::layout::Rect;
use shepr_config::theme::Palette;
use shepr_protocol::ClientShellSnapshot;

use crate::endpoint::{ClientEndpointId, ClientEndpointStatus};
use crate::shell::config::ClientShellConfig;
use crate::shell::endpoints::{ClientShellEndpoint, endpoint_status_presentation};
use crate::shell::navigation::aggregate_navigation::AgentPanelModel;
use crate::shell::navigation::location::{Location, PinnedLocation};
use crate::shell::notices::machine_diagnostics::MachineDiagnostics;
use crate::shell::presentation::text::display_width;
use crate::shell::sidebar::scroll::{SidebarScroll, SidebarScrollResolution, WorkspaceReveal};
use crate::shell::sidebar::sidebar_tokens::{
    SectionSplit, expanded_sidebar_sections, sidebar_section_divider_rect,
};
use crate::shell::view::list::{ListView, resolve_list};
use crate::shell::view::{AgentHit, MachineHit, WorkspaceHit};

/// Rows the workspace section header occupies above the workspace entries.
const WORKSPACE_HEADER_ROWS: u16 = 2;

/// Which sidebar the caller decided to lay out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum SidebarForm {
    Hidden,
    Collapsed,
    Expanded,
}

pub(in crate::shell) enum SidebarView {
    Hidden,
    Collapsed(CollapsedSidebarView),
    Expanded(ExpandedSidebarView),
}

pub(in crate::shell) struct ExpandedSidebarView {
    pub(in crate::shell) area: Rect,
    pub(in crate::shell) divider: Rect,
    pub(in crate::shell) section_divider: Rect,
    pub(in crate::shell) toggle: Rect,
    pub(in crate::shell) workspace_area: Rect,
    pub(in crate::shell) workspaces: ListView<ExpandedSlot>,
    pub(in crate::shell) drop_indicator: Option<u16>,
    /// `None` without mouse capture (not drawn).
    pub(in crate::shell) footer: Option<SidebarFooter>,
    /// `None` when the agent section has no rows at all.
    pub(in crate::shell) agents: Option<AgentPanelView>,
}

pub(in crate::shell) enum ExpandedSlot {
    Machine {
        hit: MachineHit,
        endpoint: usize,
    },
    Workspace {
        hit: WorkspaceHit,
        nested: Rect,
        endpoint: usize,
        entry: usize,
    },
}

pub(in crate::shell) struct SidebarFooter {
    pub(in crate::shell) new_workspace: Rect,
    pub(in crate::shell) global_launcher: Rect,
    pub(in crate::shell) label: String,
}

pub(in crate::shell) struct AgentPanelView {
    pub(in crate::shell) area: Rect,
    /// `None` when the panel is too short for its header row.
    pub(in crate::shell) sort_toggle: Option<Rect>,
    /// `None` when the panel is too short for its header row.
    pub(in crate::shell) list: Option<ListView<AgentSlot>>,
}

pub(in crate::shell) struct AgentSlot {
    pub(in crate::shell) hit: AgentHit,
    /// Index into `AgentPanelModel::rows`.
    pub(in crate::shell) row: usize,
}

pub(in crate::shell) struct CollapsedSidebarView {
    pub(in crate::shell) area: Rect,
    /// No scrollbar, as the collapsed sidebar never draws one.
    pub(in crate::shell) workspaces: ListView<CollapsedSlot>,
    pub(in crate::shell) divider_y: Option<u16>,
    pub(in crate::shell) agents: Vec<AgentSlot>,
    pub(in crate::shell) toggle: Rect,
}

pub(in crate::shell) enum CollapsedSlot {
    Machine {
        hit: MachineHit,
        endpoint: usize,
        machine_number: usize,
    },
    Workspace {
        hit: WorkspaceHit,
        endpoint: usize,
        entry: usize,
    },
}

/// Everything sidebar layout and drawing read, borrowed from the shell.
pub(in crate::shell) struct SidebarInputs<'a> {
    pub(in crate::shell) endpoints: &'a [ClientShellEndpoint],
    pub(in crate::shell) presented: &'a ClientEndpointId,
    pub(in crate::shell) collapsed: &'a HashSet<ClientEndpointId>,
    pub(in crate::shell) model: &'a AgentPanelModel,
    pub(in crate::shell) config: &'a ClientShellConfig,
    pub(in crate::shell) machine_diagnostics: &'a MachineDiagnostics,
    pub(in crate::shell) active_snapshot: Option<&'a ClientShellSnapshot>,
    pub(in crate::shell) selected: Option<&'a PinnedLocation>,
    pub(in crate::shell) section_split: SectionSplit,
    pub(in crate::shell) dragged_workspace: Option<&'a shepr_protocol::WorkspaceId>,
    pub(in crate::shell) drop_indicator_row: Option<u16>,
}

impl SidebarInputs<'_> {
    pub(in crate::shell) fn single_endpoint(&self) -> bool {
        self.endpoints.len() == 1
    }

    /// The presented endpoint's label, for the footer.
    fn presented_label(&self) -> &str {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == self.presented)
            .map_or(self.presented.display_label(), |endpoint| {
                endpoint.endpoint_id.display_label()
            })
    }
}

/// One row of the flattened workspace list.
#[derive(Clone, Copy)]
enum Row {
    Endpoint(usize),
    Workspace { endpoint: usize, entry: usize },
}

pub(in crate::shell) fn agent_sort_label(config: &ClientShellConfig) -> &'static str {
    match config.agent_panel_sort {
        shepr_config::AgentPanelSortConfig::Spaces => "grouped",
        shepr_config::AgentPanelSortConfig::Priority => "priority",
    }
}

/// The text and color of an expanded machine row's status signal.
pub(in crate::shell) fn endpoint_signal(
    endpoint: &ClientShellEndpoint,
    diagnostics: &MachineDiagnostics,
    palette: &Palette,
) -> (String, ratatui::style::Color) {
    let (glyph, state, color) = endpoint_status_presentation(endpoint.state.status(), palette);
    let state = if endpoint.state.usable() { "" } else { state };
    let signal = if diagnostics.required_for(endpoint) {
        "! auth".to_owned()
    } else if endpoint.state.status() == ClientEndpointStatus::Attention {
        // The shared status presentation spells it, Local included.
        format!("{glyph} {state}")
    } else if endpoint.endpoint_id.is_local() {
        String::new()
    } else if state.is_empty() {
        glyph.to_owned()
    } else {
        format!("{glyph} {state}")
    };
    (signal, color)
}

/// Lays out the sidebar in `area` and resolves the scroll state against that layout.
/// `implied_selected_reveal` asks for the selected workspace to be revealed without a
/// stored request (the size changed while navigating).
pub(in crate::shell) fn resolve_sidebar(
    area: Rect,
    form: SidebarForm,
    inputs: &SidebarInputs<'_>,
    scroll: &SidebarScroll,
    implied_selected_reveal: bool,
) -> (SidebarView, SidebarScrollResolution) {
    match form {
        SidebarForm::Hidden => (
            SidebarView::Hidden,
            SidebarScrollResolution {
                workspaces: None,
                agents: None,
                workspace_reveal_consumed: false,
                agent_reveal_consumed: false,
            },
        ),
        SidebarForm::Collapsed => {
            let (view, resolution) =
                resolve_collapsed(area, inputs, scroll, implied_selected_reveal);
            (SidebarView::Collapsed(view), resolution)
        }
        SidebarForm::Expanded => {
            let (view, resolution) =
                resolve_expanded(area, inputs, scroll, implied_selected_reveal);
            (SidebarView::Expanded(view), resolution)
        }
    }
}

fn workspace_of<'a>(
    inputs: &'a SidebarInputs<'_>,
    endpoint: usize,
    entry: usize,
) -> Option<(
    &'a ClientEndpointId,
    &'a shepr_protocol::ClientShellWorkspace,
)> {
    let endpoint = inputs.endpoints.get(endpoint)?;
    let workspace = endpoint.snapshot()?.workspaces.get(entry)?;
    Some((&endpoint.endpoint_id, workspace))
}

/// The row of the first pending workspace reveal that has a target in the list:
/// selected, then explicit, then focused.
fn workspace_reveal_row(
    rows: &[Row],
    inputs: &SidebarInputs<'_>,
    reveal: &WorkspaceReveal,
    implied_selected_reveal: bool,
) -> Option<usize> {
    let find =
        |matches: &dyn Fn(&ClientEndpointId, &shepr_protocol::ClientShellWorkspace) -> bool| {
            rows.iter().position(|row| match *row {
                Row::Workspace { endpoint, entry } => workspace_of(inputs, endpoint, entry)
                    .is_some_and(|(endpoint_id, workspace)| matches(endpoint_id, workspace)),
                Row::Endpoint(_) => false,
            })
        };
    if (reveal.selected_pending() || implied_selected_reveal)
        && let Some(selected) = inputs.selected
        && let Some(row) = find(&|endpoint_id, workspace| {
            selected.matches_workspace(endpoint_id, &workspace.workspace_id)
        })
    {
        return Some(row);
    }
    if let Some(location) = reveal.explicit()
        && let Some(workspace_id) = location.workspace_id()
        && let Some(row) = find(&|endpoint_id, workspace| {
            endpoint_id == &location.endpoint && workspace.workspace_id == workspace_id
        })
    {
        return Some(row);
    }
    if reveal.focused_pending()
        && let Some(focused) = inputs
            .active_snapshot
            .and_then(|snapshot| snapshot.focused_workspace_id)
    {
        return find(&|endpoint_id, workspace| {
            endpoint_id == inputs.presented && workspace.workspace_id == focused
        });
    }
    None
}

/// The flattened endpoint and workspace rows, in drawing order.
fn flattened_rows(inputs: &SidebarInputs<'_>) -> Vec<Row> {
    let single_endpoint = inputs.single_endpoint();
    let mut rows = Vec::new();
    for (endpoint_index, endpoint) in inputs.endpoints.iter().enumerate() {
        if !single_endpoint {
            rows.push(Row::Endpoint(endpoint_index));
        }
        if inputs.collapsed.contains(&endpoint.endpoint_id) {
            continue;
        }
        if let Some(snapshot) = endpoint.snapshot() {
            rows.extend((0..snapshot.workspaces.len()).map(|entry| Row::Workspace {
                endpoint: endpoint_index,
                entry,
            }));
        }
    }
    rows
}

fn resolve_collapsed(
    area: Rect,
    inputs: &SidebarInputs<'_>,
    scroll: &SidebarScroll,
    implied_selected_reveal: bool,
) -> (CollapsedSidebarView, SidebarScrollResolution) {
    let palette = &inputs.config.palette;
    let (workspace_area, divider_y, detail_area) =
        crate::shell::sidebar::collapsed_sidebar_sections(area);
    let rows = flattened_rows(inputs);
    let heights = vec![1u16; rows.len()];
    let gaps = vec![0u16; rows.len()];
    let target = workspace_reveal_row(
        &rows,
        inputs,
        scroll.workspace_reveal(),
        implied_selected_reveal,
    );
    let list = resolve_list(
        &heights,
        &gaps,
        workspace_area,
        scroll.workspace_start(),
        target,
    );

    let mut slots = Vec::new();
    if list.start.is_some() {
        let mut y = workspace_area.y;
        for row in rows.iter().skip(list.scroll.start()) {
            if y >= workspace_area.bottom() {
                break;
            }
            let rect = Rect::new(workspace_area.x, y, workspace_area.width, 1);
            match *row {
                Row::Endpoint(endpoint_index) => {
                    let endpoint = &inputs.endpoints[endpoint_index];
                    let machine_number = inputs.endpoints[..=endpoint_index]
                        .iter()
                        .filter(|endpoint| !endpoint.endpoint_id.is_local())
                        .count();
                    let mut status_badge = Rect::default();
                    if !endpoint.endpoint_id.is_local() {
                        let (glyph, _, _) =
                            endpoint_status_presentation(endpoint.state.status(), palette);
                        let width = display_width(glyph).min(rect.width);
                        status_badge =
                            Rect::new(rect.right().saturating_sub(width), rect.y, width, 1);
                    }
                    slots.push(CollapsedSlot::Machine {
                        hit: MachineHit {
                            rect,
                            status_badge,
                            collapse_toggle: Rect::new(
                                rect.x,
                                rect.y,
                                u16::from(rect.width > 1),
                                1,
                            ),
                            location: Location::machine(endpoint.endpoint_id.clone()),
                        },
                        endpoint: endpoint_index,
                        machine_number,
                    });
                }
                Row::Workspace { endpoint, entry } => {
                    let Some((endpoint_id, workspace)) = workspace_of(inputs, endpoint, entry)
                    else {
                        continue;
                    };
                    slots.push(CollapsedSlot::Workspace {
                        hit: WorkspaceHit {
                            rect,
                            location: Location::workspace(
                                endpoint_id.clone(),
                                workspace.workspace_id,
                            ),
                        },
                        endpoint,
                        entry,
                    });
                }
            }
            y = y.saturating_add(1);
        }
    }

    let agents = inputs
        .model
        .rows
        .iter()
        .enumerate()
        .take(usize::from(detail_area.height))
        .map(|(index, row)| AgentSlot {
            hit: AgentHit {
                rect: Rect::new(
                    detail_area.x,
                    detail_area
                        .y
                        .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                    detail_area.width,
                    1,
                ),
                location: Location::pane(row.endpoint_id.clone(), row.agent.pane_id),
            },
            row: index,
        })
        .collect();

    let toggle = if area.is_empty() || workspace_area.width == 0 {
        Rect::default()
    } else {
        Rect::new(
            workspace_area.x + workspace_area.width / 2,
            area.bottom().saturating_sub(1),
            1,
            1,
        )
    };
    let resolution = SidebarScrollResolution {
        workspaces: list.start,
        agents: None,
        workspace_reveal_consumed: list.reveal_consumed,
        agent_reveal_consumed: false,
    };
    (
        CollapsedSidebarView {
            area,
            workspaces: ListView {
                body: workspace_area,
                scroll: list.scroll,
                scrollbar: None,
                slots,
            },
            divider_y,
            agents,
            toggle,
        },
        resolution,
    )
}

fn resolve_expanded(
    area: Rect,
    inputs: &SidebarInputs<'_>,
    scroll: &SidebarScroll,
    implied_selected_reveal: bool,
) -> (ExpandedSidebarView, SidebarScrollResolution) {
    let config = inputs.config;
    let single_endpoint = inputs.single_endpoint();
    let divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    let (workspace_area, detail_area) = expanded_sidebar_sections(area, inputs.section_split);
    let section_divider = sidebar_section_divider_rect(area, inputs.section_split);
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    let rows = flattened_rows(inputs);
    let heights = rows
        .iter()
        .map(|row| match *row {
            Row::Endpoint(_) => 1,
            Row::Workspace { endpoint, entry } => {
                workspace_of(inputs, endpoint, entry).map_or(1, |(_, workspace)| {
                    let len = crate::shell::sidebar::workspace_rows(
                        workspace,
                        workspace.agent_status,
                        &config.spaces,
                    )
                    .len()
                    .max(1);
                    u16::try_from(len).unwrap_or(u16::MAX)
                })
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
    let target = workspace_reveal_row(
        &rows,
        inputs,
        scroll.workspace_reveal(),
        implied_selected_reveal,
    );
    let list = resolve_list(&heights, &gaps, body, scroll.workspace_start(), target);
    let show_scrollbar = list.scroll.max_start() > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));

    let mut slots = Vec::new();
    if list.start.is_some() {
        let mut y = body.y;
        for (row_index, row) in rows.iter().enumerate().skip(list.scroll.start()) {
            let gap = gaps.get(row_index).copied().unwrap_or(0);
            match *row {
                Row::Endpoint(endpoint_index) => {
                    if y >= body.bottom() {
                        break;
                    }
                    let endpoint = &inputs.endpoints[endpoint_index];
                    let rect = Rect::new(body.x, y, content_width, 1);
                    let (signal, _) =
                        endpoint_signal(endpoint, inputs.machine_diagnostics, &config.palette);
                    let signal_width = display_width(&signal).min(rect.width);
                    slots.push(ExpandedSlot::Machine {
                        hit: MachineHit {
                            rect,
                            status_badge: Rect::new(
                                rect.right().saturating_sub(signal_width),
                                rect.y,
                                signal_width,
                                1,
                            ),
                            collapse_toggle: Rect::new(
                                rect.x.saturating_add(1),
                                rect.y,
                                u16::from(rect.width > 1),
                                1,
                            ),
                            location: Location::machine(endpoint.endpoint_id.clone()),
                        },
                        endpoint: endpoint_index,
                    });
                    y = y.saturating_add(1).saturating_add(gap);
                }
                Row::Workspace { endpoint, entry } => {
                    let Some((endpoint_id, workspace)) = workspace_of(inputs, endpoint, entry)
                    else {
                        continue;
                    };
                    let height = heights[row_index].min(body.height);
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
                    slots.push(ExpandedSlot::Workspace {
                        hit: WorkspaceHit {
                            rect,
                            location: Location::workspace(
                                endpoint_id.clone(),
                                workspace.workspace_id,
                            ),
                        },
                        nested,
                        endpoint,
                        entry,
                    });
                    y = y.saturating_add(height).saturating_add(gap);
                }
            }
        }
    }

    // The same drop marker the single-machine sidebar draws while a workspace is dragged.
    let drop_indicator = inputs.drop_indicator_row.filter(|row| {
        *row >= workspace_area.y.saturating_add(1)
            && *row < workspace_area.bottom().saturating_sub(1)
    });
    let footer = config.mouse_capture.then(|| {
        let footer_y = workspace_area.bottom().saturating_sub(1);
        let label = if single_endpoint {
            " new".to_owned()
        } else {
            format!(" new · {}", inputs.presented_label())
        };
        let launcher_width = 6.min(workspace_area.width);
        SidebarFooter {
            new_workspace: Rect::new(
                workspace_area.x,
                footer_y,
                display_width(&label).min(workspace_area.width),
                u16::from(workspace_area.height > 0),
            ),
            global_launcher: Rect::new(
                workspace_area.right().saturating_sub(launcher_width),
                footer_y,
                launcher_width,
                1,
            ),
            label,
        }
    });

    let (agents, agent_start, agent_consumed) = resolve_agent_panel(detail_area, inputs, scroll);
    let toggle = Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        u16::from(area.height > 0),
    );
    let resolution = SidebarScrollResolution {
        workspaces: list.start,
        agents: agent_start,
        workspace_reveal_consumed: list.reveal_consumed,
        agent_reveal_consumed: agent_consumed,
    };
    (
        ExpandedSidebarView {
            area,
            divider,
            section_divider,
            toggle,
            workspace_area,
            workspaces: ListView {
                body,
                scroll: list.scroll,
                scrollbar: show_scrollbar
                    .then(|| Rect::new(body.right().saturating_sub(1), body.y, 1, body.height)),
                slots,
            },
            drop_indicator,
            footer,
            agents,
        },
        resolution,
    )
}

/// Lays out the expanded agent section and resolves its list. Returns the view, the
/// start to commit and whether the agent reveal was consumed.
fn resolve_agent_panel(
    area: Rect,
    inputs: &SidebarInputs<'_>,
    scroll: &SidebarScroll,
) -> (Option<AgentPanelView>, Option<usize>, bool) {
    if area.height == 0 {
        return (None, None, false);
    }
    if area.height < 2 {
        return (
            Some(AgentPanelView {
                area,
                sort_toggle: None,
                list: None,
            }),
            None,
            false,
        );
    }
    let config = inputs.config;
    let sort_label = agent_sort_label(config);
    let sort_width = display_width(sort_label).min(area.width);
    let sort_toggle = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );

    let rows = &inputs.model.rows;
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    let heights = rows
        .iter()
        .map(|row| u16::try_from(row.agent.rows.len().max(1)).unwrap_or(u16::MAX))
        .collect::<Vec<_>>();
    let gap_after = |index: usize| {
        if index + 1 < rows.len() {
            config.agents.row_gap
        } else {
            0
        }
    };
    let gaps = (0..rows.len()).map(gap_after).collect::<Vec<_>>();
    let target = scroll.agent_reveal().and_then(|location| {
        rows.iter().position(|row| {
            row.endpoint_id == location.endpoint && Some(row.agent.pane_id) == location.pane_id()
        })
    });
    let list = resolve_list(&heights, &gaps, body, scroll.agent_start(), target);
    let show_scrollbar = list.scroll.max_start() > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut slots = Vec::new();
    if list.start.is_some() {
        let mut y = body.y;
        for index in list.scroll.start()..rows.len() {
            let height = heights[index].min(body.height);
            if y.saturating_add(height) > body.bottom() {
                break;
            }
            let row = &rows[index];
            slots.push(AgentSlot {
                hit: AgentHit {
                    rect: Rect::new(body.x, y, content_width, height),
                    location: Location::pane(row.endpoint_id.clone(), row.agent.pane_id),
                },
                row: index,
            });
            y = y.saturating_add(height).saturating_add(gap_after(index));
        }
    }
    (
        Some(AgentPanelView {
            area,
            sort_toggle: Some(sort_toggle),
            list: Some(ListView {
                body,
                scroll: list.scroll,
                scrollbar: show_scrollbar
                    .then(|| Rect::new(body.right().saturating_sub(1), body.y, 1, body.height)),
                slots,
            }),
        }),
        list.start,
        list.reveal_consumed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::state::ClientShellState;
    use crate::shell::tests::snapshot;
    use crate::tests::test_workspace_id;
    use shepr_protocol::ClientShellWorkspace;

    fn workspace(id: &str) -> ClientShellWorkspace {
        let mut value = snapshot().workspaces.remove(0);
        value.workspace_id = test_workspace_id(id);
        value.label = id.into();
        value
    }

    /// A shell with a local endpoint holding `count` workspaces and a remote endpoint
    /// holding `remote_count`, whose data feeds the pure inputs.
    fn shell(count: usize, remote_count: usize) -> (ClientShellState, ClientEndpointId) {
        let machine = shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("Build").expect("test precondition"),
            ssh: shepr_config::SshTarget::parse("dev@build.example").expect("test precondition"),
        };
        let remote = ClientEndpointId::Ssh(machine.label.clone());
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        state.set_machines(&[machine]);
        state.set_snapshot(Box::new(snapshot()));
        let mut remote_snapshot = snapshot();
        remote_snapshot.boot_id = crate::tests::test_boot_id("remote-boot");
        state.connect_endpoint_with_snapshot(&remote, 1, Box::new(remote_snapshot));
        let local_endpoint = ClientEndpointId::Local;
        for (endpoint_id, total) in [(local_endpoint, count), (remote.clone(), remote_count)] {
            let endpoint = state
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id)
                .expect("test precondition");
            let snapshot =
                std::sync::Arc::make_mut(endpoint.snapshot_mut().expect("test precondition"));
            snapshot.workspaces = (1..=total).map(|i| workspace(&format!("w{i}"))).collect();
            snapshot.focused_workspace_id = Some(test_workspace_id("w1"));
        }
        (state, remote)
    }

    fn inputs<'a>(
        state: &'a ClientShellState,
        selected: Option<&'a PinnedLocation>,
    ) -> SidebarInputs<'a> {
        SidebarInputs {
            endpoints: &state.endpoints,
            presented: state.endpoints.presented(),
            collapsed: &state.endpoints.collapsed,
            model: &state.endpoints.agent_panel_model,
            config: &state.config,
            machine_diagnostics: &state.machine_diagnostics,
            active_snapshot: state.endpoints.active.snapshot(),
            selected,
            section_split: SectionSplit::DEFAULT,
            dragged_workspace: None,
            drop_indicator_row: None,
        }
    }

    fn pinned(state: &ClientShellState, endpoint: &ClientEndpointId, id: &str) -> PinnedLocation {
        let endpoint_state = state
            .endpoints
            .iter()
            .find(|candidate| &candidate.endpoint_id == endpoint)
            .expect("test precondition");
        PinnedLocation::new(
            Location::workspace(endpoint.clone(), test_workspace_id(id)),
            endpoint_state
                .snapshot()
                .expect("test precondition")
                .boot_id
                .clone(),
            endpoint_state
                .snapshot_generation()
                .expect("test precondition"),
        )
    }

    fn drawn_workspaces(view: &SidebarView) -> Vec<Location> {
        match view {
            SidebarView::Expanded(view) => view
                .workspaces
                .slots
                .iter()
                .filter_map(|slot| match slot {
                    ExpandedSlot::Workspace { hit, .. } => Some(hit.location.clone()),
                    ExpandedSlot::Machine { .. } => None,
                })
                .collect(),
            SidebarView::Collapsed(view) => view
                .workspaces
                .slots
                .iter()
                .filter_map(|slot| match slot {
                    CollapsedSlot::Workspace { hit, .. } => Some(hit.location.clone()),
                    CollapsedSlot::Machine { .. } => None,
                })
                .collect(),
            SidebarView::Hidden => Vec::new(),
        }
    }

    #[test]
    fn selected_reveal_wins_over_focused_and_consumes_both() {
        let (state, remote) = shell(2, 12);
        let selected = pinned(&state, &remote, "w10");
        let mut scroll = SidebarScroll::new();
        scroll.reveal_selected_workspace();
        let inputs = inputs(&state, Some(&selected));

        let (view, resolution) = resolve_sidebar(
            Rect::new(0, 0, 30, 12),
            SidebarForm::Expanded,
            &inputs,
            &scroll,
            false,
        );

        assert!(resolution.workspace_reveal_consumed);
        assert!(drawn_workspaces(&view).contains(&selected.location));
        assert!(
            !drawn_workspaces(&view).contains(&Location::workspace(
                ClientEndpointId::Local,
                test_workspace_id("w1")
            )),
            "the focused workspace's reveal did not also apply"
        );
        scroll.commit(&resolution);
        assert!(!scroll.workspace_reveal().selected_pending());
        assert!(!scroll.workspace_reveal().focused_pending());
    }

    #[test]
    fn explicit_reveal_targets_the_named_endpoint_not_a_same_id_elsewhere() {
        let (state, remote) = shell(12, 12);
        let mut scroll = SidebarScroll::new();
        scroll.reveal_workspace(Location::workspace(
            remote.clone(),
            test_workspace_id("w11"),
        ));
        let inputs = inputs(&state, None);
        let area = Rect::new(0, 0, 30, 12);

        let (view, _) = resolve_sidebar(area, SidebarForm::Expanded, &inputs, &scroll, false);

        let drawn = drawn_workspaces(&view);
        assert!(drawn.contains(&Location::workspace(remote, test_workspace_id("w11"))));
        assert!(
            !drawn.contains(&Location::workspace(
                ClientEndpointId::Local,
                test_workspace_id("w11")
            )),
            "the same id on the local endpoint is a different row"
        );
    }

    #[test]
    fn empty_workspace_body_keeps_start_and_reveals_pending() {
        let (state, _) = shell(8, 1);
        let mut scroll = SidebarScroll::new();
        scroll.scroll_workspaces_to(3);
        scroll.reveal_selected_workspace();
        let inputs = inputs(&state, None);

        // The expanded body is empty at height 2 (header and footer take the rows); the
        // collapsed list has no chrome, so only a zero-height area empties it.
        for (form, height) in [(SidebarForm::Expanded, 2), (SidebarForm::Collapsed, 0)] {
            let (view, resolution) =
                resolve_sidebar(Rect::new(0, 0, 30, height), form, &inputs, &scroll, false);
            assert_eq!(resolution.workspaces, None, "{form:?}");
            assert!(!resolution.workspace_reveal_consumed, "{form:?}");
            assert!(drawn_workspaces(&view).is_empty(), "{form:?}");
        }
        assert_eq!(scroll.workspace_start(), 3);
        assert!(scroll.workspace_reveal().selected_pending());
        assert!(scroll.workspace_reveal().focused_pending());
    }

    #[test]
    fn empty_agent_body_keeps_start_and_reveal_pending() {
        let (state, _) = shell(2, 2);
        let mut scroll = SidebarScroll::new();
        scroll.scroll_agents_to(5);
        scroll.reveal_agent(Location::pane(
            ClientEndpointId::Local,
            crate::tests::test_pane_id("w1:p1"),
        ));
        let inputs = inputs(&state, None);

        // The agent section is two rows tall here: a header fits, its body does not.
        let (view, resolution) = resolve_sidebar(
            Rect::new(0, 0, 30, 6),
            SidebarForm::Expanded,
            &inputs,
            &scroll,
            false,
        );

        let SidebarView::Expanded(view) = view else {
            panic!("expanded sidebar expected");
        };
        let list = view
            .agents
            .as_ref()
            .and_then(|panel| panel.list.as_ref())
            .expect("the header fits, so the list exists");
        assert!(list.body.is_empty());
        assert_eq!(resolution.agents, None);
        assert!(!resolution.agent_reveal_consumed);
        assert_eq!(scroll.agent_start(), 5);
        assert!(scroll.agent_reveal().is_some());
    }
}
