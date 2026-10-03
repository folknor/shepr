//! Client shell state. `TextEditor`, `TypedText` and shell endpoint requests
//! redact their contents in `Debug`; other types here may carry typed or pasted
//! text (queued keys and clipboard bytes), so log ids, lengths or kinds instead.

use crate::shell::ledger::DropReason;
use crate::shell::presentation::surfaces::Pairing;

use crate::endpoint::{ClientEndpointBootKey, ClientEndpointId};
use crate::shell::endpoints::{ClientEndpointFocusTarget, Endpoints, MachineHit, local_endpoint};
use crate::shell::input::copy_mode::CopyPipeline;
use crate::shell::input::word_selection::ClientWordSelection;
use crate::shell::ledger::Ledger;
use shepr_protocol::{ClientMessage, ClientShellSnapshot, ClientSurfaceSize, PaneSurfaceFrame};
use std::collections::HashSet;
use std::sync::Arc;

use crate::shell::input::scroll_lanes::ScrollLanes;
use crate::shell::navigation::workspace_navigation::{
    PendingWorkspaceHighlight, WorkspaceNavigationTarget,
};
use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::presentation::surfaces::PaneSurfaces;
use ratatui::layout::Rect;
use shepr_config::theme::Palette;
use shepr_config::{LiveKeybindConfig, SidebarCollapsedModeConfig, SpacesSidebarConfig};

use crate::shell::presentation::surfaces;

use crate::shell::overlays::preferences;

/// User-entered text stored in shell state, with redacted debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub(in crate::shell) struct TypedText(String);

impl TypedText {
    pub(in crate::shell) fn as_str(&self) -> &str {
        &self.0
    }

    pub(in crate::shell) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for TypedText {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for TypedText {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl std::fmt::Debug for TypedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TypedText([redacted])")
    }
}

pub struct ClientShellConfig {
    pub(in crate::shell) sidebar_width: shepr_config::SidebarWidth,
    pub(in crate::shell) sidebar_bounds: shepr_config::SidebarBounds,
    pub(in crate::shell) sidebar_start_collapsed: bool,
    pub(in crate::shell) sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    pub(in crate::shell) spaces: SpacesSidebarConfig,
    pub(in crate::shell) agents: shepr_config::AgentsSidebarConfig,
    pub(in crate::shell) agent_panel_sort: shepr_config::AgentPanelSortConfig,
    pub(in crate::shell) status_indicators: shepr_config::StatusIndicatorStyle,
    pub(in crate::shell) copy_on_select: bool,
    pub(in crate::shell) palette: Palette,
    pub(in crate::shell) keybinds: LiveKeybindConfig,
    pub(in crate::shell) prompt_new_workspace_name: bool,
    pub(in crate::shell) confirm_close: bool,
    pub(in crate::shell) mouse_capture: bool,
    pub(in crate::shell) mouse_scroll_lines: u16,
    pub(in crate::shell) right_click_passthrough_modifiers: Option<crossterm::event::KeyModifiers>,
    pub(in crate::shell) redraw_on_focus_gained: bool,
    pub(in crate::shell) preferences_path: Option<std::path::PathBuf>,
    pub(in crate::shell) preferences: preferences::ClientChromePreferences,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) struct ClientShellLayout {
    pub sidebar: Rect,
    pub pane_surface: Rect,
}

#[derive(Default)]
pub(in crate::shell) struct ShellHitMap {
    pub(in crate::shell) machines: Vec<MachineHit>,
    pub(in crate::shell) workspaces: Vec<WorkspaceHit>,
    pub(in crate::shell) workspace_body: Rect,
    pub(in crate::shell) workspace_scrollbar: Rect,
    pub(in crate::shell) workspace_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(in crate::shell) workspace_max_scroll: usize,
    pub(in crate::shell) panes: Vec<PaneHit>,
    pub(in crate::shell) pane_splits: Vec<PaneSplitHit>,
    pub(in crate::shell) agents: Vec<(Rect, shepr_protocol::PublicPaneId)>,
    pub(in crate::shell) endpoint_agents:
        Vec<(Rect, ClientEndpointId, shepr_protocol::PublicPaneId)>,
    pub(in crate::shell) agent_body: Rect,
    pub(in crate::shell) agent_scrollbar: Rect,
    pub(in crate::shell) agent_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(in crate::shell) agent_max_scroll: usize,
    pub(in crate::shell) agent_sort_toggle: Rect,
    pub(in crate::shell) sidebar_divider: Rect,
    pub(in crate::shell) sidebar_section_divider: Rect,
    pub(in crate::shell) sidebar_toggle: Rect,
    pub(in crate::shell) new_workspace: Rect,
    pub(in crate::shell) global_launcher: Rect,
    pub(in crate::shell) notification_toast: Rect,
    pub(in crate::shell) global_menu_rows: Vec<(Rect, usize)>,
    pub(in crate::shell) context_menu_rows: Vec<(Rect, usize)>,
    pub(in crate::shell) overlay_primary: Rect,
    pub(in crate::shell) overlay_clear: Rect,
    pub(in crate::shell) overlay_cancel: Rect,
    pub(in crate::shell) navigator_popup: Rect,
    pub(in crate::shell) navigator_search: Rect,
    pub(in crate::shell) navigator_rows: Vec<(Rect, ClientNavigatorTarget)>,
    pub(in crate::shell) navigator_scrollbar: Rect,
    pub(in crate::shell) navigator_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(in crate::shell) help_popup: Rect,
    pub(in crate::shell) help_scrollbar: Rect,
    pub(in crate::shell) help_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(in crate::shell) help_max_scroll: usize,
}

#[derive(Clone)]
pub(in crate::shell) struct PaneHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) inner_rect: Rect,
    pub(in crate::shell) scrollbar_rect: Option<Rect>,
    pub(in crate::shell) scroll: Option<shepr_termio::ScrollMetrics>,
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) mouse_reporting: bool,
    pub(in crate::shell) sgr_pixel_mouse: bool,
    pub(in crate::shell) pixel_width: u32,
    pub(in crate::shell) pixel_height: u32,
}

#[derive(Clone)]
pub(in crate::shell) struct PaneSplitHit {
    pub(in crate::shell) direction: shepr_protocol::PaneSurfaceSplitDirection,
    pub(in crate::shell) pos: u16,
    pub(in crate::shell) area: Rect,
    pub(in crate::shell) hit_rect: Rect,
    pub(in crate::shell) path: Vec<shepr_core::geometry::SplitBranch>,
    pub(in crate::shell) topology_signature: u64,
}

pub(in crate::shell) struct ClientPaneMouseGesture {
    pub(in crate::shell) hit: PaneHit,
    pub(in crate::shell) button: crossterm::event::MouseButton,
    pub(in crate::shell) stripped_modifiers: crossterm::event::KeyModifiers,
    pub(in crate::shell) last_event: crossterm::event::MouseEvent,
    pub(in crate::shell) last_position: shepr_protocol::ClientMousePosition,
}

pub(in crate::shell) struct ClientWorkspacePress {
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    pub(in crate::shell) workspace_id: shepr_protocol::WorkspaceId,
    pub(in crate::shell) start_column: u16,
    pub(in crate::shell) start_row: u16,
}

pub(in crate::shell) enum ClientChromeDrag {
    /// Dragging the sidebar edge. The width follows the pointer, but the endpoint is resized
    /// once, on release: each resize reflows every PTY, and one per column crossed would make
    /// every pane redraw repeatedly mid-drag.
    SidebarWidth {
        resize_pending: bool,
    },
    SidebarSection,
    WorkspaceScrollbar {
        grab_row_offset: u16,
    },
    AgentScrollbar {
        grab_row_offset: u16,
    },
    HelpScrollbar {
        grab_row_offset: u16,
    },
    NavigatorScrollbar {
        grab_row_offset: u16,
    },
    Workspace {
        source_workspace_id: shepr_protocol::WorkspaceId,
        target: Option<(Option<shepr_protocol::WorkspaceId>, u16)>,
    },
    PaneSplit {
        first_panes: Vec<shepr_protocol::PublicPaneId>,
        second_panes: Vec<shepr_protocol::PublicPaneId>,
        hit: PaneSplitHit,
        workspace_id: shepr_protocol::WorkspaceId,
        grab_offset: i32,
        last_sent_ratio: Option<shepr_core::layout::SplitRatio>,
        throttle: crate::shell::input::mouse::Throttle,
    },
    PaneScrollbar {
        hit: PaneHit,
        grab_row_offset: u16,
        last_sent_offset: Option<usize>,
        throttle: crate::shell::input::mouse::Throttle,
    },
}

pub(in crate::shell) struct WorkspaceHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    pub(in crate::shell) workspace_id: shepr_protocol::WorkspaceId,
}

/// One command bound for the active endpoint, with the id its answer comes
/// back under.
pub(crate) struct ClientShellEndpointRequest {
    pub(crate) id: String,
    pub(crate) command: shepr_protocol::command::EndpointCommand,
}

impl std::fmt::Debug for ClientShellEndpointRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientShellEndpointRequest")
            .field("id", &self.id)
            .field("command", &"[redacted]")
            .finish()
    }
}

#[derive(Debug)]
pub(crate) enum ClientShellAction {
    Endpoint {
        endpoint_id: ClientEndpointId,
        boot_id: shepr_protocol::BootId,
        request: Box<ClientShellEndpointRequest>,
    },
    ClipboardWrite(Vec<u8>),
    ActivateEndpoint {
        endpoint_id: ClientEndpointId,
        target: Option<ClientEndpointFocusTarget>,
    },
}

#[derive(Default)]
pub(crate) struct ClientShellInput {
    pub detach: bool,
    pub repaint: bool,
    /// Present the next frame in full rather than diffed against what the
    /// host terminal is assumed to show (`ui.redraw_on_focus_gained`).
    pub full_redraw: bool,
    pub resize: bool,
    pub query_host_appearance: bool,
    pub query_host_theme: bool,
    pub requests: Vec<ClientMessage>,
    pub actions: Vec<ClientShellAction>,
}

impl ClientShellInput {
    /// Folds a later outcome into this one, keeping request and action order.
    pub(crate) fn merge(&mut self, later: ClientShellInput) {
        self.detach |= later.detach;
        self.repaint |= later.repaint;
        self.resize |= later.resize;
        self.query_host_appearance |= later.query_host_appearance;
        self.query_host_theme |= later.query_host_theme;
        self.full_redraw |= later.full_redraw;
        self.requests.extend(later.requests);
        self.actions.extend(later.actions);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientShellMode {
    Terminal,
    Prefix,
    Navigate,
    Resize,
    Copy,
}

/// The live mouse range, pending focus, gestures and timers form one lifecycle.
/// Copy-mode anchors stay in `ClientCopyModeState` so focus return can rebuild
/// the projected range; click history can outlive a cleared range for double-clicks.
#[derive(Default)]
pub(in crate::shell) struct MouseSelection {
    pub(in crate::shell) selection:
        Option<shepr_vt::selection::Selection<shepr_protocol::PublicPaneId>>,
    pub(in crate::shell) focus_pending: Option<shepr_protocol::PublicPaneId>,
    pub(in crate::shell) last_pane_click: Option<ClientPaneClick>,
    pub(in crate::shell) autoscroll: Option<ClientSelectionAutoscroll>,
    pub(in crate::shell) autoscroll_deadline: Option<std::time::Instant>,
    pub(in crate::shell) highlight_clear_deadline: Option<std::time::Instant>,
    pub(in crate::shell) repaint_deadline: Option<std::time::Instant>,
    pub(in crate::shell) word_gesture: Option<ClientWordSelection>,
}

impl MouseSelection {
    /// End the range interaction while preserving click history for double-click detection.
    pub(in crate::shell) fn clear_range(&mut self) {
        self.selection = None;
        self.focus_pending = None;
        self.autoscroll = None;
        self.autoscroll_deadline = None;
        self.highlight_clear_deadline = None;
        self.repaint_deadline = None;
        self.word_gesture = None;
    }

    pub(in crate::shell) fn clear(&mut self) {
        self.clear_range();
        self.last_pane_click = None;
    }

    pub(in crate::shell) fn stop_autoscroll(&mut self) {
        self.autoscroll = None;
        self.autoscroll_deadline = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientShellOverlayKind {
    Rename,
    ConfirmClose,
    Help,
    Navigator,
    ContextMenu,
    GlobalMenu,
}

#[derive(Debug)]
pub(in crate::shell) enum ClientRenameTarget {
    NewWorkspace {
        cwd: Option<String>,
        suggested_name: String,
        label_lookup: Option<shepr_protocol::RequestId>,
    },
    Workspace {
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        pane_id: shepr_protocol::PublicPaneId,
    },
}

#[derive(Debug)]
pub(in crate::shell) struct ClientRenameOverlay {
    pub(in crate::shell) title: &'static str,
    pub(in crate::shell) input: TextEditor,
    pub(in crate::shell) target: ClientRenameTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum ClientNavigatorFilter {
    Blocked,
    Working,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientNavigatorTarget {
    Machine {
        endpoint_id: ClientEndpointId,
    },
    Workspace {
        endpoint_id: ClientEndpointId,
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        endpoint_id: ClientEndpointId,
        pane_id: shepr_protocol::PublicPaneId,
    },
}

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientNavigatorRow {
    pub(in crate::shell) depth: u8,
    pub(in crate::shell) label: String,
    pub(in crate::shell) meta: String,
    pub(in crate::shell) detail: String,
    pub(in crate::shell) agent: Option<String>,
    pub(in crate::shell) status: Option<shepr_protocol::AgentStatus>,
    pub(in crate::shell) stale: bool,
    pub(in crate::shell) current: bool,
    pub(in crate::shell) target: ClientNavigatorTarget,
}

#[derive(Debug)]
pub(in crate::shell) struct ClientNavigatorOverlay {
    pub(in crate::shell) query: TextEditor,
    pub(in crate::shell) search_focused: bool,
    pub(in crate::shell) selected: Option<ClientNavigatorTarget>,
    pub(in crate::shell) scroll: usize,
    pub(in crate::shell) filter: Option<ClientNavigatorFilter>,
}

#[derive(Debug)]
pub(in crate::shell) struct ClientHelpOverlay {
    pub(in crate::shell) query: TextEditor,
    pub(in crate::shell) search_focused: bool,
    pub(in crate::shell) scroll: usize,
}

#[derive(Debug)]
pub(in crate::shell) struct ClientGlobalMenuOverlay {
    pub(in crate::shell) highlighted: usize,
    pub(in crate::shell) launcher: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientContextMenuAction {
    Rename,
    Close,
    RenamePane,
    ClearPaneName,
    SwapWithFocusedPane,
    SplitRight,
    SplitDown,
    Zoom,
    ToggleRightClickPassthrough,
    ClosePane,
}

#[derive(Debug)]
pub(in crate::shell) enum ClientContextMenuTarget {
    Workspace {
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        pane_id: shepr_protocol::PublicPaneId,
        source_pane_id: Option<shepr_protocol::PublicPaneId>,
        has_manual_label: bool,
        right_click_passthrough: bool,
    },
}

#[derive(Debug)]
pub(in crate::shell) struct ClientContextMenuOverlay {
    pub(in crate::shell) target: ClientContextMenuTarget,
    pub(in crate::shell) x: u16,
    pub(in crate::shell) y: u16,
    pub(in crate::shell) highlighted: usize,
}

pub(in crate::shell) struct ClientContextMenuItem {
    pub(in crate::shell) label: &'static str,
    pub(in crate::shell) action: ClientContextMenuAction,
}

#[derive(Debug)]
pub(in crate::shell) struct ClientConfirmCloseOverlay {
    pub(in crate::shell) workspace_id: shepr_protocol::WorkspaceId,
    pub(in crate::shell) title: String,
    pub(in crate::shell) detail: String,
    /// Cancelling returns to Navigate mode only when the dialog came from it;
    /// otherwise the user lands back in the mode they were in.
    pub(in crate::shell) return_to_navigate: bool,
}

#[derive(Debug)]
pub(in crate::shell) enum ClientShellOverlay {
    Rename(ClientRenameOverlay),
    ConfirmClose(ClientConfirmCloseOverlay),
    Help(ClientHelpOverlay),
    Navigator(ClientNavigatorOverlay),
    ContextMenu(ClientContextMenuOverlay),
    GlobalMenu(ClientGlobalMenuOverlay),
}

impl ClientShellOverlay {
    pub(in crate::shell) fn kind(&self) -> ClientShellOverlayKind {
        match self {
            Self::Rename(_) => ClientShellOverlayKind::Rename,
            Self::ConfirmClose(_) => ClientShellOverlayKind::ConfirmClose,
            Self::Help(_) => ClientShellOverlayKind::Help,
            Self::Navigator(_) => ClientShellOverlayKind::Navigator,
            Self::ContextMenu(_) => ClientShellOverlayKind::ContextMenu,
            Self::GlobalMenu(_) => ClientShellOverlayKind::GlobalMenu,
        }
    }
}

/// Why an endpoint command failed: the client raises `Timeout`
/// itself; every other failure is the server's own typed error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientShellEndpointError {
    Timeout,
    Server(shepr_protocol::command::EndpointError),
}

impl std::fmt::Display for ClientShellEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("this server did not respond to the action"),
            Self::Server(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for ClientShellEndpointError {}

impl From<shepr_protocol::command::EndpointError> for ClientShellEndpointError {
    fn from(error: shepr_protocol::command::EndpointError) -> Self {
        Self::Server(error)
    }
}

#[derive(Debug)]
pub(crate) struct ClientPresentationLogContext {
    /// Owned: the context is taken before a presentation step that borrows
    /// the shell mutably, and is logged only if that step fails.
    pub(crate) endpoint: ClientEndpointId,
    pub(crate) generation: Option<u64>,
    pub(crate) boot_id: Option<String>,
    pub(crate) projection_revision: Option<u64>,
    pub(crate) surface_revision: Option<u64>,
    pub(crate) pane_ids: Vec<shepr_protocol::PublicPaneId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct ClientInputContext {
    pub(in crate::shell) mode: ClientShellMode,
    pub(in crate::shell) overlay: Option<ClientShellOverlayKind>,
    pub(in crate::shell) retained_selection: bool,
}

type ClientInputLeases =
    shepr_termio::input::InputLeaseTable<u8, ClientInputContext, shepr_protocol::PublicPaneId>;

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientPaneClick {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) viewport_row: u16,
    pub(in crate::shell) col: u16,
    pub(in crate::shell) at: std::time::Instant,
}

impl ClientPaneClick {
    pub(in crate::shell) fn is_double_click_for(&self, next: &Self) -> bool {
        self.pane_id == next.pane_id
            && next.at.duration_since(self.at) <= crate::limits::DOUBLE_CLICK_WINDOW
            && self.viewport_row.abs_diff(next.viewport_row) <= 1
            && self.col.abs_diff(next.col) <= 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientSelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientSelectionAutoscroll {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) direction: ClientSelectionAutoscrollDirection,
    pub(in crate::shell) last_mouse_column: u16,
    pub(in crate::shell) last_mouse_row: u16,
    pub(in crate::shell) inner_rect: Rect,
    pub(in crate::shell) offset_from_bottom: usize,
    pub(in crate::shell) max_offset_from_bottom: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientCopySelection {
    Character {
        anchor: shepr_vt::Point<shepr_vt::AbsRow>,
    },
    Linewise {
        anchor_row: shepr_vt::AbsRow,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopySearchPrompt {
    pub(in crate::shell) direction: shepr_protocol::command::PaneCopySearchDirection,
    pub(in crate::shell) query: TextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientCopyOperation {
    Motion(shepr_protocol::command::PaneCopyMotion),
    Search {
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
    },
}

pub(in crate::shell) struct ClientCopySearchResult {
    pub(in crate::shell) matches: Vec<shepr_protocol::command::PaneTextRange>,
    pub(in crate::shell) total: u64,
    pub(in crate::shell) current: Option<usize>,
    pub(in crate::shell) current_global: Option<u64>,
}

/// One live search lifecycle: prompt, query, result projection and deferred-copy intent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopySearch {
    pub(in crate::shell) prompt: Option<ClientCopySearchPrompt>,
    pub(in crate::shell) query: TypedText,
    pub(in crate::shell) direction: Option<shepr_protocol::command::PaneCopySearchDirection>,
    pub(in crate::shell) matches: Vec<shepr_protocol::command::PaneTextRange>,
    pub(in crate::shell) total: u64,
    pub(in crate::shell) current: Option<usize>,
    pub(in crate::shell) current_global: Option<u64>,
    pub(in crate::shell) copy_after_result: bool,
}

impl ClientCopySearch {
    pub(in crate::shell) fn clear_results(&mut self) {
        self.matches.clear();
        self.total = 0;
        self.current = None;
        self.current_global = None;
        self.copy_after_result = false;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopyModeState {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) scroll: shepr_vt::ScrollMetrics,
    pub(in crate::shell) geometry: (u16, u16),
    pub(in crate::shell) alternate_screen_active: bool,
    pub(in crate::shell) cursor: shepr_protocol::command::PaneTextPoint,
    pub(in crate::shell) entry_offset_from_bottom: usize,
    /// The anchor and selection shape drive the projected VT range in `MouseSelection`.
    /// They are not a duplicate range: the projection changes as the copy cursor moves.
    pub(in crate::shell) selection: Option<ClientCopySelection>,
    pub(in crate::shell) search: Option<ClientCopySearch>,
    /// Invalidates replies from searches canceled while their endpoint request is pending.
    /// It survives clearing the optional search state so late replies stay stale.
    pub(in crate::shell) operation_generation: u64,
}

/// The selected pane as the previously presented surface showed it. The selection
/// invalidation needs only this, so no previous surface is cloned, and an in-place patch
/// can capture it before changing the surface.
pub(in crate::shell) enum PreviousPane {
    /// Nothing was presented: nothing shows the selection's coordinates still describe
    /// this pane's grid, and highlighting them could mark stale cells. Invalidate.
    NoSurface,
    /// No selection, or the pane is missing from a compared surface: keep it.
    Absent,
    /// Compare with the next surface.
    Present(PaneFacts),
}
pub(in crate::shell) struct PaneFacts {
    inner_width: u16,
    inner_height: u16,
    alternate_screen_active: bool,
    content_revision: u64,
}

pub struct ClientShellState {
    /// The client loop's time for the event being handled, set on each event,
    /// so shell code that stamps deadlines never reads the clock itself.
    pub(crate) now: std::time::Instant,
    pub(in crate::shell) machine_diagnostics:
        crate::shell::overlays::machine_diagnostics::MachineDiagnostics,
    pub(in crate::shell) config: ClientShellConfig,
    pub(in crate::shell) snapshot: Option<Arc<ClientShellSnapshot>>,
    pub(in crate::shell) active_snapshot_generation: Option<u64>,
    pub(in crate::shell) agent_panel_model:
        crate::shell::navigation::aggregate_navigation::AgentPanelModel,
    pub(in crate::shell) navigator_index:
        crate::shell::navigation::aggregate_navigation::NavigatorIndex,
    pub(in crate::shell) surfaces: PaneSurfaces,
    pub(in crate::shell) ledger: Ledger,
    pub(in crate::shell) scroll_lanes: ScrollLanes,
    pub(in crate::shell) copy_pipeline: CopyPipeline,
    /// Identifies the currently active endpoint and its boot, so a switch of endpoint or a
    /// restart of its server is detectable when the next snapshot arrives.
    pub(in crate::shell) active_boot_key: Option<ClientEndpointBootKey>,
    pub(in crate::shell) chrome: crate::shell::sidebar::chrome::ChromeLayout,
    pub(in crate::shell) agent_panel_sort_chrome:
        crate::shell::sidebar::chrome::Chrome<shepr_config::AgentPanelSortConfig>,
    pub(in crate::shell) last_sidebar_divider_click: Option<std::time::Instant>,
    pub(in crate::shell) chrome_drag: Option<ClientChromeDrag>,
    pub(in crate::shell) workspace_press: Option<ClientWorkspacePress>,
    pub(in crate::shell) workspace_scroll: usize,
    pub(in crate::shell) agent_scroll: usize,
    pub(in crate::shell) pending_agent_reveal:
        Option<(ClientEndpointId, shepr_protocol::PublicPaneId)>,
    pub(in crate::shell) reveal_focused_workspace: bool,
    pub(in crate::shell) last_composed_size: Option<(u16, u16)>,
    pub(in crate::shell) last_composed_at: Option<std::time::Instant>,
    pub(in crate::shell) mouse_selection: MouseSelection,
    pub(in crate::shell) hits: ShellHitMap,
    pub(crate) endpoints: Endpoints,
    pub(in crate::shell) collapsed_endpoints: HashSet<ClientEndpointId>,
    /// This is the input-mode authority. A copy session can be parked while its pane
    /// stays focused, so focus alone cannot say whether copy input is active.
    pub(in crate::shell) mode: ClientShellMode,
    /// Navigation remains active when no workspace target exists, so the preview is optional.
    pub(in crate::shell) navigate_workspace_id: Option<WorkspaceNavigationTarget>,
    pub(in crate::shell) pending_workspace_highlight: Option<PendingWorkspaceHighlight>,
    pub(in crate::shell) reveal_navigation_workspace: bool,
    pub(in crate::shell) overlay: Option<ClientShellOverlay>,
    pub(in crate::shell) previous_pane_id: Option<shepr_protocol::PublicPaneId>,
    pub(in crate::shell) pane_mouse_gesture: Option<ClientPaneMouseGesture>,
    /// Stored copy cursor state can survive while the Copy input mode is parked.
    pub(in crate::shell) copy_mode: Option<ClientCopyModeState>,
    pub(in crate::shell) host_mouse_pixels: Option<shepr_termio::input::mouse::HostPixels>,
    pub(in crate::shell) input_leases: ClientInputLeases,
    /// Whether the host sends every key, text keys included, as an escape
    /// code with its release (kitty REPORT_ALL_KEYS). Set per host input batch.
    pub(in crate::shell) host_reports_all_keys: bool,
    pub(in crate::shell) notices: crate::shell::overlays::notices::Notices,
    pub(in crate::shell) outer_focused: Option<bool>,
    pub(in crate::shell) host_background: Option<shepr_termio::host_term::theme::RgbColor>,
    pub(in crate::shell) endpoint_error: crate::shell::overlays::transient_error::TransientError,
}

/// Drops search matches whose rows scrolled out of history. They name rows
/// that no longer exist, so they could never render or be copied; the oldest
/// rows go first, so each dropped match was ahead of the current one in the
/// server's global count.
fn prune_evicted_search_matches(copy_mode: &mut ClientCopyModeState) {
    let Some(search) = copy_mode.search.as_mut() else {
        return;
    };
    let origin = copy_mode.scroll.history_origin;
    let before = search.matches.len();
    let current = search
        .current
        .and_then(|index| search.matches.get(index).copied());
    let current_global = search.current_global;
    search.matches.retain(|found| found.start.row >= origin);
    let removed = before - search.matches.len();
    if removed == 0 {
        return;
    }
    let removed = u64::try_from(removed).unwrap_or(u64::MAX);
    search.total = search.total.saturating_sub(removed);
    search.current =
        current.and_then(|current| search.matches.iter().position(|found| *found == current));
    search.current_global = match search.current {
        Some(_) => current_global.map(|global| global.saturating_sub(removed)),
        None => None,
    };
}

impl ClientShellState {
    pub fn new_at(mut config: ClientShellConfig, now: std::time::Instant) -> Self {
        let preferences = config.preferences.clone();
        let overlay = None;
        let chrome = crate::shell::sidebar::chrome::ChromeLayout::new(&config);
        let sort_origin = if preferences.configured.agent_panel_sort {
            crate::shell::sidebar::chrome::ChromeOrigin::Configured
        } else if preferences.agent_panel_sort.is_some() {
            crate::shell::sidebar::chrome::ChromeOrigin::Remembered
        } else {
            crate::shell::sidebar::chrome::ChromeOrigin::Default
        };
        let agent_panel_sort = if preferences.configured.agent_panel_sort {
            config.agent_panel_sort
        } else {
            preferences
                .agent_panel_sort
                .unwrap_or(config.agent_panel_sort)
        };
        config.agent_panel_sort = agent_panel_sort;
        let agent_panel_sort_chrome =
            crate::shell::sidebar::chrome::Chrome::new(agent_panel_sort, sort_origin);
        let endpoints = vec![local_endpoint()];
        let agent_panel_model =
            crate::shell::navigation::aggregate_navigation::AgentPanelModel::build(
                &endpoints, &config,
            );
        let navigator_index =
            crate::shell::navigation::aggregate_navigation::NavigatorIndex::build(&endpoints);
        Self {
            now,
            machine_diagnostics: Default::default(),
            config,
            snapshot: None,
            active_snapshot_generation: None,
            agent_panel_model,
            navigator_index,
            surfaces: PaneSurfaces::default(),
            ledger: Ledger::default(),
            scroll_lanes: ScrollLanes::default(),
            copy_pipeline: CopyPipeline::default(),
            active_boot_key: None,
            chrome,
            agent_panel_sort_chrome,
            last_sidebar_divider_click: None,
            chrome_drag: None,
            workspace_press: None,
            workspace_scroll: 0,
            agent_scroll: 0,
            pending_agent_reveal: None,
            reveal_focused_workspace: true,
            last_composed_size: None,
            last_composed_at: None,
            mouse_selection: MouseSelection::default(),
            hits: ShellHitMap::default(),
            endpoints: Endpoints::new(endpoints),
            collapsed_endpoints: HashSet::new(),
            mode: ClientShellMode::Terminal,
            navigate_workspace_id: None,
            pending_workspace_highlight: None,
            reveal_navigation_workspace: false,
            overlay,
            previous_pane_id: None,
            pane_mouse_gesture: None,
            copy_mode: None,
            host_mouse_pixels: None,
            input_leases: ClientInputLeases::default(),
            host_reports_all_keys: false,
            notices: Default::default(),
            outer_focused: None,
            host_background: None,
            endpoint_error: Default::default(),
        }
    }

    pub(crate) fn presentation_log_context(&self) -> ClientPresentationLogContext {
        let surface = self.pane_surface();
        let mut pane_ids: Vec<_> = surface.map_or_else(Vec::new, |surface| {
            surface
                .panes
                .iter()
                .filter(|pane| pane.focused)
                .map(|pane| pane.pane_id.clone())
                .collect()
        });
        if pane_ids.is_empty()
            && let Some(pane_id) = self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_pane_id.clone())
        {
            pane_ids.push(pane_id);
        }
        let snapshot = self.snapshot.as_deref();
        ClientPresentationLogContext {
            endpoint: self.endpoints.presented().clone(),
            generation: self.active_snapshot_generation,
            boot_id: surface
                .map(|surface| surface.boot_id.to_string())
                .or_else(|| snapshot.map(|snapshot| snapshot.boot_id.to_string())),
            projection_revision: surface
                .map(|surface| surface.projection_revision.get())
                .or_else(|| snapshot.map(|snapshot| snapshot.revision.get())),
            surface_revision: surface.map(|surface| surface.surface_revision.get()),
            pane_ids,
        }
    }

    pub(in crate::shell) fn reveal_workspace(
        &mut self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) {
        if self.endpoints.len() > 1 {
            // The multi-endpoint sidebar scrolls a flattened row list with endpoint headers
            // and workspace row gaps. An index in this endpoint's snapshot is not that list
            // offset, so let the sidebar reveal the focused workspace from the next snapshot.
            self.collapsed_endpoints.remove(self.endpoints.presented());
            if self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_workspace_id.as_deref())
                == Some(workspace_id.as_str())
            {
                self.reveal_focused_workspace = true;
            }
            return;
        }
        if self.hits.workspaces.iter().any(|hit| {
            hit.endpoint_id == *self.endpoints.presented() && hit.workspace_id == *workspace_id
        }) {
            return;
        }
        let target = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == *workspace_id)
        });
        let (Some(target), Some(snapshot)) = (target, self.snapshot.as_deref()) else {
            return;
        };
        let row_heights = if self.chrome.collapsed() {
            vec![1; snapshot.workspaces.len()]
        } else {
            snapshot
                .workspaces
                .iter()
                .map(|workspace| {
                    u16::try_from(
                        crate::shell::sidebar::workspace_rows(
                            workspace,
                            workspace.agent_status,
                            &self.config.spaces,
                        )
                        .len()
                        .max(1),
                    )
                    .unwrap_or(u16::MAX)
                })
                .collect()
        };
        let mut gaps = if self.chrome.collapsed() {
            vec![0; row_heights.len()]
        } else {
            vec![self.config.spaces.row_gap; row_heights.len()]
        };
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.workspace_scroll = crate::shell::navigation::scroll::list_scroll_start_to_reveal(
            &row_heights,
            &gaps,
            self.hits.workspace_body.height,
            self.workspace_scroll,
            target,
        );
    }

    pub(in crate::shell) fn layout(&self, cols: u16, rows: u16) -> ClientShellLayout {
        self.config
            .layout(cols, rows, self.chrome.collapsed(), self.chrome.width())
    }

    /// The pane surface size under the client's layout.
    pub(crate) fn surface_size(&self, cols: u16, rows: u16) -> ClientSurfaceSize {
        let surface = self.layout(cols, rows).pane_surface;
        ClientSurfaceSize {
            cols: surface.width.max(1),
            rows: surface.height.max(1),
        }
        .clamped()
    }

    pub(in crate::shell) fn reset_endpoint_projection(&mut self) {
        self.hits = ShellHitMap::default();
        self.surfaces = PaneSurfaces::default();
        self.input_leases = ClientInputLeases::default();
        self.chrome_drag = None;
        self.workspace_press = None;
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        self.reveal_focused_workspace = true;
        self.last_composed_size = None;
        self.last_composed_at = None;
        self.mouse_selection.clear();
        self.drop_all_requests(DropReason::Reset);
        self.scroll_lanes.clear();
        self.notices.reset_endpoint();
        self.endpoint_error.dismiss();
        self.navigate_workspace_id = None;
        self.pending_workspace_highlight = None;
        self.overlay = None;
        self.previous_pane_id = None;
        self.pane_mouse_gesture = None;
        self.mouse_selection.clear();
        self.copy_mode = None;
        if self.mode == ClientShellMode::Copy {
            self.mode = ClientShellMode::Terminal;
        }
        self.reset_copy_pipeline();
        self.host_mouse_pixels = None;
    }

    pub(in crate::shell) fn apply_active_snapshot(
        &mut self,
        snapshot: Arc<ClientShellSnapshot>,
        generation: Option<u64>,
    ) {
        let active_boot_key = Some(ClientEndpointBootKey::new(
            self.endpoints.presented(),
            &snapshot.boot_id,
        ));
        let endpoint_boot_changed =
            self.snapshot.is_some() && self.active_boot_key != active_boot_key;
        let generation_changed = self.active_snapshot_generation != generation;
        if !endpoint_boot_changed
            && !generation_changed
            && self.snapshot.as_ref().is_some_and(|current| {
                current.boot_id == snapshot.boot_id && snapshot.revision < current.revision
            })
        {
            return;
        }
        // A new connection holds the last presented pair and keeps only a baseline it
        // sent itself: its first surface may arrive before its first snapshot.
        if generation_changed {
            self.surfaces.snapshot_generation_changed(generation);
        }
        self.active_snapshot_generation = generation;
        self.active_boot_key = active_boot_key;
        let boot_changed = endpoint_boot_changed
            || self
                .snapshot
                .as_ref()
                .is_some_and(|current| current.boot_id != snapshot.boot_id);
        // The hit map describes the last composed frame, which stays on screen until the
        // snapshot's matching surface is composed. Clicks are aimed at that frame, so its hits
        // stay live through the gap: emptying them dropped clicks, made copy-mode entry fail
        // silently, and let a click inside Help or the navigator close it. Targets the new
        // snapshot removed are rejected by the endpoint like any other stale ID. A reboot is
        // different (IDs can be reused), and `reset_endpoint_projection` clears the hits.
        if boot_changed {
            // A reboot must not turn Enter on a stale preview into focus on a reused ID.
            let preview = (self.mode == ClientShellMode::Navigate)
                .then(|| self.navigate_workspace_id.take())
                .flatten();
            // The reset drops everything presented, but not a baseline the incoming
            // connection already sent for this boot: its next patch follows it.
            let mut surfaces = std::mem::take(&mut self.surfaces);
            surfaces.reset_for_boot(&snapshot.boot_id, generation);
            self.reset_endpoint_projection();
            self.surfaces = surfaces;
            self.navigate_workspace_id = preview;
        } else if let Some(previous) = self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_pane_id.as_ref())
            .filter(|previous| Some(previous.as_str()) != snapshot.focused_pane_id.as_deref())
        {
            self.previous_pane_id = Some(previous.clone());
        }
        if self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_workspace_id.as_deref())
            != snapshot.focused_workspace_id.as_deref()
        {
            self.reveal_focused_workspace = true;
        }
        let selection_focus_lost = if let Some(gesture) = self.mouse_selection.word_gesture.as_mut()
        {
            let focused_pane = snapshot.focused_pane_id.as_deref();
            // Remember confirmed focus across intermediate snapshots with no
            // focused pane, without rejecting the gesture's in-flight focus request.
            gesture.focus_confirmed |= focused_pane == Some(gesture.pane_id.as_str());
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == gesture.pane_id)
                || (gesture.focus_confirmed
                    && focused_pane.is_some_and(|pane_id| pane_id != gesture.pane_id.as_str()))
        } else if let Some(selection) = self.mouse_selection.selection.as_ref() {
            let focused_pane = snapshot.focused_pane_id.as_deref();
            let focused_here = focused_pane == Some(selection.pane_id.as_str());
            // Like the word-gesture guard above: a selection started in an
            // unfocused pane survives snapshots that predate its focus
            // request, and only a focus change after that ends it.
            let awaiting_focus = !focused_here
                && self.mouse_selection.focus_pending.as_deref()
                    == Some(selection.pane_id.as_str());
            if focused_here {
                self.mouse_selection.focus_pending = None;
            }
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == selection.pane_id)
                || (!focused_here && !awaiting_focus)
        } else {
            false
        };
        if selection_focus_lost {
            self.mouse_selection.clear();
        }
        // Snapshot reconciliation reactivates or parks the session as focus settles. A session
        // can also be parked explicitly while its pane remains focused, so this is not derived
        // from focus alone.
        if let Some(copy_pane_id) = self
            .copy_mode
            .as_ref()
            .map(|copy_mode| copy_mode.pane_id.clone())
        {
            let pane_exists = snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == copy_pane_id);
            let pane_focused = self.copy_mode.as_ref().is_some_and(|copy_mode| {
                copy_mode.pane_is_focused(snapshot.focused_pane_id.as_deref())
            });
            if !pane_exists {
                // Queued copy-mode keys belonged to this removed pane. Replaying
                // them as input into another pane would be dangerous.
                self.copy_mode = None;
                self.reset_copy_pipeline();
                if self
                    .mouse_selection
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.mouse_selection.clear();
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            } else if pane_focused {
                if self.mode == ClientShellMode::Terminal {
                    self.mode = ClientShellMode::Copy;
                }
                if self.mouse_selection.selection.is_none() {
                    self.sync_copy_selection();
                }
            } else {
                if self
                    .mouse_selection
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.mouse_selection.clear();
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            }
        }
        if self.mode == ClientShellMode::Navigate && self.navigate_workspace_id.is_none() {
            self.navigate_workspace_id = snapshot
                .focused_workspace_id
                .as_ref()
                .and_then(|id| self.navigation_target(self.endpoints.presented(), id));
        }
        let pane_exists = |pane_id: &shepr_protocol::PublicPaneId| {
            snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id)
        };
        self.scroll_lanes.retain_panes(pane_exists);

        self.snapshot = Some(snapshot);
        self.reconcile_pending_workspace_highlight();
        self.pair_surfaces();
    }

    /// The presented surface: what is on screen, possibly held while unpaired. Input
    /// reads this.
    pub(in crate::shell) fn pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        self.surfaces.presented()
    }

    /// A full surface from the shown connection `generation`. It becomes the baseline
    /// (the reader enforces order and the shell mirrors it) and is presented once it
    /// pairs with that connection's snapshot, which may arrive after it.
    pub(crate) fn receive_pane_surface_from(&mut self, surface: PaneSurfaceFrame, generation: u64) {
        self.receive_tagged_pane_surface(surface, Some(generation));
    }

    pub(in crate::shell) fn receive_tagged_pane_surface(
        &mut self,
        surface: PaneSurfaceFrame,
        generation: surfaces::SurfaceGeneration,
    ) {
        // Routing admits only the shown connection, and generations only grow. This is
        // the guard behind it: a surface from an older connection than the snapshot or
        // the baseline must not replace a newer connection's baseline.
        let newest = self
            .active_snapshot_generation
            .max(self.surfaces.baseline_generation().flatten());
        if generation < newest {
            tracing::warn!(
                ?generation,
                ?newest,
                "dropping a pane surface from an older connection"
            );
            return;
        }
        self.surfaces.receive(surface, generation);
        self.pair_surfaces();
    }

    /// Pairs the baseline with the snapshot and, when that changes what is presented,
    /// runs the presentation effects. The effects never read `self.surfaces`, so it is
    /// moved out around them; a panic in between leaves `Empty`, a valid state.
    fn pair_surfaces(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if let Pairing::Presented { previous } = self.surfaces.pair(
            &snapshot.boot_id,
            snapshot.revision,
            self.active_snapshot_generation,
        ) {
            let before = self.pane_facts_before(previous.as_ref());
            let surfaces = std::mem::take(&mut self.surfaces);
            if let Some(surface) = surfaces.paired() {
                self.presented_surface_changed(before, surface);
            }
            self.surfaces = surfaces;
        }
    }

    /// Captures the selected (or word-gesture) pane as `previous` showed it.
    pub(in crate::shell) fn pane_facts_before(
        &self,
        previous: Option<&PaneSurfaceFrame>,
    ) -> PreviousPane {
        let pane_id = self
            .mouse_selection
            .word_gesture
            .as_ref()
            .map(|g| &g.pane_id)
            .or_else(|| self.mouse_selection.selection.as_ref().map(|s| &s.pane_id));
        let Some(pane_id) = pane_id else {
            return PreviousPane::Absent;
        };
        let Some(surface) = previous else {
            return PreviousPane::NoSurface;
        };
        surface
            .panes
            .iter()
            .find(|p| &p.pane_id == pane_id)
            .map_or(PreviousPane::Absent, |p| {
                PreviousPane::Present(PaneFacts {
                    inner_width: p.inner_rect.width,
                    inner_height: p.inner_rect.height,
                    alternate_screen_active: p.alternate_screen_active,
                    content_revision: p.content_revision,
                })
            })
    }

    /// A presented surface or patch shows `scroll` for `pane`: a scroll target it shows
    /// is done.
    pub(in crate::shell) fn scroll_target_shown(
        &mut self,
        pane: &shepr_protocol::PublicPaneId,
        scroll: Option<shepr_protocol::PaneSurfaceScrollMetrics>,
    ) {
        if let Some(scroll) = scroll {
            self.scroll_lanes.shown(
                pane,
                scroll.offset_from_bottom,
                scroll.max_offset_from_bottom,
            );
        }
    }

    /// Effects of a change to the presented surface, which the caller stores: invalidates
    /// a selection or word gesture whose pane changed size or screen, drops scroll targets
    /// the surface shows, and refreshes copy-mode geometry and clamping.
    pub(in crate::shell) fn presented_surface_changed(
        &mut self,
        before: PreviousPane,
        surface: &PaneSurfaceFrame,
    ) {
        let pane_id = self
            .mouse_selection
            .word_gesture
            .as_ref()
            .map(|g| &g.pane_id)
            .or_else(|| self.mouse_selection.selection.as_ref().map(|s| &s.pane_id));
        let next = pane_id.and_then(|id| surface.panes.iter().find(|p| &p.pane_id == id));
        let selection_invalidated = match (before, next) {
            (PreviousPane::NoSurface, _) => true,
            (PreviousPane::Present(previous), Some(next)) => {
                previous.inner_width != next.inner_rect.width
                    || previous.inner_height != next.inner_rect.height
                    || previous.alternate_screen_active != next.alternate_screen_active
                    // Ordinary selections are live buffer ranges. Only word gestures
                    // cache content-dependent boundaries that output can invalidate.
                    || (self.mouse_selection.word_gesture.is_some()
                        && previous.content_revision != next.content_revision)
            }
            _ => false,
        };
        if selection_invalidated {
            self.mouse_selection.clear_range();
        }
        for pane in &surface.panes {
            self.scroll_target_shown(&pane.pane_id, pane.scroll);
        }
        let mut invalidated_copy_pane = None;
        let mut clamped_copy_coordinates = false;
        if let Some(copy_mode) = self.copy_mode.as_mut()
            && let Some(pane) = surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == copy_mode.pane_id)
        {
            let geometry = (pane.inner_rect.width, pane.inner_rect.height);
            let coordinates_changed = copy_mode.geometry != geometry
                || copy_mode.alternate_screen_active != pane.alternate_screen_active;
            // Copy-mode points are absolute rows, so output alone moves nothing; a
            // resize or a screen switch reflows or replaces the rows they name.
            if coordinates_changed {
                copy_mode.geometry = geometry;
                copy_mode.alternate_screen_active = pane.alternate_screen_active;
                copy_mode.selection = None;
                invalidated_copy_pane = Some(copy_mode.pane_id.clone());
                if let Some(search) = copy_mode.search.as_mut() {
                    search.clear_results();
                }
                copy_mode.operation_generation = copy_mode.operation_generation.saturating_add(1);
            }
            if let Some(scroll) = pane.scroll {
                let offset = if self.scroll_lanes.target(&pane.pane_id).is_none() {
                    scroll.offset_from_bottom
                } else {
                    copy_mode.scroll.offset_from_bottom
                };
                copy_mode.scroll = scroll.with_offset(offset);
                let retained_cursor_row = copy_mode.retained_row(copy_mode.cursor.row);
                clamped_copy_coordinates |= retained_cursor_row != copy_mode.cursor.row;
                copy_mode.cursor.row = retained_cursor_row;
                if let Some(mut selection) = copy_mode.selection {
                    match &mut selection {
                        ClientCopySelection::Character { anchor } => {
                            let retained_row = copy_mode.retained_row(anchor.row);
                            clamped_copy_coordinates |= retained_row != anchor.row;
                            anchor.row = retained_row;
                        }
                        ClientCopySelection::Linewise { anchor_row } => {
                            let retained_row = copy_mode.retained_row(*anchor_row);
                            clamped_copy_coordinates |= retained_row != *anchor_row;
                            *anchor_row = retained_row;
                        }
                    }
                    copy_mode.selection = Some(selection);
                }
                prune_evicted_search_matches(copy_mode);
            }
        }
        if clamped_copy_coordinates {
            self.sync_copy_selection();
        }
        if invalidated_copy_pane.as_ref().is_some_and(|pane_id| {
            self.mouse_selection
                .selection
                .as_ref()
                .is_some_and(|selection| &selection.pane_id == pane_id)
        }) {
            self.mouse_selection.clear();
        }
    }

    pub(crate) fn tick_selection_highlight(&mut self, now: std::time::Instant) -> bool {
        let mut repaint = false;
        if self
            .mouse_selection
            .highlight_clear_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.mouse_selection.clear_range();
            repaint = true;
        }
        repaint
    }

    pub(in crate::shell) fn set_endpoint_error(
        &mut self,
        message: impl Into<String>,
        now: std::time::Instant,
    ) {
        self.endpoint_error.set(message, now);
    }

    pub(in crate::shell) fn tick_transient_banners(&mut self, now: std::time::Instant) -> bool {
        self.notices.tick(now)
    }

    pub(crate) fn next_timer_deadline(&self) -> Option<std::time::Instant> {
        SHELL_TIMERS
            .iter()
            .filter_map(|timer| (timer.deadline)(self))
            .min()
    }

    /// Runs the registered timer handlers in one pass, retaining action and request order.
    pub(crate) fn tick_timers(&mut self, now: std::time::Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        for timer in SHELL_TIMERS {
            outcome.merge((timer.tick)(self, now));
        }
        outcome
    }
}

struct ShellTimer {
    deadline: fn(&ClientShellState) -> Option<std::time::Instant>,
    tick: fn(&mut ClientShellState, std::time::Instant) -> ClientShellInput,
}

// This is the single inventory used for both the client's wake deadline and timer work.
const SHELL_TIMERS: &[ShellTimer] = &[
    ShellTimer {
        deadline: selection_timer_deadline,
        tick: tick_selection_timer,
    },
    ShellTimer {
        deadline: selection_highlight_deadline,
        tick: tick_selection_highlight_timer,
    },
    ShellTimer {
        deadline: workspace_highlight_deadline,
        tick: tick_workspace_highlight_timer,
    },
    ShellTimer {
        deadline: endpoint_error_deadline,
        tick: tick_endpoint_error_timer,
    },
    ShellTimer {
        deadline: endpoint_notice_deadline,
        tick: tick_endpoint_notice_timer,
    },
];

fn selection_timer_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell
        .mouse_selection
        .autoscroll_deadline
        .into_iter()
        .chain(shell.mouse_selection.repaint_deadline)
        .min()
}

fn selection_highlight_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell.mouse_selection.highlight_clear_deadline
}

fn workspace_highlight_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell.workspace_highlight_deadline()
}

fn endpoint_error_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell.endpoint_error.deadline()
}

fn endpoint_notice_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell.notices.deadline()
}

fn tick_selection_timer(shell: &mut ClientShellState, now: std::time::Instant) -> ClientShellInput {
    shell.tick_selection_autoscroll(now)
}

fn tick_selection_highlight_timer(
    shell: &mut ClientShellState,
    now: std::time::Instant,
) -> ClientShellInput {
    repaint_timer_outcome(shell.tick_selection_highlight(now))
}

fn tick_workspace_highlight_timer(
    shell: &mut ClientShellState,
    now: std::time::Instant,
) -> ClientShellInput {
    repaint_timer_outcome(shell.tick_workspace_highlight(now))
}

fn tick_endpoint_error_timer(
    shell: &mut ClientShellState,
    now: std::time::Instant,
) -> ClientShellInput {
    repaint_timer_outcome(shell.endpoint_error.tick(now))
}

fn tick_endpoint_notice_timer(
    shell: &mut ClientShellState,
    now: std::time::Instant,
) -> ClientShellInput {
    repaint_timer_outcome(shell.tick_transient_banners(now))
}

fn repaint_timer_outcome(repaint: bool) -> ClientShellInput {
    ClientShellInput {
        repaint,
        ..ClientShellInput::default()
    }
}

#[cfg(test)]
impl ClientShellInput {
    pub(crate) fn into_parts(self) -> (bool, Vec<ClientShellAction>) {
        (self.repaint, self.actions)
    }
}

#[cfg(test)]
impl ClientShellState {
    pub fn new(config: ClientShellConfig) -> Self {
        // clock-io-ok: this test-only constructor stands in for the client launch.
        Self::new_at(config, std::time::Instant::now())
    }

    /// Drops the retained pane surface, leaving `compose` on its no-surface placeholder. Resize
    /// and sidebar changes no longer do this (the retained surface is drawn clipped until the
    /// resized one arrives); tests use it to reach the placeholder.
    pub(crate) fn invalidate_pane_surface(&mut self) {
        self.surfaces = PaneSurfaces::default();
        self.hits = ShellHitMap::default();
        self.host_mouse_pixels = None;
    }
}
