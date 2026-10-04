//! The terminal's effects outbox: everything the child's output asks the host
//! to do, queued until the pane collects it with `Terminal::take_effects`.
//!
//! Two stages feed it. Alacritty events and the handler's own replies go
//! through one ordered queue shared with the emulator's [`Listener`], so
//! replies interleave in request order; [`Effects::drain`] sorts that queue
//! into the typed outputs after each parser batch. The byte scanner's
//! observations skip the queue and land in the outputs directly.

use std::mem;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;
use shepr_core::locks::lock_auxiliary;

use crate::limits::MAX_CLIPBOARD_BYTES;
use crate::{ColorQuery, Progress, WorkingDirectoryReport};

/// A reply the terminal wants written back to the child, in byte order.
#[derive(Debug)]
pub enum PtyResponse {
    Bytes(Vec<u8>),
    ColorQuery(ColorQuery),
}

/// A parsed title event; absence of an event is represented by `None` at the
/// collection boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleUpdate {
    Set(String),
    Reset,
}

/// Effects collected from the terminal since its previous effect drain.
#[must_use = "terminal effects must be handled or explicitly discarded"]
pub struct TerminalEffects {
    /// Replies to write to the child in parser order.
    pub pty_responses: Vec<PtyResponse>,
    /// Working-directory reports observed in child output.
    pub pwd_changes: Vec<WorkingDirectoryReport>,
    /// Clipboard stores requested by the child.
    pub clipboard_writes: Vec<Vec<u8>>,
    /// Sizes of clipboard stores dropped for exceeding the configured limit.
    pub dropped_clipboard_store_bytes: Vec<usize>,
    /// The latest uncollected window-title change.
    pub title_update: Option<TitleUpdate>,
    /// The latest uncollected OSC 9;4 progress report.
    pub progress_update: Option<Progress>,
    /// Bodies of the complete OSCs the terminal saw, in byte order. Empty
    /// unless [`Terminal::set_osc_body_capture`](crate::Terminal::set_osc_body_capture)
    /// is on.
    pub osc_bodies: Vec<Vec<u8>>,
    /// Whether the child set a default foreground or background since the drain.
    pub default_color_set: bool,
}

/// Events retained by the adapter, in emission order.
pub(super) enum TerminalEvent {
    PtyWrite(Vec<u8>),
    ColorQuery(ColorQuery),
    ClipboardStore(ClipboardType, String),
    Title(String),
    ResetTitle,
}

/// Collects the alacritty events the adapter acts on, in emission order.
/// Bells are not among them: nothing in shepr surfaces a bell.
#[derive(Clone)]
pub(super) struct Listener(Arc<Mutex<Vec<TerminalEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let event = match event {
            Event::PtyWrite(text) => Some(TerminalEvent::PtyWrite(text.into_bytes())),
            Event::ClipboardStore(clipboard, text) => {
                Some(TerminalEvent::ClipboardStore(clipboard, text))
            }
            Event::Title(title) => Some(TerminalEvent::Title(title)),
            Event::ResetTitle => Some(TerminalEvent::ResetTitle),
            _ => None,
        };
        if let Some(event) = event {
            lock_auxiliary(&self.0).push(event);
        }
    }
}

/// The ordered event queue and the typed outputs it drains into.
pub(super) struct Effects {
    queue: Arc<Mutex<Vec<TerminalEvent>>>,
    responses: Vec<PtyResponse>,
    pwd_changes: Vec<WorkingDirectoryReport>,
    clipboard_writes: Vec<Vec<u8>>,
    dropped_clipboard_store_bytes: Vec<usize>,
    /// The latest title change not yet collected.
    title_update: Option<TitleUpdate>,
    /// The latest OSC 9;4 progress report not yet collected.
    progress_update: Option<Progress>,
    osc_bodies: Vec<Vec<u8>>,
    /// The child set the default foreground or background since the last
    /// [`Effects::take`].
    default_color_set: bool,
}

impl Effects {
    pub(super) fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
            responses: Vec::new(),
            pwd_changes: Vec::new(),
            clipboard_writes: Vec::new(),
            dropped_clipboard_store_bytes: Vec::new(),
            title_update: None,
            progress_update: None,
            osc_bodies: Vec::new(),
            default_color_set: false,
        }
    }

    /// The emulator's event sink, feeding this outbox's ordered queue.
    pub(super) fn listener(&self) -> Listener {
        Listener(Arc::clone(&self.queue))
    }

    /// Queues an adapter event behind everything emitted so far.
    pub(super) fn queue(&self, event: TerminalEvent) {
        lock_auxiliary(&self.queue).push(event);
    }

    /// Queues a reply the handler computed, in request order with the
    /// emulator's own.
    pub(super) fn queue_pty_write(&self, bytes: Vec<u8>) {
        self.queue(TerminalEvent::PtyWrite(bytes));
    }

    pub(super) fn queued_len(&self) -> usize {
        lock_auxiliary(&self.queue).len()
    }

    /// Drops queued events past `len`, for an event the adapter itself
    /// provoked.
    pub(super) fn truncate_queue(&self, len: usize) {
        lock_auxiliary(&self.queue).truncate(len);
    }

    /// Appends a reply straight to the outputs, for scanner and host answers
    /// that do not go through the parser's order.
    pub(super) fn push_bytes(&mut self, bytes: Vec<u8>) {
        self.responses.push(PtyResponse::Bytes(bytes));
    }

    pub(super) fn push_working_directory(&mut self, report: WorkingDirectoryReport) {
        self.pwd_changes.push(report);
    }

    pub(super) fn set_progress(&mut self, progress: Progress) {
        self.progress_update = Some(progress);
    }

    pub(super) fn push_osc_body(&mut self, body: Vec<u8>) {
        self.osc_bodies.push(body);
    }

    pub(super) fn note_default_color_set(&mut self) {
        self.default_color_set = true;
    }

    /// Sorts the queued events into the typed outputs, in emission order.
    pub(super) fn drain(&mut self) {
        let events = {
            let mut queue = lock_auxiliary(&self.queue);
            mem::take(&mut *queue)
        };
        for event in events {
            match event {
                TerminalEvent::PtyWrite(bytes) => self.push_bytes(bytes),
                TerminalEvent::ColorQuery(query) => {
                    self.responses.push(PtyResponse::ColorQuery(query));
                }
                // Clipboard effects carry only non-empty clipboard-target
                // payloads; empty and selection-target stores are ignored.
                TerminalEvent::ClipboardStore(ClipboardType::Clipboard, text)
                    if !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES =>
                {
                    self.clipboard_writes.push(text.into_bytes());
                }
                TerminalEvent::ClipboardStore(ClipboardType::Clipboard, text)
                    if text.len() > MAX_CLIPBOARD_BYTES =>
                {
                    // `text` is already decoded valid UTF-8, so `len()` is
                    // the decoded OSC 52 store size in bytes. Keep only that
                    // count for the pane's diagnostic; never retain the text.
                    self.dropped_clipboard_store_bytes.push(text.len());
                }
                TerminalEvent::Title(title) => self.title_update = Some(TitleUpdate::Set(title)),
                TerminalEvent::ResetTitle => self.title_update = Some(TitleUpdate::Reset),
                _ => {}
            }
        }
    }

    pub(super) fn take_responses(&mut self) -> Vec<PtyResponse> {
        mem::take(&mut self.responses)
    }

    /// Every drained output at one boundary.
    pub(super) fn take(&mut self) -> TerminalEffects {
        TerminalEffects {
            pty_responses: mem::take(&mut self.responses),
            pwd_changes: mem::take(&mut self.pwd_changes),
            clipboard_writes: mem::take(&mut self.clipboard_writes),
            dropped_clipboard_store_bytes: mem::take(&mut self.dropped_clipboard_store_bytes),
            title_update: self.title_update.take(),
            progress_update: self.progress_update.take(),
            osc_bodies: mem::take(&mut self.osc_bodies),
            default_color_set: mem::take(&mut self.default_color_set),
        }
    }
}
