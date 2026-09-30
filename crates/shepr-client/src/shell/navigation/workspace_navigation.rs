use super::*;

/// A client-only preview. Snapshot identity prevents Enter from using a reused workspace ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceNavigationTarget {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: shepr_protocol::WorkspaceId,
    boot_id: shepr_protocol::BootId,
    generation: Option<u64>,
}

/// Display-only continuity while a direct focus request awaits its authoritative snapshot.
pub(super) struct PendingWorkspaceHighlight {
    pub(super) target: WorkspaceNavigationTarget,
    pub(super) request_id: shepr_protocol::RequestId,
    expires_at: std::time::Instant,
}

impl WorkspaceNavigationTarget {
    pub(super) fn matches(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        &self.endpoint_id == endpoint_id && self.workspace_id == *workspace_id
    }
}

impl ClientShellState {
    pub(super) fn keep_workspace_highlight_until_snapshot(
        &mut self,
        target: WorkspaceNavigationTarget,
        request_id: &str,
        now: std::time::Instant,
    ) {
        self.pending_workspace_highlight = Some(PendingWorkspaceHighlight {
            target,
            request_id: request_id.to_owned().into(),
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

    pub(super) fn workspace_highlight_deadline(&self) -> Option<std::time::Instant> {
        self.pending_workspace_highlight
            .as_ref()
            .map(|pending| pending.expires_at)
    }

    pub(super) fn reconcile_pending_workspace_highlight(&mut self) {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| {
                pending.target.endpoint_id != self.active_endpoint_id
                    || !self.navigation_target_valid(&pending.target)
                    || self.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.focused_workspace_id.as_deref()
                            == Some(pending.target.workspace_id.as_str())
                    })
            })
        {
            self.pending_workspace_highlight = None;
        }
    }

    pub(super) fn navigation_target(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<WorkspaceNavigationTarget> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot.as_deref()?;
        Some(WorkspaceNavigationTarget {
            endpoint_id: endpoint_id.clone(),
            workspace_id: workspace_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            generation: endpoint.snapshot_generation,
        })
    }

    pub(super) fn focused_navigation_target(&self) -> Option<WorkspaceNavigationTarget> {
        let workspace_id = self.snapshot.as_deref()?.focused_workspace_id.as_ref()?;
        self.navigation_target(&self.active_endpoint_id, workspace_id)
    }

    pub(super) fn navigation_target_valid(&self, target: &WorkspaceNavigationTarget) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.endpoint_id == target.endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot_generation == target.generation
                && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                    snapshot.boot_id == target.boot_id
                        && snapshot
                            .workspaces
                            .iter()
                            .any(|workspace| workspace.workspace_id == target.workspace_id)
                })
        })
    }

    pub(super) fn workspace_preview_action_blocked(&self) -> bool {
        self.navigate_workspace_id.as_ref().is_some_and(|target| {
            target.endpoint_id != self.active_endpoint_id || !self.navigation_target_valid(target)
        })
    }

    pub(super) fn move_navigate_workspace(&mut self, delta: isize) {
        let mut targets = Vec::new();
        for endpoint in &self.endpoints {
            if endpoint.status != ClientEndpointStatus::Online {
                continue;
            }
            let Some(snapshot) = endpoint.snapshot.as_deref() else {
                continue;
            };
            for entry in render::workspace_entries(snapshot) {
                targets.push(WorkspaceNavigationTarget {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    workspace_id: snapshot.workspaces[entry].workspace_id.clone(),
                    boot_id: snapshot.boot_id.clone(),
                    generation: endpoint.snapshot_generation,
                });
            }
        }
        if targets.is_empty() {
            return;
        }
        let current = self
            .navigate_workspace_id
            .as_ref()
            .and_then(|selected| targets.iter().position(|target| target == selected));
        let next = match current {
            Some(current) => {
                let current = isize::try_from(current).unwrap_or(isize::MAX);
                let len = isize::try_from(targets.len()).unwrap_or(isize::MAX);
                usize::try_from((current + delta).rem_euclid(len)).unwrap_or(0)
            }
            None if delta < 0 => targets.len() - 1,
            None => 0,
        };
        let target = targets.swap_remove(next);
        self.collapsed_endpoints.remove(&target.endpoint_id);
        if self.endpoints.len() == 1 {
            self.reveal_workspace(&target.workspace_id);
        }
        self.navigate_workspace_id = Some(target);
        // The machine sidebars (always used with several endpoints, and the
        // unavailable-surface fallback) scroll to the selection on their next
        // render; the single-endpoint sidebar was revealed above.
        self.reveal_navigation_workspace =
            self.endpoints.len() > 1 || self.snapshot.is_none() || self.pane_surface.is_none();
    }

    pub(super) fn accept_navigate_workspace(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.navigate_workspace_id.clone() else {
            self.mode = self.copy_or_terminal_mode();
            outcome.repaint = true;
            return;
        };
        if !self.navigation_target_valid(&target) {
            self.receive_endpoint_unavailable(
                "Workspace is no longer available; select a connected workspace".into(),
            );
            outcome.repaint = true;
            return;
        }
        // The runtime resolves explicit picks against presentation ownership. If it can use a
        // direct focus request, `focus_endpoint_target` records the pending highlight there.
        if self.focus_or_activate(
            target.endpoint_id.clone(),
            ClientEndpointFocusTarget::Workspace(target.workspace_id.clone()),
            outcome,
        ) {
            self.mode = ClientShellMode::Terminal;
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }
}
