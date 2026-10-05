//! Frame drawing: draws exactly what a resolved `ShellView` says, reading the shell by shared
//! reference for content (labels, palette, cells) and returning the frame with its effects on
//! pane output. Nothing here writes shell state.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_config::LiveKeybindConfig;
use shepr_config::theme::Palette;
use shepr_protocol::WireColor;
use shepr_surface::compose::{Canvas, ChromePalette, StylePatch};
use shepr_surface::ratatui_conversion::WireColorExt as _;

use crate::shell::copy::CopySession;
use crate::shell::notices::cards;
use crate::shell::overlays::text_editor;
use crate::shell::presentation::LastComposition;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::{display_width, put_text};
use crate::shell::sidebar::endpoint_sidebar;
use crate::shell::sidebar::layout::SidebarView;
use crate::shell::state::{ClientShellMode, ClientShellState};
use crate::shell::view::resolve::{
    client_copy_surface_coherent, overlay_context, sidebar_inputs, surface_overflows_area,
};
use crate::shell::view::{DrawnFrame, PaneHit, ShellView};

/// The colours of the pane chrome a server draws, which names each cell's role and
/// leaves the colour to the client.
fn chrome_palette(palette: &Palette) -> ChromePalette {
    let color = WireColor::from_ratatui;
    ChromePalette {
        border: color(palette.overlay0),
        border_focused: color(palette.accent),
        scroll_track: color(palette.surface_dim),
        scroll_track_focused: color(palette.overlay0),
        scroll_thumb: color(palette.overlay0),
        scroll_thumb_focused: color(palette.overlay1),
    }
}

/// Draws `view`. `None` only when the canvas refuses the drawn buffer, which keeps the last
/// frame on screen.
pub(super) fn draw_frame(state: &ClientShellState, view: &ShellView) -> Option<DrawnFrame> {
    let (cols, rows) = view.size;
    let screen = Rect::new(0, 0, cols, rows);
    let palette = &state.config.palette;
    let mut effects = LastComposition::default();
    let mut buffer = Buffer::empty(screen);
    if !view.has_surface {
        buffer.set_style(
            buffer.area,
            Style::default().fg(palette.text).bg(palette.panel_bg),
        );
    }
    let inputs = sidebar_inputs(state, view.selected.as_ref());
    match &view.sidebar {
        SidebarView::Hidden => {}
        SidebarView::Collapsed(sidebar) => {
            endpoint_sidebar::draw_collapsed(&mut buffer, sidebar, &inputs);
        }
        SidebarView::Expanded(sidebar) => {
            endpoint_sidebar::draw_expanded(&mut buffer, sidebar, &inputs);
        }
    }
    if let Some(placeholder) = &view.placeholder {
        put_text(
            &mut buffer,
            placeholder.area.x,
            placeholder.area.y,
            placeholder.area.width,
            &placeholder.message,
            Style::default().fg(palette.overlay0),
        );
    }
    // Pane cells and their local decorations are one optional layer. Everything above them
    // (lifecycle, notices, overlays and the mode bar) follows the same pipeline when the pane
    // area contains only a connection placeholder.
    // `buffer` came from `Buffer::empty` and was only drawn into, so its cells match its
    // area; a refusal keeps the last frame on screen like an unpaired surface does.
    let mut frame = Canvas::from_buffer(&buffer).ok()?;
    if let Some(surface) = state
        .presentation
        .surfaces
        .paired()
        .filter(|_| view.has_surface)
    {
        // `Canvas::compose_pane` clips the cells of a surface produced for another layout;
        // the resolved hits were clipped to match. Later draws that use these rects still go
        // through `Buffer::cell_mut`, never `buffer[(x, y)]`.
        effects.pane_cells_occluded |= surface_overflows_area(surface, view.layout.pane_surface);
        // Chrome is the only thing drawn through ratatui here; from this point the canvas's
        // wire cells are the composition target and every later stage patches or overwrites
        // them in place (see `shepr_surface::compose`).
        frame.compose_pane(
            &surface.frame,
            view.layout.pane_surface,
            &chrome_palette(palette),
        );
        let has_selection = state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_visible);
        let has_search = state
            .copy
            .as_ref()
            .and_then(|copy_mode| copy_mode.search.as_ref())
            .is_some_and(|search| !search.results.matches.is_empty());
        // Highlights restyle wire cells in the existing order: noncurrent search matches,
        // selection, the current search match, then the copy cursor.
        if has_selection || has_search {
            for hit in &view.panes {
                let copy_surface_coherent = client_copy_surface_coherent(state.copy.as_ref(), hit);
                if copy_surface_coherent {
                    render_client_copy_search_highlights(
                        &mut frame,
                        state.copy.as_ref(),
                        hit,
                        palette,
                        false,
                    );
                }
                let selection_is_stale_copy_projection = !copy_surface_coherent
                    && state.copy.as_ref().is_some_and(|copy_mode| {
                        copy_mode.pane_id == hit.pane_id
                            && state
                                .mouse_selection
                                .selection
                                .as_ref()
                                .is_some_and(|selection| selection.belongs_to(&hit.pane_id))
                    });
                if !selection_is_stale_copy_projection {
                    crate::shell::presentation::selection_render::render_selection_highlight(
                        state.mouse_selection.selection.as_ref(),
                        &hit.pane_id,
                        hit.inner_rect,
                        hit.scroll,
                        palette,
                        shepr_term::host::TerminalTheme {
                            background: state.host_theme.background,
                            ..Default::default()
                        },
                        &mut |x, y, style| {
                            frame.patch_cell(x, y, StylePatch::from_style(style));
                        },
                    );
                }
                if copy_surface_coherent {
                    render_client_copy_search_highlights(
                        &mut frame,
                        state.copy.as_ref(),
                        hit,
                        palette,
                        true,
                    );
                }
            }
        }
        if state.mode.is(ClientShellMode::Copy) {
            effects.pane_cursor_overridden = true;
            frame.set_cursor(None);
            if let Some((x, y)) = view.copy_cursor
                && x < frame.width()
                && y < frame.height()
            {
                frame.patch_cell(
                    x,
                    y,
                    StylePatch::from_style(
                        Style::default()
                            .fg(panel_contrast_fg(palette))
                            .bg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
            }
        }
    }
    if !view.hits_live {
        effects.pane_cursor_overridden = true;
        frame.set_cursor(None);
    }
    if view.lifecycle.is_some() || view.notice.is_some() {
        // Banner and card are opaque: they draw into a fresh scratch buffer and the frame
        // takes exactly their rects.
        let mut scratch = Buffer::empty(screen);
        let mut opaque = Vec::new();
        if let Some(banner) = &view.lifecycle {
            cards::draw_lifecycle_banner(
                &mut scratch,
                banner.rect,
                &banner.label,
                banner.status,
                palette,
            );
            opaque.push(banner.rect);
        }
        if let (Some(card), Some(notice)) = (&view.notice, state.notices.visible()) {
            cards::draw_notice_card(&mut scratch, card.rect, notice, palette);
            opaque.push(card.rect);
        }
        effects.pane_cells_occluded |= opaque.iter().any(|rect| !rect.is_empty());
        frame.overwrite(&opaque, &scratch);
    }
    if let Some(overlay) = state.overlay.as_ref() {
        effects.pane_cells_occluded = true;
        effects.pane_cursor_overridden = true;
        if let Some(overlay_view) = &view.overlay {
            // The overlay draws from its view into a fresh scratch buffer. The frame is
            // touched only when the layout fit, so one that gave up leaves it exactly as it
            // was for the fallback hint below.
            let mut scratch = Buffer::empty(screen);
            let paint = overlay.draw(overlay_view, &mut scratch, &overlay_context(state));
            if paint.backdrop {
                frame.patch_all(StylePatch::from_style(
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            frame.overwrite(&paint.opaque, &scratch);
            frame.set_cursor(paint.cursor);
        } else {
            // The overlay does not fit this terminal, and nothing it drew was committed:
            // the frame without the overlay is presented, so pane output keeps flowing, and
            // a one-line hint says why the overlay is missing. The overlay stays open (its
            // keys still work, esc closes it) and reappears once the terminal is large
            // enough. Overlay hit rects stay empty, so mouse input cannot hit an invisible
            // popup.
            if cols > 0 && rows > 0 {
                let hint_row = rows - 1;
                let hint_style = Style::default()
                    .fg(panel_contrast_fg(palette))
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD);
                // The whole row takes the hint's style; the text replaces the prefix the
                // shared text writer placed, whose extent is the overwritten rect (a wide
                // glyph the text boundary splits is blanked).
                frame.patch_rect(
                    Rect::new(0, hint_row, cols, 1),
                    StylePatch::from_style(hint_style),
                );
                let mut scratch = Buffer::empty(screen);
                let written_to = put_text(
                    &mut scratch,
                    0,
                    hint_row,
                    cols,
                    " window too small for this popup · esc closes",
                    hint_style,
                );
                frame.overwrite(&[Rect::new(0, hint_row, written_to, 1)], &scratch);
            }
            frame.set_cursor(None);
        }
    }
    // The mode bar is drawn last, after banners and notices, and only when no overlay is open
    // (an overlay that does not fit still counts as open). It draws into a scratch buffer like
    // the overlays. Its pane-cursor suppression is its own rule, distinct from an overlay's
    // returned input cursor: a pane cursor on the bar's row would show through the bar.
    if state.overlay.is_none() {
        let mut scratch = Buffer::empty(screen);
        if let Some(bar) = render_mode_bar(
            &mut scratch,
            view.mode_bar_area,
            state.mode.kind(),
            state.copy.as_ref(),
            state.endpoint_error.message(),
            &state.config.keybinds,
            palette,
        ) {
            effects.pane_cells_occluded = true;
            frame.overwrite(&[bar], &scratch);
            if frame.cursor().is_some_and(|cursor| cursor.y == bar.y) {
                frame.set_cursor(None);
            }
        }
    }
    Some(DrawnFrame {
        frame: frame.into_frame(),
        effects,
    })
}

fn render_client_copy_search_highlights(
    frame: &mut Canvas,
    copy_mode: Option<&CopySession>,
    hit: &PaneHit,
    palette: &Palette,
    current_only: bool,
) {
    let Some(copy_mode) = copy_mode.filter(|copy_mode| copy_mode.pane_id == hit.pane_id) else {
        return;
    };
    let Some(search) = copy_mode.search.as_ref() else {
        return;
    };
    if hit.inner_rect.is_empty() {
        return;
    }
    let top = copy_mode.viewport_top();
    let bottom = top.saturating_add(usize::from(hit.inner_rect.height.saturating_sub(1)));
    let patch = StylePatch::from_style(if current_only {
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.surface1)
    });
    for (index, text_match) in search.results.matches.iter().enumerate() {
        if (search
            .results
            .current
            .is_some_and(|current| current.window_index == index))
            != current_only
            || text_match.end.row < top
            || text_match.start.row > bottom
        {
            continue;
        }
        let start_row = text_match.start.row.max(top);
        let end_row = text_match.end.row.min(bottom);
        for absolute_row in start_row.0..=end_row.0 {
            let viewport_row =
                u16::try_from(absolute_row.saturating_sub(top.0)).unwrap_or(u16::MAX);
            let start_col = if absolute_row == text_match.start.row.0 {
                text_match.start.col
            } else {
                0
            };
            let end_col = if absolute_row == text_match.end.row.0 {
                text_match.end.col
            } else {
                hit.inner_rect.width.saturating_sub(1)
            };
            let end_col = end_col.min(hit.inner_rect.width.saturating_sub(1));
            // The hit comes from the pane surface, whose geometry may have been produced for
            // a different layout than this frame (see where `resolve_frame` builds pane
            // hits). `Canvas::patch_cell` skips positions outside the frame.
            for col in start_col..=end_col {
                let (Some(x), Some(y)) = (
                    hit.inner_rect.x.checked_add(col),
                    hit.inner_rect.y.checked_add(viewport_row),
                ) else {
                    continue;
                };
                frame.patch_cell(x, y, patch);
            }
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

fn render_mode_bar(
    buffer: &mut Buffer,
    pane_area: Rect,
    mode: ClientShellMode,
    copy_mode: Option<&CopySession>,
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
        .fg(panel_contrast_fg(palette))
        .bg(if mode == ClientShellMode::Resize {
            palette.mauve
        } else {
            palette.accent
        })
        .add_modifier(Modifier::BOLD);
    let prefix = shepr_config::format_key_chord(keybinds.prefix);
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
                // Navigate mode takes only these keys, so the bar lists all of them.
                let navigate = &keybinds.keybinds.navigate;
                segments.extend([
                    (" NAVIGATE ".to_owned(), mode_style),
                    (
                        format!("{} ", configured_key_labels(&[&navigate.back])),
                        key,
                    ),
                    ("back  ".to_owned(), base),
                    (configured_key_labels(&[&navigate.up, &navigate.down]), key),
                    (" workspace/agent  ".to_owned(), base),
                    (configured_key_labels(&[&navigate.open]), key),
                    (" open".to_owned(), base),
                ]);
            }
            ClientShellMode::Resize => {
                segments.extend([
                    (" RESIZE ".to_owned(), mode_style),
                    ("  ".to_owned(), base),
                    (
                        crate::shell::input::resize_help_keys(
                            crate::shell::input::ResizeHelpGroup::Width,
                        ),
                        key,
                    ),
                    (" width  ".to_owned(), base),
                    (
                        crate::shell::input::resize_help_keys(
                            crate::shell::input::ResizeHelpGroup::Height,
                        ),
                        key,
                    ),
                    (" height  ".to_owned(), base),
                    (
                        crate::shell::input::resize_help_keys(
                            crate::shell::input::ResizeHelpGroup::Finish,
                        ),
                        key,
                    ),
                    (" done".to_owned(), base),
                ]);
            }
            ClientShellMode::Copy => {
                // Copy-mode commands, including search prompt controls, have fixed input
                // bindings and no entries in the configurable keybinding table.
                let copy_mode = copy_mode?;
                if let Some(prompt) = copy_mode
                    .search
                    .as_ref()
                    .and_then(|search| search.prompt.as_ref())
                {
                    let marker = match prompt.direction {
                        shepr_protocol::command::PaneCopySearchDirection::Forward => "/",
                        shepr_protocol::command::PaneCopySearchDirection::Backward => "?",
                    };
                    put_text(buffer, bar.x, bar.y, bar.width, " COPY ", mode_style);
                    let prefix = 8.min(bar.width);
                    if bar.width >= 8 {
                        put_text(buffer, bar.x + 7, bar.y, 1, marker, key);
                    }
                    let footer = format!(
                        "  {} search  {} cancel",
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::SearchPromptSubmit,
                        ),
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::SearchPromptCancel,
                        ),
                    );
                    let footer_width = if bar.width >= 50 {
                        display_width(&footer)
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
                        put_text(
                            buffer,
                            bar.right() - footer_width,
                            bar.y,
                            footer_width,
                            &footer,
                            base,
                        );
                    }
                    return Some(bar);
                }
                let select = if copy_mode.selection.is_some() {
                    "selecting"
                } else {
                    "select"
                };
                let search = copy_mode.search.as_ref();
                let match_status = search
                    .and_then(|search| {
                        search.results.current.map(|current| {
                            format!(
                                " {}/{}",
                                current.global_index.saturating_add(1),
                                search.results.total
                            )
                        })
                    })
                    .or_else(|| {
                        search
                            .is_some_and(|search| !search.query.is_empty())
                            .then(|| " 0/0".to_owned())
                    })
                    .unwrap_or_default();
                let quit_keys = shepr_termio::copy_mode::copy_mode_help_keys(
                    shepr_termio::copy_mode::CopyModeHelpGroup::Exit,
                );
                let clear_keys = shepr_termio::copy_mode::copy_mode_help_keys(
                    shepr_termio::copy_mode::CopyModeHelpGroup::Clear,
                );
                let (exit_keys, exit_label) = if search.is_none_or(|search| search.query.is_empty())
                    && copy_mode.selection.is_none()
                {
                    (format!("{quit_keys}/{clear_keys}"), " exit".to_owned())
                } else {
                    (clear_keys, format!(" clear  {quit_keys} exit"))
                };
                segments.extend([
                    (" COPY ".to_owned(), mode_style),
                    (" ".to_owned(), base),
                    (
                        format!(
                            "{} {} {}",
                            shepr_termio::copy_mode::copy_mode_help_keys(
                                shepr_termio::copy_mode::CopyModeHelpGroup::Cursor,
                            ),
                            shepr_termio::copy_mode::copy_mode_help_keys(
                                shepr_termio::copy_mode::CopyModeHelpGroup::Word,
                            ),
                            shepr_termio::copy_mode::copy_mode_help_keys(
                                shepr_termio::copy_mode::CopyModeHelpGroup::Paragraph,
                            ),
                        ),
                        key,
                    ),
                    (" move  ".to_owned(), base),
                    (
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::Search,
                        ),
                        key,
                    ),
                    (" search  ".to_owned(), base),
                    (
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::Repeat,
                        ),
                        key,
                    ),
                    (format!(" repeat{match_status}  "), base),
                    (
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::Selection,
                        ),
                        key,
                    ),
                    (format!(" {select}  "), base),
                    (
                        shepr_termio::copy_mode::copy_mode_help_keys(
                            shepr_termio::copy_mode::CopyModeHelpGroup::Copy,
                        ),
                        key,
                    ),
                    (" copy  ".to_owned(), base),
                    (exit_keys, key),
                    (exit_label, base),
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
        x = x.saturating_add(put_text(buffer, x, bar.y, remaining, &text, style));
    }
    Some(bar)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use shepr_config::ClientConfig;
    use shepr_config::theme::Palette;
    use shepr_protocol::{FrameData, SurfaceRect};
    use shepr_surface::ratatui_conversion::{FrameDataExt as _, WireColorExt as _};
    use shepr_test_fixtures::ValidatedClientConfigFixture as _;

    use crate::shell::config::ClientShellConfig;
    use crate::shell::state::{ClientShellInput, ClientShellMode, ClientShellState};
    use crate::shell::tests::{answer, copy_search, copy_search_result, snapshot, surface};
    use crate::shell::view::PaneHit;
    use crate::shell::view::draw::{render_client_copy_search_highlights, render_mode_bar};

    use shepr_protocol::command::{PaneTextPoint, PaneTextRange};

    fn text_range(row: u64, start_col: u16, end_col: u16) -> PaneTextRange {
        PaneTextRange {
            start: PaneTextPoint {
                row: shepr_term::AbsRow(row),
                col: start_col,
            },
            end: PaneTextPoint {
                row: shepr_term::AbsRow(row),
                col: end_col,
            },
        }
    }

    #[test]
    fn copy_search_highlights_clip_surface_taller_than_frame() {
        // A pane surface produced for another layout (e.g. before a resize took effect)
        // is one row taller than the frame. Matches on its bottom row are off-buffer
        // and must be skipped instead of panicking on `Buffer` indexing.
        let hit = PaneHit {
            rect: Rect::new(0, 0, 6, 4),
            inner_rect: Rect::new(0, 0, 6, 4),
            scrollbar_rect: None,
            scroll: Some(shepr_term::ScrollMetrics::new(
                0,
                0,
                4,
                shepr_term::AbsRow(0),
            )),
            pane_id: crate::tests::test_pane_id("w1:p1"),
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            pane_size: (6, 4),
            presented: None,
        };
        // The session comes from a shell showing the pane at its full height: copy mode on
        // its last row, and a search answered with a match on each of its last two rows.
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::with_lines(["xxxxxx"; 4]),
            None,
            &[],
        )
        .expect("test buffer is a valid frame");
        pane_surface.panes[0].rect = SurfaceRect {
            x: 0,
            y: 0,
            width: 6,
            height: 4,
        };
        pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            0,
            4,
            shepr_term::AbsRow(0),
        ));
        state.receive_pane_surface_from(
            pane_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        state.compose(106, 20).expect("composed frame");
        assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
        let search = copy_search(&mut state);
        answer(
            &mut state,
            &search,
            Ok(copy_search_result(
                vec![text_range(2, 0, 1), text_range(3, 0, 5)],
                Some(1),
            )),
        );
        let copy_mode = state.copy.as_ref().expect("copy mode");
        assert_eq!(
            copy_mode.cursor,
            PaneTextPoint {
                row: shepr_term::AbsRow(3),
                col: 0,
            }
        );
        let palette = Palette::catppuccin();
        let mut frame =
            shepr_surface::compose::Canvas::from_buffer(&Buffer::empty(Rect::new(0, 0, 6, 3)))
                .expect("a fresh buffer is a valid canvas");

        for current_only in [false, true] {
            render_client_copy_search_highlights(
                &mut frame,
                Some(copy_mode),
                &hit,
                &palette,
                current_only,
            );
        }

        let bg = |x: usize, y: usize| frame.cells()[y * 6 + x].bg;
        let surface1 = shepr_protocol::WireColor::from_ratatui(palette.surface1);
        assert_eq!(bg(0, 2), surface1);
        assert_eq!(bg(1, 2), surface1);
        assert_ne!(bg(2, 2), surface1);
    }

    #[test]
    fn navigate_mode_bar_uses_configured_action_keys() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut config = shepr_config::ClientConfig::default();
        config.keys.navigate_back = shepr_config::BindingConfig::one("q");
        config.keys.navigate_up = shepr_config::BindingConfig::one("u");
        config.keys.navigate_down = shepr_config::BindingConfig::one("d");
        config.keys.navigate_open = shepr_config::BindingConfig::one("o");
        let validated = shepr_config::ValidatedClientConfig::test_from_config(config, None);
        let area = Rect::new(0, 0, 120, 2);
        let mut buffer = Buffer::empty(area);

        render_mode_bar(
            &mut buffer,
            area,
            ClientShellMode::Navigate,
            None,
            None,
            validated.live_keybinds(),
            validated.palette(),
        );

        let row = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<Vec<_>>()
            .concat();
        assert!(row.contains("q back"), "{row}");
        assert!(row.contains("u / d workspace/agent"), "{row}");
        assert!(row.contains("o open"), "{row}");
        assert!(!row.contains("esc back"), "{row}");
        assert!(!row.contains("enter open"), "{row}");
        // Navigate mode takes no other key, so the bar names none.
        assert!(!row.contains("pane"), "{row}");
        assert!(!row.contains("keybinds"), "{row}");
    }
}
