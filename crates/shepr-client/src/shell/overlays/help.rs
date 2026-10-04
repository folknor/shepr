//! The keybind help: its keys, text, layout, drawing and scrollbar drag. Typed and pasted
//! text land in its search editor; input content must stay out of logs and error messages
//! here.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use shepr_config::LiveKeybindConfig;
use shepr_config::theme::Palette;
use shepr_term::key::TerminalKey;
use shepr_term::scroll::ListScroll;
use shepr_termio::input::fixed_keys::{FixedKey, KeyBinding, ModifierMatch, command_for};
use shepr_termio::text_editor::TextEditor;

use super::widgets::{button, panel, panel_inner, popup};
use super::{HelpView, OverlayContext, OverlayEffect, OverlayPaint, OverlayScroll, text_editor};
use crate::limits::{
    MAX_HELP_OVERLAY_HEIGHT, MAX_HELP_OVERLAY_WIDTH, MIN_HELP_OVERLAY_INNER_HEIGHT,
    MIN_HELP_OVERLAY_INNER_WIDTH, OVERLAY_WHEEL_SCROLL_ROWS,
};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::put_text;

// The Help footer is written out instead of derived from these tables, unlike the copy-mode
// and resize mode bars. It groups keys more compactly than one label per binding can and
// lists only the keys worth naming, so the trailing close hint still fits a narrow overlay.
// The tests below check that every key the footer names routes to its command.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelpCommand {
    Close,
    Back,
    Search,
    Top,
    Bottom,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Edit,
}

const fn code(code: KeyCode, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::Code(code, modifiers)
}

const fn control(character: char, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::ControlCharacter(character, modifiers)
}

const CONTROL_ONLY: ModifierMatch = ModifierMatch::Exact(KeyModifiers::CONTROL);

const HELP_MAIN_BINDINGS: &[KeyBinding<HelpCommand>] = &[
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Enter, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Esc, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Top, code(KeyCode::Home, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Bottom, code(KeyCode::End, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::ScrollUp, code(KeyCode::Up, ModifierMatch::Any)),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    // Help scrolling accepts j and k with any modifiers, including Ctrl+J,
    // which raw-mode legacy input reports for LF.
    KeyBinding::unlisted(
        HelpCommand::ScrollUp,
        code(KeyCode::Char('k'), ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Char('j'), ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageUp,
        code(KeyCode::PageUp, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageDown,
        code(KeyCode::PageDown, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::Search, FixedKey::Character('/')),
    KeyBinding::unlisted(HelpCommand::Close, FixedKey::Character('?')),
];

const HELP_SEARCH_BINDINGS: &[KeyBinding<HelpCommand>] = &[
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Enter, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Back, code(KeyCode::Esc, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::ScrollUp, code(KeyCode::Up, ModifierMatch::Any)),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::ScrollUp, control('p', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::ScrollDown, control('n', CONTROL_ONLY)),
    KeyBinding::unlisted(
        HelpCommand::PageUp,
        code(KeyCode::PageUp, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageDown,
        code(KeyCode::PageDown, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::Left, ModifierMatch::Empty)),
    KeyBinding::unlisted(
        HelpCommand::Edit,
        code(KeyCode::Right, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::Home, ModifierMatch::Empty)),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::End, ModifierMatch::Empty)),
    KeyBinding::unlisted(HelpCommand::Edit, control('u', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::Edit, control('k', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::Edit, control('y', CONTROL_ONLY)),
];

fn help_command_for_main(key: &TerminalKey) -> Option<HelpCommand> {
    command_for(HELP_MAIN_BINDINGS, key)
}

fn help_command_for_search(key: &TerminalKey) -> Option<HelpCommand> {
    command_for(HELP_SEARCH_BINDINGS, key)
}

fn help_footer(search_focused: bool) -> &'static str {
    if search_focused {
        " edit ←→/home/end · kill ^u/^k · yank ^y · scroll ↑↓ · back esc"
    } else {
        " search / · scroll j/k/↑↓/pgup/pgdn · close esc/enter"
    }
}

fn help_lines(keybinds: &LiveKeybindConfig, query: &str, palette: &Palette) -> Vec<Line<'static>> {
    let groups = shepr_termio::input::filter_keybind_help_groups(
        shepr_termio::input::keybind_help_groups(&keybinds.keybinds, keybinds.prefix),
        query,
    );
    let key_width = groups
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|row| row.keys.chars().count()))
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
        for shepr_termio::input::HelpRow { keys, label } in entries {
            let padded_key = format!(" {keys:<key_width$} ");
            lines.push(Line::from(vec![
                Span::styled(
                    padded_key,
                    Style::default()
                        .fg(palette.mauve)
                        .bg(palette.panel_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.to_owned(),
                    Style::default().fg(palette.text).bg(palette.panel_bg),
                ),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines
}

#[derive(Debug, Default)]
pub(in crate::shell) struct HelpOverlay {
    query: TextEditor,
    search_focused: bool,
    /// Written back by `Overlay::commit_scroll` with the scroll a frame resolved.
    pub(super) scroll: usize,
    /// The grab offset of a scrollbar drag in progress, held until its release.
    drag: Option<u16>,
}

impl HelpOverlay {
    /// Lays Help out and resolves its scroll: the stored scroll clamped to the drawn range.
    pub(super) fn layout(
        &self,
        screen: Rect,
        ctx: &OverlayContext<'_>,
    ) -> Option<(HelpView, OverlayScroll)> {
        let q = popup(screen, MAX_HELP_OVERLAY_WIDTH, MAX_HELP_OVERLAY_HEIGHT)?;
        let i = panel_inner(q)?;
        if i.width < MIN_HELP_OVERLAY_INNER_WIDTH || i.height < MIN_HELP_OVERLAY_INNER_HEIGHT {
            return None;
        }
        let close = Rect::new(i.right() - 13, i.y, 13, 1);
        let body = Rect::new(i.x, i.y + 3, i.width, i.height.saturating_sub(5));
        // The scroll range counts rows with the same word wrapper that draws them.
        let paragraph = Paragraph::new(help_lines(ctx.keybinds, self.query.as_str(), ctx.palette))
            .wrap(Wrap { trim: false });
        let viewport_rows = usize::from(body.height.max(1));
        let needs_scrollbar = paragraph.line_count(body.width) > viewport_rows;
        let text_area = if needs_scrollbar {
            Rect::new(body.x, body.y, body.width.saturating_sub(1), body.height)
        } else {
            body
        };
        let total_rows = paragraph.line_count(text_area.width);
        let max_scroll = total_rows.saturating_sub(viewport_rows);
        let scroll = self.scroll.min(max_scroll);
        let scrollbar = needs_scrollbar.then_some(Rect::new(
            body.right().saturating_sub(1),
            body.y,
            1,
            body.height,
        ));
        Some((
            HelpView {
                popup: q,
                inner: i,
                close,
                text_area,
                scroll: ListScroll::new(scroll, max_scroll, viewport_rows),
                scrollbar,
            },
            OverlayScroll::Help(scroll),
        ))
    }

    pub(super) fn draw(
        &self,
        b: &mut Buffer,
        view: &HelpView,
        ctx: &OverlayContext<'_>,
    ) -> OverlayPaint {
        let p = ctx.palette;
        let q = view.popup;
        let i = view.inner;
        panel(b, q, p.accent, p.panel_bg);
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
        button(
            b,
            view.close,
            if self.search_focused {
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
            &if self.search_focused {
                " / ".to_owned()
            } else {
                " / press / to filter by command or shortcut".to_owned()
            },
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
                Rect::new(i.x + 3, sy, i.width.saturating_sub(3), 1),
                &self.query,
                Style::default().fg(p.text).bg(p.panel_bg),
            )
        } else {
            None
        };

        let paragraph = Paragraph::new(help_lines(ctx.keybinds, self.query.as_str(), p))
            .wrap(Wrap { trim: false });
        Widget::render(
            paragraph.scroll((u16::try_from(view.scroll.start()).unwrap_or(u16::MAX), 0)),
            view.text_area,
            b,
        );
        if let Some(track) = view.scrollbar {
            crate::shell::view::list::render_scrollbar_buffer(
                b,
                view.scroll,
                track,
                "▐",
                Style::default().fg(p.overlay0).bg(p.panel_bg),
                "▐",
                Style::default().fg(p.overlay1).bg(p.panel_bg),
            );
        }

        put_text(
            b,
            i.x,
            i.bottom() - 1,
            i.width,
            help_footer(self.search_focused),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
        OverlayPaint {
            opaque: vec![q],
            backdrop: true,
            cursor,
        }
    }

    /// Scrolls by `delta` rows, clamped to the drawn range when a Help view is on screen. Without
    /// one the value stays unclamped, and the next resolution clamps it.
    fn scroll_by(&mut self, delta: isize, view: Option<&HelpView>) {
        let next = self.scroll.saturating_add_signed(delta);
        self.scroll = view.map_or(next, |view| next.min(view.scroll.max_start()));
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey, view: Option<&HelpView>) -> OverlayEffect {
        if self.search_focused {
            let command = help_command_for_search(key);
            if command == Some(HelpCommand::Edit) || command.is_none() {
                let edit = self.query.handle_key(key);
                if edit.is_handled() {
                    if edit.changed() {
                        self.scroll = 0;
                    }
                    return OverlayEffect::Changed;
                }
            }
            match command {
                Some(HelpCommand::Back) => {
                    self.search_focused = false;
                    self.query.clear();
                    self.scroll = 0;
                }
                Some(HelpCommand::Close) => return OverlayEffect::Close,
                Some(HelpCommand::ScrollUp) => self.scroll_by(-1, view),
                Some(HelpCommand::ScrollDown) => self.scroll_by(1, view),
                Some(HelpCommand::PageUp) => self.scroll_by(-8, view),
                Some(HelpCommand::PageDown) => self.scroll_by(8, view),
                Some(
                    HelpCommand::Edit
                    | HelpCommand::Search
                    | HelpCommand::Top
                    | HelpCommand::Bottom,
                )
                | None => {}
            }
            return OverlayEffect::Changed;
        }
        match help_command_for_main(key) {
            Some(HelpCommand::Close) => return OverlayEffect::Close,
            Some(HelpCommand::Top) => self.scroll = 0,
            // Without a drawn Help there is no range to scroll to; the next resolution clamps
            // whatever is stored.
            Some(HelpCommand::Bottom) => {
                self.scroll = view.map_or(usize::MAX, |view| view.scroll.max_start());
            }
            Some(HelpCommand::ScrollUp) => self.scroll_by(-1, view),
            Some(HelpCommand::ScrollDown) => self.scroll_by(1, view),
            Some(HelpCommand::PageUp) => self.scroll_by(-8, view),
            Some(HelpCommand::PageDown) => self.scroll_by(8, view),
            Some(HelpCommand::Search) => {
                self.search_focused = true;
                self.scroll = 0;
            }
            Some(HelpCommand::Back | HelpCommand::Edit) | None => {}
        }
        OverlayEffect::Changed
    }

    /// Wheel scrolling, the scrollbar (press, drag, release), the close button, and a press
    /// outside the popup, which closes. A press while a drag is recorded means its release was
    /// lost, so the drag is dropped first, the way the shell settles a chrome drag.
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, view: Option<&HelpView>) -> OverlayEffect {
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let before = self.scroll;
                self.scroll = self
                    .scroll
                    .saturating_sub(OVERLAY_WHEEL_SCROLL_ROWS.unsigned_abs());
                changed_if(self.scroll != before)
            }
            MouseEventKind::ScrollDown => {
                let before = self.scroll;
                self.scroll_by(OVERLAY_WHEEL_SCROLL_ROWS, view);
                changed_if(self.scroll != before)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                let scrollbar = view.and_then(|view| view.scrollbar.map(|track| (view, track)));
                if let Some((view, track)) = scrollbar.filter(|(_, track)| contains(*track, point))
                {
                    let metrics = view.scroll;
                    if let Some(grab_row_offset) = shepr_term::scroll::scrollbar_thumb_grab_offset(
                        metrics,
                        crate::shell::view::list::scroll_track(track),
                        mouse.row,
                    ) {
                        self.drag = Some(grab_row_offset);
                        OverlayEffect::Unchanged
                    } else {
                        self.scroll = shepr_term::scroll::scrollbar_start_from_row(
                            metrics,
                            crate::shell::view::list::scroll_track(track),
                            mouse.row,
                        );
                        OverlayEffect::Changed
                    }
                } else if view.is_some_and(|view| contains(view.close, point)) {
                    if self.search_focused {
                        self.search_focused = false;
                        self.query.clear();
                        self.scroll = 0;
                        OverlayEffect::Changed
                    } else {
                        OverlayEffect::Close
                    }
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
                let Some(track) = view.scrollbar else {
                    return OverlayEffect::Unchanged;
                };
                let next = shepr_term::scroll::scrollbar_start_from_drag_row(
                    view.scroll,
                    crate::shell::view::list::scroll_track(track),
                    mouse.row,
                    grab_row_offset,
                );
                if next == self.scroll {
                    OverlayEffect::Unchanged
                } else {
                    self.scroll = next;
                    OverlayEffect::Changed
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                OverlayEffect::Unchanged
            }
            _ => OverlayEffect::Unchanged,
        }
    }

    pub(super) fn on_text(&mut self, text: &str) -> bool {
        if !self.search_focused {
            return false;
        }
        if self.query.insert(text) {
            self.scroll = 0;
        }
        true
    }

    pub(super) fn accepts_modal_paste(&self) -> bool {
        self.search_focused
    }
}

fn changed_if(changed: bool) -> OverlayEffect {
    if changed {
        OverlayEffect::Changed
    } else {
        OverlayEffect::Unchanged
    }
}

#[cfg(test)]
impl HelpOverlay {
    pub(in crate::shell) fn query(&self) -> &TextEditor {
        &self.query
    }

    pub(in crate::shell) fn search_focused(&self) -> bool {
        self.search_focused
    }

    pub(in crate::shell) fn scroll(&self) -> usize {
        self.scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> TerminalKey {
        TerminalKey::new(code, KeyModifiers::NONE)
    }

    fn ctrl(character: char) -> TerminalKey {
        TerminalKey::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn char_key(character: char) -> TerminalKey {
        key(KeyCode::Char(character))
    }

    fn view(start: usize, max_start: usize) -> HelpView {
        HelpView {
            popup: Rect::new(0, 0, 40, 20),
            inner: Rect::new(1, 1, 38, 18),
            close: Rect::new(26, 1, 13, 1),
            text_area: Rect::new(1, 4, 37, 12),
            scroll: ListScroll::new(start, max_start, 12),
            scrollbar: Some(Rect::new(38, 4, 1, 12)),
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn every_key_the_help_footers_name_routes_to_its_command() {
        use HelpCommand as C;
        let main = [
            (char_key('/'), C::Search),
            (char_key('j'), C::ScrollDown),
            (char_key('k'), C::ScrollUp),
            (key(KeyCode::Up), C::ScrollUp),
            (key(KeyCode::Down), C::ScrollDown),
            (key(KeyCode::PageUp), C::PageUp),
            (key(KeyCode::PageDown), C::PageDown),
            (key(KeyCode::Esc), C::Close),
            (key(KeyCode::Enter), C::Close),
        ];
        for (pressed, command) in main {
            assert_eq!(help_command_for_main(&pressed), Some(command));
        }
        let search = [
            (key(KeyCode::Left), C::Edit),
            (key(KeyCode::Right), C::Edit),
            (key(KeyCode::Home), C::Edit),
            (key(KeyCode::End), C::Edit),
            (ctrl('u'), C::Edit),
            (ctrl('k'), C::Edit),
            (ctrl('y'), C::Edit),
            (key(KeyCode::Up), C::ScrollUp),
            (key(KeyCode::Down), C::ScrollDown),
            (key(KeyCode::Esc), C::Back),
        ];
        for (pressed, command) in search {
            assert_eq!(help_command_for_search(&pressed), Some(command));
        }
    }

    #[test]
    fn scroll_keys_clamp_to_the_drawn_view() {
        let drawn = view(0, 5);
        let mut help = HelpOverlay::default();
        for _ in 0..10 {
            help.on_key(&key(KeyCode::Down), Some(&drawn));
        }
        assert_eq!(help.scroll, 5);
        help.on_key(&key(KeyCode::PageDown), Some(&drawn));
        assert_eq!(help.scroll, 5);
        help.on_key(&key(KeyCode::Home), Some(&drawn));
        assert_eq!(help.scroll, 0);
        help.on_key(&key(KeyCode::End), Some(&drawn));
        assert_eq!(help.scroll, 5);

        // With no Help drawn there is no range: the value is stored as pressed and the next
        // layout clamps it.
        let mut help = HelpOverlay::default();
        for _ in 0..10 {
            help.on_key(&key(KeyCode::Down), None);
        }
        assert_eq!(help.scroll, 10);
        help.on_key(&key(KeyCode::End), None);
        assert_eq!(help.scroll, usize::MAX);
    }

    #[test]
    fn a_press_drops_a_lost_scrollbar_drag() {
        let drawn = view(0, 5);
        let mut help = HelpOverlay {
            drag: Some(2),
            ..HelpOverlay::default()
        };
        // Inside the popup, off the scrollbar and the close button.
        let effect = help.on_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 10),
            Some(&drawn),
        );
        assert!(matches!(effect, OverlayEffect::Unchanged));
        assert_eq!(help.drag, None);

        // A drag with no recorded grab moves nothing.
        let effect = help.on_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 38, 14),
            Some(&drawn),
        );
        assert!(matches!(effect, OverlayEffect::Unchanged));
        assert_eq!(help.scroll, 0);
    }
}
