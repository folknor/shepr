use crate::endpoint::ClientEndpointId;
use crate::shell::endpoints::ClientShellEndpoint;
use crate::shell::navigation::location::{Location, LocationTarget, PinnedLocation};
use crate::shell::state::ClientShellMode;
use crate::shell::state::{ClientShellInput, ClientShellState};
use crate::shell::{EndpointNotice, EndpointNoticeKind};

/// Display-only continuity while a direct focus request awaits its authoritative snapshot.
pub(in crate::shell) struct PendingWorkspaceHighlight {
    pub(in crate::shell) target: PinnedLocation,
    pub(in crate::shell) request_id: shepr_protocol::RequestId,
    expires_at: std::time::Instant,
}

impl PinnedLocation {
    pub(in crate::shell) fn matches_workspace(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        self.matches(endpoint_id, LocationTarget::Workspace(*workspace_id))
    }
}

pub(in crate::shell) fn workspace_navigation_targets(
    endpoints: &[ClientShellEndpoint],
) -> Vec<PinnedLocation> {
    let mut targets = Vec::new();
    for endpoint in endpoints {
        if endpoint.state.stale() {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot() else {
            continue;
        };
        let Some(generation) = endpoint.snapshot_generation() else {
            continue;
        };
        for workspace in &snapshot.workspaces {
            targets.push(PinnedLocation::new(
                Location::workspace(endpoint.endpoint_id.clone(), workspace.workspace_id),
                snapshot.boot_id.clone(),
                generation,
            ));
        }
    }
    targets
}

impl ClientShellState {
    pub(in crate::shell) fn keep_workspace_highlight_until_snapshot(
        &mut self,
        target: PinnedLocation,
        request_id: &shepr_protocol::RequestId,
        now: std::time::Instant,
    ) {
        self.pending_workspace_highlight = Some(PendingWorkspaceHighlight {
            target,
            request_id: request_id.clone(),
            expires_at: now + crate::limits::WORKSPACE_HIGHLIGHT_TIMEOUT,
        });
        self.reconcile_pending_workspace_highlight();
    }

    pub(crate) fn tick_workspace_highlight(&mut self, now: std::time::Instant) -> bool {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| now >= pending.expires_at)
        {
            self.pending_workspace_highlight = None;
            return true;
        }
        false
    }

    pub(in crate::shell) fn workspace_highlight_deadline(&self) -> Option<std::time::Instant> {
        self.pending_workspace_highlight
            .as_ref()
            .map(|pending| pending.expires_at)
    }

    pub(in crate::shell) fn reconcile_pending_workspace_highlight(&mut self) {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| {
                pending.target.location.endpoint != *self.endpoints.presented()
                    || !self.navigation_target_valid(&pending.target)
                    || self.snapshot.as_deref().is_some_and(|snapshot| {
                        pending
                            .target
                            .location
                            .workspace_id()
                            .is_some_and(|workspace_id| {
                                snapshot.focused_workspace_id.as_ref() == Some(&workspace_id)
                            })
                    })
            })
        {
            self.pending_workspace_highlight = None;
        }
    }

    pub(in crate::shell) fn navigation_target(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<PinnedLocation> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot()?;
        let generation = endpoint.snapshot_generation()?;
        Some(PinnedLocation::new(
            Location::workspace(endpoint_id.clone(), *workspace_id),
            snapshot.boot_id.clone(),
            generation,
        ))
    }

    pub(in crate::shell) fn focused_navigation_target(&self) -> Option<PinnedLocation> {
        let workspace_id = self.snapshot.as_deref()?.focused_workspace_id.as_ref()?;
        self.navigation_target(self.endpoints.presented(), workspace_id)
    }

    pub(in crate::shell) fn navigation_target_valid(&self, target: &PinnedLocation) -> bool {
        let workspace_id = target.location.workspace_id();
        self.endpoints.iter().any(|endpoint| {
            endpoint.endpoint_id == target.location.endpoint
                && endpoint.state.usable()
                && endpoint.snapshot_generation() == Some(target.generation())
                && endpoint.snapshot().is_some_and(|snapshot| {
                    snapshot.boot_id == *target.boot_id()
                        && workspace_id.is_some_and(|workspace_id| {
                            snapshot
                                .workspaces
                                .iter()
                                .any(|workspace| workspace.workspace_id == workspace_id)
                        })
                })
        })
    }

    pub(in crate::shell) fn workspace_preview_action_blocked(&self) -> bool {
        self.navigate_workspace_id.as_ref().is_some_and(|target| {
            target.location.endpoint != *self.endpoints.presented()
                || !self.navigation_target_valid(target)
        })
    }

    pub(in crate::shell) fn move_navigate_workspace(&mut self, delta: isize) {
        let mut targets = workspace_navigation_targets(&self.endpoints);
        if targets.is_empty() {
            return;
        }
        let current = self
            .navigate_workspace_id
            .as_ref()
            .and_then(|selected| targets.iter().position(|target| target == selected));
        let Some(next) = crate::shell::navigation::aggregate_navigation::cycle_index(
            targets.len(),
            current,
            delta,
        ) else {
            return;
        };
        let target = targets.swap_remove(next);
        self.collapsed_endpoints.remove(&target.location.endpoint);
        if self.endpoints.len() == 1
            && let Some(workspace_id) = target.location.workspace_id()
        {
            self.reveal_workspace(&workspace_id);
        }
        self.navigate_workspace_id = Some(target);
        self.reveal_navigation_workspace =
            self.endpoints.len() > 1 || self.snapshot.is_none() || self.pane_surface().is_none();
    }

    pub(in crate::shell) fn accept_navigate_workspace(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.navigate_workspace_id.clone() else {
            self.mode = self.copy_or_terminal_mode();
            outcome.repaint = true;
            return;
        };
        if !self.navigation_target_valid(&target) {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                target.location.endpoint.clone(),
                EndpointNoticeKind::WorkspaceNoLongerAvailable,
            ));
            outcome.repaint = true;
            return;
        }
        if self.focus_or_activate(target.location.clone(), outcome) {
            self.mode = ClientShellMode::Terminal;
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }
}
