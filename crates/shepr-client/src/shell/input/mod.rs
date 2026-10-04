use shepr_termio::input::KeybindAction;
use shepr_termio::input::KeybindDispatch;

use crate::shell::notices::{ClientEndpointNoticeKind, NoticeCode};
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

pub(in crate::shell) fn navigate_alias_matches(
    combo: shepr_term::key::KeyChord,
    key: &shepr_term::key::TerminalKey,
) -> bool {
    combo.matches(key)
}

macro_rules! define_navigate_actions {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:ident, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:ident, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:ident, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:ident, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
    ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum NavigateAction {
            $($navigate_variant,)*
            $($navigate_indexed_variant(usize),)*
        }
    };
}

shepr_config::keybinding_table!(define_navigate_actions);

fn navigate_indexed_binding_index(
    bindings: &[shepr_config::IndexedKeybind],
    key: &shepr_term::key::TerminalKey,
) -> Option<usize> {
    shepr_config::IndexedKeybind::matched_range_index(bindings, key)
}

fn resolve_navigate_binding(
    keybinds: &shepr_config::Keybinds,
    key: &shepr_term::key::TerminalKey,
) -> Option<NavigateAction> {
    macro_rules! resolve_navigate {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:ident, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:ident, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:ident, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:ident, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {{
            $(
                if keybinds.navigate.$navigate_field.matches_direct_key(key)
                    || shepr_config::navigate_alias!($navigate_alias)
                        .is_some_and(|combo| navigate_alias_matches(combo, key))
                {
                    return Some(NavigateAction::$navigate_variant);
                }
            )*
            $(
                if let Some(index) = navigate_indexed_binding_index(
                    &keybinds.navigate.$navigate_indexed_field,
                    key,
                ) {
                    return Some(NavigateAction::$navigate_indexed_variant(index));
                }
            )*
            None
        }};
    }

    shepr_config::keybinding_table!(resolve_navigate)
}

pub(in crate::shell) fn is_modal_paste_shortcut(key: &shepr_term::key::TerminalKey) -> bool {
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
                if self.host_background != Some(color) {
                    self.host_background = Some(color);
                    outcome.repaint = true;
                }
            }
            RawInputEvent::HostColorSchemeChanged(_) => {
                // A dark/light switch changes the host's default and palette
                // colours too. Re-query them so panes and the selection
                // highlight (`host_background`) follow. The stdin framer arms
                // itself for the replies whenever it tracks scheme changes.
                outcome.query_host_theme = true;
            }
            RawInputEvent::HostDefaultColor { .. }
            | RawInputEvent::HostPaletteColors { .. }
            | RawInputEvent::HostCellSizeReport { .. }
            | RawInputEvent::Unsupported => {}
        }
    }

    fn prepare_committed_text(&mut self, text: &str, outcome: &mut ClientShellInput) -> bool {
        if !(self.mode.is(ClientShellMode::Navigate) && self.workspace_preview_action_blocked())
            && self.insert_copy_search_text(text)
        {
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
                let plan = self.input_leases.complete_press(
                    lease_key,
                    &key,
                    Some(&initial_context),
                    Some(&resulting_context),
                    target,
                    host_reports_all_keys,
                );
                self.execute_repeat_plan(lease_key, key, plan, outcome, accounting);
            }
            KeyEventKind::Repeat => {
                let context = self.input_context();
                let plan = self
                    .input_leases
                    .plan_repeat(lease_key, &key, Some(&context));
                self.execute_repeat_plan(lease_key, key, plan, outcome, accounting);
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

    fn execute_repeat_plan(
        &mut self,
        lease_key: shepr_termio::input::InputLeaseKey<u8>,
        key: shepr_term::key::TerminalKey,
        plan: shepr_termio::input::RepeatPlan<ClientInputContext, shepr_protocol::PublicPaneId>,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        match plan {
            shepr_termio::input::RepeatPlan::Forwarded(target) => {
                self.push_pane_key(target, key, outcome, accounting);
            }
            shepr_termio::input::RepeatPlan::Reprocess {
                context,
                repetitions,
                tracked,
            } => {
                for _ in 0..repetitions {
                    let current = self.input_context();
                    if !self.input_leases.reprocess_allowed(
                        lease_key,
                        &context,
                        Some(&current),
                        tracked,
                    ) {
                        break;
                    }
                    let repeated = key
                        .clone()
                        .with_repeat_count(1)
                        .with_kind(KeyEventKind::Repeat);
                    if let Some(target) = self.route_key_press(&repeated, outcome) {
                        self.push_pane_key(target, repeated, outcome, accounting);
                    }
                }
            }
            shepr_termio::input::RepeatPlan::Ignore => {}
        }
    }

    pub(in crate::shell) fn modal_paste_target_active(&self) -> bool {
        if self.overlay.is_none()
            && self.mode.is(ClientShellMode::Navigate)
            && self.workspace_preview_action_blocked()
        {
            return false;
        }
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

    pub(in crate::shell) fn handle_modal_paste_shortcut_with(
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
            ClientShellMode::Navigate => {
                self.route_navigate_key(key, outcome);
                None
            }
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

    /// How the user confirms the selected workspace, for notices: the
    /// configured `navigate_open_workspace` key, or a plain instruction when
    /// it is unbound.
    pub(in crate::shell) fn open_workspace_hint(&self) -> String {
        self.config
            .keybinds
            .keybinds
            .navigate
            .open_workspace
            .label()
            .map_or_else(|| "open it".to_owned(), |key| format!("press {key}"))
    }

    fn route_navigate_key(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        self.pending_workspace_highlight = None;
        if self.config.keybinds.prefix.matches(key) {
            self.mode.set(self.copy_or_terminal_mode());
            outcome.repaint = true;
            return;
        }

        let navigate_binding = resolve_navigate_binding(&self.config.keybinds.keybinds, key);
        match navigate_binding.as_ref() {
            Some(NavigateAction::Back) => {
                self.mode.set(self.copy_or_terminal_mode());
                outcome.repaint = true;
                return;
            }
            Some(NavigateAction::WorkspaceUp) => {
                self.move_navigate_workspace(-1);
                outcome.repaint = true;
                return;
            }
            Some(NavigateAction::WorkspaceDown) => {
                self.move_navigate_workspace(1);
                outcome.repaint = true;
                return;
            }
            Some(NavigateAction::OpenWorkspace) => {
                self.accept_navigate_workspace(outcome);
                return;
            }
            _ => {}
        }
        if self.workspace_preview_action_blocked() {
            let open_workspace = self.open_workspace_hint();
            self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                NoticeCode::NavigateEndpointInactive,
                "Confirm workspace first",
                format!(
                    "Select an available workspace and {open_workspace} before using workspace or pane actions"
                ),
            );
            outcome.repaint = true;
            return;
        }

        if let Some(navigate_binding) = navigate_binding {
            match navigate_binding {
                NavigateAction::SwitchWorkspace(index) => {
                    let valid = self
                        .endpoints
                        .active
                        .snapshot()
                        .is_some_and(|snapshot| snapshot.workspaces.get(index).is_some());
                    if valid {
                        self.mode.set(ClientShellMode::Terminal);
                        self.record_binding(&KeybindAction::SwitchWorkspace(index), outcome);
                        outcome.repaint = true;
                    }
                }
                NavigateAction::CyclePaneNext => {
                    self.record_navigate_binding(&KeybindAction::CyclePaneNext, false, outcome);
                }
                NavigateAction::CyclePanePrevious => {
                    self.record_navigate_binding(&KeybindAction::CyclePanePrevious, false, outcome);
                }
                NavigateAction::PaneLeft => {
                    self.record_navigate_binding(&KeybindAction::FocusPaneLeft, true, outcome);
                }
                NavigateAction::PaneDown => {
                    self.record_navigate_binding(&KeybindAction::FocusPaneDown, true, outcome);
                }
                NavigateAction::PaneUp => {
                    self.record_navigate_binding(&KeybindAction::FocusPaneUp, true, outcome);
                }
                NavigateAction::PaneRight => {
                    self.record_navigate_binding(&KeybindAction::FocusPaneRight, true, outcome);
                }
                NavigateAction::Back
                | NavigateAction::WorkspaceUp
                | NavigateAction::WorkspaceDown
                | NavigateAction::OpenWorkspace => {}
            }
            return;
        }

        let binding = shepr_termio::input::resolve_non_indexed_action(
            &self.config.keybinds.keybinds,
            key,
            KeybindDispatch::Prefix,
        )
        .filter(|action| {
            !matches!(
                action,
                KeybindAction::FocusPaneLeft
                    | KeybindAction::FocusPaneDown
                    | KeybindAction::FocusPaneUp
                    | KeybindAction::FocusPaneRight
            )
        })
        .or_else(|| {
            shepr_termio::input::resolve_indexed_action(
                &self.config.keybinds.keybinds,
                key,
                KeybindDispatch::Prefix,
            )
        });
        if let Some(binding) = binding {
            self.record_navigate_binding(&binding, false, outcome);
        }
    }

    fn record_navigate_binding(
        &mut self,
        binding: &shepr_termio::input::KeybindAction,
        preserve_navigate: bool,
        outcome: &mut ClientShellInput,
    ) {
        if !self.indexed_navigation_target_exists(binding) {
            return;
        }
        if let KeybindAction::CyclePaneNext = binding {
            self.cycle_pane(false, outcome);
        } else if let KeybindAction::CyclePanePrevious = binding {
            self.cycle_pane(true, outcome);
        } else {
            if !preserve_navigate {
                self.mode.set(self.copy_or_terminal_mode());
            }
            self.record_binding(binding, outcome);
            // Navigate mode was left just above, but a close dialog opened from
            // it should still cancel back into it.
            if let Some(Overlay::ConfirmClose(confirm)) = self.overlay.as_mut() {
                confirm.return_to_navigate = true;
            }
        }
        if !preserve_navigate && self.mode.is(ClientShellMode::Navigate) {
            self.mode.set(self.copy_or_terminal_mode());
        }
        outcome.repaint = true;
    }

    pub(in crate::shell) fn indexed_navigation_target_exists(
        &self,
        binding: &shepr_termio::input::KeybindAction,
    ) -> bool {
        match binding {
            KeybindAction::SwitchWorkspace(index) => self
                .endpoints
                .active
                .snapshot()
                .is_some_and(|snapshot| snapshot.workspaces.get(*index).is_some()),
            KeybindAction::FocusAgent(index) => self
                .endpoints
                .agent_panel_model
                .targets()
                .get(*index)
                .is_some(),
            _ => true,
        }
    }

    fn cycle_pane(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let action = if reverse {
            shepr_termio::input::KeybindAction::CyclePanePrevious
        } else {
            shepr_termio::input::KeybindAction::CyclePaneNext
        };
        // Cycling never carries a sidebar reveal.
        if let Some(action_command) = self.endpoint_command_for_action(action) {
            self.push_endpoint_command(action_command.command, outcome);
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

    use super::{navigate_indexed_binding_index, read_clipboard_text_bounded_with};
    use crate::shell::state::{ClientShellRequest, ClientShellState};
    use crossterm::event::{KeyCode, KeyModifiers};
    use shepr_protocol::ClientMessage;
    use shepr_protocol::MAX_INPUT_PAYLOAD;
    use shepr_termio::input::raw_input::RawInputEvent;

    fn shell() -> ClientShellState {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        state
    }

    #[test]
    fn navigate_indexed_helper_uses_the_configured_range_matcher() {
        let state = shell();
        let bindings = &state.config.keybinds.keybinds.navigate.switch_workspace;
        let key = shepr_term::key::TerminalKey::new(KeyCode::Char('3'), KeyModifiers::empty());

        assert_eq!(navigate_indexed_binding_index(bindings, &key), Some(2));
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
