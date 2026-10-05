use crate::endpoint::{
    ClientEndpointBootKey, ClientEndpointId, ClientEndpointStatus, EndpointFailureStatus,
};
use crate::shell::config::ClientShellConfig;
use crate::shell::ledger::DropReason;
use crate::shell::navigation::aggregate_navigation::{AgentPanelModel, NavigatorIndex};
use crate::shell::presentation::surfaces::PaneSurfaces;
use crate::shell::state::ClientShellState;
use shepr_protocol::{ClientShellSnapshot, ConnectionGeneration};
use std::sync::Arc;

use crate::shell::palette::Palette;
use std::collections::HashMap;

static OPERATOR_ENTRYPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn operator_entrypoint() -> &'static str {
    OPERATOR_ENTRYPOINT.get_or_init(shepr_launch::guidance::operator_entrypoint)
}

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    pub(in crate::shell) state: EndpointState,
    pub(in crate::shell) agent_recency: HashMap<shepr_protocol::PublicPaneId, u64>,
    /// What a configured machine's entry says while it is not connected. Unused
    /// for the Local endpoint, which has no entry.
    pub(in crate::shell) machine: MachineState,
    /// The ssh command that reaches a configured machine by hand, for the entry
    /// of a machine that needs a login; `None` for the Local endpoint.
    pub(in crate::shell) ssh_check: Option<String>,
}

/// What a configured machine's sidebar entry says while the machine is not
/// connected. A failed attempt sets it from what the failure says
/// ([`MachineState::after_failure`]); the operator's Connect and Restart set the
/// state their attempt is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MachineState {
    /// Being reached: the first attempt, a retry after a transient failure, or
    /// a connection waiting for its first snapshot.
    Connecting,
    /// Reachable, with no server running and none started: the entry offers
    /// Connect.
    NotRunning,
    /// Its server is starting, or the operator's Connect is starting it.
    Starting,
    /// Its server is stopping.
    Stopping,
    /// Its server is another shepr build: the entry offers Restart.
    DifferentBuild,
    /// The operator's Restart is stopping that server and starting this
    /// build's.
    Restarting,
    /// The machine did not answer (host down, network).
    Offline,
    /// SSH refused the client's credentials: someone has to log in.
    NeedsLogin,
    /// The machine answered and cannot be used until it is repaired there.
    Unavailable,
}

/// What activating a machine's entry, or its machine row, does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum MachineAction {
    /// Start the machine's server and attach.
    Connect,
    /// Ask, then stop the server of another build and start this build's.
    Restart,
}

impl MachineState {
    /// The state a failed attempt, a failed wait or a lost connection leaves,
    /// from the failure's typed cause and the operator action it needs.
    pub(crate) fn after_failure(failure: &shepr_launch::EndpointFailure) -> Self {
        use shepr_launch::{FailureCause, FailureDisposition};
        match failure.cause() {
            FailureCause::NoServer => Self::NotRunning,
            FailureCause::ServerStarting => Self::Starting,
            FailureCause::ServerStopping | FailureCause::Shutdown(_) => Self::Stopping,
            FailureCause::DifferentBuild => Self::DifferentBuild,
            _ => match failure.disposition() {
                FailureDisposition::Authentication => Self::NeedsLogin,
                // Only startup can test an ambiguous timeout interactively.
                // At runtime a missing round trip is evidence of an offline host.
                FailureDisposition::PossibleAuthentication | FailureDisposition::Offline => {
                    Self::Offline
                }
                FailureDisposition::Retry => Self::Connecting,
                FailureDisposition::HostKey
                | FailureDisposition::Incompatible
                | FailureDisposition::Repair => Self::Unavailable,
            },
        }
    }

    pub(in crate::shell) fn action(self) -> Option<MachineAction> {
        match self {
            Self::NotRunning => Some(MachineAction::Connect),
            Self::DifferentBuild => Some(MachineAction::Restart),
            Self::Connecting
            | Self::Starting
            | Self::Stopping
            | Self::Restarting
            | Self::Offline
            | Self::NeedsLogin
            | Self::Unavailable => None,
        }
    }

    /// The entry's text.
    pub(in crate::shell) fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting...",
            Self::NotRunning => "Connect",
            Self::Starting => "Starting...",
            Self::Stopping => "Stopping...",
            Self::DifferentBuild => "Restart (other build)",
            Self::Restarting => "Restarting...",
            Self::Offline => "Offline",
            Self::NeedsLogin => "Needs SSH login",
            Self::Unavailable => "Unavailable",
        }
    }

    /// The one-cell glyph the machine row shows for this state, and its colour.
    pub(in crate::shell) fn glyph(
        self,
        palette: &Palette,
    ) -> (&'static str, ratatui::style::Color) {
        match self {
            Self::Connecting | Self::Starting | Self::Stopping | Self::Restarting => {
                ("◐", palette.yellow)
            }
            Self::NotRunning => ("○", palette.accent),
            Self::DifferentBuild => ("↻", palette.accent),
            Self::Offline => ("×", palette.overlay0),
            Self::NeedsLogin | Self::Unavailable => ("!", palette.red),
        }
    }
}

/// The entry a configured machine shows in place of its workspaces while it is
/// not connected.
pub(in crate::shell) struct MachineEntry {
    pub(in crate::shell) state: MachineState,
    /// A second line, for the entry of a machine that needs a login: how to
    /// give one.
    pub(in crate::shell) hint: Option<String>,
}

/// Owns endpoint selection and the endpoint presentations read by the shell.
pub(crate) struct Endpoints {
    pub(crate) choice: crate::endpoint::EndpointChoice,
    entries: Vec<ClientShellEndpoint>,
    pub(in crate::shell) agent_panel_model: AgentPanelModel,
    pub(in crate::shell) navigator_index: NavigatorIndex,
    /// The snapshot, connection generation and identity the shell presents.
    pub(in crate::shell) active: ActiveProjection,
}

impl Endpoints {
    pub(in crate::shell) fn presented(&self) -> &ClientEndpointId {
        self.choice.presented()
    }

    pub(in crate::shell) fn new(
        entries: Vec<ClientShellEndpoint>,
        config: &ClientShellConfig,
        agent_panel_sort: crate::shell::sidebar::agent_sidebar::AgentPanelSort,
    ) -> Self {
        let agent_panel_model = AgentPanelModel::build(&entries, config, agent_panel_sort);
        let navigator_index = NavigatorIndex::build(&entries, &config.local_label);
        Self {
            choice: crate::endpoint::EndpointChoice::showing(ClientEndpointId::Local),
            entries,
            agent_panel_model,
            navigator_index,
            active: ActiveProjection::default(),
        }
    }
}

/// An endpoint's projection as read before a commit makes it the presented one. Only
/// `endpoint_projection` makes one and only `present_projection` consumes it.
pub(crate) struct EndpointProjection {
    endpoint_id: ClientEndpointId,
    snapshot: Arc<ClientShellSnapshot>,
    generation: ConnectionGeneration,
    /// Whether `endpoint_id` differed from the presented endpoint when read.
    switching: bool,
}

/// The projection the shell presents: the active endpoint's last accepted snapshot and
/// the connection generation that delivered it.
#[derive(Debug, Default)]
pub(in crate::shell) struct ActiveProjection {
    snapshot: Option<Arc<ClientShellSnapshot>>,
    generation: Option<ConnectionGeneration>,
    /// The endpoint and boot of `snapshot`, so a switch of endpoint or a restart of its
    /// server is detectable when the next snapshot arrives.
    boot_key: Option<ClientEndpointBootKey>,
    previous_pane_id: Option<shepr_protocol::PublicPaneId>,
}

/// What accepting a snapshot changed, for the reactions that follow it.
pub(in crate::shell) struct ProjectionChange {
    pub(in crate::shell) generation_changed: bool,
    pub(in crate::shell) focused_workspace_changed: bool,
    pub(in crate::shell) step: ProjectionStep,
}

pub(in crate::shell) enum ProjectionStep {
    /// The first snapshot this shell presents.
    First,
    /// Same endpoint and boot.
    Advanced,
    /// The presented endpoint changed, or its server rebooted (ids can be reused).
    Replaced(ProjectionReset),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ProjectionReset {
    EndpointSwitched,
    Rebooted,
}

/// The one older-revision rule, for the active projection and the per-endpoint cache:
/// within one connection generation and boot, a lower revision is older.
fn revision_is_older(
    current: &ClientShellSnapshot,
    current_generation: ConnectionGeneration,
    next: &ClientShellSnapshot,
    next_generation: ConnectionGeneration,
) -> bool {
    current_generation == next_generation
        && current.boot_id == next.boot_id
        && next.revision < current.revision
}

impl ActiveProjection {
    pub(in crate::shell) fn snapshot(&self) -> Option<&ClientShellSnapshot> {
        self.snapshot.as_deref()
    }

    pub(in crate::shell) fn generation(&self) -> Option<ConnectionGeneration> {
        self.generation
    }

    pub(in crate::shell) fn previous_pane_id(&self) -> Option<&shepr_protocol::PublicPaneId> {
        self.previous_pane_id.as_ref()
    }

    /// Accepts `snapshot` as the presented projection, or returns `None` for an older
    /// revision of the same endpoint, boot and generation. Stores it on acceptance.
    pub(in crate::shell) fn accept(
        &mut self,
        presented: &ClientEndpointId,
        snapshot: Arc<ClientShellSnapshot>,
        generation: ConnectionGeneration,
    ) -> Option<ProjectionChange> {
        let boot_key = Some(ClientEndpointBootKey::new(presented, &snapshot.boot_id));
        let step = match &self.snapshot {
            None => ProjectionStep::First,
            Some(_) if self.boot_key == boot_key => ProjectionStep::Advanced,
            Some(_)
                if self
                    .boot_key
                    .as_ref()
                    .map(ClientEndpointBootKey::endpoint_id)
                    != Some(presented) =>
            {
                ProjectionStep::Replaced(ProjectionReset::EndpointSwitched)
            }
            Some(_) => ProjectionStep::Replaced(ProjectionReset::Rebooted),
        };
        let generation_changed = self.generation != Some(generation);
        if matches!(step, ProjectionStep::Advanced)
            && self.snapshot.as_deref().zip(self.generation).is_some_and(
                |(current, current_generation)| {
                    revision_is_older(current, current_generation, &snapshot, generation)
                },
            )
        {
            return None;
        }
        let focused_workspace_changed = self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_workspace_id.as_ref())
            != snapshot.focused_workspace_id.as_ref();
        if matches!(step, ProjectionStep::Advanced)
            && let Some(previous) = self
                .snapshot
                .as_deref()
                .and_then(|current| current.focused_pane_id)
                .filter(|previous| Some(*previous) != snapshot.focused_pane_id)
        {
            self.previous_pane_id = Some(previous);
        }
        self.snapshot = Some(snapshot);
        self.generation = Some(generation);
        self.boot_key = boot_key;
        Some(ProjectionChange {
            generation_changed,
            focused_workspace_changed,
            step,
        })
    }

    /// For a reset: drops `previous_pane_id`. The snapshot, generation and boot key stay,
    /// because `accept` has already stored the incoming ones.
    pub(in crate::shell) fn reset(&mut self) {
        self.previous_pane_id = None;
    }
}

/// Read-only: every change to an entry goes through `ClientShellState`'s endpoint methods,
/// which rebuild the agent panel model and the navigator index derived from the entries.
impl std::ops::Deref for Endpoints {
    type Target = Vec<ClientShellEndpoint>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

#[derive(Clone, Debug)]
pub(in crate::shell) struct EndpointSnapshot {
    snapshot: Arc<ClientShellSnapshot>,
    generation: ConnectionGeneration,
}

/// A live presentation always has a snapshot. A disconnected presentation keeps its
/// last snapshot for display, but cannot supply navigation or pane commands.
#[derive(Clone, Debug)]
pub(in crate::shell) enum EndpointState {
    Connecting {
        last: Option<EndpointSnapshot>,
        connected: bool,
        generation: Option<ConnectionGeneration>,
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

    pub(in crate::shell) fn usable(&self) -> bool {
        matches!(self, Self::Online(_))
    }

    pub(in crate::shell) fn stale(&self) -> bool {
        !self.usable()
    }

    pub(in crate::shell) fn status(&self) -> ClientEndpointStatus {
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
    pub(in crate::shell) fn snapshot(&self) -> Option<&ClientShellSnapshot> {
        self.state.last().map(|last| last.snapshot.as_ref())
    }

    /// The snapshot whose workspaces and agents the sidebar and the navigator
    /// list. A configured machine that is not connected lists none: its state
    /// entry stands in for them, with no stale rows. Local keeps its last
    /// snapshot listed, dimmed, while it reconnects.
    pub(in crate::shell) fn listed_snapshot(&self) -> Option<&ClientShellSnapshot> {
        if self.endpoint_id.is_local() || self.state.usable() {
            self.snapshot()
        } else {
            None
        }
    }

    /// The entry a configured machine shows in place of its workspaces, `None`
    /// while it is connected and for the Local endpoint.
    pub(in crate::shell) fn machine_entry(&self) -> Option<MachineEntry> {
        if self.endpoint_id.is_local() || self.state.usable() {
            return None;
        }
        let hint = self
            .ssh_check
            .as_ref()
            .filter(|_| self.machine == MachineState::NeedsLogin)
            .map(|ssh| format!("run {} again, or {ssh}", operator_entrypoint()));
        Some(MachineEntry {
            state: self.machine,
            hint,
        })
    }

    /// The glyph and colour the machine row shows: the connection status for
    /// Local and for a connected machine, else the machine's state.
    pub(in crate::shell) fn row_glyph(
        &self,
        palette: &Palette,
    ) -> (&'static str, ratatui::style::Color) {
        match self.machine_entry() {
            Some(entry) => entry.state.glyph(palette),
            None => {
                let (glyph, _, color) = endpoint_status_presentation(self.state.status(), palette);
                (glyph, color)
            }
        }
    }

    fn shared_snapshot(&self) -> Option<Arc<ClientShellSnapshot>> {
        self.state.last().map(|last| Arc::clone(&last.snapshot))
    }

    pub(in crate::shell) fn snapshot_generation(&self) -> Option<ConnectionGeneration> {
        self.state.last().map(|last| last.generation)
    }
}

impl ClientShellState {
    /// Sets the configured machines, once at launch: Local first, then one endpoint per
    /// `[[machines]]` entry, each Connecting with no snapshot. The set never changes
    /// while the client runs.
    pub(crate) fn set_machines(&mut self, machines: &[shepr_config::MachineConfig]) {
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
                machine: MachineState::Connecting,
                ssh_check: Some(shepr_remote::ssh_check_command(&machine.ssh)),
            });
        }
        self.endpoints.entries = next;
        self.rebuild_endpoint_models();
    }

    /// Sets what a configured machine's entry says. The Local endpoint has no
    /// entry and ignores it.
    pub(crate) fn set_machine_state(
        &mut self,
        endpoint_id: &ClientEndpointId,
        state: MachineState,
    ) {
        if endpoint_id.is_local() {
            return;
        }
        if let Some(endpoint) = self
            .endpoints
            .entries
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.machine = state;
        }
        // The navigate selection may sit on an entry whose action just went away.
        self.reconcile_navigate_machine_entry();
    }

    /// A handshake starts a new presentation generation. Until its own snapshot arrives,
    /// the previous generation is retained only as stale display data.
    pub(crate) fn endpoint_connected(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .entries
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            let last = endpoint.state.last().cloned();
            endpoint.state = EndpointState::Connecting {
                last,
                connected: true,
                generation: Some(generation),
            };
            endpoint.machine = MachineState::Connecting;
        }
        self.clear_machine_diagnostic(endpoint_id);
        self.rebuild_endpoint_models();
    }

    /// Records a status the supervisor reported for an endpoint with no live
    /// connection (a failed or pending attempt). The selection is untouched: a
    /// move waiting for Local's reconnect keeps waiting through its failed
    /// attempts.
    pub(crate) fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) {
        self.apply_endpoint_status(endpoint_id, status);
    }

    /// A live connection was lost: the endpoint takes its failure status and, when it is
    /// presented, every request still in the ledger is interrupted. The choice is the hub's
    /// to update. A status report for an endpoint with no live connection leaves the
    /// requests alone.
    pub(crate) fn endpoint_failed(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) {
        self.apply_endpoint_status(endpoint_id, status);
        if self.endpoint_is_active(endpoint_id) {
            self.drop_all_requests(DropReason::Interrupted);
        }
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
            .entries
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

    pub(in crate::shell) fn endpoint_usable(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .any(|endpoint| &endpoint.endpoint_id == endpoint_id && endpoint.state.usable())
    }

    /// The presented endpoint the shell can switch to, read before the choice commits.
    /// `None` unless the endpoint is usable and has a snapshot and generation.
    pub(crate) fn endpoint_projection(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<EndpointProjection> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
        if !endpoint.state.usable() {
            return None;
        }
        Some(EndpointProjection {
            endpoint_id: endpoint_id.clone(),
            snapshot: endpoint.shared_snapshot()?,
            generation: endpoint.snapshot_generation()?,
            switching: endpoint_id != self.endpoints.presented(),
        })
    }

    /// Presents `projection` once the choice shows its endpoint: the surfaces are cleared on
    /// a switch, then the snapshot becomes the active projection. The aggregate agent list
    /// belongs to the client, not one endpoint: the snapshot is accepted as
    /// `Replaced(EndpointSwitched)`, which keeps the agent start.
    pub(crate) fn present_projection(&mut self, projection: &EndpointProjection) {
        if projection.switching {
            self.presentation.surfaces = PaneSurfaces::default();
        }
        self.apply_active_snapshot(&projection.snapshot, projection.generation);
        self.sidebar_scroll
            .endpoint_activated(&projection.endpoint_id);
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
        generation: ConnectionGeneration,
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
        generation: ConnectionGeneration,
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

    pub(in crate::shell) fn active_endpoint_label(&self) -> &str {
        self.endpoints
            .presented()
            .display_label(&self.config.local_label)
    }

    pub(crate) fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.presented() == endpoint_id
    }

    pub(in crate::shell) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    pub(crate) fn cache_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        snapshot: impl Into<Arc<ClientShellSnapshot>>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, generation, snapshot.into());
    }

    fn cache_endpoint_snapshot_at_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        snapshot: Arc<ClientShellSnapshot>,
    ) {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if let Some((previous, previous_generation)) = self.endpoints[index]
            .snapshot()
            .zip(self.endpoints[index].snapshot_generation())
            && revision_is_older(previous, previous_generation, &snapshot, generation)
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
        let endpoint = &mut self.endpoints.entries[index];
        endpoint.agent_recency = recency;
        endpoint.state.cache(EndpointSnapshot {
            generation,
            snapshot,
        });
        self.rebuild_endpoint_models();
    }

    pub(crate) fn set_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
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
            self.apply_active_snapshot(&snapshot, generation);
        }
    }

    /// Rebuilds the agent panel, keeping a Navigate selection on an agent through the
    /// change.
    pub(in crate::shell) fn rebuild_agent_panel_model(&mut self) {
        let selected_agent = self.navigate_agent_index();
        let model = AgentPanelModel::build(
            &self.endpoints,
            &self.config,
            self.agent_panel_sort_chrome.value(),
        );
        self.endpoints.agent_panel_model = model;
        self.reconcile_navigate_agent(selected_agent);
    }

    /// Applies a sort chosen at runtime (the sidebar toggle) for this session and
    /// rebuilds the agent panel in that order.
    pub(in crate::shell) fn set_agent_panel_sort(
        &mut self,
        sort: crate::shell::sidebar::agent_sidebar::AgentPanelSort,
    ) {
        self.agent_panel_sort_chrome.set_manual(sort);
        self.rebuild_agent_panel_model();
    }

    fn rebuild_endpoint_models(&mut self) {
        self.rebuild_agent_panel_model();
        let index = NavigatorIndex::build(&self.endpoints, &self.config.local_label);
        self.endpoints.navigator_index = index;
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
        machine: MachineState::Connecting,
        ssh_check: None,
    }
}

#[cfg(test)]
impl ClientShellState {
    pub(in crate::shell) fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        let endpoint_id = self.endpoints.presented().clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    pub(in crate::shell) fn cache_endpoint_snapshot(
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
            .unwrap_or(ConnectionGeneration::FIRST);
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

    pub(in crate::shell) fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }
}

#[cfg(test)]
impl ClientShellState {
    /// Re-caches `endpoint_id`'s snapshot with `edit` applied, at the generation that
    /// delivered it, the way a connection delivers one: through the cache, which rebuilds
    /// the models derived from the endpoints. The active projection is left as it was.
    pub(in crate::shell) fn edit_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        edit: impl FnOnce(&mut ClientShellSnapshot),
    ) {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .expect("test precondition: the endpoint is configured");
        let generation = endpoint
            .snapshot_generation()
            .expect("test precondition: the endpoint has a snapshot");
        let mut snapshot = endpoint
            .snapshot()
            .expect("test precondition: the endpoint has a snapshot")
            .clone();
        edit(&mut snapshot);
        self.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    }
}

#[cfg(test)]
impl ActiveProjection {
    pub(in crate::shell) fn shared_snapshot(&self) -> Option<&Arc<ClientShellSnapshot>> {
        self.snapshot.as_ref()
    }

    /// Forgets the presented snapshot, as if none had arrived.
    pub(in crate::shell) fn clear_snapshot(&mut self) {
        self.snapshot = None;
    }
}

#[cfg(test)]
impl ClientShellState {
    /// Commits a preparing move to `endpoint_id`, or shows it when no move is pending,
    /// then presents its projection. False when the endpoint has no usable projection or a
    /// move to another endpoint is in the way. Production commits through the hub's view
    /// steps; the shell tests use this as the shortcut to another endpoint.
    pub(in crate::shell) fn activate_endpoint_projection(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) -> bool {
        let Some(projection) = self.endpoint_projection(endpoint_id) else {
            return false;
        };
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
        self.present_projection(&projection);
        true
    }

    pub(in crate::shell) fn mark_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_status(endpoint_id, EndpointFailureStatus::Reconnecting);
        if self.endpoint_is_active(endpoint_id) {
            self.drop_all_requests(DropReason::Interrupted);
        }
    }

    /// Brings an endpoint online the way a connection does: the handshake opens
    /// the generation at `position` of the test's own numbering, and that
    /// generation's snapshot makes the endpoint usable.
    pub(in crate::shell) fn connect_endpoint_with_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        position: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        let generation = crate::tests::test_generation(position);
        self.endpoint_connected(endpoint_id, generation);
        self.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    }

    pub(in crate::shell) fn endpoint_has_snapshot(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| endpoint.snapshot().is_some())
    }

    pub(in crate::shell) fn active_endpoint_id(&self) -> &ClientEndpointId {
        self.endpoints.presented()
    }
}

#[cfg(test)]
impl ClientShellState {
    /// What a configured machine's entry says, `None` for an endpoint that is not
    /// configured.
    pub(crate) fn machine_state(&self, endpoint_id: &ClientEndpointId) -> Option<MachineState> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.machine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_generation as generation_at;

    fn snapshot(position: u64) -> EndpointSnapshot {
        EndpointSnapshot {
            snapshot: Arc::new(crate::shell::tests::snapshot()),
            generation: generation_at(position),
        }
    }

    #[test]
    fn online_requires_a_connection_and_its_snapshot() {
        let mut shell = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        let local = ClientEndpointId::Local;
        shell.set_endpoint_status(&local, EndpointFailureStatus::Reconnecting);
        shell.cache_endpoint_snapshot_for_generation(
            &local,
            generation_at(1),
            Arc::new(crate::shell::tests::snapshot()),
        );
        // A snapshot without a live connection is display data only.
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Reconnecting)
        );
        assert!(!shell.endpoint_usable(&local));

        shell.endpoint_connected(&local, generation_at(2));
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Connecting)
        );
        assert!(!shell.endpoint_usable(&local));
        shell.cache_endpoint_snapshot_for_generation(
            &local,
            generation_at(2),
            Arc::new(crate::shell::tests::snapshot()),
        );
        assert_eq!(
            shell.endpoint_status(&local),
            Some(ClientEndpointStatus::Online)
        );
        assert!(shell.endpoint_usable(&local));
    }

    fn projection(boot: &str, revision: u64) -> Arc<ClientShellSnapshot> {
        let mut next = crate::shell::tests::snapshot();
        next.boot_id = crate::tests::test_boot_id(boot);
        next.revision = shepr_test_fixtures::counter_at(revision);
        Arc::new(next)
    }

    #[test]
    fn accept_reports_a_switch_and_a_reboot_apart() {
        let local = ClientEndpointId::Local;
        let remote =
            ClientEndpointId::Ssh(shepr_config::MachineLabel::parse("Build").expect("test label"));
        let mut active = ActiveProjection::default();

        let first = active
            .accept(&local, projection("boot-1", 1), generation_at(1))
            .expect("the first snapshot is accepted");
        assert!(matches!(first.step, ProjectionStep::First));
        assert!(first.generation_changed);

        let advanced = active
            .accept(&local, projection("boot-1", 2), generation_at(1))
            .expect("a newer revision is accepted");
        assert!(matches!(advanced.step, ProjectionStep::Advanced));
        assert!(!advanced.generation_changed);
        assert!(!advanced.focused_workspace_changed);

        let rebooted = active
            .accept(&local, projection("boot-2", 1), generation_at(1))
            .expect("a reboot is accepted at a lower revision");
        assert!(matches!(
            rebooted.step,
            ProjectionStep::Replaced(ProjectionReset::Rebooted)
        ));

        let switched = active
            .accept(&remote, projection("boot-2", 1), generation_at(2))
            .expect("another endpoint is accepted, even with the same boot id");
        assert!(matches!(
            switched.step,
            ProjectionStep::Replaced(ProjectionReset::EndpointSwitched)
        ));
        assert!(switched.generation_changed);
        assert_eq!(active.generation(), Some(generation_at(2)));
    }

    #[test]
    fn older_revision_rule_is_shared_by_cache_and_projection() {
        // (generation, boot, revision) of the next snapshot against a current one at
        // generation 2, boot-1, revision 5; whether it is dropped as older.
        let cases = [
            (2, "boot-1", 4, true),
            (2, "boot-1", 5, false),
            (2, "boot-1", 6, false),
            (3, "boot-1", 1, false),
            (2, "boot-2", 1, false),
        ];
        let local = ClientEndpointId::Local;
        for (position, boot, revision, older) in cases {
            let generation = generation_at(position);
            let mut active = ActiveProjection::default();
            assert!(
                active
                    .accept(&local, projection("boot-1", 5), generation_at(2))
                    .is_some()
            );
            let accepted = active
                .accept(&local, projection(boot, revision), generation)
                .is_some();
            assert_eq!(accepted, !older, "projection: {position} {boot} {revision}");

            let mut shell = ClientShellState::new(ClientShellConfig::from_config(
                &shepr_config::ClientConfig::default(),
            ));
            shell.endpoint_connected(&local, generation_at(2));
            shell.cache_endpoint_snapshot_for_generation(
                &local,
                generation_at(2),
                projection("boot-1", 5),
            );
            shell.cache_endpoint_snapshot_for_generation(
                &local,
                generation,
                projection(boot, revision),
            );
            let (cached_generation, cached_boot, cached_revision) = if older {
                (generation_at(2), "boot-1", 5)
            } else {
                (generation, boot, revision)
            };
            assert!(
                shell.endpoint_snapshot_matches(
                    &local,
                    cached_generation,
                    &crate::tests::test_boot_id(cached_boot),
                    shepr_test_fixtures::counter_at(cached_revision),
                ),
                "cache: {position} {boot} {revision}"
            );
        }
    }

    #[test]
    fn a_reconnect_requires_its_own_generation_snapshot() {
        let mut state = EndpointState::Connecting {
            last: Some(snapshot(1)),
            connected: true,
            generation: Some(generation_at(2)),
        };
        assert!(state.stale());
        state.cache(snapshot(1));
        assert!(state.stale());
        state.cache(snapshot(2));
        assert!(state.usable());
    }

    /// Each state a probe of a machine can find maps to its own entry: what it says, and
    /// whether activating it connects or restarts.
    #[test]
    fn every_probed_machine_state_maps_to_its_sidebar_entry() {
        use shepr_launch::{EndpointFailure, SshFailureClass};
        for (failure, state, label, action) in [
            (
                EndpointFailure::no_server("no shepr server is running"),
                MachineState::NotRunning,
                "Connect",
                Some(MachineAction::Connect),
            ),
            (
                EndpointFailure::server_starting("the server is starting"),
                MachineState::Starting,
                "Starting...",
                None,
            ),
            (
                EndpointFailure::server_stopping("the server is stopping"),
                MachineState::Stopping,
                "Stopping...",
                None,
            ),
            (
                EndpointFailure::server_shutdown(shepr_protocol::ShutdownReason::Stopping),
                MachineState::Stopping,
                "Stopping...",
                None,
            ),
            (
                EndpointFailure::different_build("the server is another build"),
                MachineState::DifferentBuild,
                "Restart (other build)",
                Some(MachineAction::Restart),
            ),
            (
                EndpointFailure::ssh(
                    SshFailureClass::AuthenticationPending,
                    "SSH round trip did not answer",
                ),
                MachineState::Offline,
                "Offline",
                None,
            ),
            (
                EndpointFailure::ssh(SshFailureClass::Link, "Connection timed out"),
                MachineState::Offline,
                "Offline",
                None,
            ),
            (
                EndpointFailure::from_error(&std::io::Error::from(
                    std::io::ErrorKind::HostUnreachable,
                )),
                MachineState::Offline,
                "Offline",
                None,
            ),
            (
                EndpointFailure::ssh(
                    SshFailureClass::Authentication,
                    "Permission denied (publickey)",
                ),
                MachineState::NeedsLogin,
                "Needs SSH login",
                None,
            ),
            (
                EndpointFailure::ssh(SshFailureClass::HostKey, "Host key verification failed"),
                MachineState::Unavailable,
                "Unavailable",
                None,
            ),
            (
                EndpointFailure::retry("connection closed"),
                MachineState::Connecting,
                "Connecting...",
                None,
            ),
        ] {
            let mapped = MachineState::after_failure(&failure);
            assert_eq!(mapped, state, "{failure}");
            assert_eq!(mapped.label(), label, "{failure}");
            assert_eq!(mapped.action(), action, "{failure}");
        }
    }

    #[test]
    fn only_a_machine_that_is_not_connected_shows_an_entry() {
        let machine = shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("build").expect("test label"),
            ssh: shepr_config::SshTarget::parse("build.example").expect("test target"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        };
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut shell = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        shell.set_machines(&[machine]);
        let entry = |shell: &ClientShellState| {
            shell
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == id)
                .and_then(ClientShellEndpoint::machine_entry)
                .map(|entry| entry.state)
        };
        assert_eq!(entry(&shell), Some(MachineState::Connecting));
        shell.set_machine_state(&id, MachineState::NeedsLogin);
        let login = shell
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == id)
            .and_then(ClientShellEndpoint::machine_entry)
            .and_then(|entry| entry.hint)
            .expect("a login hint");
        let entrypoint = operator_entrypoint();
        if shepr_paths::BuildProfile::current() == shepr_paths::BuildProfile::Dev {
            assert_ne!(entrypoint, "shepr");
        }
        assert!(
            login.contains(&format!("run {entrypoint} again")),
            "{login}"
        );
        assert!(login.contains("ssh build.example"), "{login}");

        shell.connect_endpoint_with_snapshot(&id, 2, Box::new(crate::shell::tests::snapshot()));
        assert_eq!(entry(&shell), None);
        // Local never shows one.
        assert!(
            shell
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id.is_local())
                .and_then(ClientShellEndpoint::machine_entry)
                .is_none()
        );
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
            assert_eq!(
                state.last().expect("retained snapshot").generation,
                generation_at(2)
            );
        }
    }
}
