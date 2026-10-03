use crate::endpoint::ClientEndpointStatus;
use crate::shell::state::ClientNavigatorFilter;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
mod context_menu;
pub(in crate::shell) mod endpoint_notices;
pub(in crate::shell) mod global_menu;
pub(in crate::shell) mod machine_diagnostics;
pub(in crate::shell) mod notices;
mod overlay_input;
pub(in crate::shell) mod preferences;
pub(in crate::shell) mod text_editor;
pub(in crate::shell) mod transient_error;

use crate::endpoint::ClientEndpointId;
use crate::shell::endpoints::endpoint_status_presentation;
use crate::shell::presentation::render::{display_width, put_right_text, put_text};
use crate::shell::state::{
    ClientConfirmCloseOverlay, ClientContextMenuOverlay, ClientGlobalMenuOverlay,
    ClientHelpOverlay, ClientNavigatorOverlay, ClientNavigatorTarget, ClientRenameOverlay,
    ClientShellOverlay,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use shepr_config::LiveKeybindConfig;
use shepr_config::theme::Palette;

use crate::shell::presentation::status::{panel_contrast_fg, status_glyph, status_text};

use crate::limits::{
    MAX_NAVIGATOR_OVERLAY_HEIGHT, MAX_NAVIGATOR_OVERLAY_WIDTH, MIN_CONTEXT_MENU_WIDTH,
    MIN_NAVIGATOR_OVERLAY_HEIGHT, MIN_NAVIGATOR_OVERLAY_WIDTH,
};

/// What an overlay renderer painted into its scratch buffer, and the hit rects it produced.
///
/// Renderers draw into a fresh full-screen scratch buffer, never into the frame; composition
/// commits only a successful render: the `backdrop` dimming first, then the scratch cells of
/// the `opaque` rects (the union is one replacement). A renderer that gives up returns
/// `None` and commits nothing.
#[derive(Default)]
pub(crate) struct OverlayRender {
    /// Absolute rects the overlay painted opaquely: every scratch cell it drew is inside one.
    pub(crate) opaque: Vec<Rect>,
    /// Whether the whole frame is dimmed behind the overlay (Help, Rename, ConfirmClose).
    pub(crate) backdrop: bool,
    pub(crate) menu_rows: Vec<(Rect, usize)>,
    pub(crate) primary: Rect,
    pub(crate) clear: Rect,
    pub(crate) cancel: Rect,
    pub(crate) navigator_popup: Rect,
    pub(crate) navigator_search: Rect,
    pub(crate) navigator_rows: Vec<(Rect, ClientNavigatorTarget)>,
    pub(crate) navigator_scrollbar: Rect,
    pub(crate) navigator_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(crate) help_popup: Rect,
    pub(crate) help_scrollbar: Rect,
    pub(crate) help_scroll_metrics: Option<shepr_termio::scroll::ListScroll>,
    pub(crate) help_max_scroll: usize,
    pub(crate) cursor: Option<shepr_protocol::CursorState>,
}

pub(crate) fn render_client_overlay(
    b: &mut Buffer,
    o: &ClientShellOverlay,
    navigator_index: &crate::shell::navigation::aggregate_navigation::NavigatorIndex,
    active_endpoint_id: &ClientEndpointId,
    k: &LiveKeybindConfig,
    p: &Palette,
) -> Option<OverlayRender> {
    // Help, Rename and ConfirmClose dim everything behind them; the navigator and the menus
    // do not. The dimming is composition's to apply, and only when the render succeeds.
    let backdrop = |rendered: Option<OverlayRender>| {
        rendered.map(|rendered| OverlayRender {
            backdrop: true,
            ..rendered
        })
    };
    match o {
        ClientShellOverlay::Rename(v) => backdrop(render_rename_overlay(b, v, p)),
        ClientShellOverlay::ConfirmClose(v) => backdrop(render_confirm_close_overlay(b, v, p)),
        ClientShellOverlay::Help(v) => backdrop(render_help_overlay(b, v, k, p)),
        ClientShellOverlay::Navigator(v) => {
            render_navigator_overlay(b, v, navigator_index, active_endpoint_id, p)
        }
        ClientShellOverlay::ContextMenu(_) | ClientShellOverlay::GlobalMenu(_) => None,
    }
}

pub(crate) fn render_global_menu(
    buffer: &mut Buffer,
    menu: &ClientGlobalMenuOverlay,
    palette: &Palette,
) -> Option<OverlayRender> {
    let items = crate::shell::overlays::global_menu::global_menu_items();
    let screen = buffer.area;
    let width = items
        .iter()
        .map(|(label, _)| display_width(label))
        .max()
        .unwrap_or(8)
        .saturating_add(4)
        .min(screen.width.max(1));
    let height = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(screen.height.max(1));
    let launcher = menu.launcher;
    let x = launcher
        .right()
        .saturating_sub(width)
        .min(screen.right().saturating_sub(width));
    let y = launcher.y.saturating_sub(height).max(screen.y);
    let rect = Rect::new(x, y, width, height);
    let inner = panel(buffer, rect, palette.accent, palette.panel_bg)?;
    let mut rows = Vec::new();
    for (index, (label, _)) in items.iter().enumerate() {
        let row_y = inner
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if row_y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        let highlighted = index == menu.highlighted;
        let style = if highlighted {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text).bg(palette.panel_bg)
        };
        buffer.set_style(row, style);
        put_text(buffer, row.x, row.y, row.width, &format!(" {label}"), style);
        rows.push((row, index));
    }
    Some(OverlayRender {
        opaque: vec![rect],
        menu_rows: rows,
        ..OverlayRender::default()
    })
}

pub(crate) fn render_context_menu(
    buffer: &mut Buffer,
    menu: &ClientContextMenuOverlay,
    palette: &Palette,
) -> Option<OverlayRender> {
    let items = menu.items();
    let screen = buffer.area;
    let max_item_width = items
        .iter()
        .map(|item| display_width(item.label))
        .max()
        .unwrap_or(0);
    let width = max_item_width
        .saturating_add(4)
        .max(MIN_CONTEXT_MENU_WIDTH)
        .min(screen.width.max(1));
    let height = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(screen.height.max(1));
    let x = menu
        .x
        .min(screen.x.saturating_add(screen.width.saturating_sub(width)));
    let y = menu.y.min(
        screen
            .y
            .saturating_add(screen.height.saturating_sub(height)),
    );
    let rect = Rect::new(x, y, width, height);
    let inner = panel(buffer, rect, palette.accent, palette.panel_bg)?;
    let mut rows = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let row_y = inner
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if row_y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        let highlighted = index == menu.highlighted;
        let style = if highlighted {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text).bg(palette.panel_bg)
        };
        buffer.set_style(row, style);
        put_text(buffer, row.x, row.y, row.width, item.label, style);
        rows.push((row, index));
    }
    Some(OverlayRender {
        opaque: vec![rect],
        menu_rows: rows,
        ..OverlayRender::default()
    })
}

fn panel(
    b: &mut Buffer,
    a: Rect,
    c: ratatui::style::Color,
    bg: ratatui::style::Color,
) -> Option<Rect> {
    if a.width < 2 || a.height < 2 {
        return None;
    }
    let background = Style::default().bg(bg).remove_modifier(Modifier::DIM);
    let border = Style::default().fg(c).bg(bg).remove_modifier(Modifier::DIM);
    // A panel is opaque: every cell is reset before anything is drawn, so nothing already in
    // the buffer (pane attributes, an earlier draw) leaks into the popup.
    for y in a.y..a.bottom() {
        for x in a.x..a.right() {
            if let Some(cell) = b.cell_mut((x, y)) {
                cell.reset();
            }
            set_cell(b, x, y, " ", background);
        }
    }
    for x in a.x..a.right() {
        let top = if x == a.x {
            "┌"
        } else if x + 1 == a.right() {
            "┐"
        } else {
            "─"
        };
        set_cell(b, x, a.y, top, border);
        let bottom = if x == a.x {
            "└"
        } else if x + 1 == a.right() {
            "┘"
        } else {
            "─"
        };
        set_cell(b, x, a.bottom() - 1, bottom, border);
    }
    for y in a.y + 1..a.bottom() - 1 {
        set_cell(b, a.x, y, "│", border);
        set_cell(b, a.right() - 1, y, "│", border);
    }
    Some(Rect::new(a.x + 1, a.y + 1, a.width - 2, a.height - 2))
}

/// Writes one cell, skipping positions outside the buffer. Menus are placed from pointer
/// positions and popups from the frame size; `Buffer` indexing would panic on any rect that
/// reaches past the frame.
fn set_cell(b: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = b.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}
fn popup(a: Rect, w: u16, h: u16) -> Option<Rect> {
    let w = w.min(a.width.saturating_sub(4));
    let h = h.min(a.height.saturating_sub(2));
    if w < 4 || h < 4 {
        return None;
    }
    Some(Rect::new(
        a.x + (a.width - w) / 2,
        a.y + (a.height - h) / 2,
        w,
        h,
    ))
}
fn button(b: &mut Buffer, r: Rect, t: &str, s: Style) {
    b.set_style(r, s);
    let w = display_width(t).min(r.width);
    put_text(b, r.x + (r.width - w) / 2, r.y, w, t, s);
}
fn row(i: Rect, ws: &[u16], gap: u16, off: u16) -> Vec<Rect> {
    let total = ws.iter().sum::<u16>()
        + gap * u16::try_from(ws.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let mut x = i.x + i.width.saturating_sub(total) / 2;
    ws.iter()
        .map(|w| {
            let r = Rect::new(
                x,
                i.y + off.min(i.height.saturating_sub(1)),
                (*w).min(i.width.saturating_sub(x - i.x)),
                1,
            );
            x += *w + gap;
            r
        })
        .collect()
}
fn render_rename_overlay(
    b: &mut Buffer,
    v: &ClientRenameOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let q = popup(b.area, 56, 7)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        v.title,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let input = Rect::new(i.x, i.y + 2, i.width, 1);
    b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
    let cursor = text_editor::render(
        b,
        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1),
        &v.input,
        Style::default().fg(p.text).bg(p.surface0),
    );
    let rs = row(i, &[8, 10, 12], 2, 3);
    let [save, clear, cancel] = rs.as_slice() else {
        return None;
    };
    button(
        b,
        *save,
        " ↵ save ",
        Style::default()
            .fg(panel_contrast_fg(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    let n = Style::default()
        .fg(p.text)
        .bg(p.surface0)
        .add_modifier(Modifier::BOLD);
    button(b, *clear, " ^c clear ", n);
    button(b, *cancel, " esc cancel ", n);
    Some(OverlayRender {
        opaque: vec![q],
        primary: *save,
        clear: *clear,
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        cursor,
        ..OverlayRender::default()
    })
}

fn render_navigator_overlay(
    b: &mut Buffer,
    n: &ClientNavigatorOverlay,
    navigator_index: &crate::shell::navigation::aggregate_navigation::NavigatorIndex,
    active_endpoint_id: &ClientEndpointId,
    p: &Palette,
) -> Option<OverlayRender> {
    let a = b.area;
    let width = a.width.saturating_sub(4).min(MAX_NAVIGATOR_OVERLAY_WIDTH);
    let height = a.height.saturating_sub(2).min(MAX_NAVIGATOR_OVERLAY_HEIGHT);
    if width < MIN_NAVIGATOR_OVERLAY_WIDTH || height < MIN_NAVIGATOR_OVERLAY_HEIGHT {
        return None;
    }
    let q = Rect::new(
        a.x + (a.width - width) / 2,
        a.y + (a.height - height) / 2,
        width,
        height,
    )
    .intersection(a);
    let i = panel(b, q, p.accent, p.panel_bg)?;
    put_text(
        b,
        q.x + 2,
        q.y,
        q.width.saturating_sub(4),
        " Go to ",
        Style::default().fg(p.accent).bg(p.panel_bg),
    );
    let rows = navigator_index.rows(active_endpoint_id, n);
    let search = if n.search_focused {
        " / ".to_owned()
    } else if let Some(f) = n.filter {
        format!(
            " / {}",
            match f {
                ClientNavigatorFilter::Blocked => "blocked",
                ClientNavigatorFilter::Working => "working",
                ClientNavigatorFilter::Idle => "idle",
            }
        )
    } else if n.query.is_empty() {
        " / search agents and terminals".to_owned()
    } else {
        format!(" / {}", n.query)
    };
    let terminal_count = rows
        .iter()
        .filter(|row| matches!(row.target, ClientNavigatorTarget::Pane { .. }))
        .count();
    let count = format!(
        "{terminal_count} {}",
        if terminal_count == 1 {
            "terminal"
        } else {
            "terminals"
        }
    );
    put_text(
        b,
        i.x,
        i.y,
        i.width.saturating_sub(display_width(&count) + 1),
        &search,
        Style::default()
            .fg(if n.search_focused { p.text } else { p.overlay0 })
            .bg(p.panel_bg),
    );
    let cursor = if n.search_focused {
        text_editor::render(
            b,
            Rect::new(
                i.x + 3,
                i.y,
                i.width.saturating_sub(4 + display_width(&count)),
                1,
            ),
            &n.query,
            Style::default().fg(p.text).bg(p.panel_bg),
        )
    } else {
        None
    };
    put_right_text(
        b,
        i,
        i.y,
        &count,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &"─".repeat(i.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(5));
    let selected =
        crate::shell::navigation::aggregate_navigation::navigator_selected_index(&rows, n)
            .unwrap_or(0);
    let max = rows.len().saturating_sub(body.height as usize);
    let scroll = n
        .scroll
        .max(selected.saturating_sub(body.height.saturating_sub(1) as usize))
        .min(selected)
        .min(max);
    let metrics = shepr_termio::scroll::ListScroll::new(scroll, max, usize::from(body.height));
    let scrollbar =
        (max > 0 && body.width > 1).then_some(Rect::new(body.right() - 1, body.y, 1, body.height));
    let row_width = body.width.saturating_sub(u16::from(scrollbar.is_some()));
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            " No matching agents or terminals",
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    for (ix, r) in rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body.height as usize)
    {
        let rect = Rect::new(
            body.x,
            body.y + u16::try_from(ix - scroll).unwrap_or(u16::MAX),
            row_width,
            1,
        );
        row_hits.push((rect, r.target.clone()));
        let st = if r.stale {
            Style::default()
                .fg(p.overlay0)
                .bg(if ix == selected {
                    p.surface0
                } else {
                    p.panel_bg
                })
                .add_modifier(Modifier::DIM)
        } else if ix == selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(
                    if matches!(r.target, ClientNavigatorTarget::Machine { .. }) {
                        p.subtext0
                    } else {
                        p.text
                    },
                )
                .bg(p.panel_bg)
        };
        let is_pane = matches!(r.target, ClientNavigatorTarget::Pane { .. });
        let connector = if !is_pane {
            ""
        } else if rows
            .get(ix + 1)
            .is_some_and(|next| matches!(next.target, ClientNavigatorTarget::Pane { .. }))
        {
            "├─ "
        } else {
            "└─ "
        };
        let padding = u16::from(r.depth.saturating_sub(u8::from(is_pane))) * 2 + 1;
        let connector_x = rect.x + padding;
        let indent = format!("{:width$}{connector}", "", width = usize::from(padding));
        let current = if r.current { "◆ " } else { "" };
        let glyph_option = r.status.map(|status| {
            status_glyph(status, shepr_config::StatusIndicatorStyle::Dots, p, r.stale)
        });
        let status = glyph_option.map_or("", |glyph| glyph.text);
        let status_separator = if status.is_empty() { "" } else { " " };
        let label = format!("{indent}{current}{status}{status_separator}{}", r.label);
        let st = if r.status.is_none() {
            st.add_modifier(Modifier::BOLD)
        } else {
            st
        };
        b.set_style(rect, st);
        let columns = if r.status.is_some() {
            if rect.width >= 64 {
                24
            } else if rect.width >= 36 {
                12
            } else {
                0
            }
        } else {
            0
        };
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width.saturating_sub(columns),
            &label,
            st,
        );
        if is_pane {
            put_text(
                b,
                connector_x,
                rect.y,
                rect.right().saturating_sub(connector_x).min(2),
                connector,
                if r.stale || ix == selected {
                    st
                } else {
                    st.fg(p.overlay0)
                },
            );
        }
        if let (Some(status), Some(glyph)) = (r.status, glyph_option) {
            let prefix = format!("{indent}{current}");
            let status_style = if ix == selected && !r.stale {
                st
            } else {
                glyph.style.bg(if ix == selected {
                    p.surface0
                } else {
                    p.panel_bg
                })
            };
            put_text(
                b,
                rect.x.saturating_add(display_width(&prefix)),
                rect.y,
                display_width(glyph.text),
                glyph.text,
                status_style,
            );
            let meta_style = if r.stale || ix == selected {
                st
            } else {
                st.fg(p.overlay0)
            };
            if columns > 0 {
                put_text(
                    b,
                    rect.right() - columns + 1,
                    rect.y,
                    11,
                    r.agent.as_deref().unwrap_or("terminal"),
                    meta_style,
                );
            }
            if columns == 24 {
                put_text(
                    b,
                    rect.right() - 11,
                    rect.y,
                    11,
                    if r.agent.is_some() {
                        status_text(status)
                    } else {
                        "shell"
                    },
                    meta_style,
                );
            }
        }
        let machine_status = match &r.target {
            ClientNavigatorTarget::Machine { endpoint_id } if !endpoint_id.is_local() => {
                navigator_index.endpoint_status(endpoint_id)
            }
            _ => None,
        };
        if let Some(status) = machine_status {
            let (glyph, state, color) = endpoint_status_presentation(status, p);
            let signal = if status == ClientEndpointStatus::Online {
                glyph.to_owned()
            } else {
                format!("{glyph} {state}")
            };
            let signal_style = if ix == selected {
                st
            } else {
                Style::default()
                    .fg(color)
                    .bg(p.panel_bg)
                    .add_modifier(if r.stale {
                        Modifier::DIM
                    } else {
                        Modifier::empty()
                    })
            };
            put_right_text(b, rect, rect.y, &signal, signal_style);
        } else if r.status.is_none() && !r.meta.is_empty() {
            let label_width = display_width(&label).min(rect.width);
            let meta = Rect::new(
                rect.x.saturating_add(label_width).saturating_add(1),
                rect.y,
                rect.width.saturating_sub(label_width.saturating_add(1)),
                1,
            );
            put_right_text(b, meta, rect.y, &r.meta, st);
        }
    }
    if let Some(track) = scrollbar {
        shepr_termio::scroll::render_scrollbar_buffer(
            b,
            metrics,
            track,
            "▕",
            Style::default().fg(p.overlay0),
            "▐",
            Style::default().fg(p.overlay1),
        );
    }
    if let Some(r) = rows.get(selected) {
        put_text(
            b,
            i.x,
            i.bottom() - 3,
            i.width,
            &format!(" {}", r.detail),
            Style::default().fg(p.subtext0).bg(p.panel_bg),
        );
        put_text(
            b,
            i.x,
            i.bottom() - 2,
            i.width,
            &format!(" {}", r.meta),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    // Navigator controls are fixed by `route_overlay_key`; none are [keys] actions.
    put_text(
        b,
        i.x,
        i.bottom() - 1,
        i.width,
        if n.search_focused {
            " search type · move ↑↓/ctrl+n/p · open enter · back esc"
        } else {
            // Filters cover all agents or one of the three agent states; Ctrl+D pages by eight.
            " ↑↓/j/k rows · ←→ workspace · / search · a/b/w/i filter · enter open · esc close"
        },
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        opaque: vec![q],
        primary: Rect::default(),
        clear: Rect::default(),
        cancel: Rect::default(),
        navigator_popup: q,
        navigator_search: Rect::new(i.x, i.y, i.width, 1),
        navigator_rows: row_hits,
        navigator_scrollbar: scrollbar.unwrap_or_default(),
        navigator_scroll_metrics: Some(metrics),
        cursor,
        ..OverlayRender::default()
    })
}

fn help_lines(
    keybinds: &LiveKeybindConfig,
    query: &str,
    palette: &Palette,
) -> Vec<ratatui::text::Line<'static>> {
    let groups = shepr_termio::input::filter_keybind_help_groups(
        shepr_termio::input::keybind_help_groups(&keybinds.keybinds, keybinds.prefix),
        query,
    );
    let key_width = groups
        .iter()
        .flat_map(|(_, entries)| entries.iter().map(|(key, _)| key.chars().count()))
        .max()
        .unwrap_or(8);
    if groups.is_empty() {
        let message = " no matching keybinds";
        return vec![Line::from(Span::styled(
            message,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        ))];
    }

    let mut lines = Vec::new();
    for (group, entries) in groups {
        lines.push(Line::from(Span::styled(
            format!(" {group}"),
            Style::default()
                .fg(palette.accent)
                .bg(palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, label) in entries {
            let padded_key = format!(" {key:<key_width$} ");
            lines.push(Line::from(vec![
                Span::styled(
                    padded_key,
                    Style::default()
                        .fg(palette.mauve)
                        .bg(palette.panel_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.into_owned(),
                    Style::default().fg(palette.text).bg(palette.panel_bg),
                ),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines
}

fn render_help_overlay(
    b: &mut Buffer,
    h: &ClientHelpOverlay,
    k: &LiveKeybindConfig,
    p: &Palette,
) -> Option<OverlayRender> {
    use ratatui::widgets::Wrap;

    let q = popup(b.area, 76, 22)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    if i.width < 20 || i.height < 6 {
        return None;
    }
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        "keybinds",
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let close = Rect::new(i.right() - 13, i.y, 13, 1);
    button(
        b,
        close,
        if h.search_focused {
            " esc back "
        } else {
            " esc close "
        },
        Style::default()
            .fg(panel_contrast_fg(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    let sy = i.y + 1;
    put_text(
        b,
        i.x,
        sy,
        i.width,
        &if h.search_focused {
            " / ".to_owned()
        } else {
            " / press / to filter by command or shortcut".to_owned()
        },
        Style::default()
            .fg(if h.search_focused { p.text } else { p.overlay0 })
            .bg(p.panel_bg),
    );
    let cursor = if h.search_focused {
        text_editor::render(
            b,
            Rect::new(i.x + 3, sy, i.width.saturating_sub(3), 1),
            &h.query,
            Style::default().fg(p.text).bg(p.panel_bg),
        )
    } else {
        None
    };

    let body = Rect::new(i.x, i.y + 3, i.width, i.height.saturating_sub(5));
    // The scroll range counts rows with the same word wrapper that draws them.
    let paragraph = Paragraph::new(help_lines(k, &h.query, p)).wrap(Wrap { trim: false });
    let viewport_rows = usize::from(body.height.max(1));
    let needs_scrollbar = paragraph.line_count(body.width) > viewport_rows;
    let text_area = if needs_scrollbar {
        Rect::new(body.x, body.y, body.width.saturating_sub(1), body.height)
    } else {
        body
    };
    let total_rows = paragraph.line_count(text_area.width);
    let max_scroll = total_rows.saturating_sub(viewport_rows);
    let scroll = h.scroll.min(max_scroll);
    let metrics = shepr_termio::scroll::ListScroll::new(scroll, max_scroll, viewport_rows);
    let scrollbar = needs_scrollbar.then_some(Rect::new(
        body.right().saturating_sub(1),
        body.y,
        1,
        body.height,
    ));
    Widget::render(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        text_area,
        b,
    );
    if let Some(track) = scrollbar {
        shepr_termio::scroll::render_scrollbar_buffer(
            b,
            metrics,
            track,
            "▐",
            Style::default().fg(p.overlay0).bg(p.panel_bg),
            "▐",
            Style::default().fg(p.overlay1).bg(p.panel_bg),
        );
    }

    // Help search and scrolling controls are fixed by `route_overlay_key`; they are not
    // configurable keybinding actions.
    put_text(
        b,
        i.x,
        i.bottom() - 1,
        i.width,
        if h.search_focused {
            " edit ←→/home/end · kill ^u/^k · yank ^y · scroll ↑↓ · back esc"
        } else {
            " search / · scroll j/k/↑↓/pgup/pgdn · close esc/enter"
        },
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        opaque: vec![q],
        cancel: close,
        help_popup: q,
        help_scrollbar: scrollbar.unwrap_or_default(),
        help_scroll_metrics: Some(metrics),
        help_max_scroll: max_scroll,
        cursor,
        ..OverlayRender::default()
    })
}
fn render_confirm_close_overlay(
    b: &mut Buffer,
    c: &ClientConfirmCloseOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let q = popup(b.area, 64, 6)?;
    let i = panel(b, q, p.red, p.panel_bg)?;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &format!(" {}", c.title),
        Style::default()
            .fg(p.red)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &format!(" {}", c.detail),
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    let rs = row(i, &[13, 12], 2, 3);
    let [ok, cancel] = rs.as_slice() else {
        return None;
    };
    button(
        b,
        *ok,
        " ↵ confirm ",
        Style::default()
            .fg(panel_contrast_fg(p))
            .bg(p.red)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        " esc cancel ",
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    Some(OverlayRender {
        opaque: vec![q],
        primary: *ok,
        clear: Rect::default(),
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        cursor: None,
        ..OverlayRender::default()
    })
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::panel;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::style::Style;

    #[test]
    fn panel_resets_every_cell_so_nothing_leaks_into_the_popup() {
        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let attributed = Style::default()
            .fg(Color::Red)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED | Modifier::DIM);
        for y in 0..5 {
            for x in 0..8 {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol("x").set_style(attributed);
                }
            }
        }
        let inner = panel(
            &mut buffer,
            Rect::new(1, 1, 6, 3),
            Color::Green,
            Color::Black,
        )
        .expect("panel fits");
        assert_eq!(inner, Rect::new(2, 2, 4, 1));
        for y in 1..4 {
            for x in 1..7 {
                let cell = &buffer[(x, y)];
                let border = x == 1 || x == 6 || y == 1 || y == 3;
                assert_eq!(cell.bg, Color::Black, "({x}, {y})");
                assert_eq!(cell.modifier, Modifier::empty(), "({x}, {y})");
                if border {
                    assert_eq!(cell.fg, Color::Green, "({x}, {y})");
                } else {
                    assert_eq!(cell.symbol(), " ", "({x}, {y})");
                    assert_eq!(cell.fg, Color::Reset, "({x}, {y})");
                }
            }
        }
        // Cells outside the panel are untouched.
        assert_eq!(buffer[(0, 0)].symbol(), "x");
    }
}
