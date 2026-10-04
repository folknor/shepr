/// The help-screen group a `keybinding_table!` row is listed under. The table
/// names a variant, so a misspelled group does not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpGroup {
    Global,
    Navigation,
    Workspaces,
    Panes,
}

impl HelpGroup {
    /// The groups in the order the help screen lists them.
    pub const DISPLAY_ORDER: [Self; 4] = [
        Self::Global,
        Self::Navigation,
        Self::Workspaces,
        Self::Panes,
    ];

    /// The heading the help screen shows for the group.
    pub fn title(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Navigation => "navigation",
            Self::Workspaces => "workspaces",
            Self::Panes => "panes",
        }
    }
}

impl std::fmt::Display for HelpGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}

/// The key chord of a navigate row's fixed arrow alias, by the alias column's
/// identifier in `keybinding_table!` (`None` for a row without one).
#[macro_export]
macro_rules! navigate_alias {
    ($alias:ident) => {
        $crate::Keybinds::navigate_alias_chord_from_table(stringify!($alias))
    };
}

/// The help label of a navigate row's fixed arrow alias, as `navigate_alias!`.
#[macro_export]
macro_rules! navigate_alias_label {
    ($alias:ident) => {
        $crate::Keybinds::navigate_alias_label_from_table(stringify!($alias))
    };
}

/// The one list of configurable keybindings. Each consumer is a macro that
/// receives every row and generates its part: the `[keys]` config fields and
/// defaults, the resolved keybinds, the apply step, the action enums,
/// dispatch and the help screen.
///
/// Rows within a section are in help-screen order for their group.
/// - `actions`: (field, action variant, default, help group, help label, doc)
/// - `indexed`: as `actions`, plus the help label of the `actions` row the
///   entry follows in its group (a test in `shepr-termio` checks the label
///   names a row); the help group is a `HelpGroup` variant name
/// - `navigate`: (config field, resolved field, variant, default, help group,
///   help label, doc, fixed arrow alias). Rows sharing a help label share one
///   help row, their keys joined and any aliases listed last.
/// - `navigate_indexed`: as `navigate`.
#[macro_export]
macro_rules! keybinding_table {
    ($consumer:ident) => {
        $consumer! {
            actions {
                (help, Help, "prefix+?", Global, "keybinds", "Open keybinding help."),
                (detach, Detach, "prefix+q", Global, "detach", "Detach this client from its server."),
                (workspace_picker, WorkspacePicker, "prefix+w", Workspaces, "workspace navigation", "Open the workspace navigation surface."),
                (goto, OpenNavigator, "prefix+g", Workspaces, "session navigator", "Open the session navigator."),
                (new_workspace, NewWorkspace, "prefix+shift+n", Workspaces, "new workspace", "Create a new workspace."),
                (rename_workspace, RenameWorkspace, "prefix+shift+w", Workspaces, "rename workspace", "Rename the selected workspace."),
                (close_workspace, CloseWorkspace, "prefix+shift+d", Workspaces, "close workspace", "Close the selected workspace."),
                (previous_workspace, PreviousWorkspace, "prefix+p", Workspaces, "previous workspace", "Select the previous workspace."),
                (next_workspace, NextWorkspace, "prefix+n", Workspaces, "next workspace", "Select the next workspace."),
                (previous_agent, PreviousAgent, "", Workspaces, "previous agent", "Focus the previous agent in the sidebar."),
                (next_agent, NextAgent, "", Workspaces, "next agent", "Focus the next agent in the sidebar."),
                (split_vertical, SplitVertical, "prefix+v", Panes, "split vertical", "Split the focused pane side by side."),
                (split_horizontal, SplitHorizontal, "prefix+minus", Panes, "split horizontal", "Split the focused pane into stacked panes."),
                (close_pane, ClosePane, "prefix+x", Panes, "close pane", "Close the focused pane."),
                (rename_pane, RenamePane, "prefix+shift+p", Panes, "rename pane", "Rename the focused pane."),
                (clear_pane, ClearPane, "", Panes, "clear pane", "Clear the focused pane."),
                (copy_mode, CopyMode, "prefix+[", Panes, "copy mode", "Enter keyboard copy mode for the focused pane."),
                (zoom, Zoom, "prefix+z", Panes, "zoom pane", "Toggle zoom for the focused pane."),
                (resize_mode, EnterResizeMode, "prefix+r", Panes, "resize mode", "Enter pane resize mode."),
                (resize_pane_left, ResizePaneLeft, "", Panes, "resize pane left", "Resize the focused pane toward the left."),
                (resize_pane_down, ResizePaneDown, "", Panes, "resize pane down", "Resize the focused pane downward."),
                (resize_pane_up, ResizePaneUp, "", Panes, "resize pane up", "Resize the focused pane upward."),
                (resize_pane_right, ResizePaneRight, "", Panes, "resize pane right", "Resize the focused pane toward the right."),
                (toggle_sidebar, ToggleSidebar, "prefix+b", Panes, "toggle sidebar", "Toggle sidebar collapse."),
                (focus_pane_left, FocusPaneLeft, "prefix+h", Panes, "focus pane left", "Focus the pane to the left."),
                (focus_pane_down, FocusPaneDown, "prefix+j", Panes, "focus pane down", "Focus the pane below."),
                (focus_pane_up, FocusPaneUp, "prefix+k", Panes, "focus pane up", "Focus the pane above."),
                (focus_pane_right, FocusPaneRight, "prefix+l", Panes, "focus pane right", "Focus the pane to the right."),
                (swap_pane_left, SwapPaneLeft, "prefix+shift+h", Panes, "swap pane left", "Swap the focused pane with the pane to the left."),
                (swap_pane_down, SwapPaneDown, "prefix+shift+j", Panes, "swap pane down", "Swap the focused pane with the pane below."),
                (swap_pane_up, SwapPaneUp, "prefix+shift+k", Panes, "swap pane up", "Swap the focused pane with the pane above."),
                (swap_pane_right, SwapPaneRight, "prefix+shift+l", Panes, "swap pane right", "Swap the focused pane with the pane to the right."),
                (cycle_pane_next, CyclePaneNext, "prefix+tab", Panes, "cycle pane next", "Cycle to the next pane."),
                (cycle_pane_previous, CyclePanePrevious, "prefix+shift+tab", Panes, "cycle pane previous", "Cycle to the previous pane."),
                (last_pane, LastPane, "", Panes, "last pane", "Focus the last focused pane across workspaces."),
            }
            indexed {
                (switch_workspace, SwitchWorkspace, "prefix+1..9", Workspaces, "switch workspace 1-9", "Switch to a workspace by index from prefix mode.", "next workspace"),
                (focus_agent, FocusAgent, "", Workspaces, "focus agent 1-9", "Focus a sidebar agent by index.", "next agent"),
            }
            navigate {
                (navigate_back, back, Back, "esc", Navigation, "back", "Leave navigate mode.", None),
                (navigate_workspace_up, workspace_up, WorkspaceUp, "up", Navigation, "workspace list", "Move the workspace selection up.", None),
                (navigate_workspace_down, workspace_down, WorkspaceDown, "down", Navigation, "workspace list", "Move the workspace selection down.", None),
                (navigate_pane_left, pane_left, PaneLeft, "h", Navigation, "move focus", "Focus the pane to the left in navigate mode. The left arrow always does too.", Left),
                (navigate_pane_down, pane_down, PaneDown, "j", Navigation, "move focus", "Focus the pane below in navigate mode.", None),
                (navigate_pane_up, pane_up, PaneUp, "k", Navigation, "move focus", "Focus the pane above in navigate mode.", None),
                (navigate_pane_right, pane_right, PaneRight, "l", Navigation, "move focus", "Focus the pane to the right in navigate mode. The right arrow always does too.", Right),
                (navigate_cycle_pane_next, cycle_pane_next, CyclePaneNext, "tab", Navigation, "cycle pane", "Focus the next pane while navigate mode is open.", None),
                (navigate_cycle_pane_previous, cycle_pane_previous, CyclePanePrevious, "shift+tab", Navigation, "cycle pane", "Focus the previous pane while navigate mode is open.", None),
                (navigate_open_workspace, open_workspace, OpenWorkspace, "enter", Navigation, "open workspace", "Open the selected workspace.", None),
            }
            navigate_indexed {
                (navigate_switch_workspace, switch_workspace, SwitchWorkspace, "1..9", Navigation, "switch workspace", "Switch to a workspace by index while navigate mode is open.", None),
            }
        }
    };
}
