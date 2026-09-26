use super::*;

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
    pub(super) host_mouse_mode: terminal_setup::HostMouseMode,
    /// Latest physical host theme observations, retained so an endpoint selected after the
    /// observation receives the same client-owned baseline.
    pub(super) host_theme_updates: Vec<crate::protocol::ClientHostThemeUpdate>,
    pub(super) direct_keyboard_protocol: crate::terminal_modes::DirectHostKeyboardState,
    pub(super) pane_keyboard_report_all: bool,
    pub(super) keyboard_report_all_active: bool,
    pub(super) reported_size: (u16, u16),
    pub(super) reported_cell_size: (u32, u32),
    pub(super) pixel_geometry_enabled: bool,
    pub(super) pixel_geometry_exact: bool,
    pub(super) mode: SessionMode,
    pub(super) mouse_scroll_lines: usize,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) repaint_pending: bool,
    /// During a source-off-first endpoint activation the currently blitted frame remains
    /// authoritative until an acknowledged target snapshot/surface pair commits.
    pub(super) presentation_frozen: bool,
    /// Latest explicit Local selection awaiting this client's replacement Local connection.
    pub(super) deferred_local_activation: Option<endpoint::EndpointActivationIntent>,
    pub(super) draw_host_cursor: bool,
    /// Whether this client has written an outer window title since the last
    /// reset. An empty `ui.window_title` means the server never sends one, and
    /// then the host title must be left alone rather than reset to "shepr".
    pub(super) window_title_written: bool,
}

impl Drop for ClientState {
    fn drop(&mut self) {
        if self.mode.is_escape_attach() {
            let _ = crate::terminal_modes::set_direct_host_keyboard_protocol(
                &mut io::stdout(),
                &mut self.direct_keyboard_protocol,
                0,
                0,
            );
        }
    }
}

impl ClientState {
    #[cfg(test)]
    pub(super) fn test_new() -> Self {
        Self {
            blit_encoder: render_ansi::BlitEncoder::new(),
            host_mouse_mode: terminal_setup::HostMouseMode::new(false, false, false),
            host_theme_updates: Vec::new(),
            direct_keyboard_protocol: Default::default(),
            pane_keyboard_report_all: false,
            keyboard_report_all_active: false,
            reported_size: (100, 30),
            reported_cell_size: (0, 0),
            pixel_geometry_enabled: false,
            pixel_geometry_exact: false,
            mode: SessionMode::Shell(Box::new(shell::ClientShellState::new(
                shell::ClientShellConfig::from_config(&crate::config::Config::default()),
            ))),
            mouse_scroll_lines: 3,
            redraw_on_focus_gained: false,
            repaint_pending: false,
            presentation_frozen: false,
            deferred_local_activation: None,
            draw_host_cursor: false,
            window_title_written: false,
        }
    }

    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    pub(super) fn set_host_size(&mut self, cols: u16, rows: u16) {
        let size = terminal_geometry::ClientHostSize::new(cols, rows, self.mode.is_shell());
        self.reported_size = (size.cols, size.rows);
    }

    pub(super) fn freeze_presentation(&mut self) {
        self.presentation_frozen = true;
    }

    pub(super) fn record_host_theme_update(
        &mut self,
        update: &crate::protocol::ClientHostThemeUpdate,
    ) {
        use crate::protocol::ClientHostThemeUpdate;

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
                &crate::protocol::ClientMessage::ClientShellHostTheme {
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
    pub(super) fn present_frozen_chrome(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) {
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
        frame_data: impl Into<frame_output::ComposedFrame>,
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
            let mut stdout = io::stdout();
            stdout.write_all(&encoded.bytes)?;
            stdout.flush()?;
        }
        let committed = self
            .blit_encoder
            .commit_patch(&rows, patch.cursor, &encoded);
        Ok(committed)
    }

    pub(super) fn present_frame(&mut self, frame_data: impl Into<frame_output::ComposedFrame>) {
        let _ = self.try_present_frame(frame_data);
    }

    fn write_composed_output(
        &mut self,
        writer: &mut impl io::Write,
        encoded: &[u8],
    ) -> io::Result<()> {
        frame_output::write_composed_frame(writer.by_ref(), encoded)?;
        writer.flush()
    }

    /// Presents and commits a frame only after all terminal output has been written successfully.
    /// Callers which acknowledge presentation-sensitive work use the return value rather than
    /// treating composition as presentation.
    pub(super) fn try_present_frame(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) -> bool {
        if self.presentation_frozen {
            return false;
        }
        let frame_output::ComposedFrame { frame: frame_data } = frame_data.into();
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
        // Unit tests drive the loop's presentation paths; a full-screen frame written to the
        // test runner's real stdout (libtest only captures `print!`) would scribble on its
        // terminal.
        #[cfg(not(test))]
        let mut stdout = io::stdout();
        #[cfg(test)]
        let mut stdout = io::sink();
        if let Err(error) = self.write_composed_output(&mut stdout, &encoded.bytes) {
            tracing::warn!(%error, "failed to present client frame");
            self.repaint_pending = true;
            return false;
        }
        self.blit_encoder.commit(frame_data, &encoded);
        self.repaint_pending = false;
        true
    }
}
