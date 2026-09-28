use super::*;
use crate::input_wire::{WireMouseButton, WirePaneInput};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use shepr_protocol::ClientPaneInputEvent;
use shepr_termio::input::raw_input::RawInputEvent;

const LOCAL_INPUT_SOURCE: u8 = 0;

fn is_retained_selection_copy_key(key: &shepr_termio::input::TerminalKey) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

/// How long Ctrl+V in a modal input waits for the clipboard helper before giving up.
const MODAL_PASTE_CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

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
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
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

fn navigate_alias_matches_left(key: &shepr_termio::input::TerminalKey) -> bool {
    shepr_config::terminal_key_matches_combo(key, (KeyCode::Left, KeyModifiers::empty()))
}

fn navigate_alias_matches_right(key: &shepr_termio::input::TerminalKey) -> bool {
    shepr_config::terminal_key_matches_combo(key, (KeyCode::Right, KeyModifiers::empty()))
}

macro_rules! define_navigate_actions {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
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
    key: &shepr_termio::input::TerminalKey,
) -> Option<usize> {
    let actual_modifiers = shepr_config::normalize_key_combo((key.code, key.modifiers)).1;
    for exact_modifiers in [true, false] {
        for binding in bindings {
            let expected_modifiers = shepr_config::normalize_key_combo(binding.trigger.combo()).1;
            if binding.trigger.is_direct()
                && (actual_modifiers == expected_modifiers) == exact_modifiers
                && let Some(index) = binding.matched_index(key)
            {
                return Some(index);
            }
        }
    }
    None
}

fn resolve_navigate_binding(
    keybinds: &shepr_config::Keybinds,
    key: &shepr_termio::input::TerminalKey,
) -> Option<NavigateAction> {
    macro_rules! alias_matches {
        (None, $key:expr) => {
            false
        };
        (Left, $key:expr) => {
            navigate_alias_matches_left($key)
        };
        (Right, $key:expr) => {
            navigate_alias_matches_right($key)
        };
    }

    macro_rules! resolve_navigate {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {{
            $(
                if keybinds.navigate.$navigate_field.matches_direct_key(key)
                    || alias_matches!($navigate_alias, key)
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

pub(super) fn is_modal_paste_shortcut(key: &shepr_termio::input::TerminalKey) -> bool {
    key.generated_text.as_deref().is_none_or(str::is_empty)
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && key.modifiers.difference(KeyModifiers::SHIFT) == KeyModifiers::CONTROL
}

fn host_theme_update(event: &RawInputEvent) -> Option<shepr_protocol::ClientHostThemeUpdate> {
    use shepr_protocol::ClientHostThemeUpdate;

    match event {
        RawInputEvent::HostDefaultColor { kind, color } => {
            Some(ClientHostThemeUpdate::DefaultColor {
                kind: (*kind).into(),
                color: (*color).into(),
            })
        }
        RawInputEvent::HostPaletteColors { colors } => Some(ClientHostThemeUpdate::PaletteColors(
            colors
                .iter()
                .map(|(index, color)| (*index, (*color).into()))
                .collect(),
        )),
        RawInputEvent::HostColorSchemeChanged(appearance) => {
            Some(ClientHostThemeUpdate::Appearance((*appearance).into()))
        }
        _ => None,
    }
}

fn push_host_theme_update(
    requests: &mut Vec<ClientMessage>,
    update: shepr_protocol::ClientHostThemeUpdate,
) {
    if let shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors) = &update
        && let Some(ClientMessage::ClientShellHostTheme {
            update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(pending),
        }) = requests.last_mut()
        && pending.len() + colors.len() <= 256
    {
        pending.extend_from_slice(colors);
        return;
    }
    requests.push(ClientMessage::ClientShellHostTheme { update });
}

impl ClientShellState {
    pub(crate) fn host_keyboard_report_all_requested(&self) -> bool {
        matches!(
            self.mode,
            ClientShellMode::Prefix | ClientShellMode::Navigate
        )
    }

    #[cfg(test)]
    pub(crate) fn handle_input_bytes(&mut self, data: &[u8]) -> ClientShellInput {
        self.handle_raw_events(shepr_test_fixtures::parse_raw_input_bytes_sync(data))
    }

    #[cfg(test)]
    pub(crate) fn handle_pixel_mouse(
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
        self.begin_input_batch(true, &mut outcome);
        let previous = self.host_mouse_pixels.replace(pixels);
        self.handle_raw_event(RawInputEvent::Mouse(mouse), &mut outcome);
        self.host_mouse_pixels = previous;
        outcome
    }

    /// `host_reports_all_keys` is the host keyboard mode the input arrived
    /// under; it decides whether text key presses can hold input leases.
    pub(crate) fn handle_host_input(
        &mut self,
        inputs: Vec<super::super::ParsedHostInput>,
        host_reports_all_keys: bool,
    ) -> ClientShellInput {
        self.host_reports_all_keys = host_reports_all_keys;
        let mut outcome = ClientShellInput::default();
        self.begin_input_batch(!inputs.is_empty(), &mut outcome);
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
                let previous = self.host_mouse_pixels.replace(pixels);
                self.handle_raw_event(RawInputEvent::Mouse(mouse), &mut outcome);
                self.host_mouse_pixels = previous;
            } else {
                self.handle_raw_event(input.event, &mut outcome);
            }
        }
        outcome
    }

    #[cfg(test)]
    pub(crate) fn handle_pixel_mouse_bytes(
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

    fn begin_input_batch(&mut self, has_events: bool, outcome: &mut ClientShellInput) {
        if has_events && self.endpoint_error.take().is_some() {
            self.endpoint_error_deadline = None;
            outcome.repaint = true;
        }
    }

    fn handle_raw_event(&mut self, event: RawInputEvent, outcome: &mut ClientShellInput) {
        if self.handle_machine_badge_event(&event, outcome) {
            return;
        }
        if let Some(update) = host_theme_update(&event) {
            push_host_theme_update(&mut outcome.requests, update);
        }
        match event {
            RawInputEvent::Key(key) => self.handle_key(key, outcome),
            RawInputEvent::Paste(text) => {
                if self.prepare_committed_text(&text, outcome) {
                    return;
                }
                if self.insert_overlay_text(&text) {
                    outcome.repaint = true;
                } else if self.overlay.is_none() && self.mode == ClientShellMode::Terminal {
                    self.push_focused_paste(text, outcome);
                }
            }
            RawInputEvent::Mouse(mouse) => self.handle_mouse(mouse, outcome),
            RawInputEvent::OuterFocusGained => {
                self.outer_focused = Some(true);
                outcome.query_host_appearance = true;
                if self.config.redraw_on_focus_gained {
                    outcome.repaint = true;
                    outcome.full_redraw = true;
                }
                outcome
                    .requests
                    .push(ClientMessage::ClientShellFocus { focused: true });
            }
            RawInputEvent::OuterFocusLost => {
                self.outer_focused = Some(false);
                self.release_input_leases(outcome);
                outcome
                    .requests
                    .push(ClientMessage::ClientShellFocus { focused: false });
            }
            RawInputEvent::HostDefaultColor {
                kind: shepr_termio::host_term::theme::DefaultColorKind::Background,
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
                // highlight (`host_background`) follow. Direct attach does the
                // same; the stdin framer arms itself for the replies whenever
                // it tracks scheme changes.
                outcome.query_host_theme = true;
            }
            RawInputEvent::HostDefaultColor { .. }
            | RawInputEvent::HostPaletteColors { .. }
            | RawInputEvent::HostCellSizeReport { .. }
            | RawInputEvent::Unsupported => {}
        }
    }

    fn prepare_committed_text(&mut self, text: &str, outcome: &mut ClientShellInput) -> bool {
        if !(self.mode == ClientShellMode::Navigate && self.workspace_preview_action_blocked())
            && self.insert_copy_search_text(text)
        {
            outcome.repaint = true;
            return true;
        }
        self.word_selection_gesture = None;
        if self.copy_or_terminal_mode() != ClientShellMode::Copy && self.selection.take().is_some()
        {
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn handle_raw_events(&mut self, events: Vec<RawInputEvent>) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        self.begin_input_batch(!events.is_empty(), &mut outcome);
        for event in events {
            self.handle_raw_event(event, &mut outcome);
        }
        outcome
    }

    pub(super) fn handle_key(
        &mut self,
        key: shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_operation_in_flight {
            self.copy_input_queue.push_back(key);
            return;
        }
        let lease_key = shepr_termio::input::InputLeaseKey::new(LOCAL_INPUT_SOURCE, &key);
        let host_reports_all_keys = self.host_reports_all_keys;
        let key = self
            .input_leases
            .normalize_press(&lease_key, key, host_reports_all_keys);
        match key.kind {
            KeyEventKind::Press => {
                let initial_context = self.input_context();
                let target = self.route_key_press(&key, outcome);
                if let Some(target) = target.as_ref() {
                    self.push_pane_key(target.clone(), key.clone(), outcome);
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
                self.execute_repeat_plan(lease_key, key, plan, outcome);
            }
            KeyEventKind::Repeat => {
                let context = self.input_context();
                let plan = self
                    .input_leases
                    .plan_repeat(lease_key, &key, Some(&context));
                self.execute_repeat_plan(lease_key, key, plan, outcome);
            }
            KeyEventKind::Release => {
                if let Some(lease) = self.input_leases.remove_forwarded(&lease_key) {
                    let release = lease
                        .key
                        .with_modifiers(key.modifiers)
                        .with_kind(KeyEventKind::Release);
                    self.push_pane_key(lease.target, release, outcome);
                } else {
                    let _ = self.input_leases.remove(&lease_key);
                }
            }
        }
    }

    fn release_input_leases(&mut self, outcome: &mut ClientShellInput) {
        for lease in self.input_leases.remove_source(LOCAL_INPUT_SOURCE) {
            self.push_pane_key(
                lease.target,
                lease.key.with_kind(KeyEventKind::Release),
                outcome,
            );
        }
        if let Some(gesture) = self.pane_mouse_gesture.take() {
            let modifiers = gesture
                .last_event
                .modifiers
                .difference(gesture.stripped_modifiers);
            let geometry = matches!(
                gesture.last_position,
                shepr_protocol::ClientMousePosition::Pixels { .. }
            )
            .then_some(shepr_protocol::ClientMouseGeometry {
                cols: gesture.hit.inner_rect.width,
                rows: gesture.hit.inner_rect.height,
                width_px: gesture.hit.pixel_width,
                height_px: gesture.hit.pixel_height,
            });
            super::push_target_event(
                ClientInputTarget::Pane(gesture.hit.pane_id),
                ClientPaneInputEvent::Mouse {
                    kind: shepr_protocol::ClientMouseKind::Up(
                        shepr_protocol::ClientMouseButton::from_crossterm(gesture.button),
                    ),
                    position: gesture.last_position,
                    geometry,
                    modifiers: crate::input_wire::wire_modifiers(modifiers),
                    lines: self.config.mouse_scroll_lines,
                },
                outcome,
            );
        }
        self.copy_input_queue.clear();
    }

    fn execute_repeat_plan(
        &mut self,
        lease_key: shepr_termio::input::InputLeaseKey<u8>,
        key: shepr_termio::input::TerminalKey,
        plan: shepr_termio::input::RepeatPlan<ClientInputContext, ClientInputTarget>,
        outcome: &mut ClientShellInput,
    ) {
        match plan {
            shepr_termio::input::RepeatPlan::Forwarded(target) => {
                self.push_pane_key(target, key, outcome);
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
                        self.push_pane_key(target, repeated, outcome);
                    }
                }
            }
            shepr_termio::input::RepeatPlan::Ignore => {}
        }
    }

    pub(super) fn modal_paste_target_active(&self) -> bool {
        if self.overlay.is_none()
            && self.mode == ClientShellMode::Navigate
            && self.workspace_preview_action_blocked()
        {
            return false;
        }
        if self.mode == ClientShellMode::Copy
            && self.overlay.is_none()
            && self
                .copy_mode
                .as_ref()
                .is_some_and(|copy_mode| copy_mode.search_prompt.is_some())
        {
            return true;
        }
        matches!(
            self.overlay.as_ref(),
            Some(
                ClientShellOverlay::Rename(_)
                    | ClientShellOverlay::Navigator(ClientNavigatorOverlay {
                        search_focused: true,
                        ..
                    })
                    | ClientShellOverlay::Help(ClientHelpOverlay {
                        search_focused: true,
                        ..
                    })
            )
        )
    }

    pub(super) fn handle_modal_paste_shortcut_with(
        &mut self,
        key: &shepr_termio::input::TerminalKey,
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
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> Option<ClientInputTarget> {
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
        self.word_selection_gesture = None;
        if self.mode != ClientShellMode::Copy
            && self.copy_or_terminal_mode() != ClientShellMode::Copy
            && !self.config.copy_on_select
            && is_retained_selection_copy_key(key)
            && self
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome, true);
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
            return None;
        }
        if self.mode != ClientShellMode::Copy
            && self.copy_or_terminal_mode() != ClientShellMode::Copy
            && self.selection.take().is_some()
        {
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
        }

        match self.mode {
            ClientShellMode::Terminal => {
                if let Some(binding) =
                    shepr_termio::input::resolve_direct_binding(&self.config.keybinds.keybinds, key)
                {
                    self.record_binding(&binding, outcome);
                    return None;
                }
                if shepr_config::terminal_key_matches_combo(key, self.config.keybinds.prefix) {
                    self.mode = ClientShellMode::Prefix;
                    outcome.repaint = true;
                    return None;
                }
                self.focused_pane_id().map(ClientInputTarget::Pane)
            }
            ClientShellMode::Prefix => {
                let return_mode = if self.copy_mode.as_ref().is_some_and(|copy_mode| {
                    self.focused_pane_id().as_deref() == Some(copy_mode.pane_id.as_str())
                }) {
                    ClientShellMode::Copy
                } else {
                    ClientShellMode::Terminal
                };
                if shepr_config::terminal_key_matches_combo(key, self.config.keybinds.prefix) {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    return self.focused_pane_id().map(ClientInputTarget::Pane);
                }
                if key.code == KeyCode::Esc {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    return None;
                }
                if let Some(binding) =
                    shepr_termio::input::resolve_prefix_binding(&self.config.keybinds.keybinds, key)
                {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    self.record_binding(&binding, outcome);
                    return None;
                }
                self.mode = return_mode;
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
                    .copy_mode
                    .as_ref()
                    .is_none_or(|copy_mode| copy_mode.search_prompt.is_none())
                    && shepr_config::terminal_key_matches_combo(key, self.config.keybinds.prefix)
                {
                    self.mode = ClientShellMode::Prefix;
                    outcome.repaint = true;
                } else {
                    self.route_copy_mode_key(key, outcome);
                }
                None
            }
        }
    }

    pub(super) fn copy_or_terminal_mode(&self) -> ClientShellMode {
        if self.copy_mode.as_ref().is_some_and(|copy_mode| {
            self.focused_pane_id().as_deref() == Some(copy_mode.pane_id.as_str())
        }) {
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
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        use shepr_termio::input::{KeybindAction, KeybindDispatch, KeybindMatch};

        self.pending_workspace_highlight = None;
        if shepr_config::terminal_key_matches_combo(key, self.config.keybinds.prefix) {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
            outcome.repaint = true;
            return;
        }

        let navigate_binding = resolve_navigate_binding(&self.config.keybinds.keybinds, key);
        match navigate_binding.as_ref() {
            Some(NavigateAction::Back) => {
                self.mode = self.copy_or_terminal_mode();
                self.navigate_workspace_id = None;
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
                "navigate_endpoint_inactive",
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
                    let valid = self.snapshot.as_deref().is_some_and(|snapshot| {
                        self.navigation_workspace_entries(snapshot)
                            .get(index)
                            .is_some()
                    });
                    if valid {
                        self.mode = ClientShellMode::Terminal;
                        self.navigate_workspace_id = None;
                        self.record_binding(
                            &KeybindMatch::Action(KeybindAction::SwitchWorkspace(index)),
                            outcome,
                        );
                        outcome.repaint = true;
                    }
                }
                NavigateAction::CyclePaneNext => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::CyclePaneNext),
                    false,
                    outcome,
                ),
                NavigateAction::CyclePanePrevious => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::CyclePanePrevious),
                    false,
                    outcome,
                ),
                NavigateAction::PaneLeft => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::FocusPaneLeft),
                    true,
                    outcome,
                ),
                NavigateAction::PaneDown => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::FocusPaneDown),
                    true,
                    outcome,
                ),
                NavigateAction::PaneUp => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::FocusPaneUp),
                    true,
                    outcome,
                ),
                NavigateAction::PaneRight => self.record_navigate_binding(
                    &KeybindMatch::Action(KeybindAction::FocusPaneRight),
                    true,
                    outcome,
                ),
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
        .map(KeybindMatch::Action)
        .or_else(|| {
            shepr_termio::input::resolve_indexed_action(
                &self.config.keybinds.keybinds,
                key,
                KeybindDispatch::Prefix,
            )
            .map(KeybindMatch::Action)
        });
        if let Some(binding) = binding {
            self.record_navigate_binding(&binding, false, outcome);
        }
    }

    fn record_navigate_binding(
        &mut self,
        binding: &shepr_termio::input::KeybindMatch,
        preserve_navigate: bool,
        outcome: &mut ClientShellInput,
    ) {
        use shepr_termio::input::{KeybindAction, KeybindMatch};

        if !self.indexed_navigation_target_exists(binding) {
            return;
        }
        if let KeybindMatch::Action(KeybindAction::CyclePaneNext) = binding {
            self.cycle_pane(false, outcome);
        } else if let KeybindMatch::Action(KeybindAction::CyclePanePrevious) = binding {
            self.cycle_pane(true, outcome);
        } else {
            if !preserve_navigate {
                self.mode = ClientShellMode::Terminal;
            }
            self.record_binding(binding, outcome);
            // Navigate mode was left just above, but a close dialog opened from
            // it should still cancel back into it.
            if let Some(ClientShellOverlay::ConfirmClose(confirm)) = self.overlay.as_mut() {
                confirm.return_to_navigate = true;
            }
        }
        if !preserve_navigate {
            if self.mode == ClientShellMode::Navigate {
                self.mode = self.copy_or_terminal_mode();
            }
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }

    pub(super) fn indexed_navigation_target_exists(
        &self,
        binding: &shepr_termio::input::KeybindMatch,
    ) -> bool {
        use shepr_termio::input::{KeybindAction, KeybindMatch};

        match binding {
            KeybindMatch::Action(KeybindAction::SwitchWorkspace(index)) => {
                self.snapshot.as_deref().is_some_and(|snapshot| {
                    self.navigation_workspace_entries(snapshot)
                        .get(*index)
                        .is_some()
                })
            }
            KeybindMatch::Action(KeybindAction::SwitchTab(index)) => self
                .snapshot
                .as_deref()
                .and_then(|snapshot| {
                    let workspace_id = snapshot.focused_workspace_id.as_deref()?;
                    snapshot
                        .tabs
                        .iter()
                        .filter(|tab| tab.workspace_id == workspace_id)
                        .nth(*index)
                })
                .is_some(),
            KeybindMatch::Action(KeybindAction::FocusAgent(index)) => {
                super::aggregate_navigation::online_agent_targets(
                    &self.endpoints,
                    &self.active_endpoint_id,
                    self.config.agent_panel_sort,
                )
                .get(*index)
                .is_some()
            }
            _ => true,
        }
    }

    fn cycle_pane(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(surface) = self.pane_surface.as_ref() else {
            return;
        };
        if surface.panes.is_empty() {
            return;
        }
        let current = snapshot
            .focused_pane_id
            .as_deref()
            .and_then(|focused| {
                surface
                    .panes
                    .iter()
                    .position(|pane| pane.pane_id == focused)
            })
            .unwrap_or(0);
        let next = if reverse {
            (current + surface.panes.len() - 1) % surface.panes.len()
        } else {
            (current + 1) % surface.panes.len()
        };
        let pane_id = surface.panes[next].pane_id.clone();
        self.push_endpoint_method(
            shepr_api::schema::Method::PaneFocus(shepr_api::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
            outcome,
        );
    }

    fn route_resize_key(
        &mut self,
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let resize_bindings = &self.config.keybinds.keybinds.resize_mode;
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Enter
            || resize_bindings.matches_prefix_key(key)
            || resize_bindings.matches_direct_key(key)
        {
            self.mode = self.copy_or_terminal_mode();
            outcome.repaint = true;
            return;
        }

        let action = match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                Some(shepr_termio::input::KeybindAction::ResizePaneLeft)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                Some(shepr_termio::input::KeybindAction::ResizePaneDown)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                Some(shepr_termio::input::KeybindAction::ResizePaneUp)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                Some(shepr_termio::input::KeybindAction::ResizePaneRight)
            }
            _ => None,
        };
        if let Some(action) = action {
            self.record_binding(&shepr_termio::input::KeybindMatch::Action(action), outcome);
        }
    }

    fn input_context(&self) -> ClientInputContext {
        ClientInputContext {
            mode: self.mode,
            overlay: self.overlay.as_ref().map(ClientShellOverlay::kind),
            retained_selection: self
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_visible),
        }
    }

    pub(super) fn focused_pane_id(&self) -> Option<shepr_protocol::PublicPaneId> {
        self.snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.clone())
    }

    fn push_pane_key(
        &self,
        target: ClientInputTarget,
        key: shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if let Some(event) = ClientPaneInputEvent::from_terminal_key(key) {
            super::push_target_event(target, event, outcome);
        }
    }

    /// Sends a paste to the focused pane, or rejects it locally when the
    /// server would.
    ///
    /// The server caps the text of one input message at `MAX_INPUT_PAYLOAD`.
    /// Checking here means an oversized paste never goes out: a paste past the
    /// frame cap would otherwise make the server drop the connection, and
    /// anything over the limit would only come back as a rejection anyway. A
    /// paste that fits on its own but would push an already batched message
    /// past the limit starts a message of its own instead of joining the batch.
    fn push_focused_paste(&mut self, text: String, outcome: &mut ClientShellInput) {
        let size = text.len();
        if size > shepr_protocol::MAX_INPUT_PAYLOAD {
            outcome.repaint |= self.receive_endpoint_error(format!(
                "Paste is {size} bytes; Shepr's limit is {} bytes",
                shepr_protocol::MAX_INPUT_PAYLOAD
            ));
            return;
        }
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        let batched = match outcome.requests.last() {
            Some(ClientMessage::ClientShellPaneInput {
                pane_id: pending,
                events,
            }) if *pending == pane_id => events
                .iter()
                .map(ClientPaneInputEvent::text_bytes)
                .fold(0usize, usize::saturating_add),
            _ => 0,
        };
        let event = ClientPaneInputEvent::Paste(text);
        if batched.saturating_add(size) > shepr_protocol::MAX_INPUT_PAYLOAD {
            outcome.requests.push(super::target_event_message(
                ClientInputTarget::Pane(pane_id),
                event,
            ));
        } else {
            super::push_target_event(ClientInputTarget::Pane(pane_id), event, outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::MAX_INPUT_PAYLOAD;

    fn shell() -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        state
    }

    fn message_text_bytes(message: &ClientMessage) -> usize {
        let ClientMessage::ClientShellPaneInput { events, .. } = message else {
            panic!("expected targeted pane input, got {message:?}");
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
