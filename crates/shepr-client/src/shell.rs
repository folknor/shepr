use std::collections::{HashMap, HashSet, VecDeque};

#[path = "shell/navigation/actions.rs"]
mod actions;
#[path = "shell/sidebar/agent_sidebar.rs"]
mod agent_sidebar;
#[path = "shell/navigation/aggregate_navigation.rs"]
mod aggregate_navigation;
#[path = "shell/overlays/machine_diagnostics.rs"]
mod machine_diagnostics;
#[path = "shell/navigation/workspace_navigation.rs"]
mod workspace_navigation;
use workspace_navigation::{PendingWorkspaceHighlight, WorkspaceNavigationTarget};
#[path = "shell/presentation/composition.rs"]
mod composition;
#[path = "shell/presentation/config.rs"]
mod config;
#[path = "shell/overlays/context_menu.rs"]
mod context_menu;
#[path = "shell/input/copy_mode.rs"]
mod copy_mode;
use copy_mode::CopyPipeline;
#[path = "shell/sidebar/endpoint_agents.rs"]
mod endpoint_agents;
#[path = "shell/navigation/endpoint_navigation.rs"]
mod endpoint_navigation;
#[path = "shell/overlays/endpoint_notices.rs"]
mod endpoint_notices;
#[path = "shell/sidebar/endpoint_sidebar.rs"]
mod endpoint_sidebar;
mod endpoints;
pub(super) use endpoints::*;
#[path = "shell/overlays/global_menu.rs"]
mod global_menu;
#[path = "shell/input/input.rs"]
mod input;
mod ledger;
pub(super) use ledger::DropReason;
use ledger::{Ledger, Work};
#[path = "shell/input/mouse.rs"]
mod mouse;
#[path = "shell/overlays/overlay_input.rs"]
mod overlay_input;
#[path = "shell/overlays/preferences.rs"]
mod preferences;
#[path = "shell/presentation/render.rs"]
mod render;
#[path = "shell/navigation/scroll.rs"]
mod scroll;
#[path = "shell/input/scroll_lanes.rs"]
mod scroll_lanes;
use scroll_lanes::{ScrollAnswer, ScrollLanes, ScrollWant};
#[path = "shell/sidebar/sidebar_tokens.rs"]
mod sidebar_tokens;
mod state;
#[path = "shell/presentation/surface_patch.rs"]
mod surface_patch;
#[path = "shell/presentation/surfaces.rs"]
mod surfaces;
pub(super) use surfaces::PatchRejection;
use surfaces::{Pairing, PaneSurfaces};
#[path = "shell/overlays/text_editor.rs"]
mod text_editor;
#[path = "shell/input/word_selection.rs"]
mod word_selection;
use text_editor::TextEditor;
use word_selection::ClientWordSelection;

pub(in crate::shell) use render::sidebar;
use sidebar_tokens::{
    AgentTokenContext, ResolvedToken, ResolvedTokenKind, SpaceTokenContext, TokenStyles,
    expanded_sidebar_sections, resolved_token_spans, sidebar_agent_rows,
    sidebar_section_divider_rect, sidebar_space_rows,
};
pub use state::*;
pub(super) use surface_patch::{
    ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome, PatchPresentation,
};

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::endpoint::{ClientEndpointId, ClientEndpointStatus};
use shepr_config::theme::Palette;
use shepr_config::{LiveKeybindConfig, SidebarCollapsedModeConfig, SpacesSidebarConfig};
use shepr_protocol::{
    ClientMessage, ClientMousePosition, ClientPaneInputEvent, ClientShellSnapshot,
    ClientShellWorkspace, ClientSurfaceSize, FrameData, PaneSurfaceFrame,
};

#[path = "shell/input/events.rs"]
mod input_events;
use input_events::*;

#[path = "shell/input/hit_test.rs"]
mod hit_test;
use hit_test::*;

#[path = "shell/presentation/topology.rs"]
mod topology;
use topology::*;

#[path = "shell/presentation/status.rs"]
mod status_presentation;
use status_presentation::*;

#[path = "shell/presentation/compose_pane_surface.rs"]
mod compose_pane_surface;
use compose_pane_surface::*;

#[cfg(test)]
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

#[cfg(test)]
use shepr_config::ClientConfig;

#[cfg(test)]
use shepr_termio::input::raw_input::RawInputEvent;

#[cfg(test)]
mod tests;

#[cfg(test)]
impl ClientShellState {
    /// A full surface from whichever connection the shown snapshot came from.
    /// Production names the connection (`receive_pane_surface_from`).
    pub(crate) fn receive_pane_surface(&mut self, surface: PaneSurfaceFrame) {
        self.receive_tagged_pane_surface(surface, self.active_snapshot_generation);
    }

    /// A patch from whichever connection the shown snapshot came from.
    /// Production names the connection (`apply_pane_surface_patch_from`).
    pub(crate) fn apply_pane_surface_patch(
        &mut self,
        patch: &shepr_protocol::PaneSurfacePatch,
    ) -> ClientPaneSurfacePatchOutcome {
        self.apply_tagged_pane_surface_patch(patch, self.active_snapshot_generation)
    }
}
