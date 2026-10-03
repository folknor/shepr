use crate::endpoint::{ClientEndpointId, ClientEndpointStatus, EndpointFailureStatus};
use crate::shell::ledger::DropReason;
use crate::shell::navigation::location::Location;
use crate::shell::presentation::surfaces::PaneSurfaces;
use crate::shell::state::ClientShellState;
use shepr_protocol::ClientShellSnapshot;
use std::sync::Arc;

use shepr_config::theme::Palette;
use std::collections::HashMap;

use ratatui::layout::Rect;

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) state: EndpointState,
    pub(crate) agent_recency: HashMap<shepr_protocol::PublicPaneId, u64>,
}

/// Owns endpoint selection and the endpoint presentations read by the shell.
#[derive(Debug)]
pub(crate) struct Endpoints {
    pub(crate) choice: crate::endpoint::EndpointChoice,
    entries: Vec<ClientShellEndpoint>,
}

impl Endpoints {
    pub(crate) fn presented(&self) -> &ClientEndpointId {
        self.choice.presented()
    }

    pub(crate) fn new(entries: Vec<ClientShellEndpoint>) -> Self {
        Self {
            choice: crate::endpoint::EndpointChoice::showing(ClientEndpointId::Local),
            entries,
        }
    }
}

impl std::ops::Deref for Endpoints {
    type Target = Vec<ClientShellEndpoint>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl std::ops::DerefMut for Endpoints {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EndpointSnapshot {
    snapshot: Arc<ClientShellSnapshot>,
    generation: u64,
}

/// A live presentation always has a snapshot. A disconnected presentation keeps its
/// last snapshot for display, but cannot supply navigation or pane commands.
#[derive(Clone, Debug)]
pub(crate) enum EndpointState {
    Connecting {
        last: Option<EndpointSnapshot>,
        connected: bool,
        generation: Option<u64>,
    },
    Online(EndpointSnapshot),
    Stale {
        last: Option<EndpointSnapshot>,
    },
    Attention {
        last: Option<EndpointSnapshot>,
    },
}

impl EndpointState {
    fn last(&self) -> Option<&EndpointSnapshot> {
        match self {
            Self::Online(snapshot) => Some(snapshot),
            Self::Connecting { last, .. } | Self::Stale { last } | Self::Attention { last } => {
                last.as_ref()
            }
        }
    }

    pub(crate) fn usable(&self) -> bool {
        matches!(self, Self::Online(_))
    }

    pub(crate) fn stale(&self) -> bool {
        !self.usable()
    }

    pub(crate) fn status(&self) -> ClientEndpointStatus {
        match self {
            Self::Connecting { .. } => ClientEndpointStatus::Connecting,
            Self::Online(_) => ClientEndpointStatus::Online,
            Self::Stale { .. } => ClientEndpointStatus::Reconnecting,
            Self::Attention { .. } => ClientEndpointStatus::Attention,
        }
    }

    /// A failure keeps the last snapshot for display only. There is deliberately no way to
    /// set Online here, not even for test fixtures: a status carries no connection
    /// generation, so promoting a retained snapshot would present one from a connection that
    /// is gone. Online is reached only through `endpoint_connected` and that generation's
    /// own snapshot, and tests arrange it the same way.
    fn set_failure(&mut self, status: EndpointFailureStatus) {
        let last = self.last().cloned();
        *self = match status {
            EndpointFailureStatus::Reconnecting => Self::Stale { last },
            EndpointFailureStatus::Attention => Self::Attention { last },
        };
    }

    fn cache(&mut self, snapshot: EndpointSnapshot) {
        let last = Some(snapshot.clone());
        *self = match self {
            Self::Online(_) => Self::Online(snapshot),
            Self::Connecting {
                connected: true,
                generation,
                ..
            } if *generation == Some(snapshot.generation) => Self::Online(snapshot),
            Self::Connecting {
                connected,
                generation,
                ..
            } => Self::Connecting {
                last,
                connected: *connected,
                generation: *generation,
            },
            Self::Stale { .. } => Self::Stale { last },
            Self::Attention { .. } => Self::Attention { last },
        };
    }
}

impl ClientShellEndpoint {
    pub(crate) fn snapshot(&self) -> Option<&ClientShellSnapshot> {
        self.state.last().map(|last| last.snapshot.as_ref())
    }

    fn shared_snapshot(&self) -> Option<Arc<ClientShellSnapshot>> {
        self.state.last().map(|last| Arc::clone(&last.snapshot))
    }

    pub(crate) fn snapshot_generation(&self) -> Option<u64> {
        self.state.last().map(|last| last.generation)
    }
}

pub(in crate::shell) struct MachineHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) status_badge: Rect,
    pub(in crate::shell) collapse_toggle: Rect,
    pub(in crate::shell) location: Location,
}

/// A focus operation after endpoint selection has supplied its endpoint context. Shell
/// destinations themselves use `Location`, which always carries that endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientEndpointFocusTarget {
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

impl ClientShellState {
    /// Sets the configured machines, once at launch: Local first, then one endpoint per
    /// `[[machines]]` entry, each Connecting with no snapshot. The set never changes
    /// while the client runs.
    pub fn set_machines(&mut self, machines: &[shepr_config::MachineConfig]) {
        let mut next = Vec::with_capacity(machines.len().saturating_add(1));
        let local = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .cloned()
            .unwrap_or_else(local_endpoint);
        next.push(local);
        for machine in machines {
            next.push(ClientShellEndpoint {
                endpoint_id: ClientEndpointId::Ssh(machine.label.clone()),
                state: EndpointState::Connecting {
                    last: None,
                    connected: false,
                    generation: None,
                },
                agent_recency: HashMap::new(),
            });
        }
        self.endpoints.entries = next;
        self.rebuild_endpoint_models();
    }

    /// A handshake starts a new presentation generation. Until its own snapshot arrives,
    /// the previous generation is retained only as stale display data.
    pub fn endpoint_connected(&mut self, endpoint_id: &ClientEndpointId, generation: u64) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            let last = endpoint.state.last().cloned();
            endpoint.state = EndpointState::Connecting {
                last,
                connected: true,
                generation: Some(generation),
            };
        }
        self.clear_machine_diagnostic(endpoint_id);
        self.rebuild_endpoint_models();
    }

    /// Records a status the supervisor reported for an endpoint with no live
    /// connection (a failed or pending attempt). The selection is untouched: a
    /// move waiting for Local's reconnect keeps waiting through its failed
    /// attempts.
    pub fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) {
        self.apply_endpoint_status(endpoint_id, status);
    }

    /// A live connection was lost: the selection learns of the loss, the endpoint
    /// takes its failure status, and the requests in flight to the presented endpoint
    /// are interrupted. A status report for an endpoint with no live connection
    /// leaves them alone.
    pub(crate) fn transition_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) -> crate::endpoint::Lost {
        let lost = self.endpoints.choice.connection_lost(endpoint_id);
        self.apply_endpoint_status(endpoint_id, status);
        if self.endpoint_is_active(endpoint_id) {
            self.drop_all_requests(DropReason::Interrupted);
        }
        lost
    }

    fn apply_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) {
        if endpoint_id == self.endpoints.presented() {
            self.pending_workspace_highlight = None;
        }
        let mut changed = false;
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            let previous = endpoint.state.status();
            endpoint.state.set_failure(status);
            changed = previous != endpoint.state.status();
        }
        if changed {
            self.rebuild_endpoint_models();
        }
    }

    /// The endpoint selection, for a caller that drives a move end to end.
    pub fn endpoint_choice(&self) -> &crate::endpoint::EndpointChoice {
        &self.endpoints.choice
    }

    /// The endpoint selection, mutably, for a caller that drives a move end to end.
    pub fn endpoint_choice_mut(&mut self) -> &mut crate::endpoint::EndpointChoice {
        &mut self.endpoints.choice
    }

    pub(crate) fn endpoint_usable(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .any(|endpoint| &endpoint.endpoint_id == endpoint_id && endpoint.state.usable())
    }

    pub(crate) fn activate_endpoint_projection(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        if !endpoint.state.usable() {
            return false;
        }
        let Some(snapshot) = endpoint.shared_snapshot() else {
            return false;
        };
        let Some(generation) = endpoint.snapshot_generation() else {
            return false;
        };
        let agent_body_height = self.hits.agent_body.height;
        let switching_endpoint = endpoint_id != self.endpoints.presented();
        let agent_scroll = self.agent_scroll;
        match self.endpoints.choice.preparing() {
            Some(preparing) if &preparing.lease().endpoint_id == endpoint_id => {
                if self.endpoints.choice.commit().is_none() {
                    return false;
                }
            }
            None if self.endpoints.choice.pending_start().is_none() => {
                self.endpoints.choice =
                    crate::endpoint::EndpointChoice::showing(endpoint_id.clone());
            }
            Some(_) | None => return false,
        }
        let pending_agent_reveal = self
            .pending_agent_reveal
            .take_if(|target| &target.endpoint == endpoint_id);
        if switching_endpoint {
            self.surfaces = PaneSurfaces::default();
        }
        self.apply_active_snapshot(snapshot, generation);
        if switching_endpoint {
            // The aggregate agent list belongs to the client, not one endpoint.
            self.agent_scroll = agent_scroll;
        }
        if let Some(target) = pending_agent_reveal
            && let Some(pane_id) = target.pane_id()
        {
            self.reveal_endpoint_agent(&target.endpoint, &pane_id, agent_body_height);
        }
        true
    }

    pub(crate) fn endpoint_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientEndpointStatus> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.state.status())
    }

    /// Local can be selected while unavailable so its reconnect can complete the pick.
    pub(in crate::shell) fn endpoint_can_select(&self, endpoint_id: &ClientEndpointId) -> bool {
        endpoint_id.is_local() || self.endpoint_usable(endpoint_id)
    }

    pub(crate) fn endpoint_boot_id(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&shepr_protocol::BootId> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot()
            .map(|snapshot| &snapshot.boot_id)
    }

    pub(crate) fn endpoint_snapshot_matches(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &shepr_protocol::BootId,
        revision: shepr_protocol::ProjectionRevision,
    ) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| {
                endpoint
                    .snapshot_generation()
                    .is_some_and(|snapshot_generation| snapshot_generation == generation)
                    && endpoint.snapshot().is_some_and(|snapshot| {
                        snapshot.boot_id == *boot_id && snapshot.revision == revision
                    })
            })
    }

    pub(crate) fn endpoint_snapshot_identity(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
    ) -> Option<(&shepr_protocol::BootId, shepr_protocol::ProjectionRevision)> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
        if endpoint.snapshot_generation() != Some(generation) {
            return None;
        }
        endpoint
            .snapshot()
            .map(|snapshot| (&snapshot.boot_id, snapshot.revision))
    }

    /// A terminal normally starts focused. `None` means this host cannot report focus events,
    /// not that the endpoint has no viewer; a move's commit therefore sends an explicit true
    /// baseline.
    pub(crate) fn host_focus_baseline(&self) -> bool {
        self.outer_focused.unwrap_or(true)
    }

    pub(crate) fn active_endpoint_label(&self) -> &str {
        self.endpoints.presented().display_label()
    }

    pub fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.presented() == endpoint_id
    }

    pub(crate) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    pub(crate) fn cache_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: impl Into<Arc<ClientShellSnapshot>>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, generation, snapshot.into());
    }

    fn cache_endpoint_snapshot_at_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Arc<ClientShellSnapshot>,
    ) {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if self.endpoints[index].snapshot_generation() == Some(generation)
            && self.endpoints[index].snapshot().is_some_and(|previous| {
                previous.boot_id == snapshot.boot_id && previous.revision > snapshot.revision
            })
        {
            return;
        }
        let previous = self.endpoints[index].snapshot();
        let previous_sequences = previous
            .into_iter()
            .flat_map(|snapshot| snapshot.agents.iter())
            .map(|agent| (agent.pane_id, agent.state_change_seq))
            .collect::<HashMap<_, _>>();
        let mut next_recency = self
            .endpoints
            .iter()
            .flat_map(|endpoint| endpoint.agent_recency.values())
            .copied()
            .max()
            .unwrap_or_default();
        let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
        agents.sort_by_key(|agent| agent.state_change_seq);
        let mut recency = self.endpoints[index].agent_recency.clone();
        for agent in agents {
            let changed = previous_sequences
                .get(&agent.pane_id)
                .is_none_or(|previous_seq| *previous_seq != agent.state_change_seq);
            if changed {
                next_recency = next_recency.saturating_add(1);
                recency.insert(agent.pane_id, next_recency);
            }
        }
        let live_agent_ids = snapshot
            .agents
            .iter()
            .map(|agent| &agent.pane_id)
            .collect::<std::collections::HashSet<_>>();
        recency.retain(|pane_id, _| live_agent_ids.contains(pane_id));
        let endpoint = &mut self.endpoints[index];
        endpoint.agent_recency = recency;
        endpoint.state.cache(EndpointSnapshot {
            generation,
            snapshot,
        });
        self.rebuild_endpoint_models();
    }

    pub fn set_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    fn apply_cached_endpoint_snapshot(&mut self, endpoint_id: &ClientEndpointId) {
        let Some((snapshot, generation)) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint
                    .shared_snapshot()
                    .zip(endpoint.snapshot_generation())
            })
        else {
            return;
        };
        if endpoint_id == self.endpoints.presented() {
            self.apply_active_snapshot(snapshot, generation);
        }
    }

    pub(in crate::shell) fn rebuild_agent_panel_model(&mut self) {
        self.agent_panel_model =
            crate::shell::navigation::aggregate_navigation::AgentPanelModel::build(
                &self.endpoints,
                &self.config,
            );
    }

    fn rebuild_endpoint_models(&mut self) {
        self.rebuild_agent_panel_model();
        self.navigator_index =
            crate::shell::navigation::aggregate_navigation::NavigatorIndex::build(&self.endpoints);
    }
}

pub(in crate::shell) fn endpoint_status_presentation(
    status: ClientEndpointStatus,
    palette: &Palette,
) -> (&'static str, &'static str, ratatui::style::Color) {
    match status {
        ClientEndpointStatus::Connecting => ("◐", "connecting", palette.yellow),
        ClientEndpointStatus::Online => ("●", "online", palette.green),
        ClientEndpointStatus::Reconnecting => ("◐", "reconnecting", palette.yellow),
        ClientEndpointStatus::Attention => ("!", "attention", palette.red),
    }
}

pub(in crate::shell) fn local_endpoint() -> ClientShellEndpoint {
    ClientShellEndpoint {
        endpoint_id: ClientEndpointId::Local,
        state: EndpointState::Connecting {
            last: None,
            connected: false,
            generation: None,
        },
        agent_recency: HashMap::new(),
    }
}

#[cfg(test)]
impl ClientShellState {
    pub fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        let endpoint_id = self.endpoints.presented().clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    pub(crate) fn cache_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: impl Into<Arc<ClientShellSnapshot>>,
    ) {
        let generation = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint.snapshot_generation().or(match &endpoint.state {
                    EndpointState::Connecting { generation, .. } => *generation,
                    _ => None,
                })
            })
            .unwrap_or(1);
        if self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id
                && endpoint.endpoint_id.is_local()
                && matches!(
                    endpoint.state,
                    EndpointState::Connecting {
                        generation: None,
                        ..
                    }
                )
        }) {
            self.endpoint_connected(endpoint_id, generation);
        }
        self.cache_endpoint_snapshot_at_generation(endpoint_id, generation, snapshot.into());
    }

    pub fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }
}

#[cfg(test)]
impl ClientShellEndpoint {
    pub(crate) fn snapshot_mut(&mut self) -> Option<&mut Arc<ClientShellSnapshot>> {
        match &mut self.state {
            EndpointState::Online(last) => Some(&mut last.snapshot),
            EndpointState::Connecting { last, .. }
            | EndpointState::Stale { last }
            | EndpointState::Attention { last } => last.as_mut().map(|last| &mut last.snapshot),
        }
    }
}

#[cfg(test)]
impl ClientShellState {
    pub(crate) fn mark_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_status(endpoint_id, EndpointFailureStatus::Reconnecting);
        if self.endpoint_is_active(endpoint_id) {
            self.drop_all_requests(DropReason::Interrupted);
        }
    }

    /// Brings an endpoint online the way a connection does: the handshake opens
    /// `generation`, and that generation's snapshot makes the endpoint usable.
    pub(crate) fn connect_endpoint_with_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.endpoint_connected(endpoint_id, generation);
        self.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    }

    pub(crate) fn endpoint_has_snapshot(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| endpoint.snapshot().is_some())
    }

    pub(crate) fn active_endpoint_id(&self) -> &ClientEndpointId {
        self.endpoints.presented()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(generation: u64) -> EndpointSnapshot {
        EndpointSnapshot {
            snapshot: Arc::new(crate::shell::tests::snapshot()),
            generation,
        }
    }

    #[test]
    fn online_requires_a_connection_and_its_snapshot() {
        let mut shell = ClientShellState::new(crate::shell::state::ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        let local = ClientEndpointId::Local;
        shell.set_endpoint_status(&local, EndpointFailureStatus::Reconnecting);
        shell.cache_endpoint_snapshot_for_generation(
            &local,
            1,
            Arc::new(crate::shell::tests::snapshot()),
        );
        // A snapshot without a live connection is display data only.
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Reconnecting)
        );
        assert!(!shell.endpoint_usable(&local));

        shell.endpoint_connected(&local, 2);
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Connecting)
        );
        assert!(!shell.endpoint_usable(&local));
        shell.cache_endpoint_snapshot_for_generation(
            &local,
            2,
            Arc::new(crate::shell::tests::snapshot()),
        );
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Online)
        );
        assert!(shell.endpoint_usable(&local));
    }

    #[test]
    fn a_reconnect_requires_its_own_generation_snapshot() {
        let mut state = EndpointState::Connecting {
            last: Some(snapshot(1)),
            connected: true,
            generation: Some(2),
        };
        assert!(state.stale());
        state.cache(snapshot(1));
        assert!(state.stale());
        state.cache(snapshot(2));
        assert!(state.usable());
    }

    #[test]
    fn caching_cannot_clear_a_failure_or_make_its_last_snapshot_usable() {
        for status in [
            EndpointFailureStatus::Reconnecting,
            EndpointFailureStatus::Attention,
        ] {
            let mut state = EndpointState::Online(snapshot(1));
            state.set_failure(status);
            state.cache(snapshot(2));
            assert_eq!(state.status(), ClientEndpointStatus::from(status));
            assert!(state.stale());
            assert_eq!(state.last().expect("retained snapshot").generation, 2);
        }
    }
}
