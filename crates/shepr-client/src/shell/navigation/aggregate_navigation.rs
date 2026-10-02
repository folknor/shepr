//! Endpoint-qualified rows shared by aggregate navigation surfaces.

use crate::endpoint::{ClientEndpointId, ClientEndpointStatus};
use crate::shell::endpoints::ClientShellEndpoint;
use crate::shell::state::{
    ClientNavigatorFilter, ClientNavigatorOverlay, ClientNavigatorRow, ClientNavigatorTarget,
    ClientShellConfig,
};
use std::collections::HashMap;

use crate::shell::presentation::status::status_priority;

pub(in crate::shell) struct AggregateAgentTarget {
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
}

pub(in crate::shell) struct AgentPanelRow {
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    pub(in crate::shell) machine_label: String,
    pub(in crate::shell) stale: bool,
    pub(in crate::shell) agent: crate::shell::sidebar::agent_sidebar::AgentRow,
    endpoint_order: usize,
    recency: u64,
}

pub(in crate::shell) struct AgentPanelModel {
    pub(in crate::shell) rows: Vec<AgentPanelRow>,
    targets: Vec<AggregateAgentTarget>,
}

impl AgentPanelModel {
    // Sidebar rows and keyboard targets share the same workspace filter and display order.
    // Token templates are prepared at refresh; painting only lays out these stored rows.
    pub(in crate::shell) fn build(
        endpoints: &[ClientShellEndpoint],
        config: &ClientShellConfig,
    ) -> Self {
        let mut rows = Vec::new();
        for (endpoint_order, endpoint) in endpoints.iter().enumerate() {
            let Some(snapshot) = endpoint.snapshot.as_deref() else {
                continue;
            };
            let machine = (endpoints.len() > 1).then(|| endpoint.endpoint_id.display_label());
            rows.extend(
                crate::shell::sidebar::agent_sidebar::agent_rows(snapshot, config, machine)
                    .into_iter()
                    .map(|agent| AgentPanelRow {
                        recency: endpoint
                            .agent_recency
                            .get(&agent.pane_id)
                            .copied()
                            .unwrap_or_default(),
                        endpoint_order,
                        endpoint_id: endpoint.endpoint_id.clone(),
                        machine_label: endpoint.endpoint_id.display_label().to_owned(),
                        stale: endpoint.status != ClientEndpointStatus::Online,
                        agent,
                    }),
            );
        }
        if config.agent_panel_sort == shepr_config::AgentPanelSortConfig::Priority {
            rows.sort_by_key(|row| {
                (
                    row.stale,
                    std::cmp::Reverse(status_priority(row.agent.status)),
                    std::cmp::Reverse(row.recency),
                    row.endpoint_order,
                    std::cmp::Reverse(row.agent.state_change_seq),
                )
            });
        }
        let targets = rows
            .iter()
            .map(|row| AggregateAgentTarget {
                endpoint_id: row.endpoint_id.clone(),
                pane_id: row.agent.pane_id.clone(),
            })
            .collect();
        Self { rows, targets }
    }

    pub(in crate::shell) fn targets(&self) -> &[AggregateAgentTarget] {
        &self.targets
    }
}

pub(in crate::shell) fn cycle_index(
    length: usize,
    current: Option<usize>,
    delta: isize,
) -> Option<usize> {
    let length = isize::try_from(length).ok().filter(|length| *length > 0)?;
    let length_usize = usize::try_from(length).ok()?;
    let next = match current.filter(|index| *index < length_usize) {
        Some(index) => {
            let index = isize::try_from(index).ok()?;
            let step = delta.rem_euclid(length);
            let wrap_at = length - step;
            if index >= wrap_at {
                index - wrap_at
            } else {
                index + step
            }
        }
        None if delta < 0 => length - 1,
        None => 0,
    };
    usize::try_from(next).ok()
}

pub(in crate::shell) fn agent_target_index(
    targets: &[AggregateAgentTarget],
    active_endpoint_id: &ClientEndpointId,
    focused_pane_id: Option<&str>,
    action: shepr_termio::input::KeybindAction,
) -> Option<usize> {
    use shepr_termio::input::KeybindAction;

    match action {
        KeybindAction::FocusAgent(index) => (index < targets.len()).then_some(index),
        KeybindAction::PreviousAgent | KeybindAction::NextAgent => {
            let current = targets.iter().position(|target| {
                &target.endpoint_id == active_endpoint_id
                    && Some(target.pane_id.as_str()) == focused_pane_id
            });
            let delta = if action == KeybindAction::PreviousAgent {
                -1
            } else {
                1
            };
            cycle_index(targets.len(), current, delta)
        }
        _ => None,
    }
}

pub(in crate::shell) struct NavigatorIndex {
    federated: bool,
    endpoints: Vec<NavigatorEndpoint>,
}

struct NavigatorEndpoint {
    endpoint_id: ClientEndpointId,
    label: String,
    search_label: String,
    status: ClientEndpointStatus,
    focused_pane_id: Option<shepr_protocol::PublicPaneId>,
    workspaces: Vec<NavigatorWorkspace>,
}

struct NavigatorWorkspace {
    row: ClientNavigatorRow,
    search_fields: Vec<String>,
    panes: Vec<NavigatorPane>,
}

struct NavigatorPane {
    row: ClientNavigatorRow,
    search_fields: Vec<String>,
}

impl NavigatorIndex {
    // Snapshot text is normalized here so key and wheel events only normalize the query.
    pub(in crate::shell) fn build(endpoints: &[ClientShellEndpoint]) -> Self {
        let federated = endpoints.len() > 1;
        let mut indexed_endpoints = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let mut indexed = NavigatorEndpoint {
                endpoint_id: endpoint.endpoint_id.clone(),
                label: endpoint.endpoint_id.display_label().to_owned(),
                search_label: endpoint.endpoint_id.display_label().to_lowercase(),
                status: endpoint.status,
                focused_pane_id: None,
                workspaces: Vec::new(),
            };
            let Some(snapshot) = endpoint.snapshot.as_deref() else {
                indexed_endpoints.push(indexed);
                continue;
            };
            indexed.focused_pane_id = snapshot.focused_pane_id.clone();
            let agents = snapshot
                .agents
                .iter()
                .map(|agent| (agent.pane_id.as_str(), agent))
                .collect::<HashMap<_, _>>();
            let mut panes_by_workspace = HashMap::new();
            for pane in &snapshot.panes {
                panes_by_workspace
                    .entry(pane.workspace_id.as_str())
                    .or_insert_with(Vec::new)
                    .push(pane);
            }
            indexed.workspaces.reserve(snapshot.workspaces.len());
            for workspace in &snapshot.workspaces {
                let workspace_panes = panes_by_workspace
                    .get(workspace.workspace_id.as_str())
                    .map_or_default(Vec::as_slice);
                let mut panes = Vec::with_capacity(workspace_panes.len());
                for (index, pane) in workspace_panes.iter().enumerate() {
                    let agent = agents.get(pane.pane_id.as_str()).copied();
                    let status = agent.map_or(shepr_protocol::AgentStatus::Idle, |agent| {
                        agent.agent_status
                    });
                    let agent_kind = agent.and_then(|agent| agent.agent.as_deref());
                    let title = agent.and_then(|agent| agent.terminal_title_stripped.as_deref());
                    let meta = pane
                        .foreground_cwd
                        .as_deref()
                        .or(pane.cwd.as_deref())
                        .unwrap_or_default();
                    let label = if workspace_panes.len() == 1 {
                        pane.label
                            .as_deref()
                            .or(title)
                            .unwrap_or(workspace.label.as_str())
                            .to_owned()
                    } else {
                        let pane_name = pane
                            .label
                            .as_deref()
                            .or(title)
                            .or(agent_kind)
                            .unwrap_or("terminal");
                        format!("{pane_name} · {}", index + 1)
                    };
                    let row = ClientNavigatorRow {
                        depth: 1 + u8::from(federated),
                        label: label.clone(),
                        meta: meta.to_owned(),
                        detail: format!("{} / {}", workspace.label, pane.pane_id),
                        agent: agent_kind.map(str::to_owned),
                        status: Some(status),
                        stale: false,
                        current: false,
                        target: ClientNavigatorTarget::Pane {
                            endpoint_id: endpoint.endpoint_id.clone(),
                            pane_id: pane.pane_id.clone(),
                        },
                    };
                    let mut search_fields = vec![
                        label.to_lowercase(),
                        meta.to_lowercase(),
                        pane.pane_id.as_str().to_lowercase(),
                    ];
                    if let Some(cwd) = pane.cwd.as_deref() {
                        search_fields.push(cwd.to_lowercase());
                    }
                    if let Some(agent_kind) = agent_kind {
                        search_fields.push(agent_kind.to_lowercase());
                    }
                    if let Some(title) = title {
                        search_fields.push(title.to_lowercase());
                    }
                    panes.push(NavigatorPane { row, search_fields });
                }
                let mut search_fields = vec![workspace.label.to_lowercase()];
                if let Some(branch) = workspace.branch.as_deref() {
                    search_fields.push(branch.to_lowercase());
                }
                indexed.workspaces.push(NavigatorWorkspace {
                    row: ClientNavigatorRow {
                        depth: u8::from(federated),
                        label: workspace.label.clone(),
                        meta: workspace.branch.clone().unwrap_or_default(),
                        detail: workspace.new_workspace_cwd.clone(),
                        agent: None,
                        status: None,
                        stale: false,
                        current: false,
                        target: ClientNavigatorTarget::Workspace {
                            endpoint_id: endpoint.endpoint_id.clone(),
                            workspace_id: workspace.workspace_id.clone(),
                        },
                    },
                    search_fields,
                    panes,
                });
            }
            indexed_endpoints.push(indexed);
        }
        Self {
            federated,
            endpoints: indexed_endpoints,
        }
    }

    pub(in crate::shell) fn endpoint_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientEndpointStatus> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.status)
    }

    pub(in crate::shell) fn rows(
        &self,
        active_endpoint_id: &ClientEndpointId,
        navigator: &ClientNavigatorOverlay,
    ) -> Vec<ClientNavigatorRow> {
        let query = navigator.query.trim().to_lowercase();
        let words = query.split_whitespace().collect::<Vec<_>>();
        let filtering = navigator.filter.is_some() || !query.is_empty();
        let mut rows = Vec::new();
        for endpoint in &self.endpoints {
            let stale = endpoint.status != ClientEndpointStatus::Online;
            let endpoint_matches = !query.is_empty()
                && search_matches(std::slice::from_ref(&endpoint.search_label), &words);
            let mut endpoint_rows = Vec::new();
            for workspace in &endpoint.workspaces {
                let workspace_matches =
                    endpoint_matches || search_matches(&workspace.search_fields, &words);
                let mut children = Vec::new();
                for pane in &workspace.panes {
                    let status = pane.row.status.unwrap_or(shepr_protocol::AgentStatus::Idle);
                    if navigator
                        .filter
                        .is_some_and(|filter| !filter_status(filter, status))
                        || !(workspace_matches || search_matches(&pane.search_fields, &words))
                    {
                        continue;
                    }
                    let mut row = pane.row.clone();
                    row.stale = stale;
                    row.current = &endpoint.endpoint_id == active_endpoint_id
                        && endpoint.focused_pane_id.as_ref().is_some_and(|focused| {
                            matches!(&row.target, ClientNavigatorTarget::Pane { pane_id, .. } if pane_id == focused)
                        });
                    children.push(row);
                }
                if !filtering
                    || !children.is_empty()
                    || (navigator.filter.is_none() && !query.is_empty() && workspace_matches)
                {
                    let mut row = workspace.row.clone();
                    row.stale = stale;
                    endpoint_rows.push(row);
                    endpoint_rows.extend(children);
                }
            }
            if !filtering || endpoint_matches || !endpoint_rows.is_empty() {
                if self.federated {
                    rows.push(ClientNavigatorRow {
                        depth: 0,
                        label: endpoint.label.clone(),
                        meta: String::new(),
                        detail: String::new(),
                        agent: None,
                        status: None,
                        stale,
                        current: false,
                        target: ClientNavigatorTarget::Machine {
                            endpoint_id: endpoint.endpoint_id.clone(),
                        },
                    });
                }
                rows.extend(endpoint_rows);
            }
        }
        rows
    }
}

fn search_matches(fields: &[String], words: &[&str]) -> bool {
    words.is_empty()
        || fields
            .iter()
            .any(|field| words.iter().all(|word| field.contains(word)))
}

fn filter_status(filter: ClientNavigatorFilter, status: shepr_protocol::AgentStatus) -> bool {
    match filter {
        ClientNavigatorFilter::Blocked => status == shepr_protocol::AgentStatus::Blocked,
        ClientNavigatorFilter::Working => status == shepr_protocol::AgentStatus::Working,
        ClientNavigatorFilter::Idle => status == shepr_protocol::AgentStatus::Idle,
    }
}

pub(in crate::shell) fn navigator_selected_index(
    rows: &[ClientNavigatorRow],
    navigator: &ClientNavigatorOverlay,
) -> Option<usize> {
    match navigator.selected.as_ref() {
        Some(target) => rows
            .iter()
            .position(|row| row.target == *target)
            // Snapshot changes can remove the selected target while the navigator stays open.
            // Keep Enter and rendering on the same visible row in that case.
            .or_else(|| (!rows.is_empty()).then_some(0)),
        None => rows
            .iter()
            .position(|row| matches!(row.target, ClientNavigatorTarget::Pane { .. }))
            .or_else(|| (!rows.is_empty()).then_some(0)),
    }
}

pub(in crate::shell) fn selected_navigator_target(
    rows: &[ClientNavigatorRow],
    navigator: &ClientNavigatorOverlay,
) -> Option<ClientNavigatorTarget> {
    navigator_selected_index(rows, navigator).map(|index| rows[index].target.clone())
}

#[cfg(test)]
pub(in crate::shell) fn navigator_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    navigator: &ClientNavigatorOverlay,
) -> Vec<ClientNavigatorRow> {
    NavigatorIndex::build(endpoints).rows(active_endpoint_id, navigator)
}
