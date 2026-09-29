use super::*;
use std::io::Write as _;

pub(super) type ShellSession = shell::ClientShellState;

/// Direct attach can use the raw byte stream or intercept its configured escape keys.
pub(super) struct AttachSession {
    pub(super) escape: Option<AttachEscapeState>,
}

/// The mutually exclusive interaction mode owned by the client session.
pub(super) enum SessionMode {
    Shell(Box<ShellSession>),
    DirectAttach(AttachSession),
}

impl SessionMode {
    pub(super) fn is_shell(&self) -> bool {
        matches!(self, Self::Shell(_))
    }

    pub(super) fn shell(&self) -> Option<&ShellSession> {
        match self {
            Self::Shell(shell) => Some(shell.as_ref()),
            Self::DirectAttach(_) => None,
        }
    }

    pub(super) fn shell_mut(&mut self) -> Option<&mut ShellSession> {
        match self {
            Self::Shell(shell) => Some(shell.as_mut()),
            Self::DirectAttach(_) => None,
        }
    }

    pub(super) fn attach_escape_mut(&mut self) -> Option<&mut AttachEscapeState> {
        match self {
            Self::Shell(_) => None,
            Self::DirectAttach(session) => session.escape.as_mut(),
        }
    }

    pub(super) fn is_escape_attach(&self) -> bool {
        matches!(self, Self::DirectAttach(AttachSession { escape: Some(_) }))
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
    pub(super) mode: SessionMode,
    pub(super) repaint_pending: bool,
    /// During a source-off-first endpoint activation the currently blitted frame remains
    /// authoritative until an acknowledged target snapshot/surface pair commits.
    pub(super) presentation_frozen: bool,
    /// Latest explicit Local selection awaiting this client's replacement Local connection.
    pub(super) deferred_local_activation: Option<endpoint::EndpointActivationIntent>,
    pub(super) draw_host_cursor: bool,
    /// Frame and pane surface patch writes, which repeat on every presented frame.
    pub(super) frame_write_failure: HostWriteFailure,
    /// Window title writes, which repeat on every title change.
    pub(super) title_write_failure: HostWriteFailure,
}

impl ClientState {
    #[cfg(test)]
    pub(super) fn test_new() -> Self {
        use shepr_test_fixtures::ValidatedConfigFixture as _;
        let config = shepr_config::ValidatedConfig::test_default();
        Self {
            blit_encoder: render_ansi::BlitEncoder::new(),
            output_writer: Box::new(io::sink()),
            host_modes: terminal_setup::HostModes::new(false, false, false),
            host_theme_updates: Vec::new(),
            settings: ClientSettings::from_config(&config),
            reported_geometry: shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
            mode: SessionMode::Shell(Box::new(shell::ClientShellState::new(
                shell::ClientShellConfig::from_validated_config(&config),
            ))),
            repaint_pending: false,
            presentation_frozen: false,
            deferred_local_activation: None,
            draw_host_cursor: false,
            frame_write_failure: HostWriteFailure::default(),
            title_write_failure: HostWriteFailure::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn test_new_with_writer(writer: impl io::Write + Send + 'static) -> Self {
        let mut state = Self::test_new();
        state.output_writer = Box::new(writer);
        state
    }

    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    pub(super) fn set_host_size(&mut self, cols: u16, rows: u16) {
        let size = terminal_geometry::ClientHostSize::new(cols, rows, self.mode.is_shell());
        self.reported_geometry = shepr_core::geometry::HostGeometry::new(
            size.cols,
            size.rows,
            self.reported_geometry.cell_width(),
            self.reported_geometry.cell_height(),
            self.reported_geometry.exact,
        );
    }

    pub(super) fn freeze_presentation(&mut self) {
        self.presentation_frozen = true;
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

    /// Replay the retained physical-host baseline only after an endpoint owns the committed
    /// presentation. The endpoint transport preserves this order ahead of the resync control.
    pub(super) fn replay_host_theme(
        &self,
        endpoints: &mut endpoint::EndpointRegistry,
        endpoint_id: &endpoint::ClientEndpointId,
    ) {
        for update in &self.host_theme_updates {
            let _ = endpoints.send_to(
                endpoint_id,
                &shepr_protocol::ClientMessage::ClientShellHostTheme {
                    update: update.clone(),
                },
            );
        }
    }

    pub(super) fn unfreeze_presentation(&mut self) {
        self.presentation_frozen = false;
        // A resize or metadata event may have happened while frozen. Force a full frame rather
        // than attempting to patch the old source frame.
        self.request_repaint();
    }

    /// Present a composed error/chrome frame while retaining the handoff input freeze. The pane
    /// cells are still the last coherent surface; only client chrome (including the error) moves.
    /// That holds because the client loop does not advance the pane projection while frozen:
    /// active-endpoint snapshots are cached rather than projected, and non-handoff pane surfaces
    /// and patches are dropped. A handoff commit installs a fresh coherent pair on unfreeze.
    pub(super) fn present_frozen_chrome(&mut self, frame_data: shepr_protocol::FrameData) {
        let frozen = self.presentation_frozen;
        self.presentation_frozen = false;
        self.present_frame(frame_data);
        self.presentation_frozen = frozen;
    }

    /// Presents a frame whose change is client chrome only: machine statuses and diagnostics,
    /// the machine list, overlays and modes. With no handoff in flight it passes a freeze left
    /// by `present_handoff_unavailable` (see `present_frozen_chrome` for why that is sound), so
    /// the machine list keeps showing live status while no endpoint owns presentation. During
    /// a handoff it obeys the freeze: the source frame stays authoritative, and the commit
    /// that ends the freeze repaints in full.
    pub(super) fn present_chrome(
        &mut self,
        frame_data: shepr_protocol::FrameData,
        handoff_in_flight: bool,
    ) {
        if handoff_in_flight {
            self.present_frame(frame_data);
        } else {
            self.present_frozen_chrome(frame_data);
        }
    }

    pub(super) fn present_surface_patch(
        &mut self,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<bool> {
        if self.presentation_frozen || self.repaint_pending {
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

    pub(super) fn presentation_log_context(&self) -> Option<shell::ClientPresentationLogContext> {
        self.mode
            .shell()
            .map(shell::ClientShellState::presentation_log_context)
    }

    /// Presents and commits a frame only after all terminal output has been written successfully.
    /// A failed write is handled here rather than by callers: the frame is not committed, the
    /// next frame repaints in full (`repaint_pending`), and the failure is logged once per cause
    /// through `frame_write_failure` rather than once per frame.
    /// Callers have no separate recovery action, so the write result stays owned by this state.
    pub(super) fn present_frame(&mut self, frame_data: shepr_protocol::FrameData) {
        if self.presentation_frozen {
            return;
        }
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
            .then(|| self.presentation_log_context())
            .flatten();
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
