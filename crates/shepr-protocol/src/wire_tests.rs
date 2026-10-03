use super::*;
use serde::Serialize;
use shepr_core::geometry::SplitBranch;
use std::io::{self, Read};

#[cfg(test)]
mod tests {
    use super::*;

    use super::codec::{self, CodecError};
    use ratatui::style::{Color, Modifier};
    use serde::de::DeserializeOwned;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// The client-side string-carrying message the framing tests use as a vehicle.
    fn paste(text: String) -> ClientMessage {
        ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p1".parse().expect("test pane id"),
            events: vec![crate::ClientPaneInputEvent::Paste(text)],
        }
    }

    #[test]
    fn client_surface_clamp_fits_server_geometry_limit() {
        let surface = ClientSurfaceSize {
            cols: u16::MAX,
            rows: u16::MAX,
        }
        .clamped();
        assert_eq!(surface.cols, MAX_SURFACE_DIMENSION);
        assert!(surface.rows > 0);
        assert!(usize::from(surface.cols) * usize::from(surface.rows) <= MAX_SURFACE_CELLS);
        assert_eq!(
            ClientSurfaceSize { cols: 80, rows: 24 }.clamped(),
            ClientSurfaceSize { cols: 80, rows: 24 }
        );
    }

    /// Encodes and decodes `value` with the wire codec, requiring the decoder
    /// to consume every encoded byte.
    fn roundtrip<T: Serialize + DeserializeOwned>(value: &T) -> Result<T, CodecError> {
        codec::from_slice_exact(&codec::to_vec(value)?)
    }

    // ---- Round-trip: ClientMessage ----

    #[test]
    fn endpoint_hello_roundtrip() -> TestResult {
        let msg = ClientMessage::EndpointHello(crate::endpoint::EndpointClientHello {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, true),
            mouse_capture: false,
            surface_active: true,
        });
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_resize_roundtrip() -> TestResult {
        let msg = ClientMessage::ClientShellResize {
            geometry: super::TerminalGeometry::new(74, 29, 8, 16, true),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_pane_input_roundtrips_semantic_keys() -> TestResult {
        let message = ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p2".into(),
            events: vec![
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('l'),
                    modifiers: crate::WireModifiers::SHIFT,
                    kind: ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: Some('L'),
                    generated_text: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('7'),
                    modifiers: crate::WireModifiers::CONTROL,
                    kind: ClientKeyKind::Press,
                    repeat_count: 3,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('x'),
                    modifiers: crate::WireModifiers::SUPER
                        | crate::WireModifiers::HYPER
                        | crate::WireModifiers::META,
                    kind: ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                },
            ],
        };
        let decoded = roundtrip(&message)?;
        assert_eq!(decoded, message);
        Ok(())
    }

    #[test]
    fn client_shell_pane_input_batch_is_bounded_at_decode() -> TestResult {
        let input = |count: usize| ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p1".parse().expect("test pane id"),
            events: vec![ClientPaneInputEvent::TextCommit(String::new()); count],
        };
        let at_cap = input(MAX_INPUT_EVENT_BATCH);
        assert_eq!(roundtrip(&at_cap)?, at_cap);
        assert!(codec::to_vec(&input(MAX_INPUT_EVENT_BATCH + 1)).is_err());

        // A peer is not bound by the encoder: splice an over-cap list behind
        // the encoding of an empty one (its last byte is the zero count).
        let mut encoded = codec::to_vec(&input(0))?;
        encoded.pop();
        let over_cap =
            vec![ClientPaneInputEvent::TextCommit(String::new()); MAX_INPUT_EVENT_BATCH + 1];
        encoded.extend(codec::to_vec(&over_cap)?);
        assert!(matches!(
            codec::from_slice_exact::<ClientMessage>(&encoded),
            Err(CodecError::LimitExceeded(error)) if error.limit.max() == MAX_INPUT_EVENT_BATCH
        ));
        Ok(())
    }

    #[test]
    fn client_shell_key_roundtrip_keeps_generated_text() -> TestResult {
        let event = ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char('/'),
            modifiers: WireModifiers::SHIFT,
            kind: ClientKeyKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: Some("/".into()),
        };
        assert_eq!(roundtrip(&event)?, event);
        Ok(())
    }

    #[test]
    fn client_shell_focus_roundtrip() -> TestResult {
        let message = ClientMessage::ClientShellFocus { focused: false };
        assert_eq!(roundtrip(&message)?, message);
        Ok(())
    }

    #[test]
    fn client_shell_host_theme_roundtrip() -> TestResult {
        let message = ClientMessage::ClientShellHostTheme {
            update: ClientHostThemeUpdate::PaletteColors(vec![(
                4,
                ClientHostColor {
                    r: 10,
                    g: 20,
                    b: 30,
                },
            )]),
        };
        assert_eq!(roundtrip(&message)?, message);
        Ok(())
    }

    #[test]
    fn client_shell_endpoint_messages_roundtrip() -> TestResult {
        use crate::command::{
            EndpointCommand, EndpointError, EndpointReply, LayoutSetSplitRatioParams,
            PaneCopyMotion, PaneCopyMotionParams, PaneCopySearch, PaneCopySearchPosition,
            PaneDirection, PaneLineMotion, PaneSplitParams, PaneSwapParams, PaneTextPoint,
            PaneTextRange, PaneWordMotion, SplitDirection, WorkspaceCreateParams,
            WorkspaceCreateSource, WorkspaceMoveParams,
        };
        use crate::{PublicPaneId, WorkspaceId};

        let workspace: WorkspaceId = "w1".parse()?;
        let pane: PublicPaneId = "w1:p1".parse()?;
        for command in [
            EndpointCommand::PaneSplit(PaneSplitParams {
                pane_id: pane,
                direction: SplitDirection::Down,
            }),
            EndpointCommand::LayoutSetSplitRatio(LayoutSetSplitRatioParams {
                workspace_id: workspace,
                first_panes: vec![pane],
                second_panes: vec!["w1:p2".parse()?],
                ratio: shepr_core::layout::SplitRatio::new(0.6).expect("wire test ratio is valid"),
            }),
            EndpointCommand::WorkspaceMove(WorkspaceMoveParams {
                workspace_id: workspace,
                before_workspace_id: Some("w2".parse()?),
            }),
            EndpointCommand::WorkspaceMove(WorkspaceMoveParams {
                workspace_id: workspace,
                before_workspace_id: None,
            }),
            EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
                source: WorkspaceCreateSource::Cwd("/tmp/x".into()),
                label: Some("x".into()),
            }),
            EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
                source: WorkspaceCreateSource::Follow(workspace),
                label: None,
            }),
            EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
                source: WorkspaceCreateSource::Default,
                label: None,
            }),
            EndpointCommand::PaneSwap(PaneSwapParams::Direction {
                pane_id: pane,
                direction: PaneDirection::Left,
            }),
            EndpointCommand::PaneSwap(PaneSwapParams::Panes {
                source: pane,
                target: "w1:p2".parse()?,
            }),
            EndpointCommand::PaneCopyMotion(PaneCopyMotionParams {
                pane_id: pane,
                cursor: PaneTextPoint {
                    row: shepr_term::AbsRow(1),
                    col: 2,
                },
                motion: PaneCopyMotion::Line(PaneLineMotion::End),
            }),
            EndpointCommand::PaneCopyMotion(PaneCopyMotionParams {
                pane_id: pane,
                cursor: PaneTextPoint {
                    row: shepr_term::AbsRow(1),
                    col: 2,
                },
                motion: PaneCopyMotion::Word(PaneWordMotion::NextBigEnd),
            }),
        ] {
            let request = ClientMessage::ClientShellEndpointRequest {
                boot_id: "1-1".into(),
                request_id: "request-a".into(),
                command,
            };
            assert_eq!(roundtrip(&request)?, request);
        }

        let point = |row: u64, col: u16| PaneTextPoint {
            row: shepr_term::AbsRow(row),
            col,
        };
        for result in [
            Ok(EndpointReply::Done),
            Ok(EndpointReply::PaneCopySearch {
                pane_id: pane,
                search: PaneCopySearch {
                    matches: vec![PaneTextRange {
                        start: point(3, 1),
                        end: point(3, 4),
                    }],
                    total: 1,
                    current: Some(PaneCopySearchPosition {
                        window_index: 0,
                        global_index: 0,
                    }),
                },
            }),
            Err(EndpointError::PaneGone(pane)),
            Err(EndpointError::InvalidArgument("not a directory".into())),
            Err(EndpointError::ShuttingDown),
            Err(EndpointError::StaleBoot),
            Err(EndpointError::SurfaceInactive),
            Err(EndpointError::LimitExceeded(LimitExceeded::new(
                Limit::new(LimitKind::EndpointResponseBytes, 8_000_000),
                9_000_000,
            ))),
        ] {
            let response = ServerMessage::ClientShellEndpointResponse {
                boot_id: "1-1".into(),
                request_id: "request-a".into(),
                result,
            };
            assert_eq!(roundtrip(&response)?, response);
        }
        Ok(())
    }

    #[test]
    fn client_detach_roundtrip() -> TestResult {
        let msg = ClientMessage::Detach;
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Round-trip: ServerMessage ----

    #[test]
    fn server_welcome_with_error_roundtrip() -> TestResult {
        for reason in [
            crate::HandshakeRefusal::ExpectedHello,
            crate::HandshakeRefusal::ConnectionLimit(LimitExceeded::new(
                Limit::new(LimitKind::ConnectionCount, 64),
                65,
            )),
            crate::HandshakeRefusal::ServerStarting,
        ] {
            let msg = ServerMessage::EndpointWelcome(
                crate::endpoint::EndpointServerWelcome::refused(reason),
            );
            assert_eq!(roundtrip(&msg)?, msg);
        }
        Ok(())
    }

    #[test]
    fn server_frame_roundtrip_nontrivial() -> TestResult {
        // Build a 3×2 frame with varied styles (≥2×2).
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "H".into(),
                    grid_width: GridCellWidth::Grapheme,
                    fg: WireColor::from_ratatui(Color::Red),
                    bg: WireColor::from_ratatui(Color::Black),
                    style: WireStyle::from_ratatui_modifier(Modifier::BOLD),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "i".into(),
                    grid_width: GridCellWidth::Grapheme,
                    fg: WireColor::from_ratatui(Color::Green),
                    bg: WireColor::from_ratatui(Color::Reset),
                    style: WireStyle::from_ratatui_modifier(Modifier::ITALIC),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "!".into(),
                    grid_width: GridCellWidth::Grapheme,
                    fg: WireColor::from_ratatui(Color::Rgb(255, 128, 0)),
                    bg: WireColor::from_ratatui(Color::Indexed(220)),
                    style: WireStyle {
                        flags: WireStyleFlags::BOLD,
                        underline: shepr_term::UnderlineStyle::Curly,
                    },
                    skip: false,
                    hyperlink: Some(0),
                },
                CellData {
                    symbol: " ".into(),
                    grid_width: GridCellWidth::Grapheme,
                    fg: WireColor::from_ratatui(Color::Reset),
                    bg: WireColor::from_ratatui(Color::Reset),
                    style: WireStyle::default(),
                    skip: true,
                    hyperlink: None,
                },
                CellData {
                    symbol: "→".into(), // multi-byte grapheme
                    grid_width: GridCellWidth::One,
                    fg: WireColor::from_ratatui(Color::Cyan),
                    bg: WireColor::from_ratatui(Color::Blue),
                    style: WireStyle::from_ratatui_modifier(Modifier::REVERSED),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "\u{1F980}".into(), // emoji, wide grapheme cluster
                    grid_width: GridCellWidth::Two,
                    fg: WireColor::from_ratatui(Color::Yellow),
                    bg: WireColor::from_ratatui(Color::Magenta),
                    style: WireStyle::default(),
                    skip: false,
                    hyperlink: None,
                },
            ],
            width: 3,
            height: 2,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: crate::CursorShapeParam::SteadyBar,
            }),
            hyperlinks: vec!["https://example.com".to_owned()],
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "1-1".into(),
            projection_revision: crate::ProjectionRevision::new(1),
            surface_revision: crate::SurfaceRevision::new(1),
            frame: frame.clone(),
            panes: Vec::new(),
            splits: vec![PaneSurfaceSplit {
                direction: PaneSurfaceSplitDirection::Horizontal,
                pos: 1,
                area: SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 2,
                },
                hit_rect: SurfaceRect {
                    x: 1,
                    y: 0,
                    width: 1,
                    height: 2,
                },
                path: vec![SplitBranch::First, SplitBranch::Second],
            }],
        });
        let decoded = roundtrip(&msg)?;
        assert_eq!(msg, decoded);
        match decoded {
            ServerMessage::PaneSurface(surface) => {
                assert_eq!(surface.frame.cells[2].hyperlink, Some(0));
                assert_eq!(
                    surface.frame.hyperlinks,
                    vec!["https://example.com".to_owned()]
                );
            }
            other => panic!("expected pane surface, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn surface_update_roundtrip() -> TestResult {
        let msg = ServerMessage::SurfaceUpdate(SurfaceUpdate {
            boot_id: "1-1".into(),
            base_projection_revision: crate::ProjectionRevision::new(3),
            projection_revision: crate::ProjectionRevision::new(3),
            base_surface_revision: crate::SurfaceRevision::new(7),
            surface_revision: crate::SurfaceRevision::new(8),
            meta: None,
            spans: vec![PaneSurfacePatchRow {
                x: 2,
                y: 4,
                cells: vec![CellData {
                    symbol: "x".into(),
                    grid_width: GridCellWidth::One,
                    fg: WireColor::Indexed(1),
                    bg: WireColor::Rgb(0, 0, 2),
                    style: WireStyle::from_ratatui_modifier(Modifier::BOLD | Modifier::ITALIC),
                    skip: false,
                    hyperlink: None,
                }],
            }],
        });
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_snapshot_roundtrip() -> TestResult {
        let msg = ClientShellSnapshot {
            boot_id: "1-1".into(),
            restore_notice: Some(SessionRestoreNotice {
                loss: crate::SessionRestoreLoss::Workspaces {
                    dropped: std::num::NonZeroUsize::new(2).ok_or("nonzero dropped count")?,
                    panes_pruned: true,
                },
                backup_dir: "/state/session-backups".into(),
            }),
            session_saves_stopped: true,
            revision: crate::ProjectionRevision::new(1),
            focused_workspace_id: Some("w1".into()),
            focused_pane_id: Some("w1:p1".into()),
            workspaces: vec![ClientShellWorkspace {
                workspace_id: "w1".into(),
                new_workspace_cwd: Some("/tmp".into()),
                label: "shell".into(),
                branch: Some("main".into()),
                git_ahead_behind: None,
                agent_status: crate::AgentStatus::Idle,
            }],
            panes: vec![ClientShellPane {
                pane_id: "w1:p1".into(),
                label: None,
                cwd: Some("/repo".into()),
                foreground_cwd: Some("/repo".into()),
                right_click_passthrough: false,
            }],
            agents: vec![ClientShellAgent {
                pane_id: "w1:p1"
                    .parse()
                    .map_err(|_| std::io::Error::other("invalid test pane id"))?,
                agent: Some(shepr_agent::Agent::Codex),
                terminal_title: None,
                terminal_title_stripped: None,
                agent_status: crate::AgentStatus::Working,
                state_change_seq: 1,
            }],
        };
        let decoded: ClientShellSnapshot = roundtrip(&msg)?;
        assert_eq!(msg, decoded);
        Ok(())
    }

    #[test]
    fn server_shutdown_roundtrip() -> TestResult {
        let msg = ServerMessage::ServerShutdown {
            reason: crate::ShutdownReason::Stopping,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_clipboard_roundtrip() -> TestResult {
        let msg = ServerMessage::Clipboard {
            data: b"test".to_vec(),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_window_title_roundtrip() -> TestResult {
        for title in [Some("shepr api".to_owned()), None] {
            let msg = ServerMessage::WindowTitle { title };
            assert_eq!(roundtrip(&msg)?, msg);
        }
        Ok(())
    }

    #[test]
    fn server_mouse_capture_roundtrip() -> TestResult {
        let msg = ServerMessage::MouseCapture {
            enabled: true,
            sgr_pixels: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_keyboard_report_all_roundtrip() -> TestResult {
        let msg = ServerMessage::ClientShellKeyboardReportAll { enabled: true };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_error_roundtrip() -> TestResult {
        let msg = ServerMessage::ClientShellError {
            kind: crate::NoticeKind::LimitExceeded(LimitExceeded::new(
                Limit::new(LimitKind::InputPayloadBytes, 10),
                20,
            )),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Framing ----

    #[test]
    fn framing_small_message_roundtrip() {
        let msg = ClientMessage::ClientShellResize {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, false),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage = read_message(&mut buf.as_slice()).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_large_payload_roundtrip() {
        // Create a pane-surface message that is ≥128 KB.
        // Use a large frame with verbose cell data to exceed 128 KB after encoding.
        // 200×50 = 10000 cells. With varied symbols and styles, this should easily exceed 128 KB.
        let width: u16 = 200;
        let height: u16 = 50;
        let cells: Vec<CellData> = (0..(width as usize) * (height as usize))
            .map(|i| CellData {
                symbol: if i % 256 < 32 {
                    " ".to_owned()
                } else {
                    format!("{:03}", i % 1000)
                },
                grid_width: GridCellWidth::Grapheme,
                fg: WireColor::from_ratatui(Color::Rgb(
                    u8::try_from(i % 256).unwrap_or(u8::MAX),
                    u8::try_from((i / 256) % 256).unwrap_or(u8::MAX),
                    128,
                )),
                bg: WireColor::from_ratatui(Color::Indexed(
                    u8::try_from(i % 256).unwrap_or(u8::MAX),
                )),
                style: WireStyle::from_ratatui_modifier(Modifier::from_bits_retain(
                    u16::try_from(i % 256).unwrap_or(u16::MAX),
                )),
                skip: i % 100 == 0,
                hyperlink: None,
            })
            .collect();

        let frame = FrameData {
            cells,
            width,
            height,
            cursor: Some(CursorState {
                x: 10,
                y: 5,
                visible: true,
                shape: crate::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "1-1".into(),
            projection_revision: crate::ProjectionRevision::new(1),
            surface_revision: crate::SurfaceRevision::new(1),
            frame,
            panes: Vec::new(),
            splits: Vec::new(),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        // Verify the payload is at least 128 KB
        assert!(
            buf.len() >= 128 * 1024,
            "framed payload should be >= 128 KB, got {} bytes",
            buf.len()
        );

        let decoded: ServerMessage = read_message(&mut buf.as_slice()).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_multiple_messages_sequential() {
        // Write 100+ messages of varying types and read them back.
        let mut buf = Vec::new();
        let mut expected = Vec::new();

        for i in 0..150u32 {
            let msg = match i % 4 {
                0 => ClientMessage::ClientShellResize {
                    geometry: super::TerminalGeometry::new(
                        80 + u16::try_from(i % 40).unwrap_or(u16::MAX),
                        24 + u16::try_from(i % 20).unwrap_or(u16::MAX),
                        8,
                        16,
                        i % 2 == 0,
                    ),
                },
                1 => paste("x".repeat((i as usize % 50) + 1)),
                2 => ClientMessage::ClientShellFocus {
                    focused: i % 2 == 0,
                },
                3 => ClientMessage::Detach,
                _ => unreachable!(),
            };
            write_message(&mut buf, &msg).expect("test precondition");
            expected.push(msg);
        }

        let mut cursor = buf.as_slice();
        for expected_msg in &expected {
            let decoded: ClientMessage = read_message(&mut cursor).expect("test precondition");
            assert_eq!(*expected_msg, decoded);
        }
    }

    #[test]
    fn framing_oversized_rejected_without_panic() {
        // Craft a frame with a huge length prefix (4 GB claim).
        let mut buf: Vec<u8> = (u32::MAX).to_le_bytes().to_vec();
        // Add a few garbage bytes after the length prefix.
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        match result {
            Err(FramingError::LimitExceeded(error)) => {
                // The top bit is the continuation marker, not length.
                assert_eq!(error.actual, (u32::MAX >> 1) as usize);
                assert_eq!(error.limit.max(), MAX_FRAME_SIZE);
            }
            other => panic!("expected a size-limit error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_malformed_payload_rejected_without_panic() {
        // Valid length prefix pointing to garbage data.
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let mut buf = u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes()
            .to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        assert!(result.is_err(), "malformed payload should be rejected");
        match result {
            Err(FramingError::Codec(_)) => {} // expected
            other => panic!("expected codec error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_truncated_stream_returns_unexpected_eof() {
        // Write a length prefix claiming 100 bytes, but only provide 4.
        let mut buf: Vec<u8> = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        match result {
            Err(FramingError::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got: {other:?}"),
        }
    }

    #[test]
    fn framing_zero_length_message() {
        // The smallest real message: Detach encodes as its one-byte variant index.
        let msg = ClientMessage::Detach;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");

        // Verify the length prefix is correct
        let len = u32::from_le_bytes(buf[..4].try_into().expect("test precondition")) as usize;
        assert_eq!(
            len,
            buf.len() - 4,
            "length prefix should match payload size"
        );

        let decoded: ClientMessage = read_message(&mut buf.as_slice()).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_partial_read_reassembly() {
        // Simulate partial reads by using a reader that yields small chunks.
        let msg = paste("x".repeat(500));
        let mut full_buf = Vec::new();
        write_message(&mut full_buf, &msg).expect("test precondition");

        // Wrap in a chunked reader that only yields 7 bytes at a time.
        let mut chunked = ChunkedReader::new(full_buf, 7);
        let decoded: ClientMessage = read_message(&mut chunked).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    // ---- Malformed/oversized input ----

    #[test]
    fn oversized_frame_does_not_panic() {
        // Claim 4GB payload - should return Oversized error, not panic.
        let mut buf: Vec<u8> = 0xFFC00000u32.to_le_bytes().to_vec(); // ~4 GB claim
        buf.extend_from_slice(&[0; 8]);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        assert!(result.is_err());
        // Did not panic - test passing is proof.
    }

    #[test]
    fn malformed_frame_does_not_panic() {
        // Random garbage bytes after a valid-ish length prefix.
        let garbage: Vec<u8> = (0..200i32)
            .map(|i| u8::try_from(i ^ 0xAA).unwrap_or(u8::MAX))
            .collect();
        let mut buf = u32::try_from(garbage.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes()
            .to_vec();
        buf.extend_from_slice(&garbage);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        assert!(result.is_err());
        // Did not panic.
    }

    #[test]
    fn handshake_frame_limit_is_fixed() {
        let claimed = 64 * 1024 + 1;
        let prefix = u32::try_from(claimed)
            .expect("test precondition")
            .to_le_bytes()
            .to_vec();
        let result: Result<ClientMessage, FramingError> =
            read_handshake_message(&mut prefix.as_slice());
        assert!(matches!(
            result,
            Err(FramingError::LimitExceeded(error))
                if error.actual == claimed && error.limit.max() == 64 * 1024
        ));
    }

    // ---- FrameData from a ratatui Buffer ----

    #[test]
    fn frame_data_from_ratatui_buffer_keeps_cells_and_cursor() {
        let area = ratatui::layout::Rect::new(0, 0, 5, 3);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));

        // Write some styled content.
        buffer
            .cell_mut((0, 0))
            .expect("test precondition")
            .set_symbol("H");
        buffer.cell_mut((0, 0)).expect("test precondition").fg = Color::Red;
        buffer.cell_mut((0, 0)).expect("test precondition").modifier = Modifier::BOLD;

        buffer
            .cell_mut((1, 0))
            .expect("test precondition")
            .set_symbol("i");
        buffer.cell_mut((1, 0)).expect("test precondition").fg = Color::Green;
        buffer.cell_mut((1, 0)).expect("test precondition").modifier = Modifier::ITALIC;

        buffer
            .cell_mut((2, 0))
            .expect("test precondition")
            .set_symbol("!");
        buffer.cell_mut((2, 0)).expect("test precondition").fg = Color::Rgb(255, 128, 0);
        buffer.cell_mut((2, 0)).expect("test precondition").bg = Color::Indexed(220);

        let cursor = CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: crate::CursorShapeParam::Default,
        };
        let frame = FrameData::from_ratatui_buffer(&buffer, Some(cursor.clone()));

        // Verify frame dimensions.
        assert_eq!(frame.width, 5);
        assert_eq!(frame.height, 3);
        assert_eq!(frame.cells.len(), 15);
        assert_eq!(frame.cursor, Some(cursor));

        // Verify specific cells survived the conversion.
        assert_eq!(frame.cells[0].symbol, "H");
        assert_eq!(frame.cells[0].fg, WireColor::from_ratatui(Color::Red));
        assert!(frame.cells[0].style.flags.contains(WireStyleFlags::BOLD));

        assert_eq!(frame.cells[1].symbol, "i");
        assert_eq!(frame.cells[1].fg, WireColor::from_ratatui(Color::Green));
        assert!(frame.cells[1].style.flags.contains(WireStyleFlags::ITALIC));

        assert_eq!(frame.cells[2].symbol, "!");
        assert_eq!(
            frame.cells[2].fg,
            WireColor::from_ratatui(Color::Rgb(255, 128, 0))
        );
        assert_eq!(
            frame.cells[2].bg,
            WireColor::from_ratatui(Color::Indexed(220))
        );

        let with_links = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            None,
            &[((1, 0), "i".to_owned(), "https://example.com".to_owned())],
        );
        assert_eq!(with_links.cells[1].hyperlink, Some(0));
        assert_eq!(
            with_links.hyperlinks,
            vec!["https://example.com".to_owned()]
        );
    }

    // ---- Color conversion coverage ----

    #[test]
    fn color_roundtrip_all_named_colors() {
        let named = [
            Color::Reset,
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        for c in named {
            assert_eq!(
                WireColor::from_ratatui(c).to_ratatui(),
                c,
                "roundtrip failed for {c:?}"
            );
        }
    }

    #[test]
    fn color_roundtrip_indexed() {
        for i in 0..=255u8 {
            let c = Color::Indexed(i);
            assert_eq!(
                WireColor::from_ratatui(c).to_ratatui(),
                c,
                "roundtrip failed for Indexed({i})"
            );
        }
    }

    #[test]
    fn color_roundtrip_rgb() {
        let c = Color::Rgb(0xAB, 0xCD, 0xEF);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);

        let c = Color::Rgb(0, 0, 0);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);

        let c = Color::Rgb(255, 255, 255);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);
    }

    // ---- Style conversion ----

    #[test]
    fn wire_style_from_ratatui_modifier_maps_flags_and_a_single_underline() {
        let cases = [
            (Modifier::BOLD, WireStyleFlags::BOLD),
            (Modifier::ITALIC, WireStyleFlags::ITALIC),
            (Modifier::REVERSED, WireStyleFlags::REVERSED),
            (Modifier::DIM, WireStyleFlags::DIM),
            (Modifier::SLOW_BLINK, WireStyleFlags::SLOW_BLINK),
            (Modifier::RAPID_BLINK, WireStyleFlags::RAPID_BLINK),
            (Modifier::HIDDEN, WireStyleFlags::HIDDEN),
            (Modifier::CROSSED_OUT, WireStyleFlags::CROSSED_OUT),
            (
                Modifier::BOLD | Modifier::ITALIC,
                WireStyleFlags::BOLD.union(WireStyleFlags::ITALIC),
            ),
            (Modifier::empty(), WireStyleFlags::default()),
        ];
        for (modifier, flags) in cases {
            let style = WireStyle::from_ratatui_modifier(modifier);
            assert_eq!(style.flags, flags, "{modifier:?}");
            assert_eq!(style.underline, shepr_term::UnderlineStyle::None);
        }
        let underlined = WireStyle::from_ratatui_modifier(Modifier::UNDERLINED | Modifier::BOLD);
        assert_eq!(underlined.underline, shepr_term::UnderlineStyle::Single);
        assert_eq!(underlined.flags, WireStyleFlags::BOLD);
    }

    #[test]
    fn blank_frame_is_a_grid_of_unstyled_spaces() {
        let frame = FrameData::blank(3, 2);
        assert_eq!((frame.width, frame.height), (3, 2));
        assert_eq!(frame.cells.len(), 6);
        assert!(frame.cells.iter().all(|cell| *cell == CellData::blank()));
        assert!(frame.cursor.is_none() && frame.hyperlinks.is_empty());
        assert!(FrameData::blank(0, 5).cells.is_empty());
    }

    #[test]
    fn interned_hyperlinks_are_deduplicated_and_bounded() {
        let mut frame = FrameData::blank(1, 1);
        assert_eq!(frame.intern_hyperlink("https://a.example"), Some(0));
        assert_eq!(frame.intern_hyperlink("https://b.example"), Some(1));
        assert_eq!(frame.intern_hyperlink("https://b.example"), Some(1));
        assert_eq!(frame.intern_hyperlink("https://a.example"), Some(0));
        assert_eq!(frame.hyperlinks.len(), 2);

        frame.hyperlinks = (0..crate::MAX_SURFACE_HYPERLINKS)
            .map(|index| index.to_string())
            .collect();
        assert_eq!(frame.intern_hyperlink("7"), Some(7));
        assert_eq!(frame.intern_hyperlink("full"), None);
        assert_eq!(frame.hyperlinks.len(), crate::MAX_SURFACE_HYPERLINKS);
    }

    #[test]
    fn read_message_rejects_trailing_bytes() -> TestResult {
        // Encode a valid message, then append an extra byte after it.
        let msg = ClientMessage::Detach;
        let mut payload = codec::to_vec(&msg)?;
        let original_len = payload.len();
        payload.push(0xDE); // trailing garbage

        // Frame it with the inflated length (original + 1).
        let mut buf = u32::try_from(payload.len())?.to_le_bytes().to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice());
        match result {
            Err(FramingError::Codec(error)) => {
                assert_eq!(
                    error,
                    CodecError::TrailingBytes {
                        consumed: original_len,
                        total: original_len + 1,
                    }
                );
                let message = error.to_string();
                assert!(
                    message.contains("trailing bytes"),
                    "error should mention trailing bytes: {message}"
                );
            }
            other => panic!("expected a trailing-bytes codec error, got: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn read_message_accepts_exact_payload() {
        // A normally-framed message should decode without error.
        let msg = ClientMessage::ClientShellResize {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, false),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage = read_message(&mut buf.as_slice()).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    /// The length prefixes of an encoded frame sequence, walked frame by frame.
    fn frame_prefixes(mut frames: &[u8]) -> Vec<u32> {
        let mut prefixes = Vec::new();
        while !frames.is_empty() {
            let prefix = u32::from_le_bytes(frames[..4].try_into().expect("test precondition"));
            prefixes.push(prefix);
            let len = (prefix & !(1 << 31)) as usize;
            frames = &frames[4 + len..];
        }
        prefixes
    }

    fn frame_len(len: usize) -> u32 {
        u32::try_from(len).expect("test precondition")
    }

    // Clipboard is one variant-index byte followed by a varint byte count.
    // Data sizes just below a frame use a three-byte count; the two-frame case
    // uses four bytes and subtracts one extra byte below.
    const CLIPBOARD_ENVELOPE: usize = 4;
    // Everything in a `paste` message but its text, measured from an empty paste so no pane id
    // or variant byte is counted by hand. The text's own length varint is one byte at length 0
    // and three for the lengths these tests use (all below 2 MiB), hence the two extra bytes.
    fn paste_envelope() -> usize {
        codec::encoded_len(&paste(String::new())).expect("paste encoding") + 2
    }
    #[test]
    fn replay_host_effects_roundtrips() {
        let message = ClientMessage::ReplayHostEffects;
        let mut bytes = Vec::new();
        codec::encode_into(&mut bytes, &message).expect("encode replay");
        assert_eq!(
            codec::from_slice_exact::<ClientMessage>(&bytes).expect("decode replay"),
            message
        );
    }

    #[test]
    fn a_message_at_the_frame_cap_is_one_plain_frame() {
        let at_limit = paste("x".repeat(MAX_FRAME_SIZE - paste_envelope()));
        assert_eq!(
            codec::encoded_len(&at_limit).expect("test precondition"),
            MAX_FRAME_SIZE
        );
        let frames = encode_message(&at_limit).expect("test precondition");
        assert_eq!(frame_prefixes(&frames), vec![frame_len(MAX_FRAME_SIZE)]);
        assert_eq!(frames, encode_frame(&at_limit).expect("one frame fits"));
        let decoded: ClientMessage =
            read_message(&mut frames.as_slice()).expect("test precondition");
        assert_eq!(decoded, at_limit);
    }

    #[test]
    fn a_message_past_the_frame_cap_is_split_and_reassembled() {
        let continued = frame_len(MAX_FRAME_SIZE) | (1 << 31);
        // One byte past the cap: a full continued frame and a one-byte final one.
        let over = ServerMessage::Clipboard {
            data: vec![b'x'; MAX_FRAME_SIZE - CLIPBOARD_ENVELOPE + 1],
        };
        let len = codec::encoded_len(&over).expect("test precondition");
        let frames = encode_message(&over).expect("test precondition");
        assert_eq!(len, MAX_FRAME_SIZE + 1);
        assert_eq!(frame_prefixes(&frames), vec![continued, 1]);
        let decoded: ServerMessage =
            read_message(&mut ChunkedReader::new(frames.clone(), 4093)).expect("reassembled");
        assert_eq!(decoded, over);
        let mut written = Vec::new();
        write_message(&mut written, &over).expect("test precondition");
        assert_eq!(written, frames);

        // Exactly two frames' worth: the second frame is full and final, with
        // no empty frame after it.
        let two_full = ServerMessage::Clipboard {
            data: vec![b'x'; 2 * MAX_FRAME_SIZE - CLIPBOARD_ENVELOPE - 1],
        };
        assert_eq!(
            codec::encoded_len(&two_full).expect("test precondition"),
            2 * MAX_FRAME_SIZE
        );
        let frames = encode_message(&two_full).expect("test precondition");
        assert_eq!(
            frame_prefixes(&frames),
            vec![continued, frame_len(MAX_FRAME_SIZE)]
        );
        let decoded: ServerMessage = read_message(&mut frames.as_slice()).expect("reassembled");
        assert_eq!(decoded, two_full);
    }

    #[test]
    fn encode_frame_refuses_what_one_frame_cannot_carry() {
        let over_limit = paste("x".repeat(MAX_FRAME_SIZE - paste_envelope() + 1));
        match encode_frame(&over_limit) {
            Err(FramingError::LimitExceeded(error)) => {
                assert_eq!(error.actual, MAX_FRAME_SIZE + 1);
                assert_eq!(error.limit.max(), MAX_FRAME_SIZE);
            }
            other => panic!("expected a size-limit error, got {other:?}"),
        }
    }

    #[test]
    fn a_limited_reader_refuses_a_message_past_its_cap() {
        let over = paste("x".repeat(MAX_FRAME_SIZE));
        let frames = encode_message(&over).expect("test precondition");
        let result: Result<ClientMessage, FramingError> =
            read_message_limited(&mut frames.as_slice(), MAX_CLIENT_MESSAGE_SIZE);
        assert!(matches!(
            result,
            Err(FramingError::LimitExceeded(error))
                if error.limit.max() == MAX_CLIENT_MESSAGE_SIZE
        ));
        let decoded: ClientMessage =
            read_message(&mut frames.as_slice()).expect("the full cap reads it");
        assert_eq!(decoded, over);
    }

    #[test]
    fn a_stream_ending_inside_a_split_message_is_eof() {
        let over = ServerMessage::Clipboard {
            data: vec![b'x'; MAX_FRAME_SIZE],
        };
        let frames = encode_message(&over).expect("test precondition");
        let cut = &frames[..MAX_FRAME_SIZE + 4];
        let result: Result<ServerMessage, FramingError> = read_message(&mut &cut[..]);
        assert!(matches!(result, Err(FramingError::UnexpectedEof)));
    }

    #[test]
    fn handshake_reader_refuses_a_continued_hello() {
        let mut buf = (16u32 | (1 << 31)).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 16]);
        buf.extend_from_slice(&(64u32 * 1024).to_le_bytes());
        let result: Result<ClientMessage, FramingError> =
            read_handshake_message(&mut buf.as_slice());
        assert!(matches!(result, Err(FramingError::UnexpectedContinuation)));
    }

    #[test]
    fn continued_frame_must_be_full_sized() {
        let prefix = (1u32 | (1 << 31)).to_le_bytes();
        let result: Result<ClientMessage, FramingError> = read_message(&mut prefix.as_slice());
        assert!(matches!(
            result,
            Err(FramingError::InvalidContinuation {
                claimed: 1,
                expected: MAX_FRAME_SIZE,
            })
        ));
    }

    #[test]
    fn zero_length_continuation_is_rejected_without_reading_another_prefix() {
        let prefix = (1u32 << 31).to_le_bytes();
        let result: Result<ClientMessage, FramingError> = read_message(&mut prefix.as_slice());
        assert!(matches!(
            result,
            Err(FramingError::InvalidContinuation {
                claimed: 0,
                expected: MAX_FRAME_SIZE,
            })
        ));
    }

    #[test]
    fn single_frame_reader_rejects_a_continued_client_message() {
        let message = paste("x".repeat(MAX_FRAME_SIZE));
        let frames = encode_message(&message).expect("encode test message");
        let result: Result<ClientMessage, FramingError> =
            read_message_single_frame_limited(&mut frames.as_slice(), MAX_CLIENT_MESSAGE_SIZE);
        assert!(matches!(result, Err(FramingError::UnexpectedContinuation)));
    }

    #[test]
    fn encode_frame_matches_write_message_and_enforces_the_cap() {
        let msg = ServerMessage::WindowTitle {
            title: Some("frame".into()),
        };
        let frame = encode_frame(&msg).expect("test precondition");
        let mut written = Vec::new();
        write_message(&mut written, &msg).expect("test precondition");
        assert_eq!(frame, written);
        let decoded: ServerMessage =
            read_message(&mut frame.as_slice()).expect("test precondition");
        assert_eq!(decoded, msg);

        let over_limit = paste("x".repeat(MAX_FRAME_SIZE));
        assert!(matches!(
            encode_frame(&over_limit),
            Err(FramingError::LimitExceeded(error)) if error.limit.max() == MAX_FRAME_SIZE
        ));
    }

    // ---- Unix socketpair integration test ----

    #[test]
    fn framing_over_unix_socketpair() {
        use std::os::unix::net::UnixStream;

        let (mut a, mut b) = UnixStream::pair().expect("socketpair");

        let messages = vec![
            ClientMessage::ClientShellResize {
                geometry: super::TerminalGeometry::new(200, 60, 8, 16, true),
            },
            paste("hello world".to_owned()),
            ClientMessage::ClientShellResize {
                geometry: super::TerminalGeometry::new(100, 30, 8, 16, true),
            },
            ClientMessage::Detach,
        ];

        // Set non-blocking so we can write and read in the same test.
        a.set_nonblocking(false).expect("test precondition");
        b.set_nonblocking(false).expect("test precondition");

        for msg in &messages {
            write_message(&mut a, msg).expect("test precondition");
        }

        for expected in &messages {
            let decoded: ClientMessage = read_message(&mut b).expect("test precondition");
            assert_eq!(*expected, decoded);
        }
    }

    // ---- Helper: chunked reader for simulating partial reads ----

    /// A `Read` wrapper that yields at most `chunk_size` bytes per `read()` call,
    /// simulating partial reads on a real socket.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk_size,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let remaining = self.data.len() - self.pos;
            let to_read = buf.len().min(remaining).min(self.chunk_size);
            buf[..to_read].copy_from_slice(&self.data[self.pos..self.pos + to_read]);
            self.pos += to_read;
            Ok(to_read)
        }
    }
}
