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
/// - `indexed`: as `actions`, plus the help label the entry follows in its
///   group
/// - `navigate`: (config field, resolved field, variant, default, help group,
///   help label, doc, fixed arrow alias). Rows sharing a help label share one
///   help row, their keys joined and any aliases listed last.
/// - `navigate_indexed`: as `navigate`.
#[macro_export]
macro_rules! keybinding_table {
    ($consumer:ident) => {
        $consumer! {
            actions {
                (help, Help, "prefix+?", "global", "keybinds", "Open keybinding help."),
                (detach, Detach, "prefix+q", "global", "detach", "Detach this client from its server."),
                (workspace_picker, WorkspacePicker, "prefix+w", "workspaces", "workspace navigation", "Open the workspace navigation surface."),
                (goto, OpenNavigator, "prefix+g", "workspaces", "session navigator", "Open the session navigator."),
                (new_workspace, NewWorkspace, "prefix+shift+n", "workspaces", "new workspace", "Create a new workspace."),
                (rename_workspace, RenameWorkspace, "prefix+shift+w", "workspaces", "rename workspace", "Rename the selected workspace."),
                (close_workspace, CloseWorkspace, "prefix+shift+d", "workspaces", "close workspace", "Close the selected workspace."),
                (previous_workspace, PreviousWorkspace, "prefix+p", "workspaces", "previous workspace", "Select the previous workspace."),
                (next_workspace, NextWorkspace, "prefix+n", "workspaces", "next workspace", "Select the next workspace."),
                (previous_agent, PreviousAgent, "", "workspaces", "previous agent", "Focus the previous agent in the sidebar."),
                (next_agent, NextAgent, "", "workspaces", "next agent", "Focus the next agent in the sidebar."),
                (split_vertical, SplitVertical, "prefix+v", "panes", "split vertical", "Split the focused pane side by side."),
                (split_horizontal, SplitHorizontal, "prefix+minus", "panes", "split horizontal", "Split the focused pane into stacked panes."),
                (close_pane, ClosePane, "prefix+x", "panes", "close pane", "Close the focused pane."),
                (rename_pane, RenamePane, "prefix+shift+p", "panes", "rename pane", "Rename the focused pane."),
                (clear_pane, ClearPane, "", "panes", "clear pane", "Clear the focused pane."),
                (copy_mode, CopyMode, "prefix+[", "panes", "copy mode", "Enter keyboard copy mode for the focused pane."),
                (zoom, Zoom, "prefix+z", "panes", "zoom pane", "Toggle zoom for the focused pane."),
                (resize_mode, EnterResizeMode, "prefix+r", "panes", "resize mode", "Enter pane resize mode."),
                (resize_pane_left, ResizePaneLeft, "", "panes", "resize pane left", "Resize the focused pane toward the left."),
                (resize_pane_down, ResizePaneDown, "", "panes", "resize pane down", "Resize the focused pane downward."),
                (resize_pane_up, ResizePaneUp, "", "panes", "resize pane up", "Resize the focused pane upward."),
                (resize_pane_right, ResizePaneRight, "", "panes", "resize pane right", "Resize the focused pane toward the right."),
                (toggle_sidebar, ToggleSidebar, "prefix+b", "panes", "toggle sidebar", "Toggle sidebar collapse."),
                (focus_pane_left, FocusPaneLeft, "prefix+h", "panes", "focus pane left", "Focus the pane to the left."),
                (focus_pane_down, FocusPaneDown, "prefix+j", "panes", "focus pane down", "Focus the pane below."),
                (focus_pane_up, FocusPaneUp, "prefix+k", "panes", "focus pane up", "Focus the pane above."),
                (focus_pane_right, FocusPaneRight, "prefix+l", "panes", "focus pane right", "Focus the pane to the right."),
                (swap_pane_left, SwapPaneLeft, "prefix+shift+h", "panes", "swap pane left", "Swap the focused pane with the pane to the left."),
                (swap_pane_down, SwapPaneDown, "prefix+shift+j", "panes", "swap pane down", "Swap the focused pane with the pane below."),
                (swap_pane_up, SwapPaneUp, "prefix+shift+k", "panes", "swap pane up", "Swap the focused pane with the pane above."),
                (swap_pane_right, SwapPaneRight, "prefix+shift+l", "panes", "swap pane right", "Swap the focused pane with the pane to the right."),
                (cycle_pane_next, CyclePaneNext, "prefix+tab", "panes", "cycle pane next", "Cycle to the next pane."),
                (cycle_pane_previous, CyclePanePrevious, "prefix+shift+tab", "panes", "cycle pane previous", "Cycle to the previous pane."),
                (last_pane, LastPane, "", "panes", "last pane", "Focus the last focused pane across workspaces."),
            }
            indexed {
                (switch_workspace, SwitchWorkspace, "prefix+1..9", "workspaces", "switch workspace 1-9", "Switch to a workspace by index from prefix mode.", "next workspace"),
                (focus_agent, FocusAgent, "", "workspaces", "focus agent 1-9", "Focus a sidebar agent by index.", "next agent"),
            }
            navigate {
                (navigate_back, back, Back, "esc", "navigation", "back", "Leave navigate mode.", None),
                (navigate_workspace_up, workspace_up, WorkspaceUp, "up", "navigation", "workspace list", "Move the workspace selection up.", None),
                (navigate_workspace_down, workspace_down, WorkspaceDown, "down", "navigation", "workspace list", "Move the workspace selection down.", None),
                (navigate_pane_left, pane_left, PaneLeft, "h", "navigation", "move focus", "Focus the pane to the left in navigate mode. The left arrow always does too.", Left),
                (navigate_pane_down, pane_down, PaneDown, "j", "navigation", "move focus", "Focus the pane below in navigate mode.", None),
                (navigate_pane_up, pane_up, PaneUp, "k", "navigation", "move focus", "Focus the pane above in navigate mode.", None),
                (navigate_pane_right, pane_right, PaneRight, "l", "navigation", "move focus", "Focus the pane to the right in navigate mode. The right arrow always does too.", Right),
                (navigate_cycle_pane_next, cycle_pane_next, CyclePaneNext, "tab", "navigation", "cycle pane", "Focus the next pane while navigate mode is open.", None),
                (navigate_cycle_pane_previous, cycle_pane_previous, CyclePanePrevious, "shift+tab", "navigation", "cycle pane", "Focus the previous pane while navigate mode is open.", None),
                (navigate_open_workspace, open_workspace, OpenWorkspace, "enter", "navigation", "open workspace", "Open the selected workspace.", None),
            }
            navigate_indexed {
                (navigate_switch_workspace, switch_workspace, SwitchWorkspace, "1..9", "navigation", "switch workspace", "Switch to a workspace by index while navigate mode is open.", None),
            }
        }
    };
}
