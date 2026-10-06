//! Client shell state. `TextEditor`, `TypedText` and shell endpoint requests
//! redact their contents in `Debug`; other types here may carry typed or pasted
//! text (queued keys and clipboard bytes), so log ids, lengths or kinds instead.

use crate::shell::ledger::ClientShellEndpointRequest;

use crate::endpoint::ClientEndpointId;
use crate::shell::copy::CopySession;
use crate::shell::endpoints::{Endpoints, local_endpoint};
use crate::shell::input::pointer::Pointer;
use crate::shell::input::selection::MouseSelection;
use crate::shell::ledger::Ledger;
use shepr_protocol::{ClientMessage, ClientSurfaceSize, PaneSurfaceFrame};

use crate::shell::config::{ClientShellConfig, ClientShellLayout};
use crate::shell::input::scroll_lanes::ScrollLanes;
use crate::shell::navigation::location::Location;
use crate::shell::navigation::workspace_navigation::PendingWorkspaceHighlight;
use crate::shell::overlays::{Overlay, OverlayKind};
use crate::shell::presentation::Presentation;
use crate::shell::sidebar::agent_sidebar::AgentPanelSort;

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Repaint {
    #[default]
    Unchanged,
    Needed,
}

impl Repaint {
    pub(crate) fn is_needed(self) -> bool {
        self == Self::Needed
    }

    pub(in crate::shell) fn apply_to(self, outcome: &mut ClientShellInput) {
        outcome.repaint |= self.is_needed();
    }
}

impl std::ops::BitOr for Repaint {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        if self == Self::Needed || rhs == Self::Needed {
            Self::Needed
        } else {
            Self::Unchanged
        }
    }
}

impl std::ops::BitOrAssign for Repaint {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
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
    /// Pick an endpoint, and the navigation inside it the pick names.
    ActivateEndpoint(Location),
    /// The operator's Connect on a configured machine with no server: start
    /// its server and attach.
    ConnectMachine(ClientEndpointId),
    /// The operator's confirmed Restart of a configured machine running
    /// another build: stop that server, start this build's and attach.
    RestartMachine(ClientEndpointId),
    /// The operator's Retry on a configured machine whose SSH refused the
    /// client's credentials: attach again, once.
    RetryMachine(ClientEndpointId),
}

#[derive(Default)]
pub(crate) struct ClientShellInput {
    // These are independent one-shot effects accumulated with OR semantics, rather than
    // phases of a state machine. Requests carry their routing in `ClientShellRequest` below.
    pub(crate) detach: bool,
    pub(crate) repaint: bool,
    /// Present the next frame in full rather than diffed against what the
    /// host terminal is assumed to show (set when the host terminal regains focus).
    pub(crate) full_redraw: bool,
    pub(crate) resize: bool,
    pub(crate) query_host_appearance: bool,
    pub(crate) query_host_theme: bool,
    pub(crate) requests: Vec<ClientShellRequest>,
    pub(crate) actions: Vec<ClientShellAction>,
}

#[derive(Debug)]
pub(crate) enum ClientShellRequest {
    Shown(ClientMessage),
    HostTheme(shepr_protocol::ClientHostThemeUpdate),
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
    pub(crate) generation: Option<shepr_protocol::ConnectionGeneration>,
    pub(crate) boot_id: Option<shepr_protocol::BootId>,
    pub(crate) projection_revision: Option<shepr_protocol::ProjectionRevision>,
    pub(crate) surface_revision: Option<shepr_protocol::SurfaceRevision>,
    pub(crate) pane_ids: Vec<shepr_protocol::PublicPaneId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct ClientInputContext {
    pub(in crate::shell) mode: ClientShellMode,
    pub(in crate::shell) overlay: Option<OverlayKind>,
    pub(in crate::shell) retained_selection: bool,
}

type ClientInputLeases =
    shepr_termio::input::InputLeaseTable<u8, ClientInputContext, shepr_protocol::PublicPaneId>;

pub(crate) struct ClientShellState {
    /// The client loop's time for the event being handled, set on each event,
    /// so shell code that stamps deadlines never reads the clock itself.
    /// A method that also takes a `now` is handed the same instant by its
    /// caller; keep the two equal rather than reading a different clock.
    pub(crate) now: std::time::Instant,
    pub(in crate::shell) machine_diagnostics:
        crate::shell::notices::machine_diagnostics::MachineDiagnostics,
    pub(in crate::shell) config: ClientShellConfig,
    pub(in crate::shell) presentation: Presentation,
    pub(in crate::shell) ledger: Ledger,
    pub(in crate::shell) scroll_lanes: ScrollLanes,
    pub(in crate::shell) chrome: crate::shell::sidebar::chrome::ChromeLayout,
    pub(in crate::shell) agent_panel_sort_chrome:
        crate::shell::sidebar::chrome::Chrome<AgentPanelSort>,
    pub(in crate::shell) pointer: Pointer,
    pub(in crate::shell) sidebar_scroll: crate::shell::sidebar::scroll::SidebarScroll,
    pub(in crate::shell) mouse_selection: MouseSelection,
    pub(crate) endpoints: Endpoints,
    pub(in crate::shell) mode: crate::shell::mode::ModeState,
    pub(in crate::shell) pending_workspace_highlight: Option<PendingWorkspaceHighlight>,
    pub(in crate::shell) overlay: Option<Overlay>,
    /// Stored copy cursor state can survive while the Copy input mode is parked.
    pub(in crate::shell) copy: Option<CopySession>,
    pub(in crate::shell) input_leases: ClientInputLeases,
    /// Whether the host sends every key, text keys included, as an escape
    /// code with its release (kitty REPORT_ALL_KEYS). Set per host input batch.
    pub(in crate::shell) host_reports_all_keys: bool,
    pub(in crate::shell) notices: crate::shell::notices::Notices,
    pub(in crate::shell) outer_focused: Option<bool>,
    /// The host terminal's default and ANSI colours as last reported, which
    /// the palette, the selection highlight (its background) and the per-host
    /// sidebar colours are derived from.
    pub(in crate::shell) host_theme: shepr_term::host::TerminalTheme,
    /// The colours everything is drawn with, derived from `host_theme` with
    /// the local server's hue as accent.
    pub(in crate::shell) palette: crate::shell::palette::Palette,
    /// The per-host sidebar colours derived from `host_theme`; `None` while
    /// the terminal has reported no background.
    pub(in crate::shell) host_pills: Option<shepr_term::host_tint::HostPillPalette>,
    pub(in crate::shell) endpoint_error: crate::shell::notices::transient_error::TransientError,
    /// The host terminal's cell as the client last observed it. A pixel mouse
    /// position is only built when it is exact.
    pub(in crate::shell) host_cell: shepr_core::geometry::HostCell,
}

impl ClientShellState {
    /// Records the host cell after each resize and at startup.
    pub(crate) fn set_host_cell(&mut self, cell: shepr_core::geometry::HostCell) {
        self.host_cell = cell;
    }

    pub(crate) fn new_at(config: ClientShellConfig, now: std::time::Instant) -> Self {
        let preferences = config.preferences.clone();
        let overlay = None;
        let chrome = crate::shell::sidebar::chrome::ChromeLayout::new(&config);
        let agent_panel_sort_chrome = preferences.agent_panel_sort.map_or_else(
            || {
                crate::shell::sidebar::chrome::Chrome::new(
                    AgentPanelSort::default(),
                    crate::shell::sidebar::chrome::ChromeOrigin::Default,
                )
            },
            |sort| {
                crate::shell::sidebar::chrome::Chrome::new(
                    sort,
                    crate::shell::sidebar::chrome::ChromeOrigin::Remembered,
                )
            },
        );
        let agent_panel_sort = agent_panel_sort_chrome.value();
        let endpoints = Endpoints::new(vec![local_endpoint()], &config, agent_panel_sort);
        let host_theme = shepr_term::host::TerminalTheme::default();
        let palette = crate::shell::palette::Palette::derive(&host_theme, config.host_hues.local());
        Self {
            now,
            machine_diagnostics: Default::default(),
            config,
            presentation: Presentation::default(),
            ledger: Ledger::default(),
            scroll_lanes: ScrollLanes::default(),
            chrome,
            agent_panel_sort_chrome,
            pointer: Pointer::default(),
            sidebar_scroll: crate::shell::sidebar::scroll::SidebarScroll::new(),
            mouse_selection: MouseSelection::default(),
            endpoints,
            mode: crate::shell::mode::ModeState::default(),
            pending_workspace_highlight: None,
            overlay,
            copy: None,
            input_leases: ClientInputLeases::default(),
            host_reports_all_keys: false,
            notices: Default::default(),
            outer_focused: None,
            host_theme,
            palette,
            host_pills: None,
            endpoint_error: Default::default(),
            host_cell: shepr_core::geometry::HostCell::Unknown,
        }
    }

    pub(crate) fn presentation_log_context(&self) -> ClientPresentationLogContext {
        let surface = self.pane_surface();
        let mut pane_ids: Vec<_> = surface.map_or_else(Vec::new, |surface| {
            surface
                .panes
                .iter()
                .filter(|pane| pane.focused)
                .map(|pane| pane.pane_id)
                .collect()
        });
        if pane_ids.is_empty()
            && let Some(pane_id) = self
                .endpoints
                .active
                .snapshot()
                .and_then(|snapshot| snapshot.focused_pane_id)
        {
            pane_ids.push(pane_id);
        }
        let snapshot = self.endpoints.active.snapshot();
        ClientPresentationLogContext {
            endpoint: self.endpoints.presented().clone(),
            generation: self.endpoints.active.generation(),
            boot_id: surface
                .map(|surface| surface.boot_id.clone())
                .or_else(|| snapshot.map(|snapshot| snapshot.boot_id.clone())),
            projection_revision: surface
                .map(|surface| surface.projection_revision)
                .or_else(|| snapshot.map(|snapshot| snapshot.revision)),
            surface_revision: surface.map(|surface| surface.surface_revision),
            pane_ids,
        }
    }

    /// Asks the sidebar to reveal one of the presented endpoint's workspaces at its next
    /// composition.
    pub(in crate::shell) fn request_workspace_reveal(
        &mut self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) {
        self.sidebar_scroll.reveal_workspace(Location::workspace(
            self.endpoints.presented().clone(),
            *workspace_id,
        ));
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

    /// The presented surface: what is on screen, possibly held while unpaired. Input
    /// reads this.
    pub(in crate::shell) fn pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        self.presentation.surfaces.presented()
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

    fn tick_selection_highlight(&mut self, now: std::time::Instant) -> bool {
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

    fn tick_transient_banners(&mut self, now: std::time::Instant) -> bool {
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
    ShellTimer {
        deadline: owed_sidebar_settle_deadline,
        tick: tick_owed_sidebar_settle,
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

/// Due at once: a sidebar drag a projection reset ended still owes its resize and save.
fn owed_sidebar_settle_deadline(shell: &ClientShellState) -> Option<std::time::Instant> {
    shell.pointer.owed_sidebar_settle.map(|_| shell.now)
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

/// Does what a sidebar drag a projection reset ended still owed, as its release would
/// have (`settle_chrome_drag`): the width drag's endpoint resize and the preference save.
fn tick_owed_sidebar_settle(
    shell: &mut ClientShellState,
    _now: std::time::Instant,
) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    if let Some(owed) = shell.pointer.owed_sidebar_settle.take() {
        outcome.resize = owed.resize;
        shell.persist_chrome_preferences(&mut outcome);
    }
    outcome
}

fn repaint_timer_outcome(repaint: bool) -> ClientShellInput {
    ClientShellInput {
        repaint,
        ..ClientShellInput::default()
    }
}

#[cfg(test)]
impl ClientShellInput {
    pub(in crate::shell) fn into_parts(self) -> (bool, Vec<ClientShellAction>) {
        (self.repaint, self.actions)
    }
}

#[cfg(test)]
impl ClientShellState {
    pub(crate) fn new(config: ClientShellConfig) -> Self {
        // clock-io-ok: this test-only constructor stands in for the client launch.
        Self::new_at(config, std::time::Instant::now())
    }

    /// Drops the retained pane surface, leaving `compose_frame` on its no-surface placeholder. Resize
    /// and sidebar changes no longer do this (the retained surface is drawn clipped until the
    /// resized one arrives); tests use it to reach the placeholder.
    pub(in crate::shell) fn invalidate_pane_surface(&mut self) {
        self.presentation = Presentation::default();
        self.pointer.host_mouse_pixels = None;
    }
}
