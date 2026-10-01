use super::*;

#[path = "../overlays/overlays.rs"]
mod overlays;
#[path = "../sidebar/sidebar.rs"]
pub(in crate::shell) mod sidebar;

pub(super) use super::aggregate_navigation::navigator_rows as client_navigator_rows;
pub(super) use overlays::{render_client_overlay, render_context_menu, render_global_menu};
pub(super) use sidebar::workspace_entries;

pub(in crate::shell) fn render_sidebar_background(
    buffer: &mut Buffer,
    area: Rect,
    palette: &Palette,
) {
    buffer.set_style(area, Style::default().bg(palette.sidebar_bg));
    let separator_x = area.right().saturating_sub(1);
    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((separator_x, y)) {
            cell.set_symbol("│");
            cell.set_style(Style::default().fg(palette.surface_dim));
        }
    }
}

fn configured_key_labels(bindings: &[&shepr_config::ActionKeybinds]) -> String {
    let labels = bindings
        .iter()
        .filter_map(|bindings| bindings.label())
        .collect::<Vec<_>>();
    if labels.is_empty() {
        "unset".to_owned()
    } else {
        labels.join(" / ")
    }
}

pub(super) fn render_mode_bar(
    buffer: &mut Buffer,
    pane_area: Rect,
    mode: ClientShellMode,
    copy_mode: Option<&ClientCopyModeState>,
    endpoint_error: Option<&str>,
    keybinds: &LiveKeybindConfig,
    palette: &Palette,
) -> Option<Rect> {
    // The returned bar is the rect composition overwrites into the frame, so it must lie
    // inside the buffer: clip the area first.
    let pane_area = pane_area.intersection(buffer.area);
    if (mode == ClientShellMode::Terminal && endpoint_error.is_none()) || pane_area.is_empty() {
        return None;
    }

    let bar = Rect::new(
        pane_area.x,
        pane_area.y + pane_area.height.saturating_sub(1),
        pane_area.width,
        1,
    );
    let base = Style::default().fg(palette.overlay0).bg(palette.panel_bg);
    for x in bar.x..bar.right() {
        if let Some(cell) = buffer.cell_mut((x, bar.y)) {
            cell.set_symbol(" ").set_style(base);
        }
    }

    let key = Style::default()
        .fg(palette.accent)
        .bg(palette.panel_bg)
        .add_modifier(Modifier::BOLD);
    let mode_style = Style::default()
        .fg(match palette.panel_bg {
            ratatui::style::Color::Reset => palette.surface_dim,
            color => color,
        })
        .bg(if mode == ClientShellMode::Resize {
            palette.mauve
        } else {
            palette.accent
        })
        .add_modifier(Modifier::BOLD);
    let prefix = shepr_config::format_key_combo(keybinds.prefix);
    let prefix_rhs = |bindings: &shepr_config::ActionKeybinds| {
        bindings
            .prefix_rhs_label()
            .unwrap_or_else(|| "unset".to_owned())
    };

    let mut segments = Vec::<(String, Style)>::new();
    if let Some(error) = endpoint_error {
        segments.extend([
            (" ERROR ".to_owned(), mode_style),
            (format!(" {error}"), base),
        ]);
    } else {
        match mode {
            ClientShellMode::Prefix => {
                // Escape exits prefix mode directly; the listed actions use configured labels.
                segments.extend([
                    (" PREFIX ".to_owned(), mode_style),
                    (" ".to_owned(), base),
                    ("esc".to_owned(), key),
                    (" cancel  ".to_owned(), base),
                    (prefix, key),
                    (" send prefix  ".to_owned(), base),
                    (prefix_rhs(&keybinds.keybinds.workspace_picker), key),
                    (" workspace nav  ".to_owned(), base),
                    (prefix_rhs(&keybinds.keybinds.help), key),
                    (" keybinds".to_owned(), base),
                ]);
            }
            ClientShellMode::Navigate => {
                let navigate = &keybinds.keybinds.navigate;
                segments.extend([
                    (" NAVIGATE ".to_owned(), mode_style),
                    (
                        format!("{} ", configured_key_labels(&[&navigate.back])),
                        key,
                    ),
                    ("back  ".to_owned(), base),
                    (
                        configured_key_labels(&[&navigate.workspace_up, &navigate.workspace_down]),
                        key,
                    ),
                    (" workspace  ".to_owned(), base),
                    (
                        configured_key_labels(&[
                            &navigate.cycle_pane_next,
                            &navigate.cycle_pane_previous,
                        ]),
                        key,
                    ),
                    (" pane  ".to_owned(), base),
                    (prefix_rhs(&keybinds.keybinds.help), key),
                    (" keybinds".to_owned(), base),
                ]);
            }
            ClientShellMode::Resize => {
                // Resize controls are fixed in input routing, not [keys] bindings.
                segments.extend([
                    (" RESIZE ".to_owned(), mode_style),
                    ("  ".to_owned(), base),
                    ("h/l".to_owned(), key),
                    (" width  ".to_owned(), base),
                    ("j/k".to_owned(), key),
                    (" height  ".to_owned(), base),
                    ("esc".to_owned(), key),
                    (" done".to_owned(), base),
                ]);
            }
            ClientShellMode::Copy => {
                // Copy-mode commands, including search prompt controls, have fixed input
                // bindings and no entries in the configurable keybinding table.
                let copy_mode = copy_mode?;
                if let Some(prompt) = copy_mode.search_prompt.as_ref() {
                    let marker = match prompt.direction {
                        shepr_protocol::command::PaneCopySearchDirection::Forward => "/",
                        shepr_protocol::command::PaneCopySearchDirection::Backward => "?",
                    };
                    buffer.set_stringn(bar.x, bar.y, " COPY ", usize::from(bar.width), mode_style);
                    let prefix = 8.min(bar.width);
                    if bar.width >= 8 {
                        buffer.set_string(bar.x + 7, bar.y, marker, key);
                    }
                    let footer = "  enter search  esc cancel";
                    let footer_width = if bar.width >= 50 {
                        u16::try_from(footer.len()).unwrap_or(u16::MAX)
                    } else {
                        0
                    };
                    let field = Rect::new(
                        bar.x + prefix,
                        bar.y,
                        bar.width.saturating_sub(prefix + footer_width),
                        1,
                    );
                    if let Some(cursor) = text_editor::render(
                        buffer,
                        field,
                        &prompt.query,
                        Style::default().fg(palette.text).bg(palette.panel_bg),
                    ) && let Some(cell) = buffer.cell_mut((cursor.x, cursor.y))
                    {
                        cell.set_style(Style::default().fg(palette.panel_bg).bg(palette.text));
                    }
                    if footer_width > 0 {
                        buffer.set_string(bar.right() - footer_width, bar.y, footer, base);
                    }
                    return Some(bar);
                }
                let select = if copy_mode.selection.is_some() {
                    "selecting"
                } else {
                    "select"
                };
                let match_status = copy_mode
                    .search_current_global
                    .map(|current| format!(" {}/{}", current + 1, copy_mode.search_total))
                    .or_else(|| (!copy_mode.search_query.is_empty()).then(|| " 0/0".to_owned()))
                    .unwrap_or_default();
                let (exit_keys, exit_label) =
                    if copy_mode.search_query.is_empty() && copy_mode.selection.is_none() {
                        ("q/esc", " exit")
                    } else {
                        ("esc", " clear  q exit")
                    };
                segments.extend([
                    (" COPY ".to_owned(), mode_style),
                    (" ".to_owned(), base),
                    ("h/j/k/l w/b/e { }".to_owned(), key),
                    (" move  ".to_owned(), base),
                    ("/ ?".to_owned(), key),
                    (" search  ".to_owned(), base),
                    ("n/N".to_owned(), key),
                    (format!(" repeat{match_status}  "), base),
                    ("v/space".to_owned(), key),
                    (format!(" {select}  "), base),
                    ("y/enter".to_owned(), key),
                    (" copy  ".to_owned(), base),
                    (exit_keys.to_owned(), key),
                    (exit_label.to_owned(), base),
                ]);
            }
            // Terminal mode without an error returned at the top.
            ClientShellMode::Terminal => return None,
        }
    }

    let mut x = bar.x;
    let end = bar.x + bar.width;
    for (text, style) in segments {
        if x >= end {
            break;
        }
        let remaining = end - x;
        buffer.set_stringn(x, bar.y, &text, usize::from(remaining), style);
        x = x.saturating_add(
            u16::try_from(UnicodeWidthStr::width(text.as_str()))
                .unwrap_or(u16::MAX)
                .min(remaining),
        );
    }
    Some(bar)
}

pub(super) struct ShellRenderState<'a> {
    pub(super) machine_diagnostics: &'a super::machine_diagnostics::MachineDiagnostics,
    pub(super) endpoints: &'a [ClientShellEndpoint],
    pub(super) active_endpoint_id: &'a ClientEndpointId,
    pub(super) collapsed_endpoints: &'a HashSet<ClientEndpointId>,
    pub(super) workspace_scroll: &'a mut usize,
    pub(super) agent_scroll: &'a mut usize,
    pub(super) reveal_focused_workspace: &'a mut bool,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_section_split: super::sidebar_tokens::SectionSplit,
    pub(super) selected_workspace_id: Option<&'a WorkspaceNavigationTarget>,
    pub(super) reveal_navigation_workspace: &'a mut bool,
    pub(super) dragged_workspace_id: Option<&'a shepr_protocol::WorkspaceId>,
    pub(super) workspace_drop_indicator_row: Option<u16>,
}

pub(super) fn render_shell(
    buffer: &mut Buffer,
    layout: ClientShellLayout,
    snapshot: Option<&ClientShellSnapshot>,
    config: &ClientShellConfig,
    mut state: ShellRenderState<'_>,
) -> ShellHitMap {
    let mut hits = ShellHitMap::default();
    if layout.sidebar.width > 0 {
        if state.sidebar_collapsed {
            super::endpoint_sidebar::render_collapsed(
                buffer,
                layout.sidebar,
                config,
                &mut state,
                &mut hits,
            );
        } else {
            super::endpoint_sidebar::render_expanded(
                buffer,
                layout.sidebar,
                snapshot,
                config,
                &mut state,
                &mut hits,
            );
        }
    }
    if !config.mouse_capture {
        hits.sidebar_divider = Rect::default();
        hits.sidebar_section_divider = Rect::default();
        hits.workspace_scrollbar = Rect::default();
        hits.agent_scrollbar = Rect::default();
        hits.agent_sort_toggle = Rect::default();
        hits.new_workspace = Rect::default();
        hits.machines.clear();
        hits.workspaces.clear();
        hits.agents.clear();
        hits.endpoint_agents.clear();
        hits.pane_splits.clear();
    }
    hits
}

pub(super) fn put_right_text(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = display_width(text).min(area.width);
    put_text(
        buffer,
        area.right().saturating_sub(width),
        y,
        width,
        text,
        style,
    );
}

pub(super) fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    buffer.set_stringn(x, y, text, width as usize, style);
}

pub(super) fn display_width(text: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::ValidatedConfigFixture as _;

    #[test]
    fn navigate_mode_bar_uses_configured_action_keys() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut config = shepr_config::Config::default();
        config.keys.navigate_back = shepr_config::BindingConfig::one("q");
        config.keys.navigate_workspace_up = shepr_config::BindingConfig::one("u");
        config.keys.navigate_workspace_down = shepr_config::BindingConfig::one("d");
        config.keys.navigate_cycle_pane_next = shepr_config::BindingConfig::one("n");
        config.keys.navigate_cycle_pane_previous = shepr_config::BindingConfig::one("p");
        let validated = shepr_config::ValidatedConfig::test_from_config(config, None);
        let area = Rect::new(0, 0, 120, 2);
        let mut buffer = Buffer::empty(area);

        render_mode_bar(
            &mut buffer,
            area,
            ClientShellMode::Navigate,
            None,
            None,
            &validated.live_keybinds(),
            validated.palette(),
        );

        let row = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<Vec<_>>()
            .concat();
        assert!(row.contains("q back"), "{row}");
        assert!(row.contains("u / d workspace"), "{row}");
        assert!(row.contains("n / p pane"), "{row}");
        assert!(!row.contains("esc back"), "{row}");
        assert!(!row.contains("↑/↓"), "{row}");
        assert!(!row.contains("tab pane"), "{row}");
    }
}
