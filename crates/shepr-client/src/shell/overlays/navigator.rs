//! The navigator ("Go to"): its keys, rows, layout, drawing, selection and scrollbar drag.
//! Typed and pasted text land in its search editor; input content must stay out of logs and
//! error messages here.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_term::key::TerminalKey;
use shepr_term::scroll::ListScroll;
use shepr_termio::input::fixed_keys::{FixedKey, KeyBinding, ModifierMatch, command_for};
use shepr_termio::text_editor::TextEditor;

use super::widgets::{panel, panel_inner, popup};
use super::{
    NavigatorSlot, NavigatorView, OverlayCommand, OverlayContext, OverlayEffect, OverlayPaint,
    OverlayScroll, text_editor,
};
use crate::endpoint::ClientEndpointStatus;
use crate::limits::{
    MAX_NAVIGATOR_OVERLAY_HEIGHT, MAX_NAVIGATOR_OVERLAY_WIDTH, MIN_NAVIGATOR_OVERLAY_HEIGHT,
    OVERLAY_WHEEL_SCROLL_ROWS,
};
use crate::shell::endpoints::endpoint_status_presentation;
use crate::shell::input::hit_test::contains;
use crate::shell::navigation::aggregate_navigation::{
    navigator_selected_index, selected_navigator_target,
};
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::presentation::status::{panel_contrast_fg, status_glyph, status_text};
use crate::shell::presentation::text::{display_width, put_right_text, put_text};
use crate::shell::view::list::ListView;

// The navigator footers are written out instead of derived from these tables, unlike the
// copy-mode and resize mode bars. They group keys more compactly than one label per binding
// can (`↑↓`, `ctrl+n/p`) and list only the keys worth naming, so the trailing close hint still
// fits a narrow overlay. The navigator's tests check that every key a footer names routes to
// its command.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigatorCommand {
    BackOrClose,
    Open,
    MoveUp,
    MoveDown,
    MoveWorkspaceLeft,
    MoveWorkspaceRight,
    ClearFilter,
    Top,
    Bottom,
    Search,
    PageUp,
    PageDown,
    FilterBlocked,
    FilterWorking,
    FilterIdle,
    FilterAll,
}

const fn code(code: KeyCode, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::Code(code, modifiers)
}

const fn control(character: char, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::ControlCharacter(character, modifiers)
}

const CONTROL_ONLY: ModifierMatch = ModifierMatch::Exact(KeyModifiers::CONTROL);

const NAVIGATOR_MAIN_BINDINGS: &[KeyBinding<NavigatorCommand>] = &[
    KeyBinding::unlisted(
        NavigatorCommand::BackOrClose,
        code(KeyCode::Esc, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Open,
        code(KeyCode::Enter, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveUp,
        code(KeyCode::Up, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveDown,
        code(KeyCode::Down, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveUp, FixedKey::Character('k')),
    KeyBinding::unlisted(NavigatorCommand::MoveDown, FixedKey::Character('j')),
    KeyBinding::unlisted(
        NavigatorCommand::MoveWorkspaceLeft,
        code(KeyCode::Left, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveWorkspaceRight,
        code(KeyCode::Right, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::ClearFilter,
        code(KeyCode::Backspace, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Top,
        code(KeyCode::Home, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Bottom,
        code(KeyCode::End, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(NavigatorCommand::Bottom, FixedKey::Character('G')),
    KeyBinding::unlisted(NavigatorCommand::Search, FixedKey::Character('/')),
    KeyBinding::unlisted(
        NavigatorCommand::PageDown,
        control('d', ModifierMatch::Contains(KeyModifiers::CONTROL)),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::PageUp,
        control('u', ModifierMatch::Contains(KeyModifiers::CONTROL)),
    ),
    KeyBinding::unlisted(NavigatorCommand::FilterAll, FixedKey::Character('a')),
    KeyBinding::unlisted(NavigatorCommand::FilterBlocked, FixedKey::Character('b')),
    KeyBinding::unlisted(NavigatorCommand::FilterWorking, FixedKey::Character('w')),
    KeyBinding::unlisted(NavigatorCommand::FilterIdle, FixedKey::Character('i')),
];

const NAVIGATOR_SEARCH_BINDINGS: &[KeyBinding<NavigatorCommand>] = &[
    KeyBinding::unlisted(
        NavigatorCommand::BackOrClose,
        code(KeyCode::Esc, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Open,
        code(KeyCode::Enter, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveUp,
        code(KeyCode::Up, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveUp, control('p', CONTROL_ONLY)),
    KeyBinding::unlisted(
        NavigatorCommand::MoveDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveDown, control('n', CONTROL_ONLY)),
];

fn navigator_command_for_main(key: &TerminalKey) -> Option<NavigatorCommand> {
    command_for(NAVIGATOR_MAIN_BINDINGS, key)
}

fn navigator_command_for_search(key: &TerminalKey) -> Option<NavigatorCommand> {
    command_for(NAVIGATOR_SEARCH_BINDINGS, key)
}

fn navigator_footer(search_focused: bool) -> &'static str {
    if search_focused {
        " search type · move ↑↓/ctrl+n/p · open enter · back esc"
    } else {
        // Filters cover all agents or one of the three agent states; Ctrl+D pages by eight.
        " ↑↓/j/k rows · ←→ workspace · / search · a/b/w/i filter · enter open · esc close"
    }
}

pub(in crate::shell) type ClientNavigatorFilter = shepr_protocol::AgentStatus;

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientNavigatorRow {
    pub(in crate::shell) depth: u8,
    pub(in crate::shell) label: String,
    pub(in crate::shell) meta: String,
    pub(in crate::shell) detail: String,
    pub(in crate::shell) agent: Option<shepr_config::ConfigAgent>,
    pub(in crate::shell) status: Option<shepr_protocol::AgentStatus>,
    pub(in crate::shell) stale: bool,
    pub(in crate::shell) current: bool,
    pub(in crate::shell) target: Location,
}

#[derive(Debug, Default)]
pub(in crate::shell) struct NavigatorOverlay {
    pub(in crate::shell) query: TextEditor,
    pub(in crate::shell) search_focused: bool,
    pub(in crate::shell) selected: Option<Location>,
    /// The first row the last frame showed.
    pub(in crate::shell) scroll: usize,
    pub(in crate::shell) filter: Option<ClientNavigatorFilter>,
    /// The grab offset of a scrollbar drag in progress, held until its release.
    drag: Option<u16>,
}

impl NavigatorOverlay {
    /// Moves the selection by `delta` rows, clamped to the list.
    pub(super) fn move_selection(&mut self, rows: &[ClientNavigatorRow], delta: isize) {
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        let selected = navigator_selected_index(rows, self).unwrap_or(0);
        let max_index = rows.len().saturating_sub(1);
        let next = selected
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
        self.selected = Some(rows[next].target.clone());
    }

    /// Scrolls the list to `start`, clamped to the drawn list's range, and keeps the selection
    /// inside the new viewport so the next frame does not snap back to it.
    fn scroll_to(&mut self, start: usize, drawn: ListScroll, rows: &[ClientNavigatorRow]) {
        let start = start.min(drawn.max_start());
        self.scroll = start;
        let last_visible = start + drawn.viewport_rows().saturating_sub(1);
        let selected = navigator_selected_index(rows, self)
            .unwrap_or(0)
            .max(start)
            .min(last_visible);
        self.selected = rows.get(selected).map(|row| row.target.clone());
    }

    /// Selects the first pane of the next (or previous) workspace section.
    fn move_workspace(&mut self, rows: &[ClientNavigatorRow], forward: bool) {
        let Some(selected) = navigator_selected_index(rows, self) else {
            return;
        };
        let section = rows[..=selected]
            .iter()
            .rposition(|row| !matches!(row.target.target, LocationTarget::Pane(_)))
            .unwrap_or(selected);
        let mut destinations = rows.windows(2).enumerate().filter(|(index, pair)| {
            matches!(pair[0].target.target, LocationTarget::Workspace(_))
                && matches!(pair[1].target.target, LocationTarget::Pane(_))
                && if forward {
                    *index > section
                } else {
                    *index < section
                }
        });
        let destination = if forward {
            destinations.next()
        } else {
            destinations.next_back()
        };
        if let Some((_, pair)) = destination {
            self.selected = Some(pair[1].target.clone());
        }
    }

    pub(super) fn layout(
        &self,
        screen: Rect,
        ctx: &OverlayContext<'_>,
    ) -> Option<(NavigatorView, OverlayScroll)> {
        let q = popup(
            screen,
            MAX_NAVIGATOR_OVERLAY_WIDTH,
            MAX_NAVIGATOR_OVERLAY_HEIGHT,
        )?;
        if q.height < MIN_NAVIGATOR_OVERLAY_HEIGHT {
            return None;
        }
        let i = panel_inner(q)?;
        let rows = ctx.navigator_index.rows(ctx.active_endpoint_id, self);
        let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(5));
        let selected = navigator_selected_index(&rows, self).unwrap_or(0);
        let max = rows.len().saturating_sub(body.height as usize);
        let scroll = self
            .scroll
            .max(selected.saturating_sub(body.height.saturating_sub(1) as usize))
            .min(selected)
            .min(max);
        let metrics = ListScroll::new(scroll, max, usize::from(body.height));
        let scrollbar = (max > 0 && body.width > 1).then_some(Rect::new(
            body.right() - 1,
            body.y,
            1,
            body.height,
        ));
        let row_width = body.width.saturating_sub(u16::from(scrollbar.is_some()));
        let slots = (scroll..rows.len())
            .take(body.height as usize)
            .map(|ix| NavigatorSlot {
                rect: Rect::new(
                    body.x,
                    body.y + u16::try_from(ix - scroll).unwrap_or(u16::MAX),
                    row_width,
                    1,
                ),
                row: ix,
            })
            .collect();
        Some((
            NavigatorView {
                popup: q,
                inner: i,
                search: Rect::new(i.x, i.y, i.width, 1),
                body,
                rows,
                selected,
                list: ListView {
                    body,
                    scroll: metrics,
                    scrollbar,
                    slots,
                },
            },
            OverlayScroll::Navigator(scroll),
        ))
    }

    pub(super) fn draw(
        &self,
        b: &mut Buffer,
        view: &NavigatorView,
        ctx: &OverlayContext<'_>,
    ) -> OverlayPaint {
        let p = ctx.palette;
        let q = view.popup;
        let i = view.inner;
        let rows = &view.rows;
        let body = view.body;
        let selected = view.selected;
        panel(b, q, p.accent, p.panel_bg);
        put_text(
            b,
            q.x + 2,
            q.y,
            q.width.saturating_sub(4),
            " Go to ",
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
        let search = if self.search_focused {
            " / ".to_owned()
        } else if let Some(f) = self.filter {
            format!(" / {}", f.label())
        } else if self.query.as_str().is_empty() {
            " / search agents and terminals".to_owned()
        } else {
            format!(" / {}", self.query.as_str())
        };
        let terminal_count = rows
            .iter()
            .filter(|row| matches!(row.target.target, LocationTarget::Pane(_)))
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
                .fg(if self.search_focused {
                    p.text
                } else {
                    p.overlay0
                })
                .bg(p.panel_bg),
        );
        let cursor = if self.search_focused {
            text_editor::render(
                b,
                Rect::new(
                    i.x + 3,
                    i.y,
                    i.width.saturating_sub(4 + display_width(&count)),
                    1,
                ),
                &self.query,
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
        for slot in &view.list.slots {
            let ix = slot.row;
            let rect = slot.rect;
            let Some(r) = rows.get(ix) else {
                continue;
            };
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
                    .fg(if matches!(r.target.target, LocationTarget::Machine) {
                        p.subtext0
                    } else {
                        p.text
                    })
                    .bg(p.panel_bg)
            };
            let is_pane = matches!(r.target.target, LocationTarget::Pane(_));
            let connector = if !is_pane {
                ""
            } else if rows
                .get(ix + 1)
                .is_some_and(|next| matches!(next.target.target, LocationTarget::Pane(_)))
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
                        r.agent.map_or("terminal", shepr_config::ConfigAgent::label),
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
            let machine_status = if matches!(r.target.target, LocationTarget::Machine)
                && !r.target.endpoint.is_local()
            {
                ctx.navigator_index.endpoint_status(&r.target.endpoint)
            } else {
                None
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
        if let Some(track) = view.list.scrollbar {
            crate::shell::view::list::render_scrollbar_buffer(
                b,
                view.list.scroll,
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
        put_text(
            b,
            i.x,
            i.bottom() - 1,
            i.width,
            navigator_footer(self.search_focused),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
        OverlayPaint {
            opaque: vec![q],
            backdrop: false,
            cursor,
        }
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey, ctx: &OverlayContext<'_>) -> OverlayEffect {
        let rows = |overlay: &Self| ctx.navigator_index.rows(ctx.active_endpoint_id, overlay);
        let search_focused = self.search_focused;
        let command = if search_focused {
            navigator_command_for_search(key)
        } else {
            navigator_command_for_main(key)
        };
        match command {
            Some(NavigatorCommand::BackOrClose) => {
                return if search_focused {
                    self.search_focused = false;
                    OverlayEffect::Changed
                } else {
                    OverlayEffect::Close
                };
            }
            Some(NavigatorCommand::Open) => {
                return match selected_navigator_target(&rows(self), self) {
                    Some(target) => OverlayEffect::Command(OverlayCommand::OpenTarget(target)),
                    None => OverlayEffect::Unchanged,
                };
            }
            _ => {}
        }
        if search_focused {
            let edit = self.query.handle_key(key);
            if edit.is_handled() {
                if edit.changed() {
                    self.filter = None;
                    self.selected = None;
                }
                return OverlayEffect::Changed;
            }
            return match command {
                Some(NavigatorCommand::MoveUp) => {
                    self.move_selection(&rows(self), -1);
                    OverlayEffect::Changed
                }
                Some(NavigatorCommand::MoveDown) => {
                    self.move_selection(&rows(self), 1);
                    OverlayEffect::Changed
                }
                _ => OverlayEffect::Unchanged,
            };
        }
        match command {
            Some(NavigatorCommand::MoveWorkspaceLeft) => {
                self.move_workspace(&rows(self), false);
            }
            Some(NavigatorCommand::MoveWorkspaceRight) => {
                self.move_workspace(&rows(self), true);
            }
            Some(NavigatorCommand::ClearFilter) => {
                if self.filter.take().is_some() {
                    self.selected = None;
                }
            }
            Some(NavigatorCommand::Top) => {
                self.selected = None;
                self.scroll = 0;
            }
            Some(NavigatorCommand::Bottom) => {
                self.selected = rows(self).last().map(|row| row.target.clone());
            }
            Some(NavigatorCommand::Search) => {
                self.search_focused = true;
                self.filter = None;
            }
            Some(NavigatorCommand::MoveUp) => self.move_selection(&rows(self), -1),
            Some(NavigatorCommand::MoveDown) => self.move_selection(&rows(self), 1),
            Some(NavigatorCommand::PageDown) => self.move_selection(&rows(self), 8),
            Some(NavigatorCommand::PageUp) => self.move_selection(&rows(self), -8),
            Some(
                command @ (NavigatorCommand::FilterBlocked
                | NavigatorCommand::FilterWorking
                | NavigatorCommand::FilterIdle
                | NavigatorCommand::FilterAll),
            ) => {
                self.query.clear();
                self.filter = match command {
                    NavigatorCommand::FilterBlocked => Some(ClientNavigatorFilter::Blocked),
                    NavigatorCommand::FilterWorking => Some(ClientNavigatorFilter::Working),
                    NavigatorCommand::FilterIdle => Some(ClientNavigatorFilter::Idle),
                    _ => None,
                };
                self.selected = None;
            }
            Some(NavigatorCommand::BackOrClose | NavigatorCommand::Open) | None => {
                return OverlayEffect::Unchanged;
            }
        }
        OverlayEffect::Changed
    }

    /// Hover, click, wheel and the scrollbar (press, drag, release). A press outside the popup
    /// closes. A press while a drag is recorded means its release was lost, so the drag is
    /// dropped first, the way the shell settles a chrome drag.
    pub(super) fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&NavigatorView>,
        ctx: &OverlayContext<'_>,
    ) -> OverlayEffect {
        let point = (mouse.column, mouse.row);
        let row_hit = || {
            view.and_then(|view| {
                view.list
                    .slots
                    .iter()
                    .find(|slot| contains(slot.rect, point))
                    .and_then(|slot| view.rows.get(slot.row))
                    .map(|row| row.target.clone())
            })
        };
        match mouse.kind {
            MouseEventKind::Moved => match row_hit() {
                Some(target) => {
                    self.selected = Some(target);
                    OverlayEffect::Changed
                }
                None => OverlayEffect::Unchanged,
            },
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                let scrollbar =
                    view.and_then(|view| view.list.scrollbar.map(|track| (view, track)));
                if let Some((view, track)) = scrollbar.filter(|(_, track)| contains(*track, point))
                {
                    let metrics = view.list.scroll;
                    if let Some(grab_row_offset) = shepr_term::scroll::scrollbar_thumb_grab_offset(
                        metrics,
                        crate::shell::view::list::scroll_track(track),
                        mouse.row,
                    ) {
                        self.drag = Some(grab_row_offset);
                        OverlayEffect::Unchanged
                    } else {
                        let offset = shepr_term::scroll::scrollbar_start_from_row(
                            metrics,
                            crate::shell::view::list::scroll_track(track),
                            mouse.row,
                        );
                        self.scroll_to(offset, metrics, &view.rows);
                        OverlayEffect::Changed
                    }
                } else if view.is_some_and(|view| contains(view.search, point)) {
                    self.search_focused = true;
                    self.filter = None;
                    OverlayEffect::Changed
                } else if let Some(target) = row_hit() {
                    self.selected = Some(target.clone());
                    OverlayEffect::Command(OverlayCommand::OpenTarget(target))
                } else if !view.is_some_and(|view| contains(view.popup, point)) {
                    OverlayEffect::Close
                } else {
                    OverlayEffect::Unchanged
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (Some(grab_row_offset), Some(view)) = (self.drag, view) else {
                    return OverlayEffect::Unchanged;
                };
                let Some(track) = view.list.scrollbar else {
                    return OverlayEffect::Unchanged;
                };
                let offset = shepr_term::scroll::scrollbar_start_from_drag_row(
                    view.list.scroll,
                    crate::shell::view::list::scroll_track(track),
                    mouse.row,
                    grab_row_offset,
                );
                self.scroll_to(offset, view.list.scroll, &view.rows);
                OverlayEffect::Changed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                OverlayEffect::Unchanged
            }
            MouseEventKind::ScrollUp => {
                let rows = ctx.navigator_index.rows(ctx.active_endpoint_id, self);
                self.move_selection(&rows, -OVERLAY_WHEEL_SCROLL_ROWS);
                OverlayEffect::Changed
            }
            MouseEventKind::ScrollDown => {
                let rows = ctx.navigator_index.rows(ctx.active_endpoint_id, self);
                self.move_selection(&rows, OVERLAY_WHEEL_SCROLL_ROWS);
                OverlayEffect::Changed
            }
            _ => OverlayEffect::Unchanged,
        }
    }

    pub(super) fn on_text(&mut self, text: &str) -> bool {
        if !self.search_focused {
            return false;
        }
        if self.query.insert(text) {
            self.filter = None;
            self.selected = None;
        }
        true
    }

    pub(super) fn accepts_modal_paste(&self) -> bool {
        self.search_focused
    }
}

#[cfg(test)]
mod tests;
