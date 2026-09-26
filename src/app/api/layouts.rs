use std::path::PathBuf;

use ratatui::layout::Direction;

use crate::api::schema::{
    EventData, EventEnvelope, LayoutApplyParams, LayoutDescription, LayoutExportParams, LayoutNode,
    LayoutPane, LayoutSetSplitRatioParams, ResponseResult, SplitDirection,
};
use crate::app::{App, Mode};
use crate::layout::{Node, PaneId};

use super::responses::{encode_error, encode_success};

const MAX_LAYOUT_PANES: usize = 24;
const MAX_LAYOUT_DEPTH: usize = 16;

impl App {
    pub(super) fn handle_layout_export(
        &mut self,
        id: String,
        params: &LayoutExportParams,
    ) -> String {
        let Some((ws_idx, tab_idx)) = self.resolve_layout_export_target(params) else {
            return encode_error(id, "layout_not_found", "layout target not found");
        };
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return encode_error(id, "layout_not_found", "layout unavailable");
        };

        encode_success(id, ResponseResult::LayoutExport { layout })
    }

    pub(super) fn handle_layout_apply(&mut self, id: String, params: &LayoutApplyParams) -> String {
        let replace_target = match params.tab_id.as_deref() {
            Some(tab_id) => match self.parse_tab_id(tab_id) {
                Some(target) => Some(target),
                None => {
                    return encode_error(id, "tab_not_found", format!("tab {tab_id} not found"));
                }
            },
            None => None,
        };
        if replace_target.is_some() && params.workspace_id.is_some() {
            return encode_error(
                id,
                "invalid_target",
                "use either tab_id or workspace_id, not both",
            );
        }

        let ws_idx = if let Some((ws_idx, _)) = replace_target {
            ws_idx
        } else if let Some(workspace_id) = params.workspace_id.as_deref() {
            let Some(ws_idx) = self.parse_workspace_id(workspace_id) else {
                return encode_error(
                    id,
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                );
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        if let Err(message) = validate_layout_tree(&params.root) {
            return encode_error(id, "invalid_layout", message);
        }
        if let Err(message) = validate_layout_launches(&params.root) {
            return encode_error(id, "invalid_layout", message);
        }

        let replacement_label = params.tab_label.clone().or_else(|| {
            let (_, tab_idx) = replace_target?;
            self.state
                .workspaces
                .get(ws_idx)?
                .tabs
                .get(tab_idx)?
                .custom_name
                .clone()
        });
        let replace_was_active = replace_target.is_some_and(|(target_ws, target_tab)| {
            self.state.active == Some(target_ws)
                && self
                    .state
                    .workspaces
                    .get(target_ws)
                    .is_some_and(|ws| ws.active_tab_index() == target_tab)
        });
        let replace_close_events = replace_target
            .map(|(target_ws, target_tab)| self.tab_close_events(target_ws, target_tab))
            .unwrap_or_default();
        let root_leaf = first_layout_leaf(&params.root);
        let first_cwd = self.layout_root_cwd(ws_idx, replace_target, root_leaf);
        let (rows, cols) = self.state.pane_geometry().sole_pane_size();
        let default_shell = self.state.settings.default_shell.clone();
        let scrollback_limit_bytes = self.state.settings.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(root_leaf.env.clone()) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let command = match layout_command(root_leaf) {
            Ok(command) => command,
            Err(message) => return encode_error(id, "invalid_layout", message),
        };

        let spawn = self.pane_spawn_handles();
        let (workspace_id, tab_number, root_pane_number, created) = {
            let Some(workspace) = self.state.workspaces.get(ws_idx) else {
                return encode_error(id, "workspace_not_found", "workspace not found");
            };
            let workspace_id = workspace.id.clone();
            let tab_number = workspace.next_public_tab_number();
            let root_pane_number = workspace.next_public_pane_number();
            let created = if let Some(argv) = command.as_deref() {
                workspace.create_tab_argv_command(
                    rows,
                    cols,
                    first_cwd,
                    argv,
                    extra_env,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    &spawn,
                )
            } else {
                workspace.create_tab(
                    rows,
                    cols,
                    first_cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(
                        &default_shell,
                        self.state.settings.login_shell,
                    ),
                    extra_env,
                    &spawn,
                )
            };
            (workspace_id, tab_number, root_pane_number, created)
        };

        let (mut tab, terminal, runtime) = match created {
            Ok(result) => result,
            Err(err) => return encode_error(id, "layout_apply_failed", err.to_string()),
        };
        let new_root_pane = tab.root_pane;
        let root_terminal_id = terminal.id.clone();
        let root_cwd = terminal.cwd.clone();
        let mut pane_terminals = std::collections::HashMap::from([(new_root_pane, terminal)]);
        let mut pane_runtimes =
            std::collections::HashMap::from([(new_root_pane, (root_terminal_id, runtime))]);
        let mut pane_cwds = std::collections::HashMap::from([(new_root_pane, root_cwd)]);
        let mut next_pane_number = root_pane_number.saturating_add(1);
        let mut staging = LayoutStaging {
            workspace_id: &workspace_id,
            tab_number,
            geometry: self.state.pane_geometry(),
            default_shell: &default_shell,
            login_shell: self.state.settings.login_shell,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            default_cwd: self
                .paths
                .current_dir()
                .unwrap_or_else(|| std::path::Path::new("/")),
            next_pane_number: &mut next_pane_number,
            pane_cwds: &mut pane_cwds,
            pane_terminals: &mut pane_terminals,
            pane_runtimes: &mut pane_runtimes,
            spawn: &spawn,
        };
        if let Err(message) = stage_layout_node(&mut tab, new_root_pane, &params.root, &mut staging)
        {
            return encode_error(id, "layout_apply_failed", message);
        }
        if let Some(label) = replacement_label {
            tab.set_custom_name(label);
        }
        let terminals = pane_terminals.into_values().collect();
        let runtimes = pane_runtimes.into_values().collect::<Vec<_>>();
        let Some(tab_outcome) = self
            .state
            .commit_layout_tab_creation(ws_idx, tab, terminals, false)
        else {
            drop(runtimes);
            return encode_error(id, "layout_apply_failed", "workspace not found");
        };
        for (terminal_id, runtime) in runtimes {
            self.terminal_runtimes.insert(terminal_id, runtime);
        }

        if let Some((target_ws_idx, target_tab_idx)) = replace_target {
            let Some(plan) = self
                .state
                .prepare_tab_removal(target_ws_idx, target_tab_idx)
            else {
                return encode_error(id, "tab_not_found", "tab not found");
            };
            if matches!(
                self.state.commit_tab_removal(&plan),
                crate::app::actions::TabRemovalCommit::Removed(_)
            ) {
                self.shutdown_detached_terminal_runtimes();
                self.emit_events(replace_close_events);
            }
        }

        let new_tab_idx = tab_outcome
            .tab_index
            .saturating_sub(usize::from(replace_target.is_some()));

        if params.focus || replace_was_active {
            self.state.switch_workspace_tab(ws_idx, new_tab_idx);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        if let Some(tab) = self.tab_info(ws_idx, new_tab_idx) {
            self.emit_event(EventEnvelope {
                data: EventData::TabCreated { tab },
            });
        }
        for pane_id in self.state.workspaces[ws_idx].tabs[new_tab_idx]
            .layout
            .pane_ids()
        {
            if let Some(pane) = self.pane_info(ws_idx, pane_id) {
                self.emit_event(EventEnvelope {
                    data: EventData::PaneCreated { pane },
                });
            }
        }
        self.emit_layout_updated_event(ws_idx, new_tab_idx);

        let Some(layout) = self.layout_description(ws_idx, new_tab_idx) else {
            return encode_error(id, "layout_apply_failed", "new layout unavailable");
        };
        encode_success(id, ResponseResult::LayoutApply { layout })
    }

    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        id: String,
        params: LayoutSetSplitRatioParams,
    ) -> String {
        if !params.ratio.is_finite() {
            return encode_error(id, "invalid_ratio", "ratio must be finite");
        }
        let Some((ws_idx, tab_idx)) = self.resolve_layout_export_target(&LayoutExportParams {
            tab_id: params.tab_id,
            pane_id: params.pane_id,
        }) else {
            return encode_error(id, "layout_not_found", "layout target not found");
        };

        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
            .is_some_and(|tab| tab.layout.set_ratio_at(&params.path, params.ratio));
        if !changed {
            return encode_error(id, "split_not_found", "split path not found");
        }

        self.schedule_session_save();
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return encode_error(id, "layout_not_found", "layout unavailable");
        };
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::LayoutSplitRatioSet { layout })
    }

    fn resolve_layout_export_target(&self, params: &LayoutExportParams) -> Option<(usize, usize)> {
        match (params.tab_id.as_deref(), params.pane_id.as_deref()) {
            (Some(_), Some(_)) => None,
            (Some(tab_id), None) => self.parse_tab_id(tab_id),
            (None, Some(pane_id)) => {
                let (ws_idx, pane_id) = self.parse_pane_id(pane_id)?;
                let tab_idx = self
                    .state
                    .workspaces
                    .get(ws_idx)?
                    .find_tab_index_for_pane(pane_id)?;
                Some((ws_idx, tab_idx))
            }
            (None, None) => {
                let ws_idx = self.state.active?;
                let tab_idx = self.state.workspaces.get(ws_idx)?.active_tab_index();
                Some((ws_idx, tab_idx))
            }
        }
    }

    fn layout_description(&self, ws_idx: usize, tab_idx: usize) -> Option<LayoutDescription> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        Some(LayoutDescription {
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            zoomed: tab.zoomed,
            focused_pane_id: self.public_pane_id(ws_idx, tab.layout.focused())?,
            root: self.layout_node_description(ws_idx, tab_idx, tab.layout.root())?,
        })
    }

    fn layout_node_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        node: &Node,
    ) -> Option<LayoutNode> {
        match node {
            Node::Pane(pane_id) => Some(LayoutNode::Pane {
                pane: self.layout_pane_description(ws_idx, tab_idx, *pane_id)?,
            }),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => Some(LayoutNode::Split {
                direction: match direction {
                    Direction::Horizontal => SplitDirection::Right,
                    Direction::Vertical => SplitDirection::Down,
                },
                ratio: *ratio,
                first: Box::new(self.layout_node_description(ws_idx, tab_idx, first)?),
                second: Box::new(self.layout_node_description(ws_idx, tab_idx, second)?),
            }),
        }
    }

    fn layout_pane_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        pane_id: PaneId,
    ) -> Option<LayoutPane> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        let terminal_id = tab.terminal_id(pane_id)?;
        let terminal = self.state.terminals.get(terminal_id);
        Some(LayoutPane {
            pane_id: Some(self.public_pane_id(ws_idx, pane_id)?),
            label: terminal.and_then(|terminal| terminal.manual_label.clone()),
            cwd: tab
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            command: terminal.and_then(|terminal| terminal.launch_argv.clone()),
            env: Default::default(),
        })
    }

    fn layout_root_cwd(
        &self,
        ws_idx: usize,
        replace_target: Option<(usize, usize)>,
        pane: &LayoutPane,
    ) -> PathBuf {
        if let Some(cwd) = pane.cwd.as_ref() {
            return PathBuf::from(cwd);
        }
        let follow_cwd = replace_target.and_then(|(_, tab_idx)| {
            let pane_id = self
                .state
                .workspaces
                .get(ws_idx)?
                .tabs
                .get(tab_idx)?
                .layout
                .focused();
            self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id)
        });
        self.resolve_new_terminal_cwd(
            follow_cwd.or_else(|| self.focused_pane_cwd_in_workspace(ws_idx)),
        )
    }
}

struct LayoutStaging<'a> {
    workspace_id: &'a str,
    tab_number: usize,
    geometry: crate::workspace::PaneGeometry,
    default_shell: &'a str,
    login_shell: bool,
    scrollback_limit_bytes: usize,
    host_terminal_theme: crate::host_term::theme::TerminalTheme,
    host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
    default_cwd: &'a std::path::Path,
    next_pane_number: &'a mut usize,
    pane_cwds: &'a mut std::collections::HashMap<PaneId, PathBuf>,
    pane_terminals: &'a mut std::collections::HashMap<PaneId, crate::terminal::TerminalState>,
    pane_runtimes: &'a mut std::collections::HashMap<
        PaneId,
        (
            crate::terminal::TerminalId,
            crate::terminal::TerminalRuntime,
        ),
    >,
    spawn: &'a crate::workspace::PaneSpawnHandles,
}

fn stage_layout_node(
    tab: &mut crate::workspace::Tab,
    target_pane_id: PaneId,
    node: &LayoutNode,
    staging: &mut LayoutStaging<'_>,
) -> Result<(), String> {
    match node {
        LayoutNode::Pane { pane } => {
            if let Some(label) = pane
                .label
                .as_ref()
                .map(|label| label.trim())
                .filter(|label| !label.is_empty())
                && let Some(terminal) = staging.pane_terminals.get_mut(&target_pane_id)
            {
                terminal.set_manual_label(label.to_string());
            }
            Ok(())
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let second_leaf = first_layout_leaf(second);
            let cwd = second_leaf
                .cwd
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| {
                    staging
                        .pane_runtimes
                        .get(&target_pane_id)
                        .and_then(|(_, runtime)| runtime.follow_cwd())
                })
                .or_else(|| staging.pane_cwds.get(&target_pane_id).cloned())
                .unwrap_or_else(|| staging.default_cwd.to_path_buf());
            let extra_env = super::env::normalize_launch_env(second_leaf.env.clone())
                .map_err(|(_, message)| message)?;
            let command = layout_command(second_leaf)?;
            let launch_env = crate::pane::PaneLaunchEnv::from_extra(extra_env).with_identity(
                staging.workspace_id.to_string(),
                crate::workspace::public_tab_id_for_number(
                    staging.workspace_id,
                    staging.tab_number,
                ),
                crate::workspace::public_pane_id_for_number(
                    staging.workspace_id,
                    *staging.next_pane_number,
                ),
            );
            let direction = match direction {
                SplitDirection::Right => Direction::Horizontal,
                SplitDirection::Down => Direction::Vertical,
            };
            let new_pane = if let Some(argv) = command.as_deref() {
                tab.split_pane_argv(
                    target_pane_id,
                    false,
                    direction,
                    Some(*ratio),
                    &staging.geometry,
                    Some(cwd),
                    staging.default_cwd.to_path_buf(),
                    argv,
                    &launch_env,
                    staging.scrollback_limit_bytes,
                    staging.host_terminal_theme,
                    staging.host_terminal_appearance,
                    staging.spawn,
                )
            } else {
                tab.split_pane_shell(
                    target_pane_id,
                    false,
                    direction,
                    Some(*ratio),
                    &staging.geometry,
                    Some(cwd),
                    staging.default_cwd.to_path_buf(),
                    staging.scrollback_limit_bytes,
                    staging.host_terminal_theme,
                    staging.host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(staging.default_shell, staging.login_shell),
                    &launch_env,
                    staging.spawn,
                )
            }
            .map_err(|err| err.to_string())?;
            let crate::workspace::NewPane {
                pane_id,
                terminal,
                runtime,
                prepared_layout,
            } = new_pane;
            let terminal_id = terminal.id.clone();
            let terminal_cwd = terminal.cwd.clone();
            if !tab.commit_prepared_split(
                pane_id,
                prepared_layout,
                terminal_id.clone(),
                *staging.next_pane_number,
            ) {
                drop(runtime);
                return Err("prepared layout split no longer matches its tab".into());
            }
            *staging.next_pane_number = (*staging.next_pane_number).saturating_add(1);
            staging.pane_cwds.insert(pane_id, terminal_cwd);
            staging.pane_terminals.insert(pane_id, terminal);
            staging
                .pane_runtimes
                .insert(pane_id, (terminal_id, runtime));
            stage_layout_node(tab, target_pane_id, first, staging)?;
            stage_layout_node(tab, pane_id, second, staging)
        }
    }
}

fn first_layout_leaf(node: &LayoutNode) -> &LayoutPane {
    match node {
        LayoutNode::Pane { pane } => pane,
        LayoutNode::Split { first, .. } => first_layout_leaf(first),
    }
}

fn validate_layout_launches(node: &LayoutNode) -> Result<(), String> {
    match node {
        LayoutNode::Pane { pane } => {
            super::env::normalize_launch_env(pane.env.clone()).map_err(|(_, message)| message)?;
            let _ = layout_command(pane)?;
            Ok(())
        }
        LayoutNode::Split { first, second, .. } => {
            validate_layout_launches(first)?;
            validate_layout_launches(second)
        }
    }
}

fn layout_command(pane: &LayoutPane) -> Result<Option<Vec<String>>, String> {
    match pane.command.as_ref() {
        Some(command) if command.is_empty() => Err("pane command must not be empty".into()),
        Some(command) => Ok(Some(command.clone())),
        None => Ok(None),
    }
}

fn validate_layout_tree(root: &LayoutNode) -> Result<(), String> {
    let mut stats = LayoutTreeStats {
        panes: 0,
        max_depth: 0,
    };
    validate_layout_node(root, 1, &mut stats)?;
    if stats.panes > MAX_LAYOUT_PANES {
        return Err(format!(
            "layout has {} panes; maximum is {}",
            stats.panes, MAX_LAYOUT_PANES
        ));
    }
    if stats.max_depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "layout depth is {}; maximum is {}",
            stats.max_depth, MAX_LAYOUT_DEPTH
        ));
    }
    Ok(())
}

struct LayoutTreeStats {
    panes: usize,
    max_depth: usize,
}

fn validate_layout_node(
    node: &LayoutNode,
    depth: usize,
    stats: &mut LayoutTreeStats,
) -> Result<(), String> {
    stats.max_depth = stats.max_depth.max(depth);
    if depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "layout depth is {depth}; maximum is {MAX_LAYOUT_DEPTH}"
        ));
    }
    match node {
        LayoutNode::Pane { pane } => {
            stats.panes += 1;
            if stats.panes > MAX_LAYOUT_PANES {
                return Err(format!("layout has more than {MAX_LAYOUT_PANES} panes"));
            }
            layout_command(pane)?;
            super::env::normalize_launch_env(pane.env.clone()).map_err(|(_, message)| message)?;
            Ok(())
        }
        LayoutNode::Split {
            first,
            second,
            ratio,
            ..
        } => {
            if !ratio.is_finite() {
                return Err("split ratio must be finite".into());
            }
            validate_layout_node(first, depth + 1, stats)?;
            validate_layout_node(second, depth + 1, stats)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::{ErrorResponse, ResponseResult, SuccessResponse},
        config::Config,
        workspace::Workspace,
    };

    fn app_with_workspace() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    #[test]
    fn layout_export_returns_portable_tree() {
        let mut app = app_with_workspace();
        let root = app.state.workspaces[0].tabs[0].root_pane;
        let right = app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.workspaces[0].tabs[0].layout.focus_pane(root);
        app.state.workspaces[0].tabs[0]
            .layout
            .set_ratio_at(&[], 0.65);
        let right_terminal_id = app.state.workspaces[0].tabs[0]
            .terminal_id(right)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&right_terminal_id)
            .expect("test precondition")
            .set_manual_label("tests".into());

        let response = app.handle_layout_export(
            "req".into(),
            &LayoutExportParams {
                tab_id: None,
                pane_id: None,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        let ResponseResult::LayoutExport { layout } = success.result else {
            panic!("expected layout export response");
        };
        assert_eq!(layout.workspace_id, app.public_workspace_id(0));
        assert_eq!(
            layout.focused_pane_id,
            app.public_pane_id(0, root).expect("test precondition")
        );
        let LayoutNode::Split {
            direction,
            ratio,
            second,
            ..
        } = layout.root
        else {
            panic!("expected split layout root");
        };
        assert_eq!(direction, SplitDirection::Right);
        assert!((ratio - 0.65).abs() < f32::EPSILON);
        let LayoutNode::Pane { pane } = *second else {
            panic!("expected second pane");
        };
        assert_eq!(pane.label.as_deref(), Some("tests"));
        assert_eq!(
            pane.pane_id,
            Some(app.public_pane_id(0, right).expect("test precondition"))
        );
    }

    #[test]
    fn layout_set_split_ratio_updates_existing_split() {
        let mut app = app_with_workspace();
        app.state.workspaces[0].test_split(Direction::Horizontal);

        let response = app.handle_layout_set_split_ratio(
            "req".into(),
            LayoutSetSplitRatioParams {
                tab_id: None,
                pane_id: None,
                path: vec![],
                ratio: 0.72,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        let ResponseResult::LayoutSplitRatioSet { layout } = success.result else {
            panic!("expected layout split ratio set response");
        };
        let LayoutNode::Split { ratio, .. } = layout.root else {
            panic!("expected split layout root");
        };
        assert!((ratio - 0.72).abs() < f32::EPSILON);
        assert!(matches!(
            &app.event_hub.events_after(0).last().expect("layout event").1.data,
            EventData::LayoutUpdated { layout }
                if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition")
                    && (layout.splits[0].ratio - 0.72).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split() {
        let mut app = app_with_workspace();

        let response = app.handle_layout_set_split_ratio(
            "req".into(),
            LayoutSetSplitRatioParams {
                tab_id: None,
                pane_id: None,
                path: vec![],
                ratio: 0.72,
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).expect("test precondition");
        assert_eq!(error.error.code, "split_not_found");
    }

    #[tokio::test]
    async fn layout_apply_replaces_tab_with_requested_tree() {
        let mut app = app_with_workspace();
        let original_tab_id = app.public_tab_id(0, 0).expect("test precondition");
        let original_root = app.state.workspaces[0].tabs[0].root_pane;
        let original_pane_id = app
            .public_pane_id(0, original_root)
            .expect("test precondition");

        let response = app.handle_layout_apply(
            "req".into(),
            &LayoutApplyParams {
                workspace_id: None,
                tab_id: Some(original_tab_id.clone()),
                tab_label: Some("dev".into()),
                focus: true,
                root: LayoutNode::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.7,
                    first: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("editor".into()),
                            ..Default::default()
                        },
                    }),
                    second: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("tests".into()),
                            command: Some(vec![exiting_test_command().into()]),
                            env: std::collections::HashMap::from([(
                                "SHEPR_ROLE".into(),
                                "tests".into(),
                            )]),
                            ..Default::default()
                        },
                    }),
                },
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        let ResponseResult::LayoutApply { layout } = success.result else {
            panic!("expected layout apply response");
        };
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(
            app.state.workspaces[0].tab_display_name(0).as_deref(),
            Some("dev")
        );
        let LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } = layout.root
        else {
            panic!("expected split layout root");
        };
        assert_eq!(direction, SplitDirection::Right);
        assert!((ratio - 0.7).abs() < f32::EPSILON);
        let LayoutNode::Pane { pane: first_pane } = *first else {
            panic!("expected first pane");
        };
        let LayoutNode::Pane { pane: second_pane } = *second else {
            panic!("expected second pane");
        };
        assert_eq!(first_pane.label.as_deref(), Some("editor"));
        assert_eq!(second_pane.label.as_deref(), Some("tests"));
        assert_eq!(
            second_pane.command,
            Some(vec![exiting_test_command().into()])
        );
        assert!(matches!(
            &app.event_hub.events_after(0).last().expect("layout event").1.data,
            EventData::LayoutUpdated { layout }
                if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition")
                    && layout.panes.len() == 2
        ));
        let events = app.event_hub.events_after(0);
        let pane_closed = events
            .iter()
            .position(|(_, event)| {
                matches!(&event.data, EventData::PaneClosed { pane_id, .. } if pane_id == &original_pane_id)
            })
            .expect("the replaced tab's pane is announced closed");
        let tab_closed = events
            .iter()
            .position(|(_, event)| {
                matches!(&event.data, EventData::TabClosed { tab_id, .. } if tab_id == &original_tab_id)
            })
            .expect("the replaced tab is announced closed");
        assert!(pane_closed < tab_closed);
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn layout_apply_new_tab_follows_cached_focused_pane_cwd_without_runtime() {
        let mut app = app_with_workspace();
        let focused_pane = app.state.workspaces[0].tabs[0].root_pane;
        let scratch = crate::test_support::ScratchDir::new("cached-cwd");
        let cached_cwd = scratch.to_path_buf();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .cwd = cached_cwd.clone();

        let response = app.handle_layout_apply(
            "req".into(),
            &LayoutApplyParams {
                workspace_id: None,
                tab_id: None,
                tab_label: Some("cached".into()),
                focus: false,
                root: LayoutNode::Pane {
                    pane: LayoutPane::default(),
                },
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        assert!(matches!(success.result, ResponseResult::LayoutApply { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created
            .terminal_id(created.root_pane)
            .expect("test precondition");
        let created_cwd = &app
            .state
            .terminals
            .get(created_terminal_id)
            .expect("test precondition")
            .cwd;
        assert_eq!(
            std::fs::canonicalize(created_cwd).unwrap_or_else(|_| created_cwd.clone()),
            std::fs::canonicalize(&cached_cwd).unwrap_or_else(|_| cached_cwd.clone())
        );
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn layout_apply_rejects_invalid_deep_leaf_without_creating_tab() {
        let mut app = app_with_workspace();
        let original_tab_count = app.state.workspaces[0].tabs.len();

        let response = app.handle_layout_apply(
            "req".into(),
            &LayoutApplyParams {
                workspace_id: Some(app.public_workspace_id(0)),
                tab_id: None,
                tab_label: Some("bad".into()),
                focus: false,
                root: LayoutNode::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("editor".into()),
                            ..Default::default()
                        },
                    }),
                    second: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            command: Some(Vec::new()),
                            ..Default::default()
                        },
                    }),
                },
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).expect("test precondition");
        assert_eq!(error.error.code, "invalid_layout");
        assert_eq!(app.state.workspaces[0].tabs.len(), original_tab_count);
    }

    #[test]
    fn layout_validation_rejects_too_many_panes() {
        let mut root = LayoutNode::Pane {
            pane: LayoutPane::default(),
        };
        for _ in 0..MAX_LAYOUT_PANES {
            root = LayoutNode::Split {
                direction: SplitDirection::Right,
                ratio: 0.5,
                first: Box::new(root),
                second: Box::new(LayoutNode::Pane {
                    pane: LayoutPane::default(),
                }),
            };
        }

        let err = validate_layout_tree(&root).expect_err("test precondition");
        assert!(err.contains("maximum"));
    }
}
