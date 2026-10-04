//! Frame resolution: lays out every element of one frame and resolves every scroll position
//! against that layout, from the shell by shared reference. Nothing here writes shell state;
//! `commit_frame` stores what the resolution decided.

use ratatui::layout::Rect;
use shepr_protocol::PaneSurfaceFrame;

use crate::endpoint::ClientEndpointStatus;
use crate::shell::copy::CopySession;
use crate::shell::endpoints::endpoint_status_presentation;
use crate::shell::input::pointer::ClientChromeDrag;
use crate::shell::navigation::location::PinnedLocation;
use crate::shell::notices::cards;
use crate::shell::overlays::OverlayContext;
use crate::shell::sidebar::layout::{SidebarForm, SidebarInputs, resolve_sidebar};
use crate::shell::state::{ClientShellMode, ClientShellState};
use crate::shell::view::{
    LifecycleBanner, NoticeCard, PaneHit, PaneSplitHit, Placeholder, ResolvedFrame, ShellView,
};

/// What sidebar layout and drawing read, borrowed from the shell. `selected` is the workspace
/// the sidebar highlights as selected, which resolution decides.
pub(in crate::shell) fn sidebar_inputs<'a>(
    state: &'a ClientShellState,
    selected: Option<&'a PinnedLocation>,
) -> SidebarInputs<'a> {
    let (dragged_workspace, drop_indicator_row) = match &state.pointer.chrome_drag {
        Some(ClientChromeDrag::Workspace {
            source_workspace_id,
            target,
        }) => (
            Some(source_workspace_id),
            target.as_ref().map(|(_, row)| *row),
        ),
        _ => (None, None),
    };
    SidebarInputs {
        endpoints: &state.endpoints,
        presented: state.endpoints.presented(),
        collapsed: &state.endpoints.collapsed,
        model: &state.endpoints.agent_panel_model,
        config: &state.config,
        agent_panel_sort: state.agent_panel_sort_chrome.value(),
        machine_diagnostics: &state.machine_diagnostics,
        active_snapshot: state.endpoints.active.snapshot(),
        selected,
        section_split: state.chrome.split(),
        dragged_workspace,
        drop_indicator_row,
    }
}

/// What laying out and drawing the open overlay read besides the overlay itself.
pub(in crate::shell) fn overlay_context(state: &ClientShellState) -> OverlayContext<'_> {
    let launcher = state.presentation.shown().global_launcher();
    OverlayContext {
        navigator_index: &state.endpoints.navigator_index,
        active_endpoint_id: state.endpoints.presented(),
        keybinds: &state.config.keybinds,
        palette: &state.config.palette,
        global_launcher: (!launcher.is_empty()).then_some(launcher),
    }
}

/// Lays out the frame for a `cols` by `rows` screen. Only the exact snapshot pair is drawn,
/// so the caller has checked `Presentation::can_draw`.
pub(in crate::shell) fn resolve_frame(
    state: &ClientShellState,
    cols: u16,
    rows: u16,
) -> ResolvedFrame {
    let size = (cols, rows);
    let screen = Rect::new(0, 0, cols, rows);
    let palette = &state.config.palette;
    // A size change while navigating reveals the selection once the sidebar can show it.
    let implied_selected_reveal = state.presentation.view().map(|view| view.size) != Some(size)
        && state.mode.is(ClientShellMode::Navigate);
    let valid_navigation_target = state
        .mode
        .preview()
        .is_some_and(|target| state.navigation_target_valid(target));
    let pending_workspace_highlight =
        state
            .pending_workspace_highlight
            .as_ref()
            .filter(|pending| {
                !state.mode.is(ClientShellMode::Navigate)
                    && pending.target.location.endpoint == *state.endpoints.presented()
                    && state.navigation_target_valid(&pending.target)
            });
    let selected = state
        .mode
        .preview()
        .filter(|_| valid_navigation_target)
        .or_else(|| pending_workspace_highlight.map(|pending| &pending.target))
        .cloned();
    let has_surface = state.endpoints.active.snapshot().is_some() && state.pane_surface().is_some();
    let layout = state.layout(cols, rows);
    let mut chrome_layout = layout;
    if !has_surface && layout.sidebar.width == 0 {
        chrome_layout.sidebar = Rect::new(0, 1, cols, rows.saturating_sub(2));
    }
    let form = if chrome_layout.sidebar.width == 0 {
        SidebarForm::Hidden
    } else if layout.sidebar.width > 0 && state.chrome.collapsed() {
        SidebarForm::Collapsed
    } else {
        SidebarForm::Expanded
    };
    let (sidebar, resolution) = resolve_sidebar(
        chrome_layout.sidebar,
        form,
        &sidebar_inputs(state, selected.as_ref()),
        &state.sidebar_scroll,
        implied_selected_reveal,
    );
    let carry_selected_reveal = implied_selected_reveal && !resolution.workspace_reveal_consumed;

    let lifecycle = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == *state.endpoints.presented())
        .filter(|endpoint| endpoint.state.stale())
        .map(|endpoint| {
            let label = endpoint.endpoint_id.display_label().to_owned();
            let status = endpoint.state.status();
            let area = if !has_surface && layout.sidebar.width > 0 {
                layout.pane_surface
            } else {
                screen
            };
            LifecycleBanner {
                rect: cards::lifecycle_banner_rect(area, &label, status, palette),
                label,
                status,
            }
        });
    let healthy_local_chrome = state.endpoints.active.snapshot().is_some()
        && state.endpoints.len() == 1
        && !state.chrome.collapsed()
        && layout.sidebar.width > 0
        && state.endpoint_usable(state.endpoints.presented());
    // The lifecycle banner already carries the placeholder status. A second status line in
    // the same row can be covered by that banner on narrow surfaces. An endpoint error
    // suppressed here still shows in the mode bar.
    let placeholder = (!has_surface
        && (!healthy_local_chrome || state.endpoint_error.message().is_some())
        && lifecycle.is_none())
    .then(|| {
        let message = state.endpoint_error.message().map_or_else(
            || {
                let status = state
                    .endpoint_status(state.endpoints.presented())
                    .unwrap_or(ClientEndpointStatus::Connecting);
                let (_, label, _) = endpoint_status_presentation(status, palette);
                if state.endpoints.len() == 1 {
                    format!("{}: {label}.", state.active_endpoint_label())
                } else {
                    format!(
                        "{}: {label}. Select a connected machine.",
                        state.active_endpoint_label()
                    )
                }
            },
            str::to_owned,
        );
        let area = if layout.sidebar.width > 0 {
            layout.pane_surface
        } else {
            Rect::new(0, 0, cols, 1)
        };
        Placeholder { area, message }
    });

    // The surface may have been produced for another layout: a resize or sidebar toggle keeps
    // the retained surface until the resized one arrives, a resize can race a surface already
    // in flight. Drawing clips the cells; the hits are clipped to match through
    // `PaneHit::from_wire`, so mouse input and the copy cursor never target rows or columns
    // that are not on screen.
    let hits_live = state.endpoint_usable(state.endpoints.presented());
    let (panes, splits) = match state.presentation.surfaces.paired().filter(|_| has_surface) {
        Some(surface) => {
            let surface_overflows = surface_overflows_area(surface, layout.pane_surface);
            let panes = surface
                .panes
                .iter()
                .filter_map(|pane| {
                    PaneHit::from_wire(
                        pane,
                        (layout.pane_surface.x, layout.pane_surface.y),
                        layout.pane_surface,
                    )
                })
                .collect::<Vec<_>>();
            // A split dragged against geometry the screen does not show would send ratios
            // computed from the wrong extent; splits wait for a surface that fits.
            let splits = if surface_overflows || !state.config.mouse_capture || !hits_live {
                Vec::new()
            } else {
                split_hits(surface, layout.pane_surface)
            };
            (panes, splits)
        }
        None => (Vec::new(), Vec::new()),
    };
    let copy_cursor = if has_surface && state.mode.is(ClientShellMode::Copy) {
        state
            .copy
            .as_ref()
            .and_then(|copy_mode| client_copy_cursor_cell(copy_mode, &panes))
    } else {
        None
    };
    // The bar normally covers the pane area's bottom row. When the copy cursor sits on that
    // row (the last line of history, which scrolling cannot lift, or a pane too short to
    // reserve it) the bar moves to the top row so the cursor stays visible.
    let mode_bar_area = if !has_surface {
        screen
    } else {
        let bottom_row = layout.pane_surface.bottom().saturating_sub(1);
        if layout.pane_surface.height > 1 && copy_cursor.map(|(_, y)| y) == Some(bottom_row) {
            Rect::new(
                layout.pane_surface.x,
                layout.pane_surface.y,
                layout.pane_surface.width,
                1,
            )
        } else {
            layout.pane_surface
        }
    };

    let notice_offset = if has_surface {
        u16::from(lifecycle.is_some())
    } else if layout.sidebar.width == 0 {
        // The fallback sidebar starts below the placeholder line when its normal column is
        // hidden, so leave its header row clear as well.
        2
    } else {
        // An expanded sidebar has its header on row zero, even without a pane surface.
        1
    };
    let notice = state.notices.visible().map(|notice| NoticeCard {
        rect: cards::notice_card_rect(screen, notice, notice_offset),
    });

    // The overlay is laid out before anything is drawn. One that does not fit has no view,
    // and resolves no scroll, so it keeps the one it had.
    let (overlay, overlay_scroll) = state
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.layout(screen, &overlay_context(state)))
        .map_or((None, None), |(view, scroll)| (Some(view), scroll));

    ResolvedFrame {
        view: ShellView {
            size,
            layout,
            has_surface,
            placeholder,
            sidebar,
            selected,
            panes,
            splits,
            hits_live,
            copy_cursor,
            lifecycle,
            notice,
            mode_bar_area,
            overlay,
            chrome_armed: state.config.mouse_capture,
        },
        sidebar: resolution,
        carry_selected_reveal,
        overlay_scroll,
    }
}

fn split_hits(surface: &PaneSurfaceFrame, area: Rect) -> Vec<PaneSplitHit> {
    surface
        .splits
        .iter()
        .map(|split| PaneSplitHit {
            direction: split.direction,
            pos: match split.direction {
                shepr_protocol::PaneSurfaceSplitDirection::Horizontal => {
                    area.x.saturating_add(split.pos)
                }
                shepr_protocol::PaneSurfaceSplitDirection::Vertical => {
                    area.y.saturating_add(split.pos)
                }
            },
            area: super::surface_rect_on_screen((area.x, area.y), split.area),
            hit_rect: super::surface_rect_on_screen((area.x, area.y), split.hit_rect),
            path: split.path.clone(),
            epoch: split.epoch,
        })
        .collect()
}

/// Whether the retained surface reaches past the pane area it is drawn into. A smaller surface
/// (the panes have not grown into a larger area yet) is drawn whole and its hits stay exact.
pub(in crate::shell) fn surface_overflows_area(surface: &PaneSurfaceFrame, area: Rect) -> bool {
    surface.frame.width() > area.width || surface.frame.height() > area.height
}

pub(in crate::shell) fn client_copy_surface_coherent(
    copy_mode: Option<&CopySession>,
    hit: &PaneHit,
) -> bool {
    copy_mode
        .filter(|copy_mode| copy_mode.pane_id == hit.pane_id)
        .is_none_or(|copy_mode| {
            copy_mode.geometry == hit.pane_size
                && hit.scroll.is_some_and(|scroll| {
                    scroll.offset_from_bottom == copy_mode.scroll.offset_from_bottom
                        && scroll.max_offset_from_bottom == copy_mode.scroll.max_offset_from_bottom
                        && scroll.history_origin == copy_mode.scroll.history_origin
                })
        })
}

/// Screen cell of the copy-mode cursor, when its pane is on screen, coherent with the copy
/// state, and the cursor row is inside the pane's viewport.
fn client_copy_cursor_cell(copy_mode: &CopySession, hits: &[PaneHit]) -> Option<(u16, u16)> {
    let hit = hits.iter().find(|hit| {
        hit.pane_id == copy_mode.pane_id && client_copy_surface_coherent(Some(copy_mode), hit)
    })?;
    let viewport_row = copy_mode
        .cursor
        .row
        .0
        .checked_sub(copy_mode.viewport_top().0)?;
    let viewport_row = u16::try_from(viewport_row)
        .ok()
        .filter(|row| *row < hit.inner_rect.height)?;
    if copy_mode.cursor.col >= hit.inner_rect.width {
        return None;
    }
    Some((
        hit.inner_rect.x.saturating_add(copy_mode.cursor.col),
        hit.inner_rect.y.saturating_add(viewport_row),
    ))
}
