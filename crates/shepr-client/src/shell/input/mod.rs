use crate::shell::overlays::Overlay;
use crossterm::event::KeyEventKind;
pub(in crate::shell) mod events;
pub(in crate::shell) mod hit_test;
mod mouse;
pub(in crate::shell) mod pointer;
pub(in crate::shell) mod scroll_lanes;
pub(in crate::shell) mod selection;
mod word_bounds;
mod word_selection;

use crate::shell::state::{
    ClientInputContext, ClientShellInput, ClientShellMode, ClientShellRequest, ClientShellState,
};
use shepr_protocol::ClientMessage;

use crate::shell::input::events::PaneInputBatchAccounting;

use crate::input_wire::WirePaneInput;
use crossterm::event::{KeyCode, KeyModifiers};
use shepr_protocol::ClientPaneInputEvent;
use shepr_termio::input::fixed_keys::{self, FixedKey, KeyBinding, ModifierMatch};
use shepr_termio::input::raw_input::RawInputEvent;

use crate::limits::{
    CLIPBOARD_RESULT_QUEUE_CAPACITY, MAX_COPY_INPUT_QUEUE, MODAL_PASTE_CLIPBOARD_TIMEOUT,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResizeCommand {
    Finish,
    Left,
    Down,
    Up,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum ResizeHelpGroup {
    Width,
    Height,
    Finish,
}

const fn resize_binding(
    command: ResizeCommand,
    code: KeyCode,
    help_group: Option<ResizeHelpGroup>,
) -> KeyBinding<ResizeCommand, ResizeHelpGroup> {
    KeyBinding {
        command,
        key: FixedKey::RawCode(code, ModifierMatch::Any),
        help_group,
    }
}

// The mode bar names one key per action; arrows and Enter stay unlisted.
const RESIZE_BINDINGS: &[KeyBinding<ResizeCommand, ResizeHelpGroup>] = &[
    resize_binding(
        ResizeCommand::Finish,
        KeyCode::Esc,
        Some(ResizeHelpGroup::Finish),
    ),
    resize_binding(ResizeCommand::Finish, KeyCode::Enter, None),
    resize_binding(
        ResizeCommand::Left,
        KeyCode::Char('h'),
        Some(ResizeHelpGroup::Width),
    ),
    resize_binding(ResizeCommand::Left, KeyCode::Left, None),
    resize_binding(
        ResizeCommand::Down,
        KeyCode::Char('j'),
        Some(ResizeHelpGroup::Height),
    ),
    resize_binding(ResizeCommand::Down, KeyCode::Down, None),
    resize_binding(
        ResizeCommand::Up,
        KeyCode::Char('k'),
        Some(ResizeHelpGroup::Height),
    ),
    resize_binding(ResizeCommand::Up, KeyCode::Up, None),
    resize_binding(
        ResizeCommand::Right,
        KeyCode::Char('l'),
        Some(ResizeHelpGroup::Width),
    ),
    resize_binding(ResizeCommand::Right, KeyCode::Right, None),
];

pub(in crate::shell) fn resize_help_keys(group: ResizeHelpGroup) -> String {
    fixed_keys::help_keys(RESIZE_BINDINGS, group, "/")
}

// limits-exempt: this fixed tag identifies the local input source in the lease table.
const LOCAL_INPUT_SOURCE: u8 = 0;

fn is_user_input(event: &RawInputEvent) -> bool {
    match event {
        RawInputEvent::Key(key) => key.kind != KeyEventKind::Release,
        RawInputEvent::Paste(_) => true,
        RawInputEvent::Mouse(mouse) => mouse.kind != crossterm::event::MouseEventKind::Moved,
        RawInputEvent::OuterFocusGained
        | RawInputEvent::OuterFocusLost
        | RawInputEvent::HostDefaultColor { .. }
        | RawInputEvent::HostPaletteColors { .. }
        | RawInputEvent::HostColorSchemeChanged(_)
        | RawInputEvent::HostCellSizeReport { .. }
        | RawInputEvent::Unsupported => false,
    }
}

fn is_retained_selection_copy_key(key: &shepr_term::key::TerminalKey) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

/// Set while a clipboard helper thread is still running, including one abandoned after a
/// timeout, so a hung helper is waited on once rather than once per keypress.
static CLIPBOARD_READ_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Reads the host clipboard for a modal paste without letting a hung helper stall the client.
///
/// Key routing runs on the client's event loop, so a clipboard owner that never answers
/// (e.g. `xclip -out` against an unresponsive X selection owner) would freeze rendering and
/// input for every pane. The native reader has no timeout and cannot be cancelled, so it runs
/// on its own thread. This wrapper waits only until `MODAL_PASTE_CLIPBOARD_TIMEOUT`; an
/// abandoned read keeps its thread (and helper process) until the helper exits, and later
/// pastes are skipped immediately until then.
fn read_clipboard_text_bounded() -> Option<String> {
    read_clipboard_text_bounded_with(
        &CLIPBOARD_READ_IN_FLIGHT,
        MODAL_PASTE_CLIPBOARD_TIMEOUT,
        shepr_platform::read_clipboard_text,
    )
}

fn read_clipboard_text_bounded_with(
    in_flight: &'static std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
    read: impl FnOnce() -> Option<String> + Send + 'static,
) -> Option<String> {
    use std::sync::atomic::Ordering;

    if in_flight.swap(true, Ordering::AcqRel) {
        tracing::warn!("an earlier clipboard read is still running; paste skipped");
        return None;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(CLIPBOARD_RESULT_QUEUE_CAPACITY);
    let spawned = std::thread::Builder::new()
        .name("shepr-clipboard-read".into())
        .spawn(move || {
            let text = read();
            in_flight.store(false, Ordering::Release);
            // The receiver is gone only after the wait below timed out, which already
            // logged the skipped paste; the late text is correctly dropped.
            sender.send(text).ok();
        });
    if let Err(error) = spawned {
        in_flight.store(false, Ordering::Release);
        tracing::warn!(%error, "could not start the clipboard reader; paste skipped");
        return None;
    }
    // The channel's timed wait is the deadline boundary here; this helper does not read or
    // compare a clock, so a separate clock seam would duplicate timeout behavior.
    match receiver.recv_timeout(timeout) {
        Ok(text) => text,
        Err(_) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "clipboard helper did not answer in time; paste skipped"
            );
            None
        }
    }
}

shepr_config::keybinding_rows! {
    $ define_navigate_actions;
    navigate(variant = $navigate_variant)
    => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum NavigateAction {
            $($navigate_variant,)*
        }
    }
}

fn resolve_navigate_binding(
    keybinds: &shepr_config::Keybinds,
    key: &shepr_term::key::TerminalKey,
) -> Option<NavigateAction> {
    shepr_config::keybinding_rows! {
        $ resolve_navigate;
        navigate(field = $navigate_field, variant = $navigate_variant)
        => {
            $(
                if keybinds.navigate.$navigate_field.matches_direct_key(key) {
                    return Some(NavigateAction::$navigate_variant);
                }
            )*
        }
    }
    None
}

fn is_modal_paste_shortcut(key: &shepr_term::key::TerminalKey) -> bool {
    key.generated_text.as_deref().is_none_or(str::is_empty)
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && key.modifiers.difference(KeyModifiers::SHIFT) == KeyModifiers::CONTROL
}

fn host_theme_update(event: &RawInputEvent) -> Option<shepr_protocol::ClientHostThemeUpdate> {
    use shepr_protocol::ClientHostThemeUpdate;

    match event {
        RawInputEvent::HostDefaultColor { kind, color } => {
            Some(ClientHostThemeUpdate::DefaultColor {
                kind: *kind,
                color: *color,
            })
        }
        RawInputEvent::HostPaletteColors { colors } => {
            Some(ClientHostThemeUpdate::PaletteColors(colors.clone()))
        }
        RawInputEvent::HostColorSchemeChanged(appearance) => {
            Some(ClientHostThemeUpdate::Appearance(*appearance))
        }
        _ => None,
    }
}

fn push_host_theme_update(
    requests: &mut Vec<ClientShellRequest>,
    update: shepr_protocol::ClientHostThemeUpdate,
) {
    if let shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors) = &update
        && let Some(ClientShellRequest::HostTheme(
            shepr_protocol::ClientHostThemeUpdate::PaletteColors(pending),
        )) = requests.last_mut()
        && pending.len() + colors.len() <= shepr_core::limits::PALETTE_COLOR_COUNT
    {
        pending.extend_from_slice(colors);
        return;
    }
    requests.push(ClientShellRequest::HostTheme(update));
}

impl ClientShellState {
    pub(crate) fn host_keyboard_report_all_requested(&self) -> bool {
        matches!(
            self.mode.kind(),
            ClientShellMode::Prefix | ClientShellMode::Navigate
        )
    }

    /// `host_reports_all_keys` is the host keyboard mode the input arrived
    /// under; it decides whether text key presses can hold input leases.
    pub(crate) fn handle_host_input(
        &mut self,
        inputs: Vec<crate::events::ParsedHostInput>,
        host_reports_all_keys: bool,
        now: std::time::Instant,
    ) -> ClientShellInput {
        self.now = now;
        self.host_reports_all_keys = host_reports_all_keys;
        let mut outcome = ClientShellInput::default();
        let mut accounting = PaneInputBatchAccounting::default();
        for input in inputs {
            if let Some(pixels) = input.pixel_mouse {
                let RawInputEvent::Mouse(mut mouse) = input.event else {
                    continue;
                };
                let Some((column, row)) = pixels.geometry.cell(pixels.x, pixels.y) else {
                    continue;
                };
                mouse.column = column;
                mouse.row = row;
                let previous = self.pointer.host_mouse_pixels.replace(pixels);
                self.handle_raw_event(
                    RawInputEvent::Mouse(mouse),
                    now,
                    &mut outcome,
                    &mut accounting,
                );
                self.pointer.host_mouse_pixels = previous;
            } else {
                self.handle_raw_event(input.event, now, &mut outcome, &mut accounting);
            }
        }
        outcome
    }

    fn dismiss_endpoint_error_for_input(
        &mut self,
        event: &RawInputEvent,
        outcome: &mut ClientShellInput,
    ) {
        if is_user_input(event) && self.endpoint_error.dismiss() {
            outcome.repaint = true;
        }
    }

    fn handle_raw_event(
        &mut self,
        event: RawInputEvent,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        self.dismiss_endpoint_error_for_input(&event, outcome);
        if self.handle_machine_badge_event(&event, outcome) {
            return;
        }
        if let Some(update) = host_theme_update(&event) {
            push_host_theme_update(&mut outcome.requests, update);
        }
        match event {
            RawInputEvent::Key(key) => self.handle_key(key, outcome, accounting),
            RawInputEvent::Paste(text) => {
                if self.prepare_committed_text(&text, outcome) {
                    return;
                }
                if self.insert_overlay_text(&text) {
                    outcome.repaint = true;
                } else if self.overlay.is_none() && self.mode.is(ClientShellMode::Terminal) {
                    self.push_focused_paste(text, outcome, accounting);
                }
            }
            RawInputEvent::Mouse(mouse) => {
                self.handle_mouse_with_accounting(mouse, now, outcome, accounting);
            }
            RawInputEvent::OuterFocusGained => {
                self.outer_focused = Some(true);
                outcome.query_host_appearance = true;
                if self.config.redraw_on_focus_gained {
                    outcome.repaint = true;
                    outcome.full_redraw = true;
                }
                outcome
                    .requests
                    .push(ClientShellRequest::Shown(ClientMessage::ClientShellFocus {
                        focused: true,
                    }));
            }
            RawInputEvent::OuterFocusLost => {
                self.outer_focused = Some(false);
                // A drag's release may or may not arrive once focus is gone,
                // so the drag stays recorded; a sidebar drag's owed resize and
                // persistence are done now in case it never does.
                self.settle_sidebar_drag_in_place(outcome);
                self.release_input_leases(outcome, accounting);
                outcome
                    .requests
                    .push(ClientShellRequest::Shown(ClientMessage::ClientShellFocus {
                        focused: false,
                    }));
            }
            RawInputEvent::HostDefaultColor {
                kind: shepr_term::host::DefaultColorKind::Background,
                color,
            } => {
                if self.host_theme.background != Some(color) {
                    self.host_theme.background = Some(color);
                    // The selection highlight follows the background, and so do
                    // the per-host sidebar colours.
                    self.refresh_host_pills();
                    outcome.repaint = true;
                }
            }
            RawInputEvent::HostDefaultColor {
                kind: shepr_term::host::DefaultColorKind::Foreground,
                color,
            } => {
                if self.host_theme.foreground != Some(color) {
                    self.host_theme.foreground = Some(color);
                    outcome.repaint |= self.refresh_host_pills();
                }
            }
            RawInputEvent::HostPaletteColors { colors } => {
                // The per-host colours read only the named ANSI slots, so the
                // rest of the 256 replies do not rederive them.
                let mut named_slot_changed = false;
                for (index, color) in colors {
                    let Some(slot) = self.host_theme.palette.get_mut(usize::from(index)) else {
                        continue;
                    };
                    if *slot != Some(color) {
                        *slot = Some(color);
                        named_slot_changed |= usize::from(index) < shepr_term::NAMED_COLOR_COUNT;
                    }
                }
                if named_slot_changed {
                    outcome.repaint |= self.refresh_host_pills();
                }
            }
            RawInputEvent::HostColorSchemeChanged(_) => {
                // A dark/light switch changes the host's default and palette
                // colours too. Re-query them so panes, the selection highlight
                // and the per-host sidebar colours (both from `host_theme`)
                // follow. The stdin framer arms itself for the replies
                // whenever it tracks scheme changes.
                outcome.query_host_theme = true;
            }
            RawInputEvent::HostCellSizeReport { .. } | RawInputEvent::Unsupported => {}
        }
    }

    fn prepare_committed_text(&mut self, text: &str, outcome: &mut ClientShellInput) -> bool {
        // Copy search owns text only in Copy mode, so Navigate never reaches it.
        if self.insert_copy_search_text(text) {
            outcome.repaint = true;
            return true;
        }
        self.mouse_selection.word_gesture = None;
        if self.copy_or_terminal_mode() != ClientShellMode::Copy {
            let had_selection = self.mouse_selection.selection.is_some();
            self.mouse_selection.clear_range();
            outcome.repaint |= had_selection;
        }
        false
    }

    pub(in crate::shell) fn handle_key(
        &mut self,
        key: shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        // Preserve input order through the outstanding read, including keys that interrupt copy
        // mode. Replaying the whole stream keeps Esc and the prefix behind the keys they follow.
        if self
            .copy
            .as_ref()
            .is_some_and(|session| session.pipeline().in_flight())
            && self.copy_mode_owns_input()
        {
            if let Some(session) = self.copy.as_mut()
                && session.pipeline().keys_len() < MAX_COPY_INPUT_QUEUE
            {
                session.pipeline_mut().push_key(key);
                return;
            }
            if !self.copy_mode_interrupt_key(&key) {
                self.set_endpoint_error(
                    "copy-mode input queue is full; later keys were ignored",
                    self.now,
                );
                outcome.repaint = true;
                return;
            }
            // A full queue means the outstanding read has stopped answering. Copy mode must
            // stay possible to leave, so an interrupt key abandons that read and the keys
            // queued behind it, then routes as if nothing were in flight.
            self.abandon_copy_operation();
            self.set_endpoint_error(
                "copy-mode input queue was full; queued keys were discarded",
                self.now,
            );
            outcome.repaint = true;
        }
        let lease_key = shepr_termio::input::InputLeaseKey::new(LOCAL_INPUT_SOURCE, &key);
        let host_reports_all_keys = self.host_reports_all_keys;
        self.input_leases
            .prepare_press(&lease_key, &key, host_reports_all_keys);
        match key.kind {
            KeyEventKind::Press => {
                let initial_context = self.input_context();
                let target = self.route_key_press(&key, outcome);
                if let Some(target) = target.as_ref() {
                    self.push_pane_key(*target, key.clone(), outcome, accounting);
                }
                let resulting_context = self.input_context();
                self.input_leases.complete_press(
                    lease_key,
                    &key,
                    Some(&initial_context),
                    Some(&resulting_context),
                    target,
                    host_reports_all_keys,
                );
            }
            KeyEventKind::Repeat => {
                let context = self.input_context();
                match self.input_leases.plan_repeat(lease_key, Some(&context)) {
                    shepr_termio::input::RepeatPlan::Forwarded(target) => {
                        self.push_pane_key(target, key, outcome, accounting);
                    }
                    shepr_termio::input::RepeatPlan::Reprocess => {
                        if let Some(target) = self.route_key_press(&key, outcome) {
                            self.push_pane_key(target, key, outcome, accounting);
                        }
                    }
                    shepr_termio::input::RepeatPlan::Ignore => {}
                }
            }
            KeyEventKind::Release => {
                if let Some(lease) = self.input_leases.remove_forwarded(&lease_key) {
                    let release = lease
                        .key
                        .with_modifiers(key.modifiers)
                        .with_kind(KeyEventKind::Release);
                    self.push_pane_key(lease.target, release, outcome, accounting);
                } else {
                    let _ = self.input_leases.remove(&lease_key);
                }
            }
        }
    }

    fn release_input_leases(
        &mut self,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        for lease in self.input_leases.remove_source(LOCAL_INPUT_SOURCE) {
            self.push_pane_key(
                lease.target,
                lease.key.with_kind(KeyEventKind::Release),
                outcome,
                accounting,
            );
        }
        if let Some(gesture) = self.pointer.pane_mouse_gesture.take() {
            let modifiers = gesture
                .last_event
                .modifiers
                .difference(gesture.stripped_modifiers);
            crate::shell::input::events::push_target_event(
                gesture.hit.pane_id,
                ClientPaneInputEvent::Mouse {
                    kind: shepr_protocol::ClientMouseKind::Up(
                        shepr_protocol::ClientMouseButton::from_host(gesture.button),
                    ),
                    position: gesture.last_position,
                    modifiers: shepr_protocol::WireModifiers::from_host(modifiers),
                    lines: self.config.mouse_scroll_lines,
                },
                outcome,
                accounting,
            );
        }
        if let Some(session) = self.copy.as_mut() {
            session.pipeline_mut().clear_keys();
        }
    }

    fn modal_paste_target_active(&self) -> bool {
        if self.copy_mode_owns_input()
            && self
                .copy
                .as_ref()
                .and_then(|copy_mode| copy_mode.search.as_ref())
                .is_some_and(|search| search.prompt.is_some())
        {
            return true;
        }
        self.overlay
            .as_ref()
            .is_some_and(Overlay::accepts_modal_paste)
    }

    /// The clipboard-paste shortcut, with the clipboard read passed in so tests can stand
    /// in for the host clipboard.
    fn handle_modal_paste_shortcut_with(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
        read_clipboard_text: impl FnOnce() -> Option<String>,
    ) -> bool {
        if !is_modal_paste_shortcut(key) || !self.modal_paste_target_active() {
            return false;
        }
        if let Some(text) = read_clipboard_text() {
            let inserted = self.insert_copy_search_text(&text) || self.insert_overlay_text(&text);
            outcome.repaint |= inserted;
        }
        true
    }

    fn route_key_press(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> Option<shepr_protocol::PublicPaneId> {
        if self.handle_modal_paste_shortcut_with(key, outcome, read_clipboard_text_bounded) {
            return None;
        }
        if self.overlay.is_some() {
            self.route_overlay_key(key, outcome);
            return None;
        }
        if matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        // Navigate mode takes only its own keys; every other key, the prefix
        // included, does nothing, not even clear or copy a mouse selection.
        if self.mode.is(ClientShellMode::Navigate) {
            self.route_navigate_key(key, outcome);
            return None;
        }
        self.mouse_selection.word_gesture = None;
        if self.copy_or_terminal_mode() != ClientShellMode::Copy
            && !self.config.copy_on_select
            && is_retained_selection_copy_key(key)
            && self
                .mouse_selection
                .selection
                .as_ref()
                .is_some_and(shepr_term::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome);
            self.mouse_selection.clear_range();
            outcome.repaint = true;
            return None;
        }
        if self.copy_or_terminal_mode() != ClientShellMode::Copy {
            let had_selection = self.mouse_selection.selection.is_some();
            self.mouse_selection.clear_range();
            outcome.repaint |= had_selection;
        }

        match self.mode.kind() {
            ClientShellMode::Terminal => {
                if let Some(binding) =
                    shepr_termio::input::resolve_direct_binding(&self.config.keybinds.keybinds, key)
                {
                    self.record_binding(&binding, outcome);
                    return None;
                }
                if self.config.keybinds.prefix.matches(key) {
                    self.mode.set(ClientShellMode::Prefix);
                    outcome.repaint = true;
                    return None;
                }
                self.focused_pane_id()
            }
            ClientShellMode::Prefix => {
                let return_mode = if self.copy.as_ref().is_some_and(|copy_mode| {
                    copy_mode.pane_is_focused(self.focused_pane_id().as_ref())
                }) {
                    ClientShellMode::Copy
                } else {
                    ClientShellMode::Terminal
                };
                if self.config.keybinds.prefix.matches(key) {
                    self.mode.set(return_mode);
                    outcome.repaint = true;
                    return self.focused_pane_id();
                }
                if key.code == KeyCode::Esc {
                    self.mode.set(return_mode);
                    outcome.repaint = true;
                    return None;
                }
                if let Some(binding) =
                    shepr_termio::input::resolve_prefix_binding(&self.config.keybinds.keybinds, key)
                {
                    self.mode.set(return_mode);
                    outcome.repaint = true;
                    self.record_binding(&binding, outcome);
                    return None;
                }
                self.mode.set(return_mode);
                outcome.repaint = true;
                None
            }
            // Routed above, before any mode-independent key handling.
            ClientShellMode::Navigate => None,
            ClientShellMode::Resize => {
                self.route_resize_key(key, outcome);
                None
            }
            ClientShellMode::Copy => {
                if self
                    .copy
                    .as_ref()
                    .and_then(|copy_mode| copy_mode.search.as_ref())
                    .is_none_or(|search| search.prompt.is_none())
                    && self.config.keybinds.prefix.matches(key)
                {
                    self.mode.set(ClientShellMode::Prefix);
                    outcome.repaint = true;
                } else {
                    self.route_copy_mode_key(key, outcome);
                }
                None
            }
        }
    }

    /// Return the copy session's mode when leaving a temporary mode. This does
    /// not decide whether copy currently owns input; `copy_mode_owns_input` does.
    pub(in crate::shell) fn copy_or_terminal_mode(&self) -> ClientShellMode {
        if self
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.pane_is_focused(self.focused_pane_id().as_ref()))
        {
            ClientShellMode::Copy
        } else {
            ClientShellMode::Terminal
        }
    }

    /// A key in navigate mode. Only the navigate bindings act: move the
    /// selection, open it, or leave. Every other key does nothing.
    fn route_navigate_key(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let Some(action) = resolve_navigate_binding(&self.config.keybinds.keybinds, key) else {
            return;
        };
        self.pending_workspace_highlight = None;
        match action {
            NavigateAction::Back => {
                self.mode.set(self.copy_or_terminal_mode());
                outcome.repaint = true;
            }
            NavigateAction::Up => {
                self.move_navigate_selection(-1);
                outcome.repaint = true;
            }
            NavigateAction::Down => {
                self.move_navigate_selection(1);
                outcome.repaint = true;
            }
            NavigateAction::Open => self.accept_navigate_selection(outcome),
        }
    }

    fn route_resize_key(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let resize_bindings = &self.config.keybinds.keybinds.resize_mode;
        if resize_bindings.matches_prefix_key(key) || resize_bindings.matches_direct_key(key) {
            self.mode.set(self.copy_or_terminal_mode());
            outcome.repaint = true;
            return;
        }
        match fixed_keys::command_for(RESIZE_BINDINGS, key) {
            Some(ResizeCommand::Finish) => {
                self.mode.set(self.copy_or_terminal_mode());
                outcome.repaint = true;
            }
            Some(ResizeCommand::Left) => {
                self.record_binding(&shepr_termio::input::KeybindAction::ResizePaneLeft, outcome);
            }
            Some(ResizeCommand::Down) => {
                self.record_binding(&shepr_termio::input::KeybindAction::ResizePaneDown, outcome);
            }
            Some(ResizeCommand::Up) => {
                self.record_binding(&shepr_termio::input::KeybindAction::ResizePaneUp, outcome);
            }
            Some(ResizeCommand::Right) => self.record_binding(
                &shepr_termio::input::KeybindAction::ResizePaneRight,
                outcome,
            ),
            None => {}
        }
    }

    fn input_context(&self) -> ClientInputContext {
        ClientInputContext {
            mode: self.mode.kind(),
            overlay: self.overlay.as_ref().map(Overlay::kind),
            retained_selection: self
                .mouse_selection
                .selection
                .as_ref()
                .is_some_and(shepr_term::selection::Selection::is_visible),
        }
    }

    pub(in crate::shell) fn focused_pane_id(&self) -> Option<shepr_protocol::PublicPaneId> {
        self.endpoints
            .active
            .snapshot()
            .and_then(|snapshot| snapshot.focused_pane_id)
    }

    fn push_pane_key(
        &self,
        target: shepr_protocol::PublicPaneId,
        key: shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        if let Some(event) = ClientPaneInputEvent::from_terminal_key(key) {
            crate::shell::input::events::push_target_event(target, event, outcome, accounting);
        }
    }

    /// Sends a paste to the focused pane, or rejects it locally when the
    /// server would.
    ///
    /// The server refuses a message whose `InputBatchCharge` does not fit, and
    /// a lone paste can only overflow its text bytes. Checking that charge here
    /// means an oversized paste never goes out: a paste past the frame cap
    /// would otherwise make the server drop the connection, and anything over
    /// the limit would only come back as a rejection anyway. A paste that fits
    /// on its own but would not fit with the pending message starts a new
    /// message instead of joining the batch.
    fn push_focused_paste(
        &mut self,
        text: String,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let event = ClientPaneInputEvent::Paste(text);
        let charge = shepr_protocol::InputBatchCharge::of(&event);
        if !charge.fits() {
            outcome.repaint |= self.receive_paste_rejection(paste_rejected_notice(
                charge.text_bytes(),
                shepr_protocol::MAX_INPUT_PAYLOAD,
            ));
            return;
        }
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        crate::shell::input::events::push_target_event(pane_id, event, outcome, accounting);
    }
}

/// The notice shown for a paste over the server's per-message input limit.
fn paste_rejected_notice(size: usize, max: usize) -> String {
    format!("Paste is {size} bytes; Shepr's limit is {max} bytes")
}

#[cfg(test)]
impl ClientShellState {
    pub(super) fn handle_input_bytes(&mut self, data: &[u8]) -> ClientShellInput {
        self.handle_raw_events(shepr_test_fixtures::parse_raw_input_bytes_sync(data))
    }

    fn handle_pixel_mouse(
        &mut self,
        mut mouse: crossterm::event::MouseEvent,
        pixels: shepr_termio::input::mouse::HostPixels,
    ) -> ClientShellInput {
        let Some((column, row)) = pixels.geometry.cell(pixels.x, pixels.y) else {
            return ClientShellInput::default();
        };
        mouse.column = column;
        mouse.row = row;
        let mut outcome = ClientShellInput::default();
        let mut accounting = PaneInputBatchAccounting::default();
        let previous = self.pointer.host_mouse_pixels.replace(pixels);
        // clock-io-ok: this test-only entry stands in for the client loop.
        let now = std::time::Instant::now();
        self.now = now;
        self.handle_raw_event(
            RawInputEvent::Mouse(mouse),
            now,
            &mut outcome,
            &mut accounting,
        );
        self.pointer.host_mouse_pixels = previous;
        outcome
    }

    pub(super) fn handle_pixel_mouse_bytes(
        &mut self,
        data: &[u8],
        geometry: shepr_termio::input::mouse::HostPixelExtent,
    ) -> ClientShellInput {
        let Some((x, y)) = shepr_test_fixtures::parse_sgr_mouse_report(data) else {
            return ClientShellInput::default();
        };
        let mut events = shepr_test_fixtures::parse_raw_input_bytes_sync(data);
        if events.len() != 1 {
            return ClientShellInput::default();
        }
        let Some(RawInputEvent::Mouse(mouse)) = events.pop() else {
            return ClientShellInput::default();
        };
        self.handle_pixel_mouse(
            mouse,
            shepr_termio::input::mouse::HostPixels { x, y, geometry },
        )
    }

    pub(super) fn handle_raw_events(&mut self, events: Vec<RawInputEvent>) -> ClientShellInput {
        // clock-io-ok: this test-only entry stands in for the client loop.
        let now = std::time::Instant::now();
        self.now = now;
        let mut outcome = ClientShellInput::default();
        let mut accounting = PaneInputBatchAccounting::default();
        for event in events {
            self.handle_raw_event(event, now, &mut outcome, &mut accounting);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::config::ClientShellConfig;
    use shepr_config::ClientConfig;
    use shepr_protocol::ClientPaneInputEvent;

    use super::{is_modal_paste_shortcut, read_clipboard_text_bounded_with};
    use crate::shell::overlays::Overlay;
    use crate::shell::state::{
        ClientShellInput, ClientShellMode, ClientShellRequest, ClientShellState,
    };
    use crate::shell::tests::{
        enter_navigation, fill_prompt, press, preview_key, prompt_shell, prompt_text,
        state_with_remote, surface,
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use shepr_protocol::ClientMessage;
    use shepr_protocol::MAX_INPUT_PAYLOAD;
    use shepr_term::key::TerminalKey;
    use shepr_termio::input::raw_input::RawInputEvent;

    fn shell() -> ClientShellState {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        state
    }

    #[test]
    fn modal_paste_shortcut_is_ctrl_v() {
        let key = |code, modifiers| shepr_term::key::TerminalKey::new(code, modifiers);
        assert!(!is_modal_paste_shortcut(&key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert!(is_modal_paste_shortcut(&key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
        assert!(is_modal_paste_shortcut(&key(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_modal_paste_shortcut(&key(
            KeyCode::Char('v'),
            KeyModifiers::SUPER
        )));
    }

    #[test]
    fn modal_paste_inserts_clipboard_text_through_overlay_text_path() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        // With no snapshot the new-workspace prompt opens on its default suggestion, which
        // the first typed or pasted text replaces.
        state.open_new_workspace_overlay();
        let mut outcome = ClientShellInput::default();
        let key = shepr_term::key::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);

        assert!(
            state.handle_modal_paste_shortcut_with(&key, &mut outcome, || {
                Some("feature/pasted".into())
            })
        );
        assert!(outcome.repaint);
        assert!(matches!(
            state.overlay.as_ref(),
            Some(Overlay::Rename(rename)) if rename.input().as_str() == "feature/pasted"
        ));
    }

    #[test]
    fn text_delivery_paths_insert_at_the_cursor() {
        for delivery in 0..3 {
            let mut state = prompt_shell(0);
            fill_prompt(&mut state, "ab");
            press(&mut state, KeyCode::Left, KeyModifiers::NONE);
            let result = match delivery {
                0 => state.handle_raw_events(vec![RawInputEvent::Key(
                    TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE)
                        .with_generated_text(Some("X".into())),
                )]),
                1 => state.handle_raw_events(vec![RawInputEvent::Paste("X".into())]),
                _ => {
                    let mut result = ClientShellInput::default();
                    assert!(state.handle_modal_paste_shortcut_with(
                        &TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                        &mut result,
                        || Some("X".into())
                    ));
                    result
                }
            };
            assert!(result.repaint, "delivery {delivery}");
            assert!(result.requests.is_empty() && result.actions.is_empty());
            assert_eq!(prompt_text(&state).as_str(), "aXb");
        }
    }

    #[test]
    fn copy_search_owns_prefix_but_parked_prompt_does_not_steal_input() {
        let mut state = prompt_shell(5);
        fill_prompt(&mut state, "ab");
        press(&mut state, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        state.handle_raw_events(vec![RawInputEvent::Paste("X".into())]);
        assert_eq!(prompt_text(&state).as_str(), "aXb");
        state.open_rename_pane_overlay();
        assert!(state.modal_paste_target_active());
        state.handle_raw_events(vec![RawInputEvent::Paste("name".into())]);
        assert_eq!(prompt_text(&state).as_str(), "name");
        // Escape closes the name prompt; the copy search parked behind it is untouched.
        press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(prompt_text(&state).as_str(), "aXb");
        // Focus moving to another pane parks the session with its prompt, and a paste
        // goes to that pane.
        let copy_pane = crate::tests::test_pane_id("w1:p1");
        let other_pane = crate::tests::test_pane_id("w1:p2");
        let mut two_panes = crate::shell::tests::snapshot();
        let mut second = two_panes.panes[0].clone();
        second.pane_id = other_pane;
        two_panes.panes.push(second);
        two_panes.focused_pane_id = Some(other_pane);
        state.set_snapshot(Box::new(two_panes.clone()));
        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        assert!(!state.modal_paste_target_active());
        let input = state.handle_raw_events(vec![RawInputEvent::Paste("terminal".into())]);
        assert!(
            matches!(&input.requests[..], [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })] if *pane_id == other_pane && matches!(&events[..], [ClientPaneInputEvent::Paste(text)] if text == "terminal"))
        );
        assert_eq!(prompt_text(&state).as_str(), "aXb");
        // Focus coming back resumes the session.
        two_panes.focused_pane_id = Some(copy_pane);
        state.set_snapshot(Box::new(two_panes));
        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut state, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(state.mode.kind(), ClientShellMode::Prefix);
    }

    #[test]
    fn foreign_workspace_preview_blocks_paste_into_hidden_copy_search() {
        let (mut state, _) = state_with_remote();
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            20,
            2,
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
        state.compose(100, 28).expect("test precondition");
        assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
        state.handle_input_bytes(b"/original");
        // The open prompt takes every key while its pane has focus. Focus moving to
        // another pane parks it, open, and the prefix then reaches navigation.
        let mut two_panes = state
            .endpoints
            .active
            .snapshot()
            .expect("test precondition")
            .clone();
        let mut second = two_panes.panes[0].clone();
        second.pane_id = crate::tests::test_pane_id("w1:p2");
        two_panes.focused_pane_id = Some(second.pane_id);
        two_panes.panes.push(second);
        state.set_snapshot(Box::new(two_panes));
        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        assert!(state.workspace_preview_action_blocked());
        assert!(!state.modal_paste_target_active());
        let key = shepr_term::key::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(!state.handle_modal_paste_shortcut_with(
            &key,
            &mut ClientShellInput::default(),
            || { panic!("hidden search must not read the clipboard") }
        ));
        let paste = state.handle_raw_events(vec![RawInputEvent::Paste("unexpected".into())]);
        assert!(paste.actions.is_empty() && paste.requests.is_empty());
        assert_eq!(
            state
                .copy
                .expect("test precondition")
                .search
                .expect("test precondition")
                .prompt
                .expect("test precondition")
                .query,
            "original".into()
        );
    }

    fn message_text_bytes(request: &ClientShellRequest) -> usize {
        let ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { events, .. }) = request
        else {
            panic!("expected targeted pane input, got {request:?}");
        };
        events.iter().map(ClientPaneInputEvent::text_bytes).sum()
    }

    #[test]
    fn paste_over_the_input_limit_is_rejected_locally_and_never_sent() {
        let mut state = shell();
        let outcome = state.handle_raw_events(vec![RawInputEvent::Paste(
            "x".repeat(MAX_INPUT_PAYLOAD + 1),
        )]);
        assert!(outcome.requests.is_empty());

        let at_limit =
            state.handle_raw_events(vec![RawInputEvent::Paste("x".repeat(MAX_INPUT_PAYLOAD))]);
        assert_eq!(at_limit.requests.len(), 1);
        assert_eq!(message_text_bytes(&at_limit.requests[0]), MAX_INPUT_PAYLOAD);
    }

    #[test]
    fn pastes_that_would_overflow_one_message_go_out_separately() {
        let mut state = shell();
        let half = MAX_INPUT_PAYLOAD / 2 + 1;
        let outcome = state.handle_raw_events(vec![
            RawInputEvent::Paste("a".repeat(half)),
            RawInputEvent::Paste("b".repeat(half)),
        ]);
        assert_eq!(outcome.requests.len(), 2);
        for request in &outcome.requests {
            assert!(message_text_bytes(request) <= MAX_INPUT_PAYLOAD);
        }

        let small = state.handle_raw_events(vec![
            RawInputEvent::Paste("a".into()),
            RawInputEvent::Paste("b".into()),
        ]);
        assert_eq!(small.requests.len(), 1, "small pastes still batch");
    }

    #[test]
    fn bounded_clipboard_read_returns_a_prompt_answer() {
        static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let text =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), || {
                Some("clip".to_owned())
            });
        assert_eq!(text.as_deref(), Some("clip"));
    }

    #[test]
    fn hung_clipboard_helper_is_abandoned_and_not_waited_on_again() {
        static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let gated_read = move || {
            started_sender.send(()).expect("test receiver is waiting");
            release_receiver
                .recv()
                .expect("test releases the clipboard reader");
            finished_sender.send(()).expect("test receiver is waiting");
            Some("late".to_owned())
        };

        let first =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::ZERO, gated_read);
        assert!(first.is_none());
        started_receiver
            .recv()
            .expect("clipboard reader starts before the retry");
        // The abandoned reader is still running: the next paste gives up at once.
        let second =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), || {
                Some("should not run".to_owned())
            });
        assert!(second.is_none());

        release_sender
            .send(())
            .expect("clipboard reader is still waiting");
        finished_receiver
            .recv()
            .expect("clipboard reader returns after release");
        while IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::yield_now();
        }
        // Once the helper exits, reads work again.
        let third =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), || {
                Some("clip".to_owned())
            });
        assert_eq!(third.as_deref(), Some("clip"));
    }
}
