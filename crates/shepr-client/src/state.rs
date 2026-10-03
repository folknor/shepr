use super::*;
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
    pub(super) presentation_dirty: PresentationDirty,
    pub(super) pending_surface_patch: Option<shell::ClientComposedSurfacePatch>,
    /// What is shown and the move toward what is selected; held in memory only, every
    /// client starts on Local.
    pub(super) choice: endpoint::EndpointChoice,
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

    /// Presents a frame whose change is client chrome only: machine statuses and diagnostics,
    /// the machine list, overlays, notices and modes. It always writes. That is sound because
    /// the pane cells in it are always coherent: only the shown endpoint's snapshots project
    /// and only its surfaces apply, so while nothing is shown the last coherent cells stay,
    /// and a move installs the target's pair only at its commit.
    pub(super) fn present_chrome(&mut self, frame_data: shepr_protocol::FrameData) {
        self.write_frame(frame_data);
    }

    pub(super) fn present_surface_patch(
        &mut self,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<bool> {
        if self.repaint_pending {
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

    /// Presents a frame from the shown endpoint or a committed endpoint move.
    pub(super) fn present_frame(&mut self, frame_data: shepr_protocol::FrameData) {
        self.write_frame(frame_data);
    }

    /// Presents the accumulated change once after a client turn. Pane work dominates chrome,
    /// and a full composition dominates a queued patch when both happened in the same turn.
    pub(super) fn present_pending(&mut self) {
        match self.take_presentation_dirty() {
            PresentationDirty::Clean => {}
            PresentationDirty::Chrome => {
                if let Some(frame) = self
                    .shell
                    .compose(self.reported_geometry.cols(), self.reported_geometry.rows())
                {
                    self.present_chrome(frame);
                }
            }
            PresentationDirty::Pane => match self.pending_surface_patch.take() {
                None => {
                    if let Some(frame) = self
                        .shell
                        .compose(self.reported_geometry.cols(), self.reported_geometry.rows())
                    {
                        self.present_frame(frame);
                    }
                }
                Some(patch) => {
                    let context = self.shell.presentation_log_context();
                    match self.present_surface_patch(patch) {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Some(frame) = self.shell.compose(
                                self.reported_geometry.cols(),
                                self.reported_geometry.rows(),
                            ) {
                                self.present_frame(frame);
                            }
                        }
                        Err(error) => {
                            self.frame_write_failure.observe(
                                "pane surface patch",
                                &Err(error),
                                Some(&context),
                            );
                            self.request_repaint();
                        }
                    }
                }
            },
        }
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
                        endpoint = %EndpointLogValue(context.map(|context| &context.endpoint)),
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
            presentation_dirty: PresentationDirty::Clean,
            pending_surface_patch: None,
            choice: endpoint::EndpointChoice::showing(endpoint::ClientEndpointId::Local),
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
