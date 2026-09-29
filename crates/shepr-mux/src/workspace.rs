use std::collections::{HashMap, HashSet};
use std::ops::Index;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Notify, mpsc};

use crate::events::AppEvent;
use crate::git::{AheadBehind, GitSpaceMetadata, fallback_label_from_cwd};
use crate::limits::FIRST_WORKSPACE_NUMBER;
use crate::pane::{PaneLaunchEnv, PaneRuntime, PaneRuntimeRegistry, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalState;
use shepr_core::layout::{Direction, PaneId, TileLayout};
use shepr_protocol::{PublicPaneId, PublicTabId, TerminalId, WorkspaceId};

mod aggregate;
mod geometry;
mod tab;

pub use self::geometry::apply_pane_chrome;
pub use self::tab::{NewPane, Tab, TabPane};
pub use self::{
    geometry::{PaneChromeInfo, PaneGeometry, layout_rect, pane_inner_rect, terminal_content_rect},
    tab::MovedPane,
};

/// The channels a pane runtime reports through once it is spawned, plus the
/// resolved API socket path its child needs. `App` owns them and lends a copy
/// to each call that spawns a pane, so the workspace tree itself holds no
/// channels or async handles and stays plain data.
#[derive(Clone)]
pub struct PaneSpawnHandles {
    pub events: mpsc::Sender<AppEvent>,
    pub render_notify: Arc<Notify>,
    pub render_dirty: Arc<RenderSignal>,
    /// Counts this app's pane session teardowns, so its exit waits on them
    /// and on no other app's.
    pub pane_teardowns: Arc<crate::pane::PaneTeardownTracker>,
    /// Resolved API socket passed into every pane launched by this app.
    pub api_socket_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRemovalScope {
    Pane,
    Tab,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRemovalPlan {
    workspace_id: String,
    pub pane_id: PaneId,
    pub tab_index: usize,
    pub scope: PaneRemovalScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRemoval {
    pub workspace_id: String,
    pub pane_id: PaneId,
    pub tab_index: usize,
    pub scope: PaneRemovalScope,
    pub pane_ids: Vec<PaneId>,
    pub terminal_ids: Vec<TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRemoval {
    pub workspace_id: WorkspaceId,
    pub tab_index: usize,
    pub tab_number: usize,
    pub pane_ids: Vec<PaneId>,
    pub terminal_ids: Vec<TerminalId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabCreationOutcome {
    pub tab_index: usize,
    pub root_pane: PaneId,
}

/// The public number the next allocated workspace ID spells.
///
/// This stays a process global rather than an allocator owned by the
/// server's app state, as the pane id counter in `shepr-core` does. One
/// process serves one session, so unique per process is unique per session;
/// an owned allocator would have to be threaded into every workspace
/// constructor, pane move and restore for no change in behaviour. Restore
/// moves the counter past every restored ID with `reserve_workspace_ids`, and
/// the counter never wraps, so a live ID is never handed out twice.
static NEXT_WORKSPACE_NUMBER: AtomicUsize = AtomicUsize::new(FIRST_WORKSPACE_NUMBER);

pub(crate) fn generate_workspace_id() -> WorkspaceId {
    match allocate_workspace_id(&NEXT_WORKSPACE_NUMBER) {
        Some(id) => id,
        // Continuing would have to reuse a live ID; there is no safe value.
        None => panic!("workspace id space exhausted"),
    }
}

/// Hands out the counter's number and advances it. `None` once the counter
/// is exhausted: advancing refuses to pass `usize::MAX` rather than wrap, so
/// that last number is never handed out and marks the space as used up.
fn allocate_workspace_id(counter: &AtomicUsize) -> Option<WorkspaceId> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .ok()
        .and_then(WorkspaceId::from_number)
}

/// Moves `counter` past every ID in `workspaces`. An ID at the top of the
/// number space leaves nothing to allocate, so the counter is exhausted
/// rather than left where it could reach that ID again.
fn reserve_workspace_numbers(counter: &AtomicUsize, workspaces: &[Workspace]) {
    let Some(max) = workspaces
        .iter()
        .map(|workspace| workspace.id.number())
        .max()
    else {
        return;
    };
    counter.fetch_max(max.saturating_add(1), Ordering::Relaxed);
}

/// Canonical public pane ID renderer; parsing uses `PublicPaneId::from_str`.
pub fn public_pane_id_for_number(workspace_id: &WorkspaceId, pane_number: usize) -> String {
    PublicPaneId::new(workspace_id, pane_number).to_string()
}

/// Canonical public tab ID renderer; parsing uses `PublicTabId::from_str`.
pub fn public_tab_id_for_number(workspace_id: &WorkspaceId, tab_number: usize) -> String {
    PublicTabId::new(workspace_id, tab_number).to_string()
}

/// A non-empty ordered tab collection with one focused position.
///
/// Keeping both pieces here means workspace callers can reorder tabs, but
/// cannot make the focused position point outside the collection or remove
/// its last tab.
struct FocusedTabs {
    items: Vec<Tab>,
    focused: usize,
}

impl FocusedTabs {
    fn new(items: Vec<Tab>, focused: usize) -> Option<Self> {
        if items.is_empty() || focused >= items.len() {
            return None;
        }
        Some(Self { items, focused })
    }

    fn one(tab: Tab) -> Self {
        Self {
            items: vec![tab],
            focused: 0,
        }
    }

    fn as_slice(&self) -> &[Tab] {
        &self.items
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn iter(&self) -> std::slice::Iter<'_, Tab> {
        self.items.iter()
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, Tab> {
        self.items.iter_mut()
    }

    fn get(&self, index: usize) -> Option<&Tab> {
        self.items.get(index)
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut Tab> {
        self.items.get_mut(index)
    }

    fn first(&self) -> Option<&Tab> {
        self.items.first()
    }

    fn focused_index(&self) -> usize {
        self.focused
    }

    // Indexing cannot panic: every constructor and mutator keeps `items`
    // non-empty and `focused < items.len()`.
    fn focused(&self) -> &Tab {
        &self.items[self.focused]
    }

    fn focus(&mut self, index: usize) -> bool {
        if index >= self.items.len() {
            return false;
        }
        self.focused = index;
        true
    }

    fn push(&mut self, tab: Tab) -> usize {
        let index = self.items.len();
        self.items.push(tab);
        index
    }

    fn replace(&mut self, index: usize, tab: Tab) -> Option<Tab> {
        let slot = self.items.get_mut(index)?;
        Some(std::mem::replace(slot, tab))
    }

    fn remove(&mut self, index: usize) -> Option<Tab> {
        if self.items.len() <= 1 || index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        if index <= self.focused && self.focused > 0 {
            self.focused -= 1;
        }
        if self.focused >= self.items.len() {
            self.focused = self.items.len() - 1;
        }
        Some(removed)
    }

    fn move_tab(&mut self, source_index: usize, insert_index: usize) -> bool {
        if source_index >= self.items.len() || insert_index > self.items.len() {
            return false;
        }
        let target_index = if source_index < insert_index {
            insert_index - 1
        } else {
            insert_index
        }
        .min(self.items.len() - 1);
        if source_index == target_index {
            return false;
        }

        let tab = self.items.remove(source_index);
        self.items.insert(target_index, tab);
        if self.focused == source_index {
            self.focused = target_index;
        } else if source_index < self.focused && self.focused <= target_index {
            self.focused -= 1;
        } else if target_index <= self.focused && self.focused < source_index {
            self.focused += 1;
        }
        true
    }
}

impl Index<usize> for FocusedTabs {
    type Output = Tab;

    fn index(&self, index: usize) -> &Self::Output {
        &self.items[index]
    }
}

/// Check a tab collection when it enters a workspace. These checks run on
/// tab creation and restore, not on the view or render paths.
fn valid_tabs<'a>(
    tabs: impl IntoIterator<Item = &'a Tab>,
    next_pane_number: usize,
    next_tab_number: usize,
) -> bool {
    let mut tab_numbers = HashSet::new();
    let mut pane_numbers = HashSet::new();
    let mut pane_ids = HashSet::new();
    let mut terminal_ids = HashSet::new();
    for tab in tabs {
        if tab.number == 0
            || tab.number >= next_tab_number
            || !tab_numbers.insert(tab.number)
            || !tab.has_consistent_panes()
        {
            return false;
        }
        for (id, pane) in &tab.panes {
            if pane.public_number == 0
                || pane.public_number >= next_pane_number
                || !pane_numbers.insert(pane.public_number)
                || !pane_ids.insert(*id)
                || !terminal_ids.insert(pane.attached_terminal_id.clone())
            {
                return false;
            }
        }
    }
    !tab_numbers.is_empty()
}

pub(crate) fn reserve_workspace_ids(workspaces: &[Workspace]) {
    reserve_workspace_numbers(&NEXT_WORKSPACE_NUMBER, workspaces);
}

/// A named workspace containing tabs.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: WorkspaceId,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests, old snapshots, or missing runtimes.
    pub identity_cwd: PathBuf,
    /// CWD from which the cached automatic label and Git metadata were derived.
    pub cached_identity_cwd: PathBuf,
    /// Automatic workspace label cached outside the render path.
    pub cached_auto_label: String,
    /// Cache key for periodic Git status associated with `cached_identity_cwd`.
    pub cached_git_status_key: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub cached_git_ahead_behind: Option<AheadBehind>,
    /// Cached derived Git repo metadata for status display.
    pub cached_git_space: Option<GitSpaceMetadata>,
    pub metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens,
    pub metadata_token_sequences: crate::terminal::metadata_tokens::SequenceMarks,
    pub next_public_pane_number: usize,
    pub next_public_tab_number: usize,
    tabs: FocusedTabs,
}

impl Workspace {
    pub(crate) fn from_restored_tabs(
        id: WorkspaceId,
        custom_name: Option<String>,
        identity_cwd: PathBuf,
        tabs: Vec<Tab>,
        active_tab: usize,
        next_public_pane_number: usize,
        next_public_tab_number: usize,
    ) -> Option<Self> {
        let tabs = FocusedTabs::new(tabs, active_tab)?;
        if !valid_tabs(tabs.iter(), next_public_pane_number, next_public_tab_number) {
            return None;
        }
        let mut workspace = Self {
            id,
            custom_name,
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label: fallback_label_from_cwd(&identity_cwd),
            cached_git_status_key: identity_cwd.clone(),
            identity_cwd,
            cached_git_branch: None,
            cached_git_ahead_behind: None,
            cached_git_space: None,
            metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            next_public_pane_number,
            next_public_tab_number,
            tabs,
        };
        workspace.mark_identity_undiscovered();
        Some(workspace)
    }

    pub fn tabs(&self) -> &[Tab] {
        self.tabs.as_slice()
    }

    pub fn set_tab_custom_name(&mut self, tab_index: usize, name: Option<String>) -> bool {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return false;
        };
        match name {
            Some(name) => tab.set_custom_name(name),
            None => tab.clear_custom_name(),
        }
        true
    }

    pub fn set_tab_zoomed(&mut self, tab_index: usize, zoomed: bool) -> bool {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return false;
        };
        tab.set_zoomed(zoomed);
        true
    }

    pub fn focus_pane_in_tab(&mut self, tab_index: usize, pane_id: PaneId) -> bool {
        self.tabs
            .get_mut(tab_index)
            .is_some_and(|tab| tab.focus_pane(pane_id))
    }

    pub fn swap_panes_in_tab(&mut self, tab_index: usize, first: PaneId, second: PaneId) -> bool {
        self.tabs
            .get_mut(tab_index)
            .is_some_and(|tab| tab.swap_panes(first, second))
    }

    pub fn resize_focused_pane_in_tab(
        &mut self,
        tab_index: usize,
        direction: shepr_core::layout::NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.tabs
            .get_mut(tab_index)
            .is_some_and(|tab| tab.resize_focused_pane(direction, delta, area))
    }

    pub fn resize_pane_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        direction: shepr_core::layout::NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.tabs
            .get_mut(tab_index)
            .is_some_and(|tab| tab.resize_pane(pane_id, direction, delta, area))
    }

    pub fn set_tab_split_ratio_at(
        &mut self,
        tab_index: usize,
        path: &[shepr_core::geometry::SplitBranch],
        ratio: f32,
    ) -> bool {
        self.tabs
            .get_mut(tab_index)
            .is_some_and(|tab| tab.set_split_ratio_at(path, ratio))
    }

    pub fn from_existing_pane(
        label: Option<String>,
        tab_label: Option<String>,
        identity_cwd: &Path,
        moved: MovedPane,
    ) -> Self {
        let root_pane = moved.pane_id;
        let tab = Tab::from_existing_pane(1, tab_label, moved);
        Self::with_first_tab(generate_workspace_id(), label, identity_cwd, tab, root_pane)
    }

    /// A workspace around its first tab. The Git identity (repo label,
    /// branch, space) is left undiscovered: finding it walks the filesystem
    /// up to `/` and can spawn `git`, which must not run on the server's main
    /// loop. The background Git refresh discovers it, because an undiscovered
    /// identity never matches the workspace's resolved cwd.
    fn with_first_tab(
        id: WorkspaceId,
        custom_name: Option<String>,
        identity_cwd: &Path,
        mut tab: Tab,
        root_pane: PaneId,
    ) -> Self {
        if let Some(pane) = tab.panes.get_mut(&root_pane) {
            pane.public_number = 1;
        }
        let mut workspace = Self {
            id,
            custom_name,
            identity_cwd: identity_cwd.to_path_buf(),
            cached_identity_cwd: PathBuf::new(),
            cached_auto_label: String::new(),
            cached_git_status_key: PathBuf::new(),
            cached_git_branch: None,
            cached_git_ahead_behind: None,
            cached_git_space: None,
            metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: FocusedTabs::one(tab),
        };
        workspace.mark_identity_undiscovered();
        workspace
    }

    /// Resets the cached Git identity to "not discovered yet": the label is
    /// the basename of `identity_cwd` (pure string work, no filesystem) and
    /// there is no branch or space. The cached identity cwd is left empty, so
    /// it differs from every resolved cwd and the next background Git refresh
    /// rediscovers the real identity off the main loop.
    pub fn mark_identity_undiscovered(&mut self) {
        self.cached_identity_cwd = PathBuf::new();
        self.cached_auto_label = fallback_label_from_cwd(&self.identity_cwd);
        self.cached_git_status_key = self.identity_cwd.clone();
        self.cached_git_branch = None;
        self.cached_git_ahead_behind = None;
        self.cached_git_space = None;
    }

    pub fn new_with_extra_env(
        initial_cwd: &Path,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        spawn: &PaneSpawnHandles,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            spawn,
            None,
            extra_env,
        )
    }

    fn new_with_tab(
        initial_cwd: &Path,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        spawn: &PaneSpawnHandles,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        let id = generate_workspace_id();
        let launch_env = PaneLaunchEnv::from_extra(extra_env, spawn.api_socket_path.clone())
            .with_pane_id(PublicPaneId::new(&id, 1));
        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                1,
                initial_cwd.to_path_buf(),
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &launch_env,
                spawn,
            )?
        } else {
            Tab::new(
                1,
                initial_cwd.to_path_buf(),
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
                spawn,
            )?
        };
        let root_pane = tab.root_pane;
        Ok((
            Self::with_first_tab(id, None, initial_cwd, tab, root_pane),
            terminal,
            runtime,
        ))
    }

    /// The focused tab. A workspace always holds at least one tab and its
    /// focused index is always in range, so there is no empty case.
    pub fn active_tab(&self) -> &Tab {
        self.tabs.focused()
    }

    pub fn active_tab_index(&self) -> usize {
        self.tabs.focused_index()
    }

    pub fn tab_display_name(&self, tab_idx: usize) -> Option<String> {
        let tab = self.tabs.get(tab_idx)?;
        // Default labels track current position for the UI; `Tab::number` is a
        // stable public identifier and intentionally may differ after a close or reorder.
        Some(
            tab.custom_name
                .clone()
                .unwrap_or_else(|| (tab_idx + 1).to_string()),
        )
    }

    /// Makes `idx` the active tab.
    pub fn switch_tab(&mut self, idx: usize) {
        self.tabs.focus(idx);
    }

    pub fn create_tab(
        &self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Tab, TerminalState, PaneRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            None,
            extra_env,
            spawn,
        )
    }

    // Same argument set as `create_tab`, with an argv instead of a shell.
    pub fn create_tab_argv_command(
        &self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Tab, TerminalState, PaneRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", false),
            Some(argv),
            extra_env,
            spawn,
        )
    }

    // Shared body of the two tab constructors above.
    fn create_tab_with_runtime(
        &self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Tab, TerminalState, PaneRuntime)> {
        let number = self.next_public_tab_number;
        let pane_number = self.next_public_pane_number;
        let launch_env = self.launch_env_for_new_pane(pane_number, extra_env, spawn);

        let (mut tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                number,
                cwd,
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &launch_env,
                spawn,
            )?
        } else {
            Tab::new(
                number,
                cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
                spawn,
            )?
        };
        let root_pane = tab.root_pane;
        if let Some(pane) = tab.panes.get_mut(&root_pane) {
            pane.public_number = pane_number;
        }
        Ok((tab, terminal, runtime))
    }

    /// Admit a prepared tab. `None`, with the workspace unchanged, when the tab
    /// would break the workspace's identity invariants (a duplicate tab, pane
    /// or terminal identity, or a pane tree that disagrees with its layout).
    pub fn commit_new_tab(&mut self, tab: Tab) -> Option<TabCreationOutcome> {
        let next_pane_number = self.next_public_pane_number.max(
            tab.panes
                .values()
                .map(|pane| pane.public_number)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        let next_tab_number = self
            .next_public_tab_number
            .max(tab.number.saturating_add(1));
        if !valid_tabs(
            self.tabs.iter().chain(std::iter::once(&tab)),
            next_pane_number,
            next_tab_number,
        ) {
            tracing::error!(
                workspace = %self.id,
                tab = tab.number,
                "refused a new tab with invalid or duplicate pane and public identities"
            );
            return None;
        }
        let tab_index = self.tabs.len();
        let root_pane = tab.root_pane;
        let pane_numbers = tab
            .panes
            .iter()
            .map(|(pane_id, pane)| (*pane_id, pane.public_number))
            .collect::<Vec<_>>();
        self.tabs.push(tab);
        for (pane_id, public_number) in pane_numbers {
            self.register_new_pane_with_number(pane_id, public_number);
        }
        let next_tab_number = self.tabs[tab_index].number.saturating_add(1);
        self.next_public_tab_number = self.next_public_tab_number.max(next_tab_number);
        Some(TabCreationOutcome {
            tab_index,
            root_pane,
        })
    }

    pub fn close_tab(&mut self, idx: usize) -> Option<TabRemoval> {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.get(idx)?;
        let removal = TabRemoval {
            workspace_id: self.id.clone(),
            tab_index: idx,
            tab_number: tab.number,
            pane_ids: tab.layout.pane_ids(),
            terminal_ids: tab
                .panes
                .values()
                .map(|pane| pane.attached_terminal_id.clone())
                .collect(),
        };
        let _removed_tab = self.tabs.remove(idx)?;
        Some(removal)
    }

    pub fn move_tab(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        self.tabs.move_tab(source_idx, insert_idx)
    }

    // Workspace split routing carries pane identity, geometry, host context, and focus policy.
    #[expect(
        clippy::too_many_arguments,
        reason = "pane creation needs launch settings and spawn context"
    )]
    pub fn split_pane(
        &self,
        pane_id: PaneId,
        direction: Direction,
        geometry: &PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
        spawn: &PaneSpawnHandles,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            geometry,
            cwd,
            default_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
            spawn,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pane creation needs launch settings and spawn context"
    )]
    pub fn split_pane_with_ratio(
        &self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        geometry: &PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
        spawn: &PaneSpawnHandles,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            geometry,
            cwd,
            default_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
            spawn,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pane creation needs launch settings and spawn context"
    )]
    fn split_pane_with_runtime(
        &self,
        pane_id: PaneId,
        direction: Direction,
        ratio: Option<f32>,
        geometry: &PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
        argv: Option<&[String]>,
        spawn: &PaneSpawnHandles,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let pane_number = self.next_public_pane_number;
        let launch_env = self.launch_env_for_new_pane(pane_number, extra_env, spawn);
        let tab = &self.tabs[tab_idx];
        let new_pane = match if let Some(argv) = argv {
            tab.split_pane_argv(
                pane_id,
                focus_new_pane,
                direction,
                ratio,
                geometry,
                cwd,
                default_cwd,
                argv,
                &launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                spawn,
            )
        } else {
            tab.split_pane_shell(
                pane_id,
                focus_new_pane,
                direction,
                ratio,
                geometry,
                cwd,
                default_cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
                spawn,
            )
        } {
            Ok(new_pane) => new_pane,
            Err(err) => return Some(Err(err)),
        };
        Some(Ok((tab_idx, new_pane)))
    }

    pub fn commit_new_pane(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        focus: bool,
    ) -> Option<()> {
        let number = self.next_public_pane_number;
        let tab = self.tabs.get_mut(tab_index)?;
        tab.commit_prepared_split(pane_id, prepared_layout, terminal_id, number)
            .then_some(())?;
        if focus && !tab.focus_pane(pane_id) {
            tracing::error!(workspace = %self.id, ?pane_id, "refused to focus a pane after admitting its split");
        }
        self.register_new_pane_with_number(pane_id, number);
        Some(())
    }

    /// Takes `pane_id` out of its tab, closing the tab when it was the tab's
    /// only pane. Refuses the workspace's only pane, since taking it would
    /// leave the workspace without a live tab: move that pane with
    /// `into_only_pane` (the workspace goes away) or `move_pane_to_new_tab`.
    pub fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<TakenPane> {
        if self.pane_count() <= 1 {
            return None;
        }
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        if self.tabs[tab_idx].panes.len() <= 1 {
            // Another tab holds the remaining panes, so this one can close
            // before its pane leaves it.
            let mut tab = self.tabs.remove(tab_idx)?;
            let moved = tab.take_pane_for_move(pane_id)?;
            return Some(TakenPane {
                moved,
                removed_tab_idx: Some(tab_idx),
            });
        }

        let moved = self.tabs.get_mut(tab_idx)?.take_pane_for_move(pane_id)?;
        Some(TakenPane {
            moved,
            removed_tab_idx: None,
        })
    }

    /// Consumes a workspace holding a single pane and returns that pane, for a
    /// move that takes the workspace's last pane elsewhere. A workspace with
    /// more panes comes back unchanged.
    pub fn into_only_pane(mut self) -> Result<MovedPane, Box<Self>> {
        if self.pane_count() != 1 || self.tabs.len() != 1 {
            return Err(Box::new(self));
        }
        let Some(pane_id) = self
            .tabs
            .first()
            .and_then(|tab| tab.panes.keys().next().copied())
        else {
            return Err(Box::new(self));
        };
        match self
            .tabs
            .get_mut(0)
            .and_then(|tab| tab.take_pane_for_move(pane_id))
        {
            Some(moved) => Ok(moved),
            None => Err(Box::new(self)),
        }
    }

    /// Moves `pane_id` into a new tab of this workspace. The workspace's only
    /// pane gets a new tab in place of its old one, so the workspace is never
    /// seen without a live tab.
    pub fn move_pane_to_new_tab(
        &mut self,
        pane_id: PaneId,
        label: Option<String>,
    ) -> Option<NewTabMove> {
        if self.pane_count() > 1 {
            let taken = self.take_pane_for_move(pane_id)?;
            let tab_idx = self.create_tab_from_existing_pane(taken.moved, label);
            return Some(NewTabMove {
                removed_tab_idx: taken.removed_tab_idx,
                tab_idx,
            });
        }
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let moved = self.tabs.get_mut(tab_idx)?.take_pane_for_move(pane_id)?;
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let _old_tab = self
            .tabs
            .replace(tab_idx, Tab::from_existing_pane(number, label, moved))?;
        self.ensure_inserted_pane_number(pane_id);
        Some(NewTabMove {
            removed_tab_idx: Some(tab_idx),
            tab_idx,
        })
    }

    pub fn insert_moved_pane_into_tab(
        &mut self,
        tab_idx: usize,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> Result<PaneId, MovedPane> {
        let inserted = {
            let Some(tab) = self.tabs.get_mut(tab_idx) else {
                return Err(moved);
            };
            tab.insert_existing_pane(target_pane_id, moved, direction, ratio, focus)
        };
        let pane_id = inserted?;
        self.ensure_inserted_pane_number(pane_id);
        Ok(pane_id)
    }

    pub fn create_tab_from_existing_pane(
        &mut self,
        moved: MovedPane,
        label: Option<String>,
    ) -> usize {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_id = moved.pane_id;
        let tab_index = self
            .tabs
            .push(Tab::from_existing_pane(number, label, moved));
        self.ensure_inserted_pane_number(pane_id);
        tab_index
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .find_map(|tab| tab.panes.get(&pane_id).map(|pane| pane.public_number))
    }

    pub fn pane_id_for_public_number(&self, number: usize) -> Option<PaneId> {
        self.tabs.iter().find_map(|tab| {
            tab.panes
                .iter()
                .find_map(|(pane_id, pane)| (pane.public_number == number).then_some(*pane_id))
        })
    }

    pub fn pane_count(&self) -> usize {
        self.tabs.iter().map(|tab| tab.panes.len()).sum()
    }

    pub(crate) fn launch_env_for_new_pane(
        &self,
        pane_number: usize,
        extra_env: Vec<(String, String)>,
        spawn: &PaneSpawnHandles,
    ) -> PaneLaunchEnv {
        PaneLaunchEnv::from_extra(extra_env, spawn.api_socket_path.clone())
            .with_pane_id(PublicPaneId::new(&self.id, pane_number))
    }

    pub fn next_public_tab_number(&self) -> usize {
        self.next_public_tab_number
    }

    pub fn next_public_pane_number(&self) -> usize {
        self.next_public_pane_number
    }

    pub fn public_tab_number(&self, tab_idx: usize) -> Option<usize> {
        self.tabs.get(tab_idx).map(|tab| tab.number)
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        self.tabs
            .first()
            .and_then(|tab| tab.cwd_for_pane(tab.root_pane, terminals, terminal_runtimes))
            .or_else(|| Some(self.identity_cwd.clone()))
    }

    /// The workspace label: the custom name, else the automatic label cached
    /// from the last admitted Git identity. Every consumer (API workspace
    /// info, sidebar, window title) reads this one value, so they cannot
    /// disagree. The cache follows the workspace's resolved cwd
    /// (`resolved_identity_cwd_from`) through the background Git refresh,
    /// which re-derives it whenever that cwd moves; reading it does no IO.
    pub fn display_name(&self) -> String {
        self.custom_name
            .clone()
            .unwrap_or_else(|| self.cached_auto_label.clone())
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn git_ahead_behind(&self) -> Option<AheadBehind> {
        self.cached_git_ahead_behind
    }

    pub fn find_tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.contains_key(&pane_id))
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.tabs
            .iter()
            .find_map(|tab| tab.panes.get(&pane_id).map(|pane| &pane.pane_state))
    }

    pub fn pane_state_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneState> {
        self.tabs
            .iter_mut()
            .find_map(|tab| tab.panes.get_mut(&pane_id).map(|pane| &mut pane.pane_state))
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.tabs.iter().find_map(|tab| tab.terminal_id(pane_id))
    }

    pub fn focused_pane_id(&self) -> PaneId {
        self.active_tab().layout.focused()
    }

    pub fn prepare_pane_removal(&self, pane_id: PaneId) -> Option<PaneRemovalPlan> {
        let tab_index = self.find_tab_index_for_pane(pane_id)?;
        let tab = self.tabs.get(tab_index)?;
        let scope = if tab.panes.len() > 1 {
            PaneRemovalScope::Pane
        } else if self.tabs.len() > 1 {
            PaneRemovalScope::Tab
        } else {
            PaneRemovalScope::Workspace
        };
        Some(PaneRemovalPlan {
            workspace_id: self.id.to_string(),
            pane_id,
            tab_index,
            scope,
        })
    }

    /// Commits a pane removal prepared from this workspace. The typed scope
    /// tells the app which surrounding container was removed as a consequence.
    /// A workspace-scoped result leaves this value intact; `AppState` owns the
    /// workspace collection and removes it as part of the same command.
    pub fn remove_pane(&mut self, plan: &PaneRemovalPlan) -> Option<PaneRemoval> {
        if plan.workspace_id != self.id
            || self.prepare_pane_removal(plan.pane_id)?.scope != plan.scope
            || self.find_tab_index_for_pane(plan.pane_id)? != plan.tab_index
        {
            return None;
        }

        let (pane_ids, terminal_ids) = match plan.scope {
            PaneRemovalScope::Pane => {
                let tab = self.tabs.get(plan.tab_index)?;
                (
                    vec![plan.pane_id],
                    vec![tab.terminal_id(plan.pane_id)?.clone()],
                )
            }
            PaneRemovalScope::Tab => {
                let tab = self.tabs.get(plan.tab_index)?;
                (
                    tab.layout.pane_ids(),
                    tab.panes
                        .values()
                        .map(|pane| pane.attached_terminal_id.clone())
                        .collect(),
                )
            }
            PaneRemovalScope::Workspace => (
                self.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect(),
                self.tabs
                    .iter()
                    .flat_map(|tab| tab.panes.values())
                    .map(|pane| pane.attached_terminal_id.clone())
                    .collect(),
            ),
        };

        match plan.scope {
            PaneRemovalScope::Pane => {
                self.tabs
                    .get_mut(plan.tab_index)?
                    .close_pane(plan.pane_id)?;
            }
            PaneRemovalScope::Tab => {
                let _removed_tab = self.tabs.remove(plan.tab_index)?;
            }
            PaneRemovalScope::Workspace => {}
        }

        Some(PaneRemoval {
            workspace_id: self.id.to_string(),
            pane_id: plan.pane_id,
            tab_index: plan.tab_index,
            scope: plan.scope,
            pane_ids,
            terminal_ids,
        })
    }

    fn register_new_pane_with_number(&mut self, pane_id: PaneId, number: usize) {
        let Some(pane) = self
            .tabs
            .iter_mut()
            .find_map(|tab| tab.panes.get_mut(&pane_id))
        else {
            tracing::error!(?pane_id, "cannot number a pane missing from its tab");
            return;
        };
        pane.public_number = number;
        self.next_public_pane_number = self.next_public_pane_number.max(number + 1);
    }

    fn ensure_inserted_pane_number(&mut self, pane_id: PaneId) {
        let Some(number) = self.public_pane_number(pane_id) else {
            tracing::error!(?pane_id, "inserted pane is missing from its tab");
            return;
        };
        let duplicate = self.tabs.iter().any(|tab| {
            tab.panes
                .iter()
                .any(|(other_id, pane)| *other_id != pane_id && pane.public_number == number)
        });
        let number = if number == 0 || duplicate {
            self.next_public_pane_number
        } else {
            number
        };
        if let Some(pane) = self
            .tabs
            .iter_mut()
            .find_map(|tab| tab.panes.get_mut(&pane_id))
        {
            pane.public_number = number;
        }
        self.next_public_pane_number = self.next_public_pane_number.max(number + 1);
    }
}

pub struct TakenPane {
    pub moved: MovedPane,
    pub removed_tab_idx: Option<usize>,
}

/// Where `Workspace::move_pane_to_new_tab` put a pane.
pub struct NewTabMove {
    /// The index the pane's old tab had, when the move closed it.
    pub removed_tab_idx: Option<usize>,
    pub tab_idx: usize,
}

#[cfg(test)]
impl FocusedTabs {
    fn focused_mut(&mut self) -> &mut Tab {
        &mut self.items[self.focused]
    }
}

#[cfg(test)]
impl Workspace {
    pub fn public_tab_number_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        self.public_tab_number(tab_idx)
    }

    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRemoval> {
        let plan = self.prepare_pane_removal(pane_id)?;
        self.remove_pane(&plan)
    }

    fn register_new_pane(&mut self, pane_id: PaneId) {
        self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_WORKSPACE_CWD: crate::test_support::ScratchDir =
        crate::test_support::ScratchDir::new("workspace-test-cwd");
}

#[cfg(test)]
impl Workspace {
    pub fn test_new(name: &str) -> Self {
        let identity_cwd = TEST_WORKSPACE_CWD.with(|cwd| cwd.to_path_buf());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut panes = HashMap::new();
        panes.insert(root_id, TabPane::new(PaneState::new(terminal_id)));
        let mut tab = Tab {
            custom_name: None,
            number: 1,
            root_pane: root_id,
            layout,
            panes,
            zoomed: false,
        };
        tab.panes
            .get_mut(&tab.root_pane)
            .expect("test pane exists")
            .public_number = 1;
        let mut workspace = Self {
            id: generate_workspace_id(),
            custom_name: Some(name.to_string()),
            identity_cwd: identity_cwd.clone(),
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label: fallback_label_from_cwd(&identity_cwd),
            cached_git_status_key: identity_cwd.clone(),
            cached_git_branch: None,
            cached_git_ahead_behind: None,
            cached_git_space: None,
            metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: FocusedTabs::one(tab),
        };
        workspace.mark_identity_undiscovered();
        workspace
    }

    pub fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab = self.tabs.focused_mut();
        let new_id = tab.layout.split_focused(direction);
        tab.panes
            .insert(new_id, TabPane::new(PaneState::new(TerminalId::alloc())));
        self.register_new_pane(new_id);
        new_id
    }

    pub fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let (layout, root_id) = TileLayout::new();
        let mut panes = HashMap::new();
        panes.insert(root_id, TabPane::new(PaneState::new(TerminalId::alloc())));
        let tab = Tab {
            custom_name: name.map(str::to_string),
            number: self.next_public_tab_number,
            root_pane: root_id,
            layout,
            panes,
            zoomed: false,
        };
        self.next_public_tab_number += 1;
        self.tabs.push(tab);
        self.register_new_pane(root_id);
        self.tabs.len() - 1
    }

    pub fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert_eq!(
            ws.close_pane(removed_pane).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(None);
        let final_tab = ws.test_add_tab(None);
        let survivor_root = ws.tabs[survivor_tab].root_pane;
        let final_root = ws.tabs[final_tab].root_pane;
        assert!(ws.close_tab(removed_tab).is_some());
        assert!(ws.move_tab(0, ws.tabs.len()));
        ws.switch_tab(
            ws.find_tab_index_for_pane(survivor_root)
                .expect("survivor tab should still exist"),
        );

        assert_ne!(
            ws.active_tab_index() + 1,
            ws.tabs[ws.active_tab_index()].number,
            "adversarial active tab must distinguish position from public tab number"
        );
        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane)
                .expect("test pane has a public pane number"),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        assert_eq!(ws.find_tab_index_for_pane(final_root), Some(1));
        ws
    }

    pub fn assert_invariants_for_test(&self) {
        let mut tab_numbers = std::collections::HashSet::new();
        let mut max_tab_number = 0usize;
        let mut live_panes = std::collections::HashSet::new();
        let mut terminal_ids = std::collections::HashSet::new();
        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;

        for (tab_idx, tab) in self.tabs.iter().enumerate() {
            assert!(
                tab.number > 0,
                "workspace {} tab {} has invalid public tab number 0",
                self.id,
                tab_idx
            );
            assert!(
                tab_numbers.insert(tab.number),
                "workspace {} has duplicate public tab number {}",
                self.id,
                tab.number
            );
            max_tab_number = max_tab_number.max(tab.number);
            assert!(
                tab.panes.contains_key(&tab.root_pane),
                "workspace {} tab {} root pane {:?} is missing from tab panes",
                self.id,
                tab_idx,
                tab.root_pane
            );

            let layout_panes = tab.layout.pane_ids();
            let layout_set: std::collections::HashSet<_> = layout_panes.iter().copied().collect();
            assert_eq!(
                layout_panes.len(),
                layout_set.len(),
                "workspace {} tab {} layout contains duplicate pane ids",
                self.id,
                tab_idx
            );
            assert!(
                layout_set.contains(&tab.layout.focused()),
                "workspace {} tab {} focused pane {:?} is not in layout",
                self.id,
                tab_idx,
                tab.layout.focused()
            );
            let pane_set: std::collections::HashSet<_> = tab.panes.keys().copied().collect();
            assert_eq!(
                layout_set, pane_set,
                "workspace {} tab {} layout panes must exactly match pane records",
                self.id, tab_idx
            );

            for (pane_id, pane) in &tab.panes {
                assert!(
                    live_panes.insert(*pane_id),
                    "workspace {} pane {:?} appears in more than one tab",
                    self.id,
                    pane_id
                );
                assert!(
                    pane.public_number > 0,
                    "workspace {} pane {:?} has invalid public pane number 0",
                    self.id,
                    pane_id
                );
                assert!(
                    pane_numbers.insert(pane.public_number),
                    "workspace {} duplicate public pane number {} for pane {:?}",
                    self.id,
                    pane.public_number,
                    pane_id
                );
                max_pane_number = max_pane_number.max(pane.public_number);
                assert!(
                    terminal_ids.insert(pane.attached_terminal_id.clone()),
                    "workspace {} terminal {} is attached to multiple panes",
                    self.id,
                    pane.attached_terminal_id
                );
            }
        }

        assert!(
            self.next_public_tab_number > 0,
            "workspace {} next_public_tab_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_tab_number > max_tab_number,
            "workspace {} next_public_tab_number {} must be greater than max live public tab number {}",
            self.id,
            self.next_public_tab_number,
            max_tab_number
        );

        assert!(
            self.next_public_pane_number > 0,
            "workspace {} next_public_pane_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_pane_number > max_pane_number,
            "workspace {} next_public_pane_number {} must be greater than max live public pane number {}",
            self.id,
            self.next_public_pane_number,
            max_pane_number
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{decode_public_number, encode_public_number};

    #[test]
    fn public_tab_and_pane_ids_share_one_canonical_format() {
        let workspace_id: WorkspaceId = "wA".parse().expect("canonical workspace id");
        let tab_id = PublicTabId::new(&workspace_id, 32);
        let pane_id = PublicPaneId::new(&workspace_id, 33);

        assert_eq!(tab_id.to_string(), "wA:t0");
        assert_eq!(pane_id.to_string(), "wA:p11");
        assert_eq!("wA:t0".parse::<PublicTabId>(), Ok(tab_id));
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id));
        assert!("wA:t".parse::<PublicTabId>().is_err());
        assert!("wA:p".parse::<PublicPaneId>().is_err());
        assert!("wA:1".parse::<PublicTabId>().is_err());
        assert!("wA:p1".parse::<PublicTabId>().is_err());
        assert!("wA:t1".parse::<PublicPaneId>().is_err());
    }

    #[test]
    fn generated_workspace_ids_are_short_base32_handles() {
        let first = generate_workspace_id();
        let second = generate_workspace_id();

        assert!(first.starts_with('w'));
        assert!(second.starts_with('w'));
        assert_ne!(first, second);
        assert!(first.len() <= 3, "unexpectedly long workspace id: {first}");
        assert!(
            second.len() <= 3,
            "unexpectedly long workspace id: {second}"
        );
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        assert_eq!(encode_public_number(1), "1");
        assert_eq!(encode_public_number(9), "9");
        assert_eq!(encode_public_number(10), "A");
        assert_eq!(encode_public_number(31), "Z");
        assert_eq!(encode_public_number(32), "0");
        assert_eq!(encode_public_number(33), "11");

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let encoded = encode_public_number(value);
            assert_eq!(decode_public_number(&encoded), Some(value));
        }
    }

    #[test]
    fn every_public_number_round_trips_including_zero() {
        for value in (0..=2048).chain([usize::MAX]) {
            let encoded = encode_public_number(value);
            assert_eq!(
                decode_public_number(&encoded),
                Some(value),
                "{value} encoded as {encoded:?}"
            );
        }
    }

    #[test]
    fn reserving_restored_workspace_ids_prevents_reuse() {
        let mut restored = Workspace::test_new("restored");
        restored.id = "wZ".parse().expect("canonical workspace id");

        reserve_workspace_ids(&[restored]);

        let generated = generate_workspace_id();
        assert_ne!(generated, "wZ");
        assert!(generated.number() > 31);
    }

    #[test]
    fn workspace_id_allocation_refuses_to_wrap() {
        let counter = AtomicUsize::new(usize::MAX - 1);
        let last = allocate_workspace_id(&counter).expect("one number left");
        assert_eq!(last.number(), usize::MAX - 1);
        assert_eq!(allocate_workspace_id(&counter), None);
        assert_eq!(allocate_workspace_id(&counter), None);
    }

    #[test]
    fn reserving_an_id_at_the_top_of_the_space_exhausts_allocation() {
        let mut restored = Workspace::test_new("restored");
        restored.id = WorkspaceId::from_number(usize::MAX).expect("nonzero number");
        let counter = AtomicUsize::new(FIRST_WORKSPACE_NUMBER);

        reserve_workspace_numbers(&counter, &[restored]);

        assert_eq!(allocate_workspace_id(&counter), None);
    }

    #[test]
    fn reserving_never_moves_the_counter_back() {
        let mut restored = Workspace::test_new("restored");
        restored.id = WorkspaceId::from_number(3).expect("nonzero number");
        let counter = AtomicUsize::new(10);

        reserve_workspace_numbers(&counter, &[restored]);

        let next = allocate_workspace_id(&counter).expect("numbers left");
        assert_eq!(next.number(), 10);
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tabs[0].root_pane;
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), Some(2));
        assert_eq!(ws.public_pane_number(third), Some(3));

        assert_eq!(
            ws.close_pane(second).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), None);
        assert_eq!(ws.public_pane_number(third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(ws.public_pane_number(fourth), Some(4));
    }

    #[test]
    fn tab_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let first_root = ws.tabs[0].root_pane;
        let second_tab = ws.test_add_tab(None);
        let second_root = ws.tabs[second_tab].root_pane;
        let third_tab = ws.test_add_tab(None);
        let third_root = ws.tabs[third_tab].root_pane;

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(second_root), Some(2));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        assert!(ws.close_tab(second_tab).is_some());

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        let fourth_tab = ws.test_add_tab(None);
        let fourth_root = ws.tabs[fourth_tab].root_pane;
        assert_eq!(ws.public_tab_number_for_pane(fourth_root), Some(4));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn adversarial_identity_state_satisfies_workspace_invariants_after_mutation() {
        let mut ws = Workspace::test_adversarial_identity_state();
        ws.assert_invariants_for_test();

        let active_index = ws.active_tab_index();
        let active_public = ws.tabs[active_index].number;
        assert_ne!(active_index + 1, active_public);
        let divergent_pane = ws
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .find_map(|(pane_id, pane)| {
                (pane_id.raw() as usize != pane.public_number).then_some(*pane_id)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            ws.public_pane_number(divergent_pane)
                .expect("test precondition")
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(ws.public_pane_number(new_pane).is_some());
        assert!(ws.move_tab(ws.active_tab_index(), ws.tabs.len()));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn failed_moved_pane_insert_returns_pane_for_recovery() {
        let source = Workspace::test_new("source");
        let source_pane = source.tabs[0].root_pane;
        let Ok(moved) = source.into_only_pane() else {
            panic!("source pane should be movable");
        };
        let mut target = Workspace::test_new("target");
        let missing_target = PaneId::alloc();

        let recovered = target
            .insert_moved_pane_into_tab(0, missing_target, moved, Direction::Horizontal, 0.5, true)
            .expect_err("invalid target should return the moved pane");

        assert_eq!(recovered.pane_id, source_pane);
        assert!(!target.tabs[0].panes.contains_key(&source_pane));
    }

    #[test]
    fn linked_worktree_auto_label_uses_checkout_name_not_repo_name() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, repo, checkout) =
            crate::git::test_support::create_repo_with_linked_worktree("linked-auto-label");

        let (snapshot, _) = crate::git::git_status_snapshot_for_cwd(&checkout, None);
        let space = snapshot.space;
        let auto_label = snapshot.auto_label;

        assert_eq!(
            space.expect("test precondition").repo_name,
            repo.file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
        assert_eq!(
            auto_label,
            checkout
                .file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
    }

    #[test]
    fn display_name_reads_cached_identity_without_rechecking_filesystem() {
        let root = crate::test_support::ScratchDir::new("label-cache");
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("create nested cwd");

        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_auto_label = "cached-repo".into();

        std::fs::remove_dir_all(root).expect("remove cwd after cache admission");

        assert_eq!(ws.display_name(), "cached-repo");
    }

    #[test]
    fn label_is_the_admitted_identity_even_when_the_live_cwd_has_moved() {
        // A subdirectory `cd` without OSC 7 used to make the API show the
        // subdirectory basename while the window title showed the repo name.
        // Both now read the one cached label until the background refresh
        // admits the new cwd.
        let mut ws = Workspace::test_new("ignored");
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0]
            .terminal_id(root_pane)
            .expect("test precondition")
            .clone();
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/old/workspace");
        ws.cached_identity_cwd = PathBuf::from("/new/repo");
        ws.cached_auto_label = "repo".into();
        let terminals = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/new/repo/deep")),
        )]);

        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &PaneRuntimeRegistry::new()),
            Some(PathBuf::from("/new/repo/deep"))
        );
        assert_eq!(ws.display_name(), "repo");
    }

    #[test]
    fn workspace_identity_follows_first_tab_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0]
            .terminal_id(root_pane)
            .expect("test precondition")
            .clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/shepr-test/pion")),
        );
        let terminal_runtimes = PaneRuntimeRegistry::new();

        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/shepr-test/pion"))
        );
    }

    #[test]
    fn undiscovered_identity_labels_by_basename_and_never_matches_a_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/shepr-test/repo/sub");
        ws.cached_git_branch = Some("main".into());

        ws.mark_identity_undiscovered();

        assert_eq!(ws.display_name(), "sub");
        assert_eq!(ws.branch(), None);
        assert_eq!(ws.cached_git_space, None);
        assert_ne!(ws.cached_identity_cwd, ws.identity_cwd);
        assert!(ws.cached_identity_cwd.as_os_str().is_empty());
    }

    #[test]
    fn workspace_built_from_a_moved_pane_does_not_discover_git_identity() {
        let source = Workspace::test_new("source");
        let pane = source.tabs[0].root_pane;
        let Ok(moved) = source.into_only_pane() else {
            panic!("test precondition");
        };
        // A path that cannot exist: discovery would have to stat it.
        let cwd = PathBuf::from("/shepr-test-nonexistent/repo/sub");

        let ws = Workspace::from_existing_pane(None, None, &cwd, moved);

        assert_eq!(ws.display_name(), "sub");
        assert!(ws.cached_identity_cwd.as_os_str().is_empty());
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.public_pane_number(pane), Some(1));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn moving_tab_keeps_active_identity_and_stable_tab_numbers() {
        let mut ws = Workspace::test_new("test");
        let moved_root = ws.tabs[0].root_pane;
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        let active_root = ws.tabs[final_auto_idx].root_pane;
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        let labels: Vec<_> = (0..ws.tabs.len())
            .map(|tab_idx| ws.tab_display_name(tab_idx).expect("test precondition"))
            .collect();
        assert_eq!(labels, vec!["foo", "2", "3"]);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("foo"));
        assert!(ws.tabs[1].custom_name.is_none());
        assert!(ws.tabs[2].custom_name.is_none());
        assert_eq!(ws.tabs[0].number, 2);
        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(ws.tabs[2].number, 1);
        assert_eq!(ws.tabs[2].root_pane, moved_root);
        assert_eq!(ws.tabs[ws.active_tab_index()].root_pane, active_root);
        ws.assert_invariants_for_test();
    }

    fn workspace_with_tabs(count: usize) -> Workspace {
        let mut ws = Workspace::test_new("tabs");
        for _ in 1..count {
            ws.test_add_tab(None);
        }
        ws
    }

    #[test]
    fn focused_tabs_reject_an_empty_list_or_an_out_of_range_focus() {
        let ws = Workspace::test_new("one");
        let tabs = ws.tabs.items;
        assert!(FocusedTabs::new(Vec::new(), 0).is_none());
        assert!(FocusedTabs::new(tabs, 1).is_none());
    }

    #[test]
    fn focused_tabs_never_drop_their_last_tab() {
        let mut ws = workspace_with_tabs(3);
        ws.switch_tab(2);

        assert!(ws.tabs.remove(3).is_none());
        assert!(ws.tabs.remove(2).is_some());
        assert_eq!(ws.active_tab_index(), 1);
        assert!(ws.tabs.remove(0).is_some());
        assert_eq!(ws.active_tab_index(), 0);
        assert!(ws.tabs.remove(0).is_none());
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active_tab().number, ws.tabs[0].number);

        ws.switch_tab(1);
        assert_eq!(ws.active_tab_index(), 0);
    }

    #[test]
    fn focused_tabs_move_keeps_focus_on_the_same_tab() {
        for focused in 0..4 {
            for source in 0..4 {
                for insert in 0..=4 {
                    let mut ws = workspace_with_tabs(4);
                    ws.switch_tab(focused);
                    let focused_number = ws.tabs[focused].number;
                    let moved_number = ws.tabs[source].number;

                    let moved = ws.move_tab(source, insert);

                    let target = if source < insert { insert - 1 } else { insert };
                    assert_eq!(moved, target != source, "{focused} {source} {insert}");
                    assert_eq!(ws.tabs[target].number, moved_number);
                    assert_eq!(
                        ws.active_tab().number,
                        focused_number,
                        "focused {focused} source {source} insert {insert}"
                    );
                }
            }
        }
        let mut ws = workspace_with_tabs(2);
        assert!(!ws.move_tab(2, 0));
        assert!(!ws.move_tab(0, 3));
    }

    #[test]
    fn a_workspace_only_pane_is_never_taken_in_place() {
        let mut ws = Workspace::test_new("only");
        let pane = ws.tabs[0].root_pane;

        assert!(ws.take_pane_for_move(pane).is_none());
        assert_eq!(ws.tabs[0].panes.len(), 1);
        ws.assert_invariants_for_test();

        let Ok(moved) = ws.into_only_pane() else {
            panic!("a single-pane workspace yields its pane");
        };
        assert_eq!(moved.pane_id, pane);

        let mut split = Workspace::test_new("split");
        split.test_split(Direction::Vertical);
        let Err(split) = split.into_only_pane() else {
            panic!("a workspace with two panes is not consumed");
        };
        assert_eq!(split.pane_count(), 2);
    }

    #[test]
    fn a_tab_only_pane_closes_its_tab_when_another_tab_remains() {
        let mut ws = workspace_with_tabs(2);
        ws.switch_tab(1);
        let pane = ws.tabs[0].root_pane;
        let survivor = ws.tabs[1].number;

        let taken = ws.take_pane_for_move(pane).expect("pane is movable");

        assert_eq!(taken.moved.pane_id, pane);
        assert_eq!(taken.removed_tab_idx, Some(0));
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active_tab().number, survivor);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn moving_the_only_pane_to_a_new_tab_replaces_its_tab() {
        let mut ws = Workspace::test_new("only");
        let pane = ws.tabs[0].root_pane;
        let old_number = ws.tabs[0].number;

        let placed = ws
            .move_pane_to_new_tab(pane, Some("moved".into()))
            .expect("pane is movable");

        assert_eq!(placed.removed_tab_idx, Some(0));
        assert_eq!(placed.tab_idx, 0);
        assert_eq!(ws.tabs.len(), 1);
        assert_ne!(ws.tabs[0].number, old_number);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("moved"));
        assert_eq!(ws.tabs[0].root_pane, pane);
        assert!(ws.tabs[0].panes.contains_key(&pane));
        assert_eq!(ws.active_tab_index(), 0);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn moving_a_pane_to_a_new_tab_appends_the_tab() {
        let mut ws = workspace_with_tabs(2);
        let pane = ws.tabs[0].root_pane;
        let kept = ws.tabs[1].number;

        let placed = ws
            .move_pane_to_new_tab(pane, None)
            .expect("pane is movable");

        assert_eq!(placed.removed_tab_idx, Some(0));
        assert_eq!(placed.tab_idx, 1);
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.tabs[0].number, kept);
        assert_eq!(ws.tabs[1].root_pane, pane);

        let split_pane = ws.test_split(Direction::Vertical);
        let placed = ws
            .move_pane_to_new_tab(split_pane, None)
            .expect("pane is movable");
        assert_eq!(placed.removed_tab_idx, None);
        assert_eq!(placed.tab_idx, 2);
        ws.assert_invariants_for_test();
    }
}
