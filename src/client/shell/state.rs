use super::*;

pub(super) const MIN_TAB_WIDTH: u16 = 8;
pub(super) const NEW_TAB_WIDTH: u16 = 3;
pub(super) const WORKSPACE_HEADER_ROWS: u16 = 2;
const ENDPOINT_ERROR_TIMEOUT_SECS: u64 = 5;
/// How long a config diagnostic banner stays up before it hides itself. A click on the banner
/// hides it sooner. Without a lifetime it would sit over the panes for the whole session, and
/// while any banner is up every pane update takes the full-compose path.
const CONFIG_DIAGNOSTIC_TIMEOUT_SECS: u64 = 20;
/// How long an endpoint notice card stays up before it hides itself. A click on the card hides
/// it sooner; the timeout is what dismisses it when `ui.mouse_capture` is off.
const ENDPOINT_NOTICE_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientShellKeybindingSource {
    RemoteLocal,
    Endpoint,
}

pub(crate) struct ClientShellConfig {
    pub(super) sidebar_width: u16,
    pub(super) sidebar_min_width: u16,
    pub(super) sidebar_max_width: u16,
    pub(super) sidebar_start_collapsed: bool,
    pub(super) sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    pub(super) tab_bar_position: TabBarPositionConfig,
    pub(super) hide_tab_bar_when_single_tab: bool,
    pub(super) spaces: SpacesSidebarConfig,
    pub(super) agents: crate::config::AgentsSidebarConfig,
    pub(super) agent_panel_sort: crate::config::AgentPanelSortConfig,
    pub(super) status_indicators: crate::config::StatusIndicatorStyle,
    pub(super) copy_on_select: bool,
    pub(super) palette: Palette,
    pub(super) keybinds: LiveKeybindConfig,
    pub(super) keybinding_source: ClientShellKeybindingSource,
    pub(super) prompt_new_tab_name: bool,
    pub(super) prompt_new_workspace_name: bool,
    pub(super) confirm_close: bool,
    pub(super) mouse_capture: bool,
    pub(super) mouse_scroll_lines: usize,
    pub(super) right_click_passthrough_modifiers: Option<crossterm::event::KeyModifiers>,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) preferences_path: Option<std::path::PathBuf>,
    pub(super) preferences: preferences::ClientChromePreferences,
    pub(super) startup_config_diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientShellLayout {
    pub sidebar: Rect,
    pub tab_bar: Rect,
    pub pane_surface: Rect,
}

#[derive(Default)]
pub(super) struct ShellHitMap {
    pub(super) machines: Vec<MachineHit>,
    pub(super) workspaces: Vec<WorkspaceHit>,
    pub(super) workspace_body: Rect,
    pub(super) workspace_scrollbar: Rect,
    pub(super) workspace_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) workspace_max_scroll: usize,
    pub(super) tabs: Vec<(Rect, String)>,
    pub(super) panes: Vec<PaneHit>,
    pub(super) pane_splits: Vec<PaneSplitHit>,
    pub(super) agents: Vec<(Rect, String)>,
    pub(super) endpoint_agents: Vec<(Rect, ClientEndpointId, String)>,
    pub(super) agent_body: Rect,
    pub(super) agent_scrollbar: Rect,
    pub(super) agent_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) agent_max_scroll: usize,
    pub(super) agent_sort_toggle: Rect,
    pub(super) sidebar_divider: Rect,
    pub(super) sidebar_section_divider: Rect,
    pub(super) sidebar_toggle: Rect,
    pub(super) new_workspace: Rect,
    pub(super) new_tab: Rect,
    pub(super) tab_scroll_left: Rect,
    pub(super) tab_scroll_right: Rect,
    pub(super) global_launcher: Rect,
    pub(super) notification_toast: Rect,
    pub(super) config_diagnostic: Rect,
    pub(super) global_menu_rows: Vec<(Rect, usize)>,
    pub(super) context_menu_rows: Vec<(Rect, usize)>,
    pub(super) overlay_primary: Rect,
    pub(super) overlay_clear: Rect,
    pub(super) overlay_cancel: Rect,
    pub(super) navigator_popup: Rect,
    pub(super) navigator_search: Rect,
    pub(super) navigator_rows: Vec<(Rect, ClientNavigatorTarget)>,
    pub(super) navigator_scrollbar: Rect,
    pub(super) navigator_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) help_popup: Rect,
    pub(super) help_scrollbar: Rect,
    pub(super) help_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) help_max_scroll: usize,
}

#[derive(Clone)]
pub(super) struct PaneHit {
    pub(super) rect: Rect,
    pub(super) inner_rect: Rect,
    pub(super) scrollbar_rect: Option<Rect>,
    pub(super) scroll: Option<crate::pane::ScrollMetrics>,
    pub(super) pane_id: String,
    pub(super) mouse_reporting: bool,
    pub(super) sgr_pixel_mouse: bool,
    pub(super) pixel_width: u32,
    pub(super) pixel_height: u32,
}

#[derive(Clone)]
pub(super) struct PaneSplitHit {
    pub(super) direction: crate::protocol::PaneSurfaceSplitDirection,
    pub(super) pos: u16,
    pub(super) area: Rect,
    pub(super) hit_rect: Rect,
    pub(super) path: Vec<bool>,
    pub(super) topology_signature: u64,
}

pub(super) struct ClientPaneMouseGesture {
    pub(super) hit: PaneHit,
    pub(super) button: crossterm::event::MouseButton,
    pub(super) stripped_modifiers: crossterm::event::KeyModifiers,
    pub(super) last_event: crossterm::event::MouseEvent,
    pub(super) last_position: crate::protocol::ClientMousePosition,
}

pub(super) struct ClientWorkspacePress {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    pub(super) start_column: u16,
    pub(super) start_row: u16,
}

pub(super) struct ClientTabPress {
    pub(super) tab_id: String,
    pub(super) workspace_id: String,
    pub(super) start_column: u16,
    pub(super) start_row: u16,
}

pub(super) enum ClientChromeDrag {
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
    Tab {
        tab_id: String,
        workspace_id: String,
        insert_index: Option<usize>,
    },
    Workspace {
        source_workspace_id: String,
        target: Option<(Option<String>, u16)>,
    },
    PaneSplit {
        hit: PaneSplitHit,
        tab_id: String,
        grab_offset: i32,
        last_sent_ratio: Option<f32>,
        last_sent_at: Option<std::time::Instant>,
    },
    PaneScrollbar {
        hit: PaneHit,
        grab_row_offset: u16,
        last_sent_offset: Option<usize>,
        last_sent_at: Option<std::time::Instant>,
    },
}

pub(super) struct WorkspaceHit {
    pub(super) rect: Rect,
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
}

#[derive(Debug)]
pub(crate) enum ClientShellAction {
    Endpoint {
        endpoint_id: ClientEndpointId,
        boot_id: String,
        request: Box<crate::api::schema::Request>,
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

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (bool, Vec<ClientShellAction>) {
        (self.repaint, self.actions)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientShellMode {
    Terminal,
    Prefix,
    Navigate,
    Resize,
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientShellOverlayKind {
    Rename,
    ConfirmClose,
    Help,
    Navigator,
    ContextMenu,
    GlobalMenu,
}

#[derive(Debug)]
pub(super) enum ClientRenameTarget {
    NewWorkspace {
        source_workspace_id: Option<String>,
        cwd: Option<String>,
        suggested_name: String,
    },
    Workspace {
        workspace_id: String,
    },
    NewTab {
        workspace_id: String,
        default_name: String,
    },
    Tab {
        tab_id: String,
        auto_name: bool,
        original_name: String,
    },
    Pane {
        pane_id: String,
    },
}

#[derive(Debug)]
pub(super) struct ClientRenameOverlay {
    pub(super) title: &'static str,
    pub(super) input: TextEditor,
    pub(super) target: ClientRenameTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientNavigatorFilter {
    Blocked,
    Working,
    Idle,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientNavigatorTarget {
    Machine {
        endpoint_id: ClientEndpointId,
    },
    Workspace {
        endpoint_id: ClientEndpointId,
        workspace_id: String,
    },
    Pane {
        endpoint_id: ClientEndpointId,
        pane_id: String,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ClientNavigatorRow {
    pub(super) depth: u8,
    pub(super) label: String,
    pub(super) meta: String,
    pub(super) detail: String,
    pub(super) agent: Option<String>,
    pub(super) status: Option<crate::api::schema::AgentStatus>,
    pub(super) stale: bool,
    pub(super) current: bool,
    pub(super) target: ClientNavigatorTarget,
}

#[derive(Debug)]
pub(super) struct ClientNavigatorOverlay {
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) selected: Option<ClientNavigatorTarget>,
    pub(super) scroll: usize,
    pub(super) filter: Option<ClientNavigatorFilter>,
}

#[derive(Debug)]
pub(super) struct ClientHelpOverlay {
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) scroll: usize,
}

#[derive(Debug)]
pub(super) struct ClientGlobalMenuOverlay {
    pub(super) highlighted: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientContextMenuAction {
    Rename,
    Close,
    NewTab,
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
pub(super) enum ClientContextMenuTarget {
    Workspace {
        workspace_id: String,
    },
    Tab {
        tab_id: String,
        workspace_id: String,
    },
    Pane {
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        has_manual_label: bool,
        right_click_passthrough: bool,
    },
}

#[derive(Debug)]
pub(super) struct ClientContextMenuOverlay {
    pub(super) target: ClientContextMenuTarget,
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) highlighted: usize,
}

pub(super) struct ClientContextMenuItem {
    pub(super) label: &'static str,
    pub(super) action: ClientContextMenuAction,
}

#[derive(Debug)]
pub(super) struct ClientTabCloseConfirmation {
    pub(super) tab_id: String,
    pub(super) workspace: WorkspaceNavigationTarget,
}

#[derive(Debug)]
pub(super) struct ClientConfirmCloseOverlay {
    pub(super) workspace_id: String,
    pub(super) tab_target: Option<ClientTabCloseConfirmation>,
    pub(super) title: String,
    pub(super) detail: String,
    /// Cancelling returns to Navigate mode only when the dialog came from it;
    /// otherwise the user lands back in the mode they were in.
    pub(super) return_to_navigate: bool,
}

#[derive(Debug)]
pub(super) enum ClientShellOverlay {
    Rename(ClientRenameOverlay),
    ConfirmClose(ClientConfirmCloseOverlay),
    Help(ClientHelpOverlay),
    Navigator(ClientNavigatorOverlay),
    ContextMenu(ClientContextMenuOverlay),
    GlobalMenu(ClientGlobalMenuOverlay),
}

impl ClientShellOverlay {
    pub(super) fn kind(&self) -> ClientShellOverlayKind {
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

#[derive(Debug)]
pub(super) enum PendingEndpointKind {
    Generic,
    SelectionCopy,
    PaneScroll {
        pane_id: String,
        serial: u64,
    },
    WordSelection {
        pane_id: String,
        absolute_row: u32,
        generation: u64,
    },
    CopyMotion {
        pane_id: String,
        origin: crate::api::schema::PaneTextPoint,
        session_generation: u64,
    },
    CopySearch {
        pane_id: String,
        origin: crate::api::schema::PaneTextPoint,
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        session_generation: u64,
    },
}

pub(super) struct PendingEndpointRequest {
    pub(super) boot_id: String,
    pub(super) method_name: String,
    pub(super) kind: PendingEndpointKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ClientEndpointNoticeKind {
    Rejected,
    Timeout,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ClientEndpointNoticeKey {
    pub(super) boot_id: String,
    pub(super) kind: ClientEndpointNoticeKind,
    pub(super) code: String,
}

pub(super) struct ClientVisibleEndpointNotice {
    pub(super) key: ClientEndpointNoticeKey,
    pub(super) title: String,
    pub(super) body: String,
}

pub(crate) struct ClientShellEndpointError {
    pub code: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientInputTarget {
    Pane(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientInputContext {
    pub(super) mode: ClientShellMode,
    pub(super) overlay: Option<ClientShellOverlayKind>,
    pub(super) retained_selection: bool,
}

type ClientInputLeases = crate::input::InputLeaseTable<u8, ClientInputContext, ClientInputTarget>;

#[derive(Clone, Debug)]
pub(super) struct ClientPaneClick {
    pub(super) pane_id: String,
    pub(super) viewport_row: u16,
    pub(super) col: u16,
    pub(super) at: std::time::Instant,
}

impl ClientPaneClick {
    pub(super) fn is_double_click_for(&self, next: &Self) -> bool {
        self.pane_id == next.pane_id
            && next.at.duration_since(self.at) <= std::time::Duration::from_millis(350)
            && self.viewport_row.abs_diff(next.viewport_row) <= 1
            && self.col.abs_diff(next.col) <= 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientSelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug)]
pub(super) struct ClientSelectionAutoscroll {
    pub(super) pane_id: String,
    pub(super) direction: ClientSelectionAutoscrollDirection,
    pub(super) last_mouse_column: u16,
    pub(super) last_mouse_row: u16,
    pub(super) inner_rect: Rect,
    pub(super) offset_from_bottom: usize,
    pub(super) max_offset_from_bottom: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientCopySelection {
    Character {
        anchor: crate::api::schema::PaneTextPoint,
    },
    Linewise {
        anchor_row: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientCopySearchPrompt {
    pub(super) direction: crate::api::schema::PaneCopySearchDirection,
    pub(super) query: TextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientCopyOperation {
    Motion(crate::api::schema::PaneCopyMotion),
    Search {
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
    },
}

pub(super) struct ClientCopySearchResult {
    pub(super) content_revision: u64,
    pub(super) matches: Vec<crate::api::schema::PaneTextRange>,
    pub(super) total: u64,
    pub(super) current: Option<usize>,
    pub(super) current_global: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientCopyModeState {
    pub(super) pane_id: String,
    pub(super) content_revision: u64,
    pub(super) geometry: (u16, u16),
    pub(super) alternate_screen_active: bool,
    pub(super) cursor: crate::api::schema::PaneTextPoint,
    pub(super) offset_from_bottom: usize,
    pub(super) max_offset_from_bottom: usize,
    pub(super) entry_offset_from_bottom: usize,
    pub(super) selection: Option<ClientCopySelection>,
    pub(super) search_prompt: Option<ClientCopySearchPrompt>,
    pub(super) search_query: String,
    pub(super) search_direction: Option<crate::api::schema::PaneCopySearchDirection>,
    pub(super) search_matches: Vec<crate::api::schema::PaneTextRange>,
    pub(super) search_total: u64,
    pub(super) search_current: Option<usize>,
    pub(super) search_current_global: Option<u64>,
    pub(super) search_generation: u64,
    pub(super) copy_after_search: bool,
}

pub(crate) struct ClientShellState {
    pub(super) machine_diagnostics: super::machine_diagnostics::MachineDiagnostics,
    pub(super) config: ClientShellConfig,
    pub(super) snapshot: Option<Box<ClientShellSnapshot>>,
    pub(super) active_snapshot_generation: Option<u64>,
    pub(super) pane_surface_generation: Option<u64>,
    pub(super) pane_surface: Option<PaneSurfaceFrame>,
    /// A future projection surface waits here until its matching snapshot arrives. The visible
    /// pane surface always remains an exact snapshot pair.
    pub(super) pending_pane_surface: Option<PaneSurfaceFrame>,
    /// Identifies the currently active endpoint's boot so a switch or restart is detectable.
    /// Retained now purely for that change-detection; Shepr no longer forwards pane images.
    pub(super) graphics_scope: String,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_collapsed_manual: bool,
    pub(super) sidebar_width: u16,
    pub(super) sidebar_width_manual: bool,
    pub(super) sidebar_section_split: f32,
    pub(super) sidebar_section_split_manual: bool,
    pub(super) agent_panel_sort_manual: bool,
    pub(super) last_sidebar_divider_click: Option<std::time::Instant>,
    pub(super) chrome_drag: Option<ClientChromeDrag>,
    pub(super) workspace_press: Option<ClientWorkspacePress>,
    pub(super) tab_press: Option<ClientTabPress>,
    pub(super) workspace_scroll: usize,
    pub(super) agent_scroll: usize,
    pub(super) pending_agent_reveal: Option<(ClientEndpointId, String)>,
    pub(super) tab_scroll: usize,
    pub(super) reveal_focused_workspace: bool,
    pub(super) reveal_focused_tab: bool,
    pub(super) last_tab_bar_width: Option<u16>,
    pub(super) last_composed_size: Option<(u16, u16)>,
    pub(super) last_composed_at: Option<std::time::Instant>,
    pub(super) selection_repaint_deadline: Option<std::time::Instant>,
    pub(super) hits: ShellHitMap,
    pub(super) endpoints: Vec<ClientShellEndpoint>,
    pub(super) active_endpoint_id: ClientEndpointId,
    pub(super) collapsed_endpoints: HashSet<ClientEndpointId>,
    pub(super) mode: ClientShellMode,
    pub(super) navigate_workspace_id: Option<WorkspaceNavigationTarget>,
    pub(super) pending_workspace_highlight: Option<PendingWorkspaceHighlight>,
    pub(super) reveal_navigation_workspace: bool,
    pub(super) overlay: Option<ClientShellOverlay>,
    pub(super) previous_pane_id: Option<String>,
    pub(super) pane_mouse_gesture: Option<ClientPaneMouseGesture>,
    pub(super) selection: Option<crate::selection::Selection<String>>,
    /// Pane a mouse selection was started in while another pane held focus.
    /// The click's `PaneFocus` travels the serialized command lane, so
    /// snapshots can still report the old focus for a while; until one shows
    /// this pane focused, those snapshots must not cancel the drag.
    pub(super) selection_focus_pending: Option<String>,
    pub(super) last_pane_click: Option<ClientPaneClick>,
    pub(super) selection_autoscroll: Option<ClientSelectionAutoscroll>,
    pub(super) selection_autoscroll_deadline: Option<std::time::Instant>,
    pub(super) selection_highlight_clear_deadline: Option<std::time::Instant>,
    pub(super) word_selection_gesture: Option<ClientWordSelection>,
    pub(super) word_selection_generation: u64,
    pub(super) copy_mode: Option<ClientCopyModeState>,
    pub(super) copy_session_generation: u64,
    pub(super) copy_operation_in_flight: bool,
    pub(super) copy_operation_queue: VecDeque<ClientCopyOperation>,
    pub(super) copy_input_queue: VecDeque<crate::input::TerminalKey>,
    pub(super) next_scroll_serial: u64,
    pub(super) pane_scroll_in_flight: HashMap<String, u64>,
    pub(super) pane_scroll_queued: HashMap<String, usize>,
    pub(super) pane_scroll_targets: HashMap<String, usize>,
    pub(super) host_mouse_pixels: Option<crate::input::mouse::HostPixels>,
    pub(super) input_leases: ClientInputLeases,
    pub(super) next_request_id: u64,
    pub(super) pending_requests: HashMap<String, PendingEndpointRequest>,
    pub(super) endpoint_notice_seen: HashSet<ClientEndpointNoticeKey>,
    pub(super) visible_endpoint_notice: Option<ClientVisibleEndpointNotice>,
    /// Expiry of the notice currently shown, with the key and body it was started for, so a
    /// replacement notice gets its own full lifetime. See `tick_transient_banners`.
    pub(super) endpoint_notice_deadline:
        Option<(ClientEndpointNoticeKey, String, std::time::Instant)>,
    pub(super) outer_focused: Option<bool>,
    pub(super) host_background: Option<crate::terminal_theme::RgbColor>,
    pub(super) local_config_diagnostic: Option<String>,
    /// The merged client + endpoint config diagnostic. It stays set while the configs are
    /// broken; whether its banner is drawn is `visible_config_diagnostic`.
    pub(super) config_diagnostic: Option<String>,
    /// A diagnostic text the user dismissed or that timed out. The banner comes back only
    /// when the diagnostic text changes.
    pub(super) config_diagnostic_hidden: Option<String>,
    /// Expiry of the banner currently shown, with the text it was started for.
    pub(super) config_diagnostic_deadline: Option<(String, std::time::Instant)>,
    pub(super) endpoint_error: Option<String>,
    pub(super) endpoint_error_deadline: Option<std::time::Instant>,
}

#[derive(Clone, Copy)]
pub(super) struct WorkspaceEntry {
    pub(super) index: usize,
}

impl ClientShellState {
    pub(crate) fn new(mut config: ClientShellConfig) -> Self {
        let preferences = config.preferences.clone();
        let local_config_diagnostic = config.startup_config_diagnostic.take();
        let overlay = None;
        let sidebar_collapsed = preferences
            .sidebar_collapsed
            .unwrap_or(config.sidebar_start_collapsed);
        let (min_width, max_width) = crate::config::validated_sidebar_bounds(
            config.sidebar_min_width,
            config.sidebar_max_width,
        )
        .unwrap_or((18, 36));
        let sidebar_width = preferences
            .sidebar_width
            .unwrap_or(config.sidebar_width)
            .clamp(min_width, max_width);
        let sidebar_section_split = preferences
            .sidebar_section_split
            .filter(|split| split.is_finite())
            .map(|split| split.clamp(0.1, 0.9))
            .unwrap_or(0.5);
        if let Some(sort) = preferences.agent_panel_sort {
            config.agent_panel_sort = sort;
        }
        Self {
            machine_diagnostics: Default::default(),
            config,
            snapshot: None,
            active_snapshot_generation: None,
            pane_surface_generation: None,
            pane_surface: None,
            pending_pane_surface: None,
            graphics_scope: String::new(),
            sidebar_collapsed,
            sidebar_collapsed_manual: preferences.sidebar_collapsed.is_some(),
            sidebar_width,
            sidebar_width_manual: preferences.sidebar_width.is_some(),
            sidebar_section_split,
            sidebar_section_split_manual: preferences.sidebar_section_split.is_some(),
            agent_panel_sort_manual: preferences.agent_panel_sort.is_some(),
            last_sidebar_divider_click: None,
            chrome_drag: None,
            workspace_press: None,
            tab_press: None,
            workspace_scroll: 0,
            agent_scroll: 0,
            pending_agent_reveal: None,
            tab_scroll: 0,
            reveal_focused_workspace: true,
            reveal_focused_tab: true,
            last_tab_bar_width: None,
            last_composed_size: None,
            last_composed_at: None,
            selection_repaint_deadline: None,
            hits: ShellHitMap::default(),
            endpoints: vec![local_endpoint()],
            active_endpoint_id: ClientEndpointId::Local,
            collapsed_endpoints: HashSet::new(),
            mode: ClientShellMode::Terminal,
            navigate_workspace_id: None,
            pending_workspace_highlight: None,
            reveal_navigation_workspace: false,
            overlay,
            previous_pane_id: None,
            pane_mouse_gesture: None,
            selection: None,
            selection_focus_pending: None,
            last_pane_click: None,
            selection_autoscroll: None,
            selection_autoscroll_deadline: None,
            selection_highlight_clear_deadline: None,
            word_selection_gesture: None,
            word_selection_generation: 0,
            copy_mode: None,
            copy_session_generation: 0,
            copy_operation_in_flight: false,
            copy_operation_queue: VecDeque::new(),
            copy_input_queue: VecDeque::new(),
            next_scroll_serial: 0,
            pane_scroll_in_flight: HashMap::new(),
            pane_scroll_queued: HashMap::new(),
            pane_scroll_targets: HashMap::new(),
            host_mouse_pixels: None,
            input_leases: ClientInputLeases::default(),
            next_request_id: 1,
            pending_requests: HashMap::new(),
            endpoint_notice_seen: HashSet::new(),
            visible_endpoint_notice: None,
            endpoint_notice_deadline: None,
            outer_focused: None,
            host_background: None,
            config_diagnostic: local_config_diagnostic.clone(),
            config_diagnostic_hidden: None,
            config_diagnostic_deadline: None,
            local_config_diagnostic,
            endpoint_error: None,
            endpoint_error_deadline: None,
        }
    }

    pub(super) fn navigation_workspace_entries(
        &self,
        snapshot: &ClientShellSnapshot,
    ) -> Vec<WorkspaceEntry> {
        render::workspace_entries(snapshot)
    }

    pub(super) fn reveal_workspace(&mut self, workspace_id: &str) {
        if self
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == workspace_id)
        {
            return;
        }
        let target = self.snapshot.as_deref().and_then(|snapshot| {
            self.navigation_workspace_entries(snapshot)
                .iter()
                .position(|entry| snapshot.workspaces[entry.index].workspace_id == workspace_id)
        });
        if let Some(target) = target {
            self.workspace_scroll = target.min(self.hits.workspace_max_scroll);
        }
    }

    pub(super) fn layout(&self, cols: u16, rows: u16) -> ClientShellLayout {
        self.config.layout(
            cols,
            rows,
            self.sidebar_collapsed,
            self.focused_tab_count(),
            self.sidebar_width,
        )
    }

    pub(crate) fn surface_size(&self, cols: u16, rows: u16) -> ClientSurfaceSize {
        let surface = self.layout(cols, rows).pane_surface;
        ClientSurfaceSize {
            cols: surface.width.max(1),
            rows: surface.height.max(1),
        }
        .clamped()
    }

    pub(super) fn reset_endpoint_projection(&mut self) {
        self.hits = ShellHitMap::default();
        self.pane_surface = None;
        self.pending_pane_surface = None;
        self.input_leases = ClientInputLeases::default();
        self.chrome_drag = None;
        self.workspace_press = None;
        self.tab_press = None;
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        self.tab_scroll = 0;
        self.reveal_focused_workspace = true;
        self.reveal_focused_tab = true;
        self.last_tab_bar_width = None;
        self.last_composed_size = None;
        self.last_composed_at = None;
        self.selection_repaint_deadline = None;
        self.pending_requests.clear();
        self.pane_scroll_in_flight.clear();
        self.pane_scroll_queued.clear();
        self.pane_scroll_targets.clear();
        self.endpoint_notice_seen.clear();
        self.visible_endpoint_notice = None;
        self.endpoint_error = None;
        self.endpoint_error_deadline = None;
        self.navigate_workspace_id = None;
        self.pending_workspace_highlight = None;
        self.overlay = None;
        self.previous_pane_id = None;
        self.pane_mouse_gesture = None;
        self.selection = None;
        self.selection_focus_pending = None;
        self.last_pane_click = None;
        self.selection_autoscroll = None;
        self.selection_autoscroll_deadline = None;
        self.selection_highlight_clear_deadline = None;
        self.word_selection_gesture = None;
        self.copy_mode = None;
        if self.mode == ClientShellMode::Copy {
            self.mode = ClientShellMode::Terminal;
        }
        self.reset_copy_pipeline();
        self.host_mouse_pixels = None;
    }

    pub(super) fn apply_active_snapshot(
        &mut self,
        snapshot: Box<ClientShellSnapshot>,
        generation: Option<u64>,
    ) {
        let graphics_scope = match &self.active_endpoint_id {
            // Local direct uploads use image IDs authored by the server from its boot ID.
            ClientEndpointId::Local => snapshot.boot_id.clone(),
            endpoint_id => format!("{}:{}", endpoint_id.storage_key(), snapshot.boot_id),
        };
        let endpoint_boot_changed =
            self.snapshot.is_some() && self.graphics_scope != graphics_scope;
        let generation_changed = self.active_snapshot_generation != generation;
        if !endpoint_boot_changed
            && !generation_changed
            && self.snapshot.as_ref().is_some_and(|current| {
                current.boot_id == snapshot.boot_id && snapshot.revision < current.revision
            })
        {
            return;
        }
        // Screen revisions restart per connection. Keep the displayed surface for selection
        // content comparisons, but retire speculative frames from the old connection.
        if generation_changed {
            self.pending_pane_surface = None;
        }
        self.active_snapshot_generation = generation;
        self.graphics_scope = graphics_scope;
        let endpoint_profile_changed = self.snapshot.as_ref().is_none_or(|current| {
            current.server_keybindings_toml != snapshot.server_keybindings_toml
        });
        // Only endpoint-sourced keymaps follow the snapshot; Local and RemoteLocal keep the
        // keymap built from this client's own config at startup.
        let snapshot_keybindings_changed =
            self.config.uses_endpoint_keybindings() && endpoint_profile_changed;
        self.config_diagnostic = super::config::merged_config_diagnostic(
            self.local_config_diagnostic.as_deref(),
            snapshot.config_diagnostic.as_deref(),
        );
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
            self.reset_endpoint_projection();
            self.navigate_workspace_id = preview;
        } else if let Some(previous) = self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_pane_id.as_ref())
            .filter(|previous| Some(previous.as_str()) != snapshot.focused_pane_id.as_deref())
        {
            self.previous_pane_id = Some(previous.clone());
        }
        if snapshot_keybindings_changed {
            if let Err(err) = self
                .config
                .apply_snapshot_keybindings(snapshot.server_keybindings_toml.as_deref())
            {
                self.set_endpoint_error(err);
            } else if matches!(
                self.mode,
                ClientShellMode::Prefix | ClientShellMode::Navigate | ClientShellMode::Resize
            ) {
                self.mode = ClientShellMode::Terminal;
            }
        }
        let tab_layout_changed = self.snapshot.as_deref().is_none_or(|current| {
            current.tabs.len() != snapshot.tabs.len()
                || current
                    .tabs
                    .iter()
                    .zip(&snapshot.tabs)
                    .any(|(left, right)| {
                        left.tab_id != right.tab_id
                            || left.workspace_id != right.workspace_id
                            || left.label != right.label
                            || left.zoomed != right.zoomed
                    })
                || render::tab_bar_status_width(current) != render::tab_bar_status_width(&snapshot)
        });
        if self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_workspace_id.as_deref())
            != snapshot.focused_workspace_id.as_deref()
        {
            self.reveal_focused_workspace = true;
        }
        if tab_layout_changed
            || self
                .snapshot
                .as_deref()
                .and_then(|current| current.focused_tab_id.as_deref())
                != snapshot.focused_tab_id.as_deref()
        {
            self.reveal_focused_tab = true;
        }
        let selection_focus_lost = if let Some(gesture) = self.word_selection_gesture.as_mut() {
            let focused_pane = snapshot.focused_pane_id.as_deref();
            // Remember confirmed focus across intermediate snapshots with no
            // focused pane, without rejecting the gesture's in-flight focus request.
            gesture.focus_confirmed |= focused_pane == Some(gesture.pane_id.as_str());
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == gesture.pane_id)
                || (gesture.focus_confirmed
                    && focused_pane.is_some_and(|pane_id| pane_id != gesture.pane_id))
        } else if let Some(selection) = self.selection.as_ref() {
            let focused_pane = snapshot.focused_pane_id.as_deref();
            let focused_here = focused_pane == Some(selection.pane_id.as_str());
            // Like the word-gesture guard above: a selection started in an
            // unfocused pane survives snapshots that predate its focus
            // request, and only a focus change after that ends it.
            let awaiting_focus = !focused_here
                && self.selection_focus_pending.as_deref() == Some(selection.pane_id.as_str());
            if focused_here {
                self.selection_focus_pending = None;
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
            self.selection = None;
            self.selection_focus_pending = None;
            self.selection_autoscroll = None;
            self.selection_autoscroll_deadline = None;
            self.selection_highlight_clear_deadline = None;
            self.word_selection_gesture = None;
            self.last_pane_click = None;
        }
        if let Some(copy_pane_id) = self
            .copy_mode
            .as_ref()
            .map(|copy_mode| copy_mode.pane_id.clone())
        {
            let pane_exists = snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == copy_pane_id);
            let pane_focused = snapshot.focused_pane_id.as_deref() == Some(copy_pane_id.as_str());
            if !pane_exists {
                self.copy_mode = None;
                self.reset_copy_pipeline();
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.selection = None;
                    self.stop_selection_autoscroll();
                    self.selection_highlight_clear_deadline = None;
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            } else if pane_focused {
                if self.mode == ClientShellMode::Terminal {
                    self.mode = ClientShellMode::Copy;
                }
                if self.selection.is_none() {
                    self.sync_copy_selection();
                }
            } else {
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.selection = None;
                    self.stop_selection_autoscroll();
                    self.selection_highlight_clear_deadline = None;
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            }
        }
        if self.mode == ClientShellMode::Navigate && self.navigate_workspace_id.is_none() {
            self.navigate_workspace_id = snapshot
                .focused_workspace_id
                .as_deref()
                .and_then(|id| self.navigation_target(&self.active_endpoint_id, id));
        }
        let pane_exists =
            |pane_id: &String| snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id);
        self.pane_scroll_in_flight
            .retain(|pane_id, _| pane_exists(pane_id));
        self.pane_scroll_queued
            .retain(|pane_id, _| pane_exists(pane_id));
        self.pane_scroll_targets
            .retain(|pane_id, _| pane_exists(pane_id));

        self.snapshot = Some(snapshot);
        self.reconcile_pending_workspace_highlight();
        let pending_surface = self.pending_pane_surface.take();
        if let Some(surface) = pending_surface {
            let matching = self.snapshot.as_ref().is_some_and(|snapshot| {
                surface.boot_id == snapshot.boot_id
                    && surface.projection_revision == snapshot.revision
            });
            if matching {
                self.install_pane_surface(surface, false);
            } else if self.snapshot.as_ref().is_some_and(|snapshot| {
                surface.boot_id == snapshot.boot_id
                    && surface.projection_revision > snapshot.revision
            }) {
                self.pending_pane_surface = Some(surface);
            }
        }
    }

    pub(crate) fn has_presented_surface(&self) -> bool {
        self.pane_surface.is_some()
    }

    pub(crate) fn set_pane_surface(&mut self, surface: PaneSurfaceFrame) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if surface.boot_id != snapshot.boot_id || surface.projection_revision < snapshot.revision {
            return;
        }
        if self.pane_surface.as_ref().is_some_and(|current| {
            self.pane_surface_generation == self.active_snapshot_generation
                && current.boot_id == surface.boot_id
                && (surface.projection_revision < current.projection_revision
                    || (surface.projection_revision == current.projection_revision
                        && surface.surface_revision < current.surface_revision))
        }) {
            return;
        }
        if surface.projection_revision == snapshot.revision.saturating_add(1) {
            // The next expected surface waits separately for its exact snapshot. Keeping the
            // current pair avoids treating this speculative successor as presentation evidence.
            // The visible pair and its hit map are untouched, so the hits stay live.
            self.pending_pane_surface = Some(surface);
            return;
        }
        // A surface that skips one or more revisions supersedes any retained pair, but is still
        // not rendered until its matching snapshot arrives. Retain it monotonically so delayed
        // intermediate surfaces cannot replace it.
        self.install_pane_surface(surface, true);
    }

    fn install_pane_surface(&mut self, surface: PaneSurfaceFrame, retain_future: bool) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if surface.boot_id != snapshot.boot_id
            || surface.projection_revision < snapshot.revision
            || (!retain_future && surface.projection_revision != snapshot.revision)
            || self.pane_surface.as_ref().is_some_and(|current| {
                self.pane_surface_generation == self.active_snapshot_generation
                    && current.boot_id == surface.boot_id
                    && (surface.projection_revision < current.projection_revision
                        || (surface.projection_revision == current.projection_revision
                            && surface.surface_revision < current.surface_revision))
            })
        {
            return;
        }
        // A retained future surface is not presentable yet; the exact-pair compose guard keeps
        // it from replacing the visible frame. Its pane geometry no longer matches the pane
        // hits on screen, so those go (pane input and copy mode read `pane_surface` alongside
        // them). Chrome hits still match the visible frame and stay.
        if surface.projection_revision != snapshot.revision {
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        self.acknowledge_active_surface_agents(&surface);
        let selection_pane = match &self.word_selection_gesture {
            Some(gesture) => Some(&gesture.pane_id),
            None => self.selection.as_ref().map(|selection| &selection.pane_id),
        };
        let selection_invalidated = selection_pane.is_some_and(|pane_id| {
            // With no surface to compare against, nothing shows the selection's coordinates
            // still describe this pane's grid; highlighting them could mark stale cells.
            let Some(previous_surface) = self.pane_surface.as_ref() else {
                return true;
            };
            let previous = previous_surface
                .panes
                .iter()
                .find(|pane| &pane.pane_id == pane_id);
            let next = surface.panes.iter().find(|pane| &pane.pane_id == pane_id);
            let (Some(previous), Some(next)) = (previous, next) else {
                return false;
            };
            previous.inner_rect.width != next.inner_rect.width
                || previous.inner_rect.height != next.inner_rect.height
                || previous.alternate_screen_active != next.alternate_screen_active
                // Ordinary selections are live buffer ranges. Only word gestures
                // cache content-dependent boundaries that output can invalidate.
                || (self.word_selection_gesture.is_some()
                    && previous.content_revision != next.content_revision)
        });
        if selection_invalidated {
            self.word_selection_gesture = None;
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
        }
        for pane in &surface.panes {
            let Some(target) = self.pane_scroll_targets.get(&pane.pane_id).copied() else {
                continue;
            };
            let Some(scroll) = pane.scroll else {
                continue;
            };
            let target =
                target.min(usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX));
            if usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX) == target {
                self.pane_scroll_targets.remove(&pane.pane_id);
            }
        }
        let mut invalidated_copy_pane = None;
        if let Some(copy_mode) = self.copy_mode.as_mut()
            && let Some(pane) = surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == copy_mode.pane_id)
        {
            let geometry = (pane.inner_rect.width, pane.inner_rect.height);
            let coordinates_changed = copy_mode.geometry != geometry
                || copy_mode.alternate_screen_active != pane.alternate_screen_active;
            if copy_mode.content_revision != pane.content_revision || coordinates_changed {
                copy_mode.content_revision = pane.content_revision;
                copy_mode.geometry = geometry;
                copy_mode.alternate_screen_active = pane.alternate_screen_active;
                if coordinates_changed {
                    copy_mode.selection = None;
                    invalidated_copy_pane = Some(copy_mode.pane_id.clone());
                }
                copy_mode.search_matches.clear();
                copy_mode.search_total = 0;
                copy_mode.search_current = None;
                copy_mode.search_current_global = None;
                copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                copy_mode.copy_after_search = false;
            }
            if let Some(scroll) = pane.scroll {
                let actual_offset =
                    usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX);
                if !self.pane_scroll_targets.contains_key(&pane.pane_id) {
                    copy_mode.offset_from_bottom = actual_offset;
                }
                copy_mode.max_offset_from_bottom =
                    usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX);
            }
        }
        if invalidated_copy_pane.as_ref().is_some_and(|pane_id| {
            self.selection
                .as_ref()
                .is_some_and(|selection| &selection.pane_id == pane_id)
        }) {
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
        }
        self.pane_surface = Some(surface);
        self.pane_surface_generation = self.active_snapshot_generation;
    }

    pub(crate) fn tick_selection_highlight(&mut self, now: std::time::Instant) -> bool {
        let mut repaint = false;
        if self
            .selection_highlight_clear_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.selection = None;
            self.selection_highlight_clear_deadline = None;
            repaint = true;
        }
        repaint
    }

    /// Show a transient client-side action error, restarting its lifetime.
    ///
    /// Every assignment must go through this setter so a repeated identical
    /// message gets a fresh deadline instead of inheriting the previous one.
    pub(super) fn set_endpoint_error(&mut self, message: impl Into<String>) {
        self.endpoint_error = Some(message.into());
        self.endpoint_error_deadline = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(ENDPOINT_ERROR_TIMEOUT_SECS),
        );
    }

    pub(crate) fn tick_endpoint_error(&mut self, now: std::time::Instant) -> bool {
        if self.endpoint_error.is_none() {
            self.endpoint_error_deadline = None;
            return false;
        }
        if self
            .endpoint_error_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.endpoint_error = None;
            self.endpoint_error_deadline = None;
            return true;
        }
        false
    }

    /// The config diagnostic banner text, unless the user dismissed it or it timed out.
    pub(super) fn visible_config_diagnostic(&self) -> Option<&str> {
        self.config_diagnostic
            .as_deref()
            .filter(|text| self.config_diagnostic_hidden.as_deref() != Some(*text))
    }

    /// Hides the config diagnostic banner until its text changes.
    pub(super) fn dismiss_config_diagnostic(&mut self) {
        self.config_diagnostic_hidden = self.config_diagnostic.clone();
        self.config_diagnostic_deadline = None;
    }

    /// Starts the lifetime of the config diagnostic banner, once per distinct text, when
    /// `compose` first draws it. A banner never drawn (no presentable frame yet) cannot expire.
    pub(super) fn config_diagnostic_drawn(&mut self, now: std::time::Instant) {
        let Some(text) = self.visible_config_diagnostic() else {
            return;
        };
        if self
            .config_diagnostic_deadline
            .as_ref()
            .is_some_and(|(shown, _)| shown == text)
        {
            return;
        }
        let text = text.to_owned();
        let deadline = now + std::time::Duration::from_secs(CONFIG_DIAGNOSTIC_TIMEOUT_SECS);
        self.config_diagnostic_deadline = Some((text, deadline));
    }

    /// Starts the lifetime of the endpoint notice card, once per distinct notice, when a
    /// compose first draws it.
    pub(super) fn endpoint_notice_drawn(&mut self, now: std::time::Instant) {
        let Some(notice) = self.visible_endpoint_notice.as_ref() else {
            return;
        };
        if self
            .endpoint_notice_deadline
            .as_ref()
            .is_some_and(|(key, body, _)| *key == notice.key && *body == notice.body)
        {
            return;
        }
        let started = (notice.key.clone(), notice.body.clone());
        let deadline = now + std::time::Duration::from_secs(ENDPOINT_NOTICE_TIMEOUT_SECS);
        self.endpoint_notice_deadline = Some((started.0, started.1, deadline));
    }

    /// Hides the config diagnostic banner and the endpoint notice card once their lifetimes
    /// (started when first drawn) run out. A replacement banner or notice carries its own
    /// lifetime, so it is not cut short by its predecessor's. Returns whether anything was
    /// hidden.
    pub(crate) fn tick_transient_banners(&mut self, now: std::time::Instant) -> bool {
        let mut repaint = false;

        let diagnostic_expired = self.visible_config_diagnostic().is_some_and(|text| {
            self.config_diagnostic_deadline
                .as_ref()
                .is_some_and(|(shown, deadline)| shown == text && now >= *deadline)
        });
        if diagnostic_expired {
            self.dismiss_config_diagnostic();
            repaint = true;
        }

        let notice_expired = self.visible_endpoint_notice.as_ref().is_some_and(|notice| {
            self.endpoint_notice_deadline
                .as_ref()
                .is_some_and(|(key, body, deadline)| {
                    *key == notice.key && *body == notice.body && now >= *deadline
                })
        });
        if notice_expired {
            self.visible_endpoint_notice = None;
            self.endpoint_notice_deadline = None;
            repaint = true;
        }

        repaint
    }

    pub(crate) fn timer_delay(&self, now: std::time::Instant) -> std::time::Duration {
        let default = std::time::Duration::from_millis(100);
        self.selection_autoscroll_deadline
            .into_iter()
            .chain(self.selection_repaint_deadline)
            .min()
            .map(|deadline| deadline.saturating_duration_since(now).min(default))
            .unwrap_or(default)
    }

    /// Drops the retained pane surface, leaving `compose` on its no-surface placeholder. Resize
    /// and sidebar changes no longer do this (the retained surface is drawn clipped until the
    /// resized one arrives); tests use it to reach the placeholder.
    #[cfg(test)]
    pub(crate) fn invalidate_pane_surface(&mut self) {
        self.pane_surface = None;
        self.pending_pane_surface = None;
        self.hits = ShellHitMap::default();
        self.host_mouse_pixels = None;
    }
}
