pub(in crate::shell) mod chrome;
use ratatui::style::{Modifier, Style};
pub(in crate::shell) mod agent_sidebar;
pub(in crate::shell) mod endpoint_agents;
pub(in crate::shell) mod endpoint_sidebar;
pub(in crate::shell) mod layout;
pub(in crate::shell) mod preferences;
pub(in crate::shell) mod scroll;
pub(in crate::shell) mod sidebar_tokens;
mod token_definitions;

use ratatui::buffer::Buffer;
use shepr_protocol::ClientShellWorkspace;

use crate::shell::presentation::text::{display_width, put_text};
use crate::shell::sidebar::sidebar_tokens::{
    ResolvedToken, SpaceTokenContext, TokenStyles, resolved_token_spans, sidebar_space_rows,
};
use ratatui::layout::Rect;
use shepr_config::SpacesSidebarConfig;
use shepr_config::theme::Palette;

use crate::shell::presentation::status::{status_glyph, status_text};

pub(in crate::shell) fn workspace_selection_background(palette: &Palette) -> ratatui::style::Color {
    if palette.selection_bg == ratatui::style::Color::Reset {
        palette.active_row_bg
    } else {
        palette.selection_bg
    }
}

pub(in crate::shell) fn workspace_active_background(
    palette: &Palette,
    navigating: bool,
) -> ratatui::style::Color {
    // The fallback cursor shares the active-row color; only fill the cursor while navigating.
    if navigating && palette.selection_bg == ratatui::style::Color::Reset {
        palette.sidebar_bg
    } else {
        palette.active_row_bg
    }
}

pub(in crate::shell) fn collapsed_sidebar_sections(area: Rect) -> (Rect, Option<u16>, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.is_empty() {
        return (Rect::default(), None, Rect::default());
    }
    if content.height < 7 {
        return (content, None, Rect::default());
    }
    let workspace_height = content.height.div_ceil(2);
    let divider_y = content.y + workspace_height;
    let detail_height = content.height.saturating_sub(workspace_height + 1);
    (
        Rect::new(content.x, content.y, content.width, workspace_height),
        Some(divider_y),
        Rect::new(content.x, divider_y + 1, content.width, detail_height),
    )
}

pub(in crate::shell::sidebar) fn workspace_rows(
    workspace: &ClientShellWorkspace,
    status: shepr_protocol::AgentStatus,
    config: &SpacesSidebarConfig,
) -> Vec<Vec<ResolvedToken>> {
    sidebar_space_rows(
        config,
        &SpaceTokenContext {
            workspace: &workspace.label,
            branch: workspace.branch.as_deref(),
            state_text: status_text(status),
            ahead_behind: workspace.git_ahead_behind,
        },
    )
}

pub(in crate::shell::sidebar) fn render_workspace_rows(
    buffer: &mut Buffer,
    area: Rect,
    workspace_number: usize,
    status: shepr_protocol::AgentStatus,
    indicators: shepr_config::StatusIndicatorStyle,
    rows: &[Vec<ResolvedToken>],
    focused: bool,
    selected: bool,
    navigating: bool,
    dragged: bool,
    palette: &Palette,
) {
    // Callers' rects come from the sidebar layout; clip to the buffer anyway so nothing below
    // writes past it.
    let area = area.intersection(buffer.area);
    let number = format!("{workspace_number:<2} ");
    let number_width = display_width(&number);
    for row_index in 0..rows.len().max(1) {
        let y = area.y + u16::try_from(row_index).unwrap_or(u16::MAX);
        if y >= area.bottom() {
            break;
        }
        let indent = if row_index == 0 { 1 } else { 3 };
        let x = area.x.saturating_add(indent).saturating_add(number_width);
        if row_index == 0 {
            let number_x = area.x.saturating_add(indent);
            put_text(
                buffer,
                number_x,
                y,
                area.right().saturating_sub(number_x),
                &number,
                Style::default().fg(palette.overlay0),
            );
        }
        let highlighted = focused || dragged;
        let workspace_style = Style::default()
            .fg(if highlighted {
                palette.text
            } else {
                palette.subtext0
            })
            .add_modifier(if highlighted {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let secondary_style = Style::default().fg(if focused {
            palette.mauve
        } else {
            palette.overlay0
        });
        let glyph = status_glyph(status, indicators, palette, false);
        let spans = resolved_token_spans(
            rows.get(row_index).map_or(&[], Vec::as_slice),
            glyph,
            TokenStyles {
                state_text: glyph.style,
                primary: workspace_style,
                secondary: secondary_style,
                terminal_title: Style::default().fg(palette.overlay1),
            },
            palette,
            area.right().saturating_sub(2).saturating_sub(x) as usize,
        );
        crate::shell::presentation::text::put_spans(
            buffer,
            Rect::new(x, y, area.right().saturating_sub(2).saturating_sub(x), 1),
            &spans,
            Style::default(),
        );
    }

    let background = if selected {
        Some(workspace_selection_background(palette))
    } else if dragged {
        Some(palette.surface1)
    } else if focused {
        Some(workspace_active_background(palette, navigating))
    } else {
        None
    };
    if let Some(background) = background {
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_bg(background);
                }
            }
        }
    }
}
