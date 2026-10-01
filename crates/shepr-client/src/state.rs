use super::*;
use std::io::Write as _;

/// Who owns the host presentation: the pane cells on screen and the pane input behind them.
/// The client shows one endpoint at a time, and this is the one place that says which, or that
/// a handoff or nothing owns it. Client chrome (machine list, statuses, overlays, notices) is
/// the client's own and presents in every state except a frozen handoff.
pub(super) enum Presentation {
    /// The registry's active endpoint owns the presentation: its pane surfaces and patches
    /// present, its snapshots project, and a pick of it is a no-op.
    Owned,
    /// A handoff owns the presentation lane until it commits (back to `Owned`) or cannot make
    /// either side safe (`Unavailable`). While its phase has not installed a coherent pair
    /// (`PendingEndpointActivation::freezes_frame`) the source frame stays authoritative, even
    /// for chrome; the commit repaints in full.
    Handoff(Box<endpoint::PendingEndpointActivation>),
    /// No endpoint has proved it owns the presentation: a handoff could not restore either
    /// side, the committed endpoint went away, or Local was unreachable at launch. The last
    /// coherent pane cells stay up and no pane output or projection change is taken, which is
    /// what lets chrome frames pass. The way out is a handoff: to a reconnected endpoint, to
    /// the user's next pick (the active endpoint included), or the automatic re-proof of an
    /// endpoint whose connection survived.
    Unavailable,
}

impl Presentation {
    /// Whether pane frames are held back: always while unavailable, and during a handoff until
    /// its committing pair is on screen.
    pub(crate) fn frames_frozen(&self) -> bool {
        match self {
            Self::Owned => false,
            Self::Handoff(activation) => activation.freezes_frame(),
            Self::Unavailable => true,
        }
    }

    pub(crate) fn handoff(&self) -> Option<&endpoint::PendingEndpointActivation> {
        match self {
            Self::Handoff(activation) => Some(&**activation),
            Self::Owned | Self::Unavailable => None,
        }
    }

    pub(crate) fn handoff_mut(&mut self) -> Option<&mut endpoint::PendingEndpointActivation> {
        match self {
            Self::Handoff(activation) => Some(&mut **activation),
            Self::Owned | Self::Unavailable => None,
        }
    }

    pub(crate) fn handoff_in_flight(&self) -> bool {
        matches!(self, Self::Handoff(_))
    }

    pub(crate) fn owned(&self) -> bool {
        matches!(self, Self::Owned)
    }

    /// Takes an in-flight handoff out to be abandoned. The presentation is `Unavailable` until
    /// the caller installs its replacement.
    pub(crate) fn take_handoff(&mut self) -> Option<Box<endpoint::PendingEndpointActivation>> {
        match std::mem::replace(self, Self::Unavailable) {
            Self::Handoff(activation) => Some(activation),
            other => {
                *self = other;
                None
            }
        }
    }
}

/// State tracking for the thin client.
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
    pub(super) reported_geometry: shepr_core::geometry::HostGeometry,
    /// The client-rendered shell.
    pub(super) shell: Box<shell::ClientShellState>,
    pub(super) repaint_pending: bool,
    pub(super) presentation: Presentation,
    /// The latest explicit Local selection, waiting for a Local connection with metadata. It
    /// is not a presentation state: whatever owns the presentation keeps it meanwhile, a remote
    /// handoff in flight included. It is newer than any successor that handoff retains, and a
    /// newer selection of any endpoint replaces it.
    pub(super) deferred_local: Option<endpoint::EndpointActivationIntent>,
    pub(super) draw_host_cursor: bool,
    /// Frame and pane surface patch writes, which repeat on every presented frame.
    pub(super) frame_write_failure: HostWriteFailure,
    /// Window title writes, which repeat on every title change.
    pub(super) title_write_failure: HostWriteFailure,
}

impl ClientState {
    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    pub(super) fn set_host_size(&mut self, cols: u16, rows: u16) {
        let size = terminal_geometry::ClientHostSize::new(cols, rows);
        self.reported_geometry = shepr_core::geometry::HostGeometry::new(
            size.cols,
            size.rows,
            self.reported_geometry.cell_width(),
            self.reported_geometry.cell_height(),
            self.reported_geometry.exact,
        );
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
            ClientHostThemeUpdate::PaletteColors(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::PaletteColors(_))),
            ClientHostThemeUpdate::Appearance(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::Appearance(_))),
        }
        self.host_theme_updates.push(update.clone());
    }

    /// Ends a handoff: `Owned` on a commit, `Unavailable` when neither side can be made safe.
    /// A resize or metadata event may have happened while the frame was frozen, so the next
    /// frame is written in full rather than patched over the old source frame.
    pub(super) fn end_handoff(&mut self, next: Presentation) {
        self.presentation = next;
        self.request_repaint();
    }

    /// Presents a chrome or error frame through any freeze. The pane cells in it are still the
    /// last coherent surface; only client chrome moves. That holds because nothing advances the
    /// pane projection while frames are frozen: active-endpoint snapshots are cached rather than
    /// projected, and pane surfaces and patches outside a handoff are dropped. A handoff commit
    /// installs a fresh coherent pair.
    pub(super) fn present_chrome_through_freeze(&mut self, frame_data: shepr_protocol::FrameData) {
        self.write_frame(frame_data);
    }

    /// Presents a frame whose change is client chrome only: machine statuses and diagnostics,
    /// the machine list, overlays and modes. With no endpoint owning the presentation it passes
    /// the freeze (see `present_chrome_through_freeze` for why that is sound), so the machine
    /// list keeps showing live status. During a handoff it obeys the handoff's freeze: the
    /// source frame stays authoritative, and the commit repaints in full.
    pub(super) fn present_chrome(&mut self, frame_data: shepr_protocol::FrameData) {
        if self.presentation.handoff_in_flight() {
            self.present_frame(frame_data);
        } else {
            self.write_frame(frame_data);
        }
    }

    pub(super) fn present_surface_patch(
        &mut self,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<bool> {
        if self.presentation.frames_frozen() || self.repaint_pending {
            return Ok(false);
        }
        let rows = if self.draw_host_cursor {
            let Some(rows) = self
                .blit_encoder
                .patch_rows_with_drawn_cursor(&patch.rows, patch.cursor.as_ref())
            else {
                return Ok(false);
            };
            rows
        } else {
            patch.rows
        };
        let Some(encoded) =
            self.blit_encoder
                .encode_patch(&rows, patch.cursor.clone(), self.draw_host_cursor)
        else {
            return Ok(false);
        };
        if !encoded.bytes.is_empty() {
            self.write_composed_output(&encoded.bytes)?;
        }
        let committed = self
            .blit_encoder
            .commit_patch(&rows, patch.cursor, &encoded);
        Ok(committed)
    }

    fn write_composed_output(&mut self, encoded: &[u8]) -> io::Result<()> {
        self.output_writer.write_all(encoded)?;
        self.output_writer.flush()
    }

    /// Presents a frame unless pane frames are frozen (see `Presentation::frames_frozen`).
    pub(super) fn present_frame(&mut self, frame_data: shepr_protocol::FrameData) {
        if self.presentation.frames_frozen() {
            return;
        }
        self.write_frame(frame_data);
    }

    /// Writes and commits a frame only after all terminal output has been written successfully.
    /// A failed write is handled here rather than by callers: the frame is not committed, the
    /// next frame repaints in full (`repaint_pending`), and the failure is logged once per cause
    /// through `frame_write_failure` rather than once per frame.
    /// Callers have no separate recovery action, so the write result stays owned by this state.
    fn write_frame(&mut self, frame_data: shepr_protocol::FrameData) {
        let frame_data = if self.draw_host_cursor {
            render_ansi::frame_with_drawn_cursor(frame_data)
        } else {
            frame_data
        };
        let encoded = if self.draw_host_cursor {
            self.blit_encoder
                .encode_with_suppressed_visible_cursor(&frame_data, self.repaint_pending)
        } else {
            self.blit_encoder.encode(&frame_data, self.repaint_pending)
        };
        let written = self.write_composed_output(&encoded.bytes);
        // Built only for a failed write: every frame passes through here.
        let context = written
            .is_err()
            .then(|| self.shell.presentation_log_context());
        if !self
            .frame_write_failure
            .observe("client frame", &written, context.as_ref())
        {
            self.repaint_pending = true;
            return;
        }
        self.blit_encoder.commit(frame_data, &encoded);
        self.repaint_pending = false;
    }
}

/// Tracks a host terminal write that repeats on every frame or event, so a persistent failure
/// is logged once per cause instead of once per write. A cause is the error kind: a change of
/// kind logs again, and the first success after a failure logs the recovery.
#[derive(Debug, Default)]
pub(super) struct HostWriteFailure {
    failing: Option<io::ErrorKind>,
}

impl HostWriteFailure {
    /// Records one write's outcome and returns whether it succeeded.
    pub(super) fn observe(
        &mut self,
        write: &'static str,
        result: &io::Result<()>,
        context: Option<&shell::ClientPresentationLogContext>,
    ) -> bool {
        match result {
            Ok(()) => {
                if let Some(kind) = self.failing.take() {
                    tracing::info!(
                        write,
                        previous_error_kind = %kind,
                        "host terminal write recovered"
                    );
                }
                true
            }
            Err(error) => {
                if self.failing != Some(error.kind()) {
                    tracing::warn!(
                        write,
                        endpoint = ?context.map(|context| context.endpoint.as_str()),
                        generation = ?context.and_then(|context| context.generation),
                        projection_revision = ?context.and_then(|context| context.projection_revision),
                        surface_revision = ?context.and_then(|context| context.surface_revision),
                        boot_id = ?context.and_then(|context| context.boot_id.as_deref()),
                        pane_ids = ?context.map(|context| &context.pane_ids),
                        error = %error,
                        "host terminal write failed; repeats of this failure are not logged until a write succeeds"
                    );
                    self.failing = Some(error.kind());
                }
                false
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
            host_modes: terminal_setup::HostModes::new(false, false),
            host_theme_updates: Vec::new(),
            settings: ClientSettings::from_config(&config),
            reported_geometry: shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
            shell: Box::new(shell::ClientShellState::new(
                shell::ClientShellConfig::from_validated_config(&config),
            )),
            repaint_pending: false,
            presentation: Presentation::Owned,
            deferred_local: None,
            draw_host_cursor: false,
            frame_write_failure: HostWriteFailure::default(),
            title_write_failure: HostWriteFailure::default(),
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
    use std::sync::{Arc, Mutex};

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

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            grid_width: shepr_protocol::GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            skip: false,
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
        );

        state.present_frame(frame);
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

        assert!(presented);
        assert!(output.lock().expect("test output lock").contains(&b'b'));
    }
}
