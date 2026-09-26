use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use ratatui::layout::Direction;

use super::PaneSpawnHandles;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

pub(crate) type DetachedPane = (PaneId, TerminalId);

pub(crate) struct MovedPane {
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
    pub(crate) fn new(pane_state: PaneState) -> Self {
        Self {
            pane_state,
            public_number: 0,
        }
    }
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: TerminalRuntime,
    pub(crate) prepared_layout: TileLayout,
}

pub struct Tab {
    pub custom_name: Option<String>,
    pub number: usize,
    /// Identity source for this tab's pane tree.
    pub root_pane: PaneId,
    pub layout: TileLayout,
    /// Runtime-independent pane records, keyed by internal ID.
    pub panes: HashMap<PaneId, TabPane>,
    pub zoomed: bool,
}

impl Tab {
    // Tab construction threads pane runtime geometry, host context, and render hooks.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
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

    // Command tab construction mirrors the shell tab runtime arguments.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_argv_command(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
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

    #[allow(clippy::too_many_arguments)]
    fn new_with_runtime(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        spawn: &PaneSpawnHandles,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let runtime = if let Some(argv) = argv {
            TerminalRuntime::spawn_argv_command(
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
            )?
        } else {
            TerminalRuntime::spawn(
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

    /// Prepare a shell split on a cloned layout and start its runtime. The
    /// returned layout is installed by the workspace command after startup.
    /// Focus moves only when `focus_new_pane` is set.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_pane_shell(
        &self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        geometry: &super::PaneGeometry,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_pane_argv(
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
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
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

    // Split construction threads geometry, host context, launch policy, and command state.
    #[allow(clippy::too_many_arguments)]
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
        host_terminal_theme: crate::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
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
            Some(argv) => TerminalRuntime::spawn_argv_command(
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
            ),
            None => TerminalRuntime::spawn(
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

    pub(crate) fn commit_prepared_split(
        &mut self,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        public_number: usize,
    ) -> bool {
        let current_ids = self.layout.pane_ids();
        let prepared_ids = prepared_layout.pane_ids();
        if self.panes.contains_key(&pane_id)
            || !prepared_ids.contains(&pane_id)
            || prepared_ids.len() != current_ids.len().saturating_add(1)
            || current_ids.iter().any(|id| !prepared_ids.contains(id))
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
        if self.panes.len() <= 1 {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        self.layout.close_pane(pane_id);

        let pane = self.panes.remove(&pane_id)?;
        let terminal_id = pane.pane_state.attached_terminal_id;
        self.zoomed = false;
        if let Some(next_root) = next_root {
            self.root_pane = next_root;
        }
        Some((pane_id, terminal_id))
    }

    pub(crate) fn from_existing_pane(
        number: usize,
        custom_name: Option<String>,
        moved: MovedPane,
    ) -> Self {
        let mut panes = HashMap::new();
        let pane_id = moved.pane_id;
        panes.insert(pane_id, moved.pane);
        Self {
            custom_name,
            number,
            root_pane: pane_id,
            layout: TileLayout::from_saved(Node::Pane(pane_id), pane_id),
            panes,
            zoomed: false,
        }
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<MovedPane> {
        if !self.panes.contains_key(&pane_id) {
            return None;
        }

        if self.panes.len() > 1 {
            let next_root = self.promoted_root_if_needed(pane_id);
            self.layout.close_pane(pane_id);
            if let Some(next_root) = next_root {
                self.root_pane = next_root;
            }
        }

        let pane = self.panes.remove(&pane_id)?;
        self.zoomed = false;
        Some(MovedPane { pane_id, pane })
    }

    pub(crate) fn insert_existing_pane(
        &mut self,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> Result<PaneId, MovedPane> {
        if !self
            .layout
            .insert_pane_near(target_pane_id, moved.pane_id, direction, ratio, focus)
        {
            return Err(moved);
        }
        let pane_id = moved.pane_id;
        self.panes.insert(pane_id, moved.pane);
        self.zoomed = false;
        Ok(pane_id)
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
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(TerminalRuntime::cwd)
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd.clone())
            })
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(TerminalRuntime::foreground_cwd)
    }
}
