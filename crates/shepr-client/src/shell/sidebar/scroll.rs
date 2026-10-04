use crate::endpoint::ClientEndpointId;
use crate::shell::navigation::location::Location;

/// The two sidebar lists' stored starts and every pending reveal request. Input and
/// transitions write requests here; composition resolves them against the layout it
/// is about to draw and commits what it consumed.
pub(in crate::shell) struct SidebarScroll {
    workspaces: usize,
    agents: usize,
    workspace_reveal: WorkspaceReveal,
    agent_reveal: Option<Location>,
    /// An agent on another endpoint, revealed once that endpoint is activated.
    agent_reveal_after_activation: Option<Location>,
}

/// Pending workspace reveals. One frame with a non-empty workspace body consumes all
/// of them; the first with a target in the list wins, in field order.
#[derive(Clone, Default)]
pub(in crate::shell) struct WorkspaceReveal {
    /// The Navigate preview, or the pending focus highlight outside Navigate.
    selected: bool,
    /// A named workspace (keyboard workspace switching).
    explicit: Option<Location>,
    /// The presented endpoint's focused workspace.
    focused: bool,
}

impl WorkspaceReveal {
    pub(in crate::shell) fn selected_pending(&self) -> bool {
        self.selected
    }

    pub(in crate::shell) fn explicit(&self) -> Option<&Location> {
        self.explicit.as_ref()
    }

    pub(in crate::shell) fn focused_pending(&self) -> bool {
        self.focused
    }
}

/// What one composition consumed of the stored scroll state.
pub(in crate::shell) struct SidebarScrollResolution {
    pub(in crate::shell) workspaces: Option<usize>,
    pub(in crate::shell) agents: Option<usize>,
    pub(in crate::shell) workspace_reveal_consumed: bool,
    pub(in crate::shell) agent_reveal_consumed: bool,
}

impl SidebarScroll {
    /// The focused workspace's reveal is requested from the start.
    pub(in crate::shell) fn new() -> Self {
        Self {
            workspaces: 0,
            agents: 0,
            workspace_reveal: WorkspaceReveal {
                focused: true,
                ..WorkspaceReveal::default()
            },
            agent_reveal: None,
            agent_reveal_after_activation: None,
        }
    }

    pub(in crate::shell) fn workspace_start(&self) -> usize {
        self.workspaces
    }

    pub(in crate::shell) fn agent_start(&self) -> usize {
        self.agents
    }

    pub(in crate::shell) fn workspace_reveal(&self) -> &WorkspaceReveal {
        &self.workspace_reveal
    }

    pub(in crate::shell) fn agent_reveal(&self) -> Option<&Location> {
        self.agent_reveal.as_ref()
    }

    /// Scrollbar and wheel input; returns whether the start changed.
    pub(in crate::shell) fn scroll_workspaces_to(&mut self, start: usize) -> bool {
        let changed = self.workspaces != start;
        self.workspaces = start;
        changed
    }

    /// Scrollbar and wheel input; returns whether the start changed.
    pub(in crate::shell) fn scroll_agents_to(&mut self, start: usize) -> bool {
        let changed = self.agents != start;
        self.agents = start;
        changed
    }

    pub(in crate::shell) fn reveal_selected_workspace(&mut self) {
        self.workspace_reveal.selected = true;
    }

    pub(in crate::shell) fn reveal_workspace(&mut self, location: Location) {
        self.workspace_reveal.explicit = Some(location);
    }

    pub(in crate::shell) fn reveal_focused_workspace(&mut self) {
        self.workspace_reveal.focused = true;
    }

    pub(in crate::shell) fn reveal_agent(&mut self, location: Location) {
        self.agent_reveal = Some(location);
    }

    pub(in crate::shell) fn reveal_agent_after_activation(&mut self, location: Location) {
        self.agent_reveal_after_activation = Some(location);
    }

    pub(in crate::shell) fn cancel_agent_reveal_after_activation(&mut self) {
        self.agent_reveal_after_activation = None;
    }

    /// Promotes the deferred agent reveal when `endpoint` is the one it waited for.
    pub(in crate::shell) fn endpoint_activated(&mut self, endpoint: &ClientEndpointId) {
        if let Some(location) = self
            .agent_reveal_after_activation
            .take_if(|location| &location.endpoint == endpoint)
        {
            self.agent_reveal = Some(location);
        }
    }

    /// Endpoint switch or reboot: start 0, focused reveal requested, other workspace
    /// reveals dropped.
    pub(in crate::shell) fn reset_workspaces(&mut self) {
        self.workspaces = 0;
        self.workspace_reveal = WorkspaceReveal {
            focused: true,
            ..WorkspaceReveal::default()
        };
    }

    /// Same-endpoint reboot and the sort toggle: start 0.
    pub(in crate::shell) fn reset_agents(&mut self) {
        self.agents = 0;
    }

    pub(in crate::shell) fn commit(&mut self, resolution: &SidebarScrollResolution) {
        if let Some(start) = resolution.workspaces {
            self.workspaces = start;
        }
        if let Some(start) = resolution.agents {
            self.agents = start;
        }
        if resolution.workspace_reveal_consumed {
            self.workspace_reveal = WorkspaceReveal::default();
        }
        if resolution.agent_reveal_consumed {
            self.agent_reveal = None;
        }
    }

    /// Drops every pending workspace reveal.
    #[cfg(test)]
    pub(in crate::shell) fn clear_reveals(&mut self) {
        self.workspace_reveal = WorkspaceReveal::default();
    }
}
