use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use super::PaneSpawnHandles;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::pane::{PaneRuntime, PaneRuntimeRegistry};
use crate::terminal::TerminalState;
use shepr_core::layout::{Direction, PaneId, TileLayout};
use shepr_protocol::TerminalId;

pub(crate) type DetachedPane = (PaneId, TerminalId);

/// A pane built outside a tab, for constructors that start a tab or workspace
/// from one pane without spawning anything.
pub struct ExistingPane {
    pub pane_id: PaneId,
    pub pane: TabPane,
}

/// One pane's state and stable public number. Keeping both in the tab record
/// makes the tab's pane map the source of pane identity metadata.
pub struct TabPane {
    pub pane_state: PaneState,
    pub public_number: usize,
}

impl Deref for TabPane {
    type Target = PaneState;

    fn deref(&self) -> &Self::Target {
        &self.pane_state
    }
}

impl DerefMut for TabPane {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pane_state
    }
}

impl TabPane {
    pub fn new(pane_state: PaneState) -> Self {
        Self {
            pane_state,
            public_number: 0,
        }
    }
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: PaneRuntime,
    pub prepared_layout: TileLayout,
}

pub struct Tab {
    // Persistence reads tabs for snapshots and fills detached tabs during
    // restore; other crates use these accessors and workspace mutators.
    pub(crate) custom_name: Option<String>,
    pub(crate) number: usize,
    /// Identity source for this tab's pane tree.
    pub(crate) root_pane: PaneId,
    pub(crate) layout: TileLayout,
    /// Runtime-independent pane records, keyed by internal ID.
    pub(crate) panes: HashMap<PaneId, TabPane>,
    pub(crate) zoomed: bool,
}

impl Tab {
    /// A detached one-pane tab around `root`, with no runtime behind it. The
    /// layout is built here, so its pane set and the record always agree;
    /// admitting the tab (`Workspace::commit_new_tab`) still checks its number
    /// and pane identities against the workspace.
    pub fn single_pane(custom_name: Option<String>, number: usize, root: TabPane) -> Self {
        let (layout, root_pane) = TileLayout::new();
        Self {
            custom_name,
            number,
            root_pane,
            layout,
            panes: HashMap::from([(root_pane, root)]),
            zoomed: false,
        }
    }

    pub fn custom_name(&self) -> Option<&str> {
        self.custom_name.as_deref()
    }

    pub fn number(&self) -> usize {
        self.number
    }

    pub fn root_pane(&self) -> PaneId {
        self.root_pane
    }

    pub fn layout(&self) -> &TileLayout {
        &self.layout
    }

    pub fn panes(&self) -> &HashMap<PaneId, TabPane> {
        &self.panes
    }

    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    pub(super) fn has_consistent_panes(&self) -> bool {
        let layout_ids = self.layout.pane_ids();
        let layout_set: HashSet<_> = layout_ids.iter().copied().collect();
        layout_ids.len() == layout_set.len()
            && layout_set.len() == self.panes.len()
            && layout_set.contains(&self.root_pane)
            && layout_set.contains(&self.layout.focused())
            && self.panes.keys().all(|id| layout_set.contains(id))
    }

    pub fn new(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            spawn,
            None,
        )
    }

    pub(crate) fn new_argv_command(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", false),
            launch_env,
            spawn,
            Some(argv),
        )
    }

    fn new_with_runtime(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let runtime = if let Some(argv) = argv {
            PaneRuntime::spawn_argv_command(
                root_id,
                rows,
                cols,
                &initial_cwd,
                argv,
                launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &spawn.events,
                &spawn.render_notify,
                &spawn.render_dirty,
                &spawn.pane_teardowns,
            )?
        } else {
            PaneRuntime::spawn(
                root_id,
                rows,
                cols,
                &initial_cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                launch_env,
                &spawn.events,
                &spawn.render_notify,
                &spawn.render_dirty,
                &spawn.pane_teardowns,
            )?
        };

        let terminal_id = TerminalId::alloc();
        let terminal = match argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), initial_cwd).with_launch_argv(argv.to_vec())
            }
            None => TerminalState::new(terminal_id.clone(), initial_cwd),
        };
        let mut panes = HashMap::new();
        panes.insert(root_id, TabPane::new(PaneState::new(terminal_id)));

        Ok((
            Self {
                custom_name: None,
                number,
                root_pane: root_id,
                layout,
                panes,
                zoomed: false,
            },
            terminal,
            runtime,
        ))
    }

    pub fn is_auto_named(&self) -> bool {
        self.custom_name.is_none()
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    pub(super) fn clear_custom_name(&mut self) {
        self.custom_name = None;
    }

    pub(super) fn set_zoomed(&mut self, zoomed: bool) {
        self.zoomed = zoomed;
    }

    pub(super) fn focus_pane(&mut self, pane_id: PaneId) -> bool {
        if !self.has_consistent_panes() || !self.panes.contains_key(&pane_id) {
            return false;
        }
        self.layout.focus_pane(pane_id);
        true
    }

    pub(super) fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        self.has_consistent_panes() && self.layout.swap_panes(first, second)
    }

    pub(super) fn resize_focused_pane(
        &mut self,
        direction: shepr_core::layout::NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        if !self.has_consistent_panes() {
            return false;
        }
        self.layout.resize_focused(direction, delta, area);
        true
    }

    pub(super) fn resize_pane(
        &mut self,
        pane_id: PaneId,
        direction: shepr_core::layout::NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.has_consistent_panes() && self.layout.resize_pane(pane_id, direction, delta, area)
    }

    pub(super) fn set_split_ratio_at(
        &mut self,
        path: &[shepr_core::geometry::SplitBranch],
        ratio: f32,
    ) -> bool {
        self.has_consistent_panes() && self.layout.set_ratio_at(path, ratio)
    }

    /// Prepare a shell split on a cloned layout and start its runtime. The
    /// returned layout is installed by the workspace command after startup.
    /// Focus moves only when `focus_new_pane` is set.
    #[expect(
        clippy::too_many_arguments,
        reason = "a split threads target, geometry, host context, launch policy, and render hooks"
    )]
    pub fn split_pane_shell(
        &self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        geometry: &super::PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            target,
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
            launch_env,
            spawn,
            None,
        )
    }

    /// Split `target` with an argv-command pane. Same focus contract as
    /// `split_pane_shell`.
    #[expect(
        clippy::too_many_arguments,
        reason = "an argv split mirrors the shell split's arguments plus the command"
    )]
    pub fn split_pane_argv(
        &self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        geometry: &super::PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            target,
            focus_new_pane,
            direction,
            ratio,
            geometry,
            cwd,
            default_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", false),
            launch_env,
            spawn,
            Some(argv),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "split construction threads geometry, host context, launch policy, and command state"
    )]
    fn split_pane_with_runtime(
        &self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        geometry: &super::PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
        argv: Option<&[String]>,
    ) -> std::io::Result<NewPane> {
        let mut prepared_layout = self.layout.clone();
        let Some(new_id) = prepared_layout.split_pane(target, direction, ratio.unwrap_or(0.5))
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "split target pane is not in the layout",
            ));
        };
        // The split un-zooms the tab (below), so size against the tiled layout.
        let (rows, cols) = geometry
            .pane_size(&prepared_layout, false, new_id)
            .unwrap_or_else(|| geometry.sole_pane_size());
        let actual_cwd = cwd.unwrap_or(default_cwd);
        let launch_argv = argv.map(<[String]>::to_vec);
        let runtime = match argv {
            Some(argv) => PaneRuntime::spawn_argv_command(
                new_id,
                rows,
                cols,
                &actual_cwd,
                argv,
                launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &spawn.events,
                &spawn.render_notify,
                &spawn.render_dirty,
                &spawn.pane_teardowns,
            ),
            None => PaneRuntime::spawn(
                new_id,
                rows,
                cols,
                &actual_cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                launch_env,
                &spawn.events,
                &spawn.render_notify,
                &spawn.render_dirty,
                &spawn.pane_teardowns,
            ),
        };
        let runtime = runtime?;
        let terminal_id = TerminalId::alloc();
        let terminal = match launch_argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), actual_cwd).with_launch_argv(argv)
            }
            None => TerminalState::new(terminal_id.clone(), actual_cwd),
        };
        if focus_new_pane {
            prepared_layout.focus_pane(new_id);
        }
        Ok(NewPane {
            pane_id: new_id,
            terminal,
            runtime,
            prepared_layout,
        })
    }

    pub fn commit_prepared_split(
        &mut self,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        public_number: usize,
    ) -> bool {
        let current_ids = self.layout.pane_ids();
        let prepared_ids = prepared_layout.pane_ids();
        if !self.has_consistent_panes()
            || self.panes.contains_key(&pane_id)
            || !prepared_ids.contains(&pane_id)
            || prepared_ids.len() != current_ids.len().saturating_add(1)
            || current_ids.iter().any(|id| !prepared_ids.contains(id))
            || !prepared_ids.contains(&prepared_layout.focused())
        {
            return false;
        }

        self.layout = prepared_layout;
        self.zoomed = false;
        let mut pane = TabPane::new(PaneState::new(terminal_id));
        pane.public_number = public_number;
        self.panes.insert(pane_id, pane);
        true
    }

    /// Detaches `pane_id` from the layout and returns it with its terminal id.
    /// The runtime is left to the caller. `None` when the pane is the tab's
    /// last one (the tab itself must go) or is not in this tab.
    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        if self.panes.len() <= 1
            || !self.has_consistent_panes()
            || !self.panes.contains_key(&pane_id)
        {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        if !self.layout.close_pane(pane_id) {
            return None;
        }

        let pane = self.panes.remove(&pane_id)?;
        let terminal_id = pane.pane_state.attached_terminal_id;
        self.zoomed = false;
        if let Some(next_root) = next_root {
            self.root_pane = next_root;
        }
        Some((pane_id, terminal_id))
    }

    pub fn from_existing_pane(
        number: usize,
        custom_name: Option<String>,
        existing: ExistingPane,
    ) -> Self {
        let mut panes = HashMap::new();
        let pane_id = existing.pane_id;
        panes.insert(pane_id, existing.pane);
        Self {
            custom_name,
            number,
            root_pane: pane_id,
            layout: TileLayout::from_live_pane(pane_id),
            panes,
            zoomed: false,
        }
    }

    fn promoted_root_if_needed(&self, closing: PaneId) -> Option<PaneId> {
        if self.root_pane != closing {
            return None;
        }
        self.layout.pane_ids().into_iter().find(|id| *id != closing)
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.panes
            .get(&pane_id)
            .map(|pane| &pane.attached_terminal_id)
    }

    pub fn cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(PaneRuntime::cwd)
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd().to_path_buf())
            })
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(PaneRuntime::foreground_cwd)
    }
}
