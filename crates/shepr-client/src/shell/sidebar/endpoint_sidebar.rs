use crate::shell::presentation::text::{display_width, put_right_text, put_text};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::limits::MACHINE_ENTRY_INDENT;
use crate::shell::endpoints::ClientShellEndpoint;
use crate::shell::notices::machine_diagnostics::MachineDiagnostics;
use crate::shell::presentation::status::status_glyph;
use crate::shell::sidebar::host_colors::PillLook;
use crate::shell::sidebar::layout::{
    CollapsedSidebarView, CollapsedSlot, ExpandedSidebarView, ExpandedSlot, SidebarInputs,
    endpoint_signal,
};

use crate::shell::palette::Palette;
use ratatui::layout::Rect;

pub(in crate::shell) fn draw_collapsed(
    buffer: &mut Buffer,
    view: &CollapsedSidebarView,
    inputs: &SidebarInputs<'_>,
) {
    let palette = inputs.palette;
    let single_endpoint = inputs.single_endpoint();
    crate::shell::presentation::text::render_sidebar_background(buffer, view.area, palette);
    let workspace_area = view.workspaces.body;
    for slot in &view.workspaces.slots {
        match slot {
            CollapsedSlot::Machine {
                hit,
                endpoint,
                machine_number,
            } => {
                let endpoint = &inputs.endpoints[*endpoint];
                let rect = hit.rect;
                // The strip has no entry rows: navigate mode's selection of a
                // machine's entry highlights the machine's row.
                if inputs.selected.is_some_and(|selected| {
                    selected.is_machine_entry()
                        && selected.location.endpoint == endpoint.endpoint_id
                }) {
                    buffer.set_style(
                        rect,
                        Style::default().bg(crate::shell::sidebar::workspace_selection_background(
                            palette,
                        )),
                    );
                }
                let label = if endpoint.endpoint_id.is_local() {
                    "L".to_owned()
                } else {
                    machine_number.to_string()
                };
                put_text(
                    buffer,
                    rect.x,
                    rect.y,
                    rect.width.saturating_sub(1),
                    &label,
                    Style::default().fg(if endpoint.state.usable() {
                        palette.text
                    } else {
                        palette.overlay0
                    }),
                );
                if !endpoint.endpoint_id.is_local() {
                    // The strip has no room for a machine's entry: the glyph shows
                    // its state, and the row acts as the entry does.
                    let (glyph, color) = endpoint.row_glyph(palette);
                    put_right_text(
                        buffer,
                        rect,
                        rect.y,
                        glyph,
                        inputs.machine_diagnostics.badge_style(
                            endpoint,
                            palette,
                            Style::default().fg(color),
                        ),
                    );
                }
            }
            CollapsedSlot::Workspace {
                hit,
                endpoint,
                entry,
            } => {
                let endpoint = &inputs.endpoints[*endpoint];
                let Some(snapshot) = endpoint.listed_snapshot() else {
                    continue;
                };
                let Some(workspace) = snapshot.workspaces.get(*entry) else {
                    continue;
                };
                let rect = hit.rect;
                let active = &endpoint.endpoint_id == inputs.presented;
                let focused = active
                    && snapshot.focused_workspace_id.as_ref() == Some(&workspace.workspace_id);
                let selected = inputs.selected.is_some_and(|target| {
                    target.matches_workspace(&endpoint.endpoint_id, &workspace.workspace_id)
                });
                let selection_background =
                    crate::shell::sidebar::workspace_selection_background(palette);
                if selected {
                    buffer.set_style(rect, Style::default().bg(selection_background));
                } else if focused {
                    buffer.set_style(
                        rect,
                        Style::default().bg(crate::shell::sidebar::workspace_active_background(
                            palette,
                            inputs.selected.is_some(),
                        )),
                    );
                }
                let stale = endpoint.state.stale();
                let glyph = status_glyph(workspace.agent_status, palette, stale);
                let number = if single_endpoint {
                    format!("{:<2}", entry + 1)
                } else {
                    format!(" {}", entry + 1)
                };
                let number_width = display_width(&number).min(rect.width);
                let dim = if stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                };
                put_text(
                    buffer,
                    rect.x,
                    rect.y,
                    number_width,
                    &number,
                    Style::default()
                        .fg(if focused && !stale {
                            palette.text
                        } else {
                            palette.overlay0
                        })
                        .add_modifier(dim),
                );
                put_text(
                    buffer,
                    rect.x.saturating_add(number_width),
                    rect.y,
                    rect.width.saturating_sub(number_width),
                    glyph.text,
                    glyph.style,
                );
            }
        }
    }
    if let Some(divider_y) = view.divider_y {
        put_text(
            buffer,
            workspace_area.x,
            divider_y,
            workspace_area.width,
            &"─".repeat(workspace_area.width as usize),
            Style::default().fg(palette.surface_dim),
        );
    }
    crate::shell::sidebar::endpoint_agents::draw_collapsed(buffer, &view.agents, inputs);
    put_text(
        buffer,
        view.toggle.x,
        view.toggle.y,
        view.toggle.width,
        "»",
        Style::default().fg(palette.overlay0),
    );
}

pub(in crate::shell) fn draw_expanded(
    buffer: &mut Buffer,
    view: &ExpandedSidebarView,
    inputs: &SidebarInputs<'_>,
) {
    let config = inputs.config;
    let palette = inputs.palette;
    let single_endpoint = inputs.single_endpoint();
    let workspace_area = view.workspace_area;
    crate::shell::presentation::text::render_sidebar_background(buffer, view.area, palette);
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        if single_endpoint {
            " workspaces"
        } else {
            " machines"
        },
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let body = view.workspaces.body;
    for slot in &view.workspaces.slots {
        match slot {
            ExpandedSlot::Machine { hit, endpoint } => {
                draw_machine_slot(buffer, hit.rect, *endpoint, inputs);
            }
            ExpandedSlot::MachineEntry { hit, endpoint } => {
                draw_machine_entry(buffer, hit.rect, *endpoint, inputs);
            }
            ExpandedSlot::Workspace {
                hit,
                nested,
                endpoint,
                entry,
            } => {
                let endpoint = &inputs.endpoints[*endpoint];
                let Some(snapshot) = endpoint.listed_snapshot() else {
                    continue;
                };
                let Some(workspace) = snapshot.workspaces.get(*entry) else {
                    continue;
                };
                let status = workspace.agent_status;
                let tokens =
                    crate::shell::sidebar::workspace_rows(workspace, status, &config.spaces);
                let endpoint_active = &endpoint.endpoint_id == inputs.presented;
                let selected = inputs.selected.is_some_and(|target| {
                    target.matches_workspace(&endpoint.endpoint_id, &workspace.workspace_id)
                });
                // Drag-reordering moves workspaces of the active machine only.
                let dragged = endpoint_active
                    && inputs
                        .dragged_workspace
                        .is_some_and(|id| id == &workspace.workspace_id);
                let stale = endpoint.state.stale();
                crate::shell::sidebar::render_workspace_rows(
                    buffer,
                    *nested,
                    *entry + 1,
                    status,
                    &tokens,
                    crate::shell::sidebar::WorkspaceEntryState {
                        focused: endpoint_active
                            && snapshot.focused_workspace_id.as_ref()
                                == Some(&workspace.workspace_id),
                        selected,
                        navigating: inputs.selected.is_some(),
                        dragged,
                        // An unreachable machine's entries lose its colours along
                        // with the rest of their highlight.
                        look: PillLook::for_endpoint(
                            &config.host_hues,
                            inputs.host_pills,
                            &endpoint.endpoint_id,
                        )
                        .filter(|_| !stale),
                    },
                    palette,
                );
                if stale {
                    buffer.set_style(
                        hit.rect,
                        Style::default()
                            .fg(palette.overlay0)
                            .add_modifier(Modifier::DIM),
                    );
                }
            }
        }
    }
    if let Some(track) = view.workspaces.scrollbar {
        crate::shell::view::list::render_list_scrollbar(
            buffer,
            track,
            view.workspaces.scroll,
            palette,
        );
    }

    if let Some(row) = view.drop_indicator {
        put_text(
            buffer,
            body.x,
            row,
            body.width,
            &"─".repeat(body.width as usize),
            Style::default().fg(palette.accent),
        );
        // A drop target marks the row above the workspace it precedes. Above a machine's
        // first workspace, and below the active machine's last one when another machine
        // follows, that row is a machine row: the machine is drawn again over the marker,
        // so its name and status stay readable and the marker fills the rest of the row.
        for slot in &view.workspaces.slots {
            if let ExpandedSlot::Machine { hit, endpoint } = slot
                && hit.rect.y == row
            {
                draw_machine_slot(buffer, hit.rect, *endpoint, inputs);
            }
        }
    }

    if let Some(footer) = &view.footer {
        let footer_y = footer.new_workspace.y;
        put_text(
            buffer,
            workspace_area.x,
            footer_y,
            workspace_area.width,
            &footer.label,
            Style::default().fg(palette.overlay0),
        );
        put_right_text(
            buffer,
            workspace_area,
            footer_y,
            "menu",
            Style::default().fg(palette.overlay0),
        );
    }
    if let Some(panel) = &view.agents {
        crate::shell::sidebar::endpoint_agents::draw_agent_panel(buffer, panel, inputs);
    }
}

/// Draws the expanded sidebar's row for the machine at `endpoint` in `inputs.endpoints`.
fn draw_machine_slot(buffer: &mut Buffer, rect: Rect, endpoint: usize, inputs: &SidebarInputs<'_>) {
    let endpoint = &inputs.endpoints[endpoint];
    draw_endpoint_row(
        buffer,
        rect,
        endpoint,
        inputs.endpoint_label(&endpoint.endpoint_id),
        inputs.machine_diagnostics,
        inputs.palette,
    );
}

/// Draws the state entry of the machine at `endpoint` in `inputs.endpoints`, nested
/// under its machine row like a workspace. An entry that offers Connect or Restart
/// is drawn as something to activate, and highlighted while navigate mode selects it.
fn draw_machine_entry(
    buffer: &mut Buffer,
    rect: Rect,
    endpoint: usize,
    inputs: &SidebarInputs<'_>,
) {
    let endpoint = &inputs.endpoints[endpoint];
    let Some(entry) = endpoint.machine_entry() else {
        return;
    };
    let palette = inputs.palette;
    let selected = inputs.selected.is_some_and(|selected| {
        selected.is_machine_entry() && selected.location.endpoint == endpoint.endpoint_id
    });
    if selected {
        buffer.set_style(
            rect,
            Style::default().bg(crate::shell::sidebar::workspace_selection_background(
                palette,
            )),
        );
    }
    let style = if entry.state.action().is_some() {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.overlay0)
    };
    let indent = MACHINE_ENTRY_INDENT;
    put_text(
        buffer,
        rect.x.saturating_add(indent),
        rect.y,
        rect.width.saturating_sub(indent),
        entry.state.label(),
        style,
    );
    if let Some(hint) = entry.hint.as_deref()
        && rect.height > 1
    {
        put_text(
            buffer,
            rect.x.saturating_add(indent),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(indent),
            hint,
            Style::default().fg(palette.overlay0),
        );
    }
}

/// `label` is the name shown for `endpoint`.
fn draw_endpoint_row(
    buffer: &mut Buffer,
    rect: Rect,
    endpoint: &ClientShellEndpoint,
    label: &str,
    diagnostics: &MachineDiagnostics,
    palette: &Palette,
) {
    let (signal, color) = endpoint_signal(endpoint, palette);
    let signal_width = display_width(&signal).min(rect.width);
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width.saturating_sub(signal_width.saturating_add(1)),
        &format!(" {label}"),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    put_right_text(
        buffer,
        rect,
        rect.y,
        &signal,
        diagnostics.badge_style(endpoint, palette, Style::default().fg(color)),
    );
}
