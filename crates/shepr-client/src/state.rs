use crate::errors::LoopExit;
use crate::limits::{
    REFUSED_OUTPUT_RETRY_GROWTH, REFUSED_OUTPUT_RETRY_MAX, REFUSED_OUTPUT_RETRY_MIN,
};
use crate::loop_config::ClientSettings;
use crate::{endpoint, shell, terminal_setup};
use shepr_termio::blit as render_ansi;
use std::io;
use std::io::Write as _;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PresentationDirty {
    Clean,
    Chrome,
    Pane,
}

fn merge_palette_colors(
    palette: &mut Vec<(u8, shepr_protocol::ClientHostColor)>,
    updates: &[(u8, shepr_protocol::ClientHostColor)],
) {
    for &(index, color) in updates {
        if let Some(slot) = palette
            .iter_mut()
            .find(|(current_index, _)| *current_index == index)
        {
            slot.1 = color;
        } else {
            palette.push((index, color));
        }
    }
}

/// State tracking for the thin client.
///
/// It holds the host modes and the presentation state together: a host-mode write can
/// invalidate the same blit baseline as a pane patch or a shell composition, and the dirty
/// and pending-patch transitions here coordinate all three.
pub(super) struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    pub(super) blit_encoder: render_ansi::BlitEncoder,
    /// Host terminal output shared by frames, host modes, titles, and clipboard writes.
    pub(super) output_writer: Box<dyn io::Write + Send>,
    pub(super) host_modes: terminal_setup::HostModes,
    /// Latest physical host theme observations, retained so an endpoint selected after the
    /// observation receives the same client-owned baseline.
    pub(super) host_theme_updates: Vec<shepr_protocol::ClientHostThemeUpdate>,
    pub(super) settings: ClientSettings,
    /// The host geometry as last observed, already bounded by
    /// `terminal_geometry::bounded_cell_geometry` (at launch and on each resize),
    /// which also fits the grid into one surface frame.
    pub(super) reported_geometry: shepr_core::geometry::HostGeometry,
    /// The client-rendered shell.
    pub(super) shell: Box<shell::ClientShellState>,
    pub(super) repaint_pending: bool,
    pub(super) presentation_dirty: PresentationDirty,
    pub(super) pending_surface_patch: Option<shell::ClientComposedSurfacePatch>,
    /// Frame and pane surface patch writes, which repeat on every presented frame.
    pub(super) frame_write_failure: HostWriteFailure,
    /// A transient mode write is retried after the next client event.
    pub(super) mode_write_failure: HostWriteFailure,
    pub(super) retry_host_modes: bool,
    /// When the client next repaints after the host refused a frame or patch.
    pub(super) refused_output_retry: RefusedOutputRetry,
}

/// The repaint a refused frame or patch owes. A refused write leaves the
/// presentation clean (its change was taken and not shown), so without a
/// deadline of its own nothing would draw it until an unrelated event
/// arrived. The wait doubles while the host keeps refusing, so a host that
/// stays broken is not rewritten on every loop turn, and resets once a
/// frame reaches it.
#[derive(Debug)]
pub(super) struct RefusedOutputRetry {
    due: Option<std::time::Instant>,
    delay: std::time::Duration,
}

impl Default for RefusedOutputRetry {
    fn default() -> Self {
        Self {
            due: None,
            delay: REFUSED_OUTPUT_RETRY_MIN,
        }
    }
}

impl RefusedOutputRetry {
    fn arm(&mut self, now: std::time::Instant) {
        self.due = Some(now + self.delay);
        self.delay = (self.delay * REFUSED_OUTPUT_RETRY_GROWTH).min(REFUSED_OUTPUT_RETRY_MAX);
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Takes the retry if it is due at `now`, keeping the grown delay.
    fn take_due(&mut self, now: std::time::Instant) -> bool {
        if self.due.is_some_and(|due| due <= now) {
            self.due = None;
            true
        } else {
            false
        }
    }
}

impl ClientState {
    /// When the repaint a refused frame or patch owes is due, if one is.
    pub(super) fn refused_output_retry_deadline(&self) -> Option<std::time::Instant> {
        self.refused_output_retry.due
    }

    /// Repaints in full if the retry a refused frame or patch armed is due:
    /// nothing of the refused output was committed, so the next composition
    /// draws the whole current state.
    pub(super) fn retry_refused_output(&mut self, now: std::time::Instant) {
        if self.refused_output_retry.take_due(now) {
            self.request_repaint();
            self.mark_pane_dirty();
        }
    }

    /// Records that the host refused a frame or patch: the next frame repaints
    /// in full, and the loop wakes for it on its own.
    fn note_refused_output(&mut self) {
        self.request_repaint();
        self.refused_output_retry.arm(self.shell.now);
    }

    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    pub(super) fn mark_chrome_dirty(&mut self) {
        if self.pending_surface_patch.take().is_some() {
            self.presentation_dirty = PresentationDirty::Pane;
        } else if self.presentation_dirty == PresentationDirty::Clean {
            self.presentation_dirty = PresentationDirty::Chrome;
        }
    }

    pub(super) fn mark_pane_dirty(&mut self) {
        self.pending_surface_patch = None;
        self.presentation_dirty = PresentationDirty::Pane;
    }

    /// Shows an endpoint notice and presents the chrome. It decides nothing about what is
    /// shown: the choice already says that.
    pub(super) fn present_notice(&mut self, notice: &shell::EndpointNotice) {
        self.shell.receive_endpoint_unavailable(notice);
        self.mark_chrome_dirty();
    }

    pub(super) fn queue_surface_patch(&mut self, patch: shell::ClientComposedSurfacePatch) {
        if self.presentation_dirty == PresentationDirty::Clean
            && self.pending_surface_patch.is_none()
        {
            self.pending_surface_patch = Some(patch);
        } else {
            self.pending_surface_patch = None;
        }
        self.presentation_dirty = PresentationDirty::Pane;
    }

    pub(super) fn take_presentation_dirty(&mut self) -> PresentationDirty {
        std::mem::replace(&mut self.presentation_dirty, PresentationDirty::Clean)
    }

    pub(super) fn record_host_mode_write(
        &mut self,
        operation: &'static str,
        result: io::Result<()>,
    ) -> Result<(), LoopExit> {
        let action = self.mode_write_failure.observe(
            HostWritePurpose::TerminalMode,
            operation,
            &result,
            None,
        );
        let Err(error) = result else {
            return Ok(());
        };
        match action {
            HostWriteAction::Fatal => Err(LoopExit::HostTerminal(error)),
            HostWriteAction::Retry => {
                self.retry_host_modes = true;
                Ok(())
            }
            HostWriteAction::Succeeded | HostWriteAction::Continue => Ok(()),
        }
    }

    pub(super) fn record_host_theme_update(
        &mut self,
        update: &shepr_protocol::ClientHostThemeUpdate,
    ) {
        use shepr_protocol::ClientHostThemeUpdate;

        match update {
            ClientHostThemeUpdate::DefaultColor { kind, .. } => {
                self.host_theme_updates.retain(|current| {
                    !matches!(
                        current,
                        ClientHostThemeUpdate::DefaultColor {
                            kind: current_kind,
                            ..
                        } if current_kind == kind
                    )
                });
            }
            ClientHostThemeUpdate::PaletteColors(colors) => {
                // A host palette reply can arrive in idle-flushed chunks. Keep
                // the latest value for every index so a later endpoint gets the
                // full observed palette when this baseline is replayed.
                let current = self
                    .host_theme_updates
                    .iter_mut()
                    .find_map(|update| match update {
                        ClientHostThemeUpdate::PaletteColors(current) => Some(current),
                        _ => None,
                    });
                if let Some(current) = current {
                    merge_palette_colors(current, colors);
                } else {
                    let mut current = Vec::new();
                    merge_palette_colors(&mut current, colors);
                    self.host_theme_updates
                        .push(ClientHostThemeUpdate::PaletteColors(current));
                }
                return;
            }
            ClientHostThemeUpdate::Appearance(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::Appearance(_))),
        }
        self.host_theme_updates.push(update.clone());
    }

    pub(super) fn present_surface_patch(
        &mut self,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<SurfacePatchPresentation> {
        if self.repaint_pending {
            return Ok(SurfacePatchPresentation::FullFrameRequired);
        }
        let rows = patch.rows;
        let Some(encoded) = self.blit_encoder.encode_patch(&rows, patch.cursor.as_ref()) else {
            return Ok(SurfacePatchPresentation::FullFrameRequired);
        };
        if !encoded.bytes.is_empty() {
            self.write_composed_output(&encoded.bytes)?;
        }
        let committed = self
            .blit_encoder
            .commit_patch(&rows, patch.cursor, &encoded);
        Ok(if committed {
            SurfacePatchPresentation::Presented
        } else {
            SurfacePatchPresentation::FullFrameRequired
        })
    }

    fn write_composed_output(&mut self, encoded: &[u8]) -> io::Result<()> {
        self.output_writer.write_all(encoded)?;
        self.output_writer.flush()
    }

    /// Presents a composed frame, whether its change is client chrome only (machine statuses
    /// and diagnostics, the machine list, overlays, notices and modes), a pane change from the
    /// shown endpoint, or a committed endpoint move. It always writes. That is sound for a
    /// chrome-only change too because the pane cells in every frame are coherent: only the
    /// shown endpoint's snapshots project and only its surfaces apply, so while nothing is
    /// shown the last coherent cells stay, and a move installs the target's pair only at its
    /// commit.
    ///
    /// What composing the frame decided is committed to the shell only once the host took
    /// the frame, so nothing starts for a frame that never reached the screen.
    pub(super) fn present_frame(&mut self, composed: shell::ComposedFrame) {
        let shell::ComposedFrame { frame, commit } = composed;
        if self.write_frame(frame) {
            self.shell.commit_frame(commit);
        }
    }

    fn compose(&self) -> Option<shell::ComposedFrame> {
        self.shell
            .compose_frame(self.reported_geometry.cols(), self.reported_geometry.rows())
    }

    /// Presents the accumulated change once after a client turn. Pane work dominates chrome,
    /// and a full composition dominates a queued patch when both happened in the same turn.
    pub(super) fn present_pending(&mut self) {
        match self.take_presentation_dirty() {
            PresentationDirty::Clean => {}
            PresentationDirty::Chrome => {
                if let Some(composed) = self.compose() {
                    self.present_frame(composed);
                }
            }
            PresentationDirty::Pane => match self.pending_surface_patch.take() {
                None => {
                    if let Some(composed) = self.compose() {
                        self.present_frame(composed);
                    }
                }
                Some(patch) => {
                    let context = self.shell.presentation_log_context();
                    match self.present_surface_patch(patch) {
                        Ok(SurfacePatchPresentation::Presented) => {}
                        Ok(SurfacePatchPresentation::FullFrameRequired) => {
                            if let Some(composed) = self.compose() {
                                self.present_frame(composed);
                            }
                        }
                        Err(error) => {
                            self.frame_write_failure.observe(
                                HostWritePurpose::Frame,
                                "pane surface patch",
                                &Err(error),
                                Some(&context),
                            );
                            self.note_refused_output();
                        }
                    }
                }
            },
        }
    }

    /// Writes and commits a frame only after all terminal output has been written successfully.
    /// A failed write is handled here rather than by callers: the frame is not committed, the
    /// next frame repaints in full (`repaint_pending`) and is scheduled on the loop's timer
    /// (`refused_output_retry`), and the failure is logged once per cause through
    /// `frame_write_failure` rather than once per frame.
    /// Callers have no separate recovery action, so the write result stays owned by this state;
    /// the returned flag only says whether the host took the frame.
    fn write_frame(&mut self, frame_data: shepr_protocol::FrameData) -> bool {
        let encoded = self.blit_encoder.encode(&frame_data, self.repaint_pending);
        let written = self.write_composed_output(&encoded.bytes);
        // Built only for a failed write: every frame passes through here.
        let context = written
            .is_err()
            .then(|| self.shell.presentation_log_context());
        if self.frame_write_failure.observe(
            HostWritePurpose::Frame,
            "client frame",
            &written,
            context.as_ref(),
        ) != HostWriteAction::Succeeded
        {
            self.note_refused_output();
            return false;
        }
        self.blit_encoder.commit(frame_data, &encoded);
        self.repaint_pending = false;
        self.refused_output_retry.clear();
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SurfacePatchPresentation {
    Presented,
    FullFrameRequired,
}

/// Tracks a host terminal write that repeats on every frame or event, so a persistent failure
/// is logged once per cause instead of once per write. A cause is the error kind: a change of
/// kind logs again, and the first success after a failure logs the recovery.
#[derive(Debug, Default)]
pub(super) struct HostWriteFailure {
    failing: Option<io::ErrorKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostWritePurpose {
    TerminalMode,
    Frame,
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostWriteAction {
    Succeeded,
    Retry,
    Continue,
    Fatal,
}

/// The client can retry stateful input modes because their desired state is retained. A
/// permanent mode failure ends the session because the host may now interpret keys or mouse
/// reports differently from the client. Frames retain a repaint request; probe writes have
/// a fallback, so those failures leave the client running.
/// A failed clipboard copy is only logged at its call site and never reaches this policy.
pub(super) fn host_write_failure_action(
    purpose: HostWritePurpose,
    error: io::ErrorKind,
) -> HostWriteAction {
    if matches!(
        error,
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        return if purpose == HostWritePurpose::TerminalMode {
            HostWriteAction::Retry
        } else {
            HostWriteAction::Continue
        };
    }
    if purpose == HostWritePurpose::TerminalMode {
        // A permanent failure while changing input modes leaves host key and mouse
        // interpretation uncertain; cosmetic and repaintable output can be retried locally.
        HostWriteAction::Fatal
    } else {
        HostWriteAction::Continue
    }
}

struct EndpointLogValue<'a>(Option<&'a endpoint::ClientEndpointId>);

impl std::fmt::Display for EndpointLogValue<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(endpoint) => write!(formatter, "{endpoint}"),
            None => formatter.write_str("None"),
        }
    }
}

impl HostWriteFailure {
    /// Records one write and applies the shared failure policy for its purpose.
    pub(super) fn observe(
        &mut self,
        purpose: HostWritePurpose,
        // This label identifies the concrete write site; HostWritePurpose carries the policy.
        write: &'static str,
        result: &io::Result<()>,
        context: Option<&shell::ClientPresentationLogContext>,
    ) -> HostWriteAction {
        match result {
            Ok(()) => {
                if let Some(kind) = self.failing.take() {
                    tracing::info!(
                        write,
                        previous_error_kind = %kind,
                        "host terminal write recovered"
                    );
                }
                HostWriteAction::Succeeded
            }
            Err(error) => {
                if self.failing != Some(error.kind()) {
                    tracing::warn!(
                        write,
                        endpoint = %EndpointLogValue(context.map(|context| &context.endpoint)),
                        generation = ?context.and_then(|context| context.generation.as_ref()),
                        projection_revision = ?context.and_then(|context| context.projection_revision.as_ref()),
                        surface_revision = ?context.and_then(|context| context.surface_revision.as_ref()),
                        boot_id = ?context.and_then(|context| context.boot_id.as_ref()),
                        pane_ids = ?context.map(|context| &context.pane_ids),
                        error = %error,
                        "host terminal write failed; repeats of this failure are not logged until a write succeeds"
                    );
                    self.failing = Some(error.kind());
                }
                host_write_failure_action(purpose, error.kind())
            }
        }
    }
}

#[cfg(test)]
impl ClientState {
    pub(super) fn test_new() -> Self {
        use shepr_test_fixtures::ValidatedClientConfigFixture as _;
        let config = shepr_config::ValidatedClientConfig::test_default();
        Self {
            blit_encoder: render_ansi::BlitEncoder::new(),
            output_writer: Box::new(io::sink()),
            host_modes: terminal_setup::HostModes::new(false),
            host_theme_updates: Vec::new(),
            settings: ClientSettings::from_config(&config),
            reported_geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(100, 30),
                shepr_core::geometry::HostCell::Unknown,
            ),
            shell: Box::new(shell::ClientShellState::new(
                shell::ClientShellConfig::from_validated_config(&config),
            )),
            repaint_pending: false,
            presentation_dirty: PresentationDirty::Clean,
            pending_surface_patch: None,
            frame_write_failure: HostWriteFailure::default(),
            mode_write_failure: HostWriteFailure::default(),
            retry_host_modes: false,
            refused_output_retry: RefusedOutputRetry::default(),
        }
    }

    pub(super) fn test_new_with_writer(writer: impl io::Write + Send + 'static) -> Self {
        let mut state = Self::test_new();
        state.output_writer = Box::new(writer);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use shepr_protocol::{CellData, PaneSurfacePatchRow, WireColor, WireStyle};
    use shepr_surface::ratatui_conversion::FrameDataExt as _;
    use std::sync::{Arc, Mutex};

    #[test]
    fn an_interrupted_machine_switch_names_the_machine_and_reads_as_one_sentence() {
        let buildbox = endpoint::ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("buildbox").expect("machine label"),
        );
        assert_eq!(
            shell::EndpointNotice::new(
                buildbox,
                shell::EndpointNoticeKind::MoveInterrupted("connection was lost; reconnecting"),
            )
            .body(&shepr_config::MachineLabel::parse("desk").expect("local label")),
            "machine switch interrupted: buildbox connection was lost; reconnecting"
        );
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("test output lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A host terminal that refuses every write.
    struct BrokenHost;

    impl io::Write for BrokenHost {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_the_host_refused_starts_no_notice_lifetime() {
        let mut state = ClientState::test_new_with_writer(BrokenHost);
        state.present_notice(&shell::EndpointNotice::new(
            endpoint::ClientEndpointId::Local,
            shell::EndpointNoticeKind::NotReady,
        ));
        state.present_pending();
        assert!(
            state.repaint_pending,
            "the refused frame asks for a repaint"
        );
        assert_eq!(
            state.shell.next_timer_deadline(),
            None,
            "the notice was never on screen, so its lifetime has not started"
        );

        state.output_writer = Box::new(io::sink());
        state.mark_chrome_dirty();
        state.present_pending();
        assert!(!state.repaint_pending);
        assert!(
            state.shell.next_timer_deadline().is_some(),
            "the frame that reached the host starts the notice's lifetime"
        );
    }

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            grid_width: shepr_protocol::GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            hyperlink: None,
        }
    }

    #[test]
    fn composed_frames_and_surface_patches_use_the_injected_writer() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut state = ClientState::test_new_with_writer(SharedWriter(Arc::clone(&output)));
        let frame = shepr_protocol::FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::with_lines(["a"]),
            None,
            &[],
        )
        .expect("test buffer is a valid frame");

        assert!(state.write_frame(frame));
        let frame_bytes = output.lock().expect("test output lock");
        assert!(frame_bytes.contains(&b'a'));
        drop(frame_bytes);

        output.lock().expect("test output lock").clear();
        let presented = state
            .present_surface_patch(shell::ClientComposedSurfacePatch {
                rows: vec![PaneSurfacePatchRow {
                    x: 0,
                    y: 0,
                    cells: vec![cell("b")],
                }],
                cursor: None,
            })
            .expect("surface patch writes through the injected writer");

        assert_eq!(presented, SurfacePatchPresentation::Presented);
        assert!(output.lock().expect("test output lock").contains(&b'b'));
    }
}
