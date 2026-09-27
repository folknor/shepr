use super::style::RATATUI_UNDERLINE_STYLE_SHIFT;
use super::*;
use serde::Serialize;
use std::io::{self, Read};

#[cfg(test)]
mod tests {
    use super::*;

    use super::codec::{self, CodecError};
    use ratatui::style::{Color, Modifier};
    use serde::de::DeserializeOwned;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

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
    fn client_hello_roundtrip() -> TestResult {
        let msg = ClientMessage::TerminalHello {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, true),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn endpoint_hello_roundtrip() -> TestResult {
        let msg = ClientMessage::EndpointHello(crate::protocol::endpoint::EndpointClientHello {
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
    fn client_input_roundtrip() -> TestResult {
        let msg = ClientMessage::Input {
            data: vec![0x1b, 0x5b, 0x41], // ESC [ A (up arrow)
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
                    modifiers: crate::protocol::WireModifiers::SHIFT,
                    kind: ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: Some('L' as u32),
                    generated_text: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('7'),
                    modifiers: crate::protocol::WireModifiers::CONTROL,
                    kind: ClientKeyKind::Press,
                    repeat_count: 3,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('x'),
                    modifiers: crate::protocol::WireModifiers::SUPER
                        | crate::protocol::WireModifiers::HYPER
                        | crate::protocol::WireModifiers::META,
                    kind: ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                },
            ],
        };
        let decoded = roundtrip(&message)?;
        assert_eq!(decoded, message);
        let ClientMessage::ClientShellPaneInput { events, .. } = decoded else {
            panic!("expected targeted semantic input");
        };
        let crate::raw_input::RawInputEvent::Key(semantic) = events[0].to_raw_input_event() else {
            panic!("expected semantic key");
        };
        assert_eq!(semantic.shifted_codepoint, Some('L' as u32));
        assert_eq!(semantic.kind, crossterm::event::KeyEventKind::Release);
        let crate::raw_input::RawInputEvent::Key(key) = events[1].to_raw_input_event() else {
            panic!("expected key");
        };
        assert_eq!(key.code, crossterm::event::KeyCode::Char('7'));
        assert_eq!(key.modifiers, crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(key.repeat_count, 3);
        let crate::raw_input::RawInputEvent::Key(key) = events[2].to_raw_input_event() else {
            panic!("expected key with extended modifiers");
        };
        assert!(
            key.modifiers
                .contains(crossterm::event::KeyModifiers::SUPER)
        );
        assert!(
            key.modifiers
                .contains(crossterm::event::KeyModifiers::HYPER)
        );
        assert!(key.modifiers.contains(crossterm::event::KeyModifiers::META));
        Ok(())
    }

    #[test]
    fn client_shell_key_roundtrip_keeps_generated_text() {
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::SHIFT,
        )
        .with_generated_text(Some("/".into()));
        let event =
            ClientPaneInputEvent::from_terminal_key(key.clone()).expect("semantic pane key");
        let crate::raw_input::RawInputEvent::Key(roundtripped) = event.to_raw_input_event() else {
            panic!("pane key should remain a key");
        };

        assert_eq!(roundtripped, key);
        assert_eq!(
            crate::input::encode_terminal_key(
                roundtripped,
                crate::input::KeyboardProtocol::Kitty { flags: 1 },
            ),
            b"/"
        );
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
        let request = ClientMessage::ClientShellEndpointRequest {
            boot_id: "boot-a".into(),
            request: r#"{"id":"request-a","method":"session.snapshot","params":{}}"#.into(),
        };
        assert_eq!(roundtrip(&request)?, request);

        let response = ServerMessage::ClientShellEndpointResponseChunk {
            boot_id: "boot-a".into(),
            request_id: "request-a".into(),
            final_chunk: true,
            data: br#"{"id":"request-a","result":{"type":"ok"}}"#.to_vec(),
        };
        assert_eq!(roundtrip(&response)?, response);
        Ok(())
    }

    #[test]
    fn client_input_large_multilingual_payload_roundtrip() -> TestResult {
        let text =
            "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.\u{1F642}".repeat(1024);
        assert!(text.len() > 64 * 1024);
        assert!(text.len() < MAX_FRAME_SIZE);
        let msg = ClientMessage::Input {
            data: text.as_bytes().to_vec(),
        };

        let encoded = codec::to_vec(&msg)?;
        let (decoded, consumed): (ClientMessage, _) = codec::from_slice(&encoded)?;

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, msg);
        Ok(())
    }

    #[test]
    fn client_resize_roundtrip() -> TestResult {
        let msg = ClientMessage::Resize {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, true),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_detach_roundtrip() -> TestResult {
        let msg = ClientMessage::Detach;
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_attach_terminal_roundtrip() -> TestResult {
        let msg = ClientMessage::AttachTerminal {
            terminal_id: "term_123".to_owned().into(),
            takeover: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_attach_scroll_roundtrip() -> TestResult {
        let msg = ClientMessage::AttachScroll {
            source: AttachScrollSource::Wheel,
            direction: AttachScrollDirection::Up,
            lines: 3,
            column: Some(12),
            row: Some(7),
            modifiers: crate::protocol::WireModifiers::from_bits_retain(4),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Round-trip: ServerMessage ----

    #[test]
    fn server_welcome_roundtrip() -> TestResult {
        let msg = ServerMessage::Welcome { error: None };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_welcome_with_error_roundtrip() -> TestResult {
        let msg = ServerMessage::Welcome {
            error: Some(crate::protocol::HandshakeRefusal::ExpectedHello),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_frame_roundtrip_nontrivial() -> TestResult {
        // Build a 3×2 frame with varied styles (≥2×2).
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "H".into(),
                    fg: WireColor::from_ratatui(Color::Red),
                    bg: WireColor::from_ratatui(Color::Black),
                    style: WireStyle::from_ratatui_modifier(Modifier::BOLD),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "i".into(),
                    fg: WireColor::from_ratatui(Color::Green),
                    bg: WireColor::from_ratatui(Color::Reset),
                    style: WireStyle::from_ratatui_modifier(Modifier::ITALIC),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "!".into(),
                    fg: WireColor::from_ratatui(Color::Rgb(255, 128, 0)),
                    bg: WireColor::from_ratatui(Color::Indexed(220)),
                    style: WireStyle {
                        flags: WireStyleFlags::BOLD,
                        underline: crate::vt::UnderlineStyle::Curly,
                    },
                    skip: false,
                    hyperlink: Some(0),
                },
                CellData {
                    symbol: " ".into(),
                    fg: WireColor::from_ratatui(Color::Reset),
                    bg: WireColor::from_ratatui(Color::Reset),
                    style: WireStyle::default(),
                    skip: true,
                    hyperlink: None,
                },
                CellData {
                    symbol: "→".into(), // multi-byte grapheme
                    fg: WireColor::from_ratatui(Color::Cyan),
                    bg: WireColor::from_ratatui(Color::Blue),
                    style: WireStyle::from_ratatui_modifier(Modifier::REVERSED),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "\u{1F980}".into(), // emoji, wide grapheme cluster
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
                shape: crate::protocol::CursorShapeParam::SteadyBar,
            }),
            hyperlinks: vec!["https://example.com".to_owned()],
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: crate::protocol::ProjectionRevision::new(1),
            surface_revision: crate::protocol::SurfaceRevision::new(1),
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
            boot_id: "boot-1".into(),
            base_projection_revision: crate::protocol::ProjectionRevision::new(3),
            projection_revision: crate::protocol::ProjectionRevision::new(3),
            base_surface_revision: crate::protocol::SurfaceRevision::new(7),
            surface_revision: crate::protocol::SurfaceRevision::new(8),
            meta: None,
            spans: vec![PaneSurfacePatchRow {
                x: 2,
                y: 4,
                cells: vec![CellData {
                    symbol: "x".into(),
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
    fn internal_surface_patch_cannot_be_framed() {
        let patch = ServerMessage::PaneSurfacePatch(PaneSurfacePatch {
            boot_id: "boot".into(),
            projection_revision: crate::protocol::ProjectionRevision::new(1),
            base_surface_revision: crate::protocol::SurfaceRevision::new(1),
            surface_revision: crate::protocol::SurfaceRevision::new(2),
            rows: Vec::new(),
            panes: Vec::new(),
            cursor: None,
        });
        assert!(write_message(&mut Vec::new(), &patch).is_err());
    }

    #[test]
    fn client_shell_snapshot_roundtrip() -> TestResult {
        let config_source = "[keys]\nprefix = \"ctrl+a\"\n";
        let config = toml::from_str(config_source)?;
        let msg = ClientShellSnapshot {
            boot_id: "boot-1".into(),
            revision: crate::protocol::ProjectionRevision::new(1),
            resolved_config: crate::config::ValidatedConfig::test_from_config(
                config,
                Some(config_source),
            ),
            focused_workspace_id: Some("w1".into()),
            focused_tab_id: Some("w1:t1".into()),
            focused_pane_id: Some("w1:p1".into()),
            tab_bar_right: vec![ClientShellTabStatusSegment {
                text: "host".into(),
                accent: false,
            }],
            tab_bar_right_separator: " · ".into(),
            workspaces: vec![ClientShellWorkspace {
                workspace_id: "w1".into(),
                active_tab_id: "w1:t1".into(),
                new_workspace_cwd: "/tmp".into(),
                number: 1,
                label: "shell".into(),
                custom_label: false,
                branch: Some("main".into()),
                git_ahead_behind: None,
                tokens: Vec::new(),
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            tabs: vec![ClientShellTab {
                tab_id: "w1:t1".into(),
                workspace_id: "w1".into(),
                number: 1,
                label: "main".into(),
                custom_label: true,
                zoomed: false,
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            panes: vec![ClientShellPane {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                label: None,
                cwd: Some("/repo".into()),
                foreground_cwd: Some("/repo".into()),
                focused: true,
                right_click_passthrough: false,
            }],
            agents: vec![ClientShellAgent {
                pane_id: "w1:p1"
                    .parse()
                    .map_err(|_| std::io::Error::other("invalid test pane id"))?,
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                name: Some("codex".into()),
                display_agent: None,
                agent: Some("codex".into()),
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                agent_status: crate::api::schema::AgentStatus::Working,
                state_change_seq: 1,
                state_labels: Vec::new(),
                tokens: Vec::new(),
                focused: true,
            }],
        };
        let expected_keybindings = msg.resolved_config.live_keybinds();
        let decoded: ClientShellSnapshot = roundtrip(&msg)?;
        assert_eq!(msg, decoded);
        let actual_keybindings = decoded.resolved_config.live_keybinds();
        assert_eq!(actual_keybindings.prefix, expected_keybindings.prefix);
        assert_eq!(
            actual_keybindings.keybinds.detach.bindings,
            expected_keybindings.keybinds.detach.bindings
        );
        Ok(())
    }

    #[test]
    fn server_shutdown_roundtrip() -> TestResult {
        let msg = ServerMessage::ServerShutdown {
            reason: Some(crate::protocol::ShutdownReason::Message(
                "updating".to_owned(),
            )),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        let detached = ServerMessage::ServerShutdown {
            reason: Some(crate::protocol::ShutdownReason::Detached),
        };
        assert_eq!(roundtrip(&detached)?, detached);
        Ok(())
    }

    #[test]
    fn server_clipboard_roundtrip() -> TestResult {
        let msg = ServerMessage::Clipboard {
            data: "dGVzdA==".to_owned(), // base64 "test"
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
    fn server_terminal_frame_roundtrip() -> TestResult {
        let msg = ServerMessage::Terminal(TerminalFrame {
            bytes: b"\x1b[1;1Hhello".to_vec(),
        });
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn byte_fields_encode_as_length_then_raw_bytes() -> TestResult {
        // The byte-buffer fields must keep the plain `Vec<u8>` wire layout
        // (varint length, raw bytes) while decoding in one copy.
        let data = vec![0u8, 1, 0x7f, 0x80, 0xff];
        let encoded = codec::to_vec(&ClientMessage::Input { data: data.clone() })?;
        assert_eq!(encoded.first(), Some(&1), "Input is variant 1");
        assert_eq!(encoded.get(1), Some(&5), "length prefix");
        assert_eq!(encoded.get(2..), Some(data.as_slice()));
        assert_eq!(codec::to_vec(&data)?, encoded.get(1..).unwrap_or_default());

        let large = ClientMessage::Input {
            data: (0..=255u8).cycle().take(300_000).collect(),
        };
        assert_eq!(roundtrip(&large)?, large);

        let page_key = ClientMessage::AttachScroll {
            source: AttachScrollSource::PageKey {
                input: b"\x1b[5~".to_vec(),
            },
            direction: AttachScrollDirection::Up,
            lines: 1,
            column: None,
            row: None,
            modifiers: crate::protocol::WireModifiers::NONE,
        };
        assert_eq!(roundtrip(&page_key)?, page_key);

        // JSON keeps accepting the number-array form serde uses for `Vec<u8>`.
        let json = serde_json::to_string(&ClientMessage::Input { data: data.clone() })?;
        let decoded: ClientMessage = serde_json::from_str(&json)?;
        assert_eq!(decoded, ClientMessage::Input { data });
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
    fn direct_terminal_keyboard_mode_roundtrip() -> TestResult {
        let msg = ServerMessage::DirectTerminalKeyboardProtocol {
            flags: KittyKeyboardFlags::from_bits_retain(15),
            modify_other_keys_level: crate::vt::ModifyOtherKeysLevel::ExceptWellDefined,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn direct_terminal_notice_roundtrip() -> TestResult {
        let msg = ServerMessage::DirectTerminalNotice {
            kind: crate::protocol::NoticeKind::PasteRejected { size: 20, max: 10 },
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Framing ----

    #[test]
    fn framing_small_message_roundtrip() {
        let msg = ClientMessage::TerminalHello {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, false),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
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
                shape: crate::protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: crate::protocol::ProjectionRevision::new(1),
            surface_revision: crate::protocol::SurfaceRevision::new(1),
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

        let decoded: ServerMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_multiple_messages_sequential() {
        // Write 100+ messages of varying types and read them back.
        let mut buf = Vec::new();
        let mut expected = Vec::new();

        for i in 0..150u32 {
            let msg = match i % 5 {
                0 => ClientMessage::TerminalHello {
                    geometry: super::TerminalGeometry::new(
                        80 + u16::try_from(i % 40).unwrap_or(u16::MAX),
                        24 + u16::try_from(i % 20).unwrap_or(u16::MAX),
                        8,
                        16,
                        i % 2 == 0,
                    ),
                },
                1 => ClientMessage::Input {
                    data: vec![u8::try_from(i % 256).unwrap_or(u8::MAX); (i as usize % 50) + 1],
                },
                2 => ClientMessage::ClientShellFocus {
                    focused: i % 2 == 0,
                },
                3 => ClientMessage::Resize {
                    geometry: super::TerminalGeometry::new(
                        100 + u16::try_from(i % 30).unwrap_or(u16::MAX),
                        30 + u16::try_from(i % 10).unwrap_or(u16::MAX),
                        8,
                        16,
                        i % 2 == 0,
                    ),
                },
                4 => ClientMessage::Detach,
                _ => unreachable!(),
            };
            write_message(&mut buf, &msg).expect("test precondition");
            expected.push(msg);
        }

        let mut cursor = buf.as_slice();
        for expected_msg in &expected {
            let decoded: ClientMessage =
                read_message(&mut cursor, MAX_FRAME_SIZE).expect("test precondition");
            assert_eq!(*expected_msg, decoded);
        }
    }

    #[test]
    fn framing_oversized_rejected_without_panic() {
        // Craft a frame with a huge length prefix (4 GB claim).
        let mut buf: Vec<u8> = (u32::MAX).to_le_bytes().to_vec();
        // Add a few garbage bytes after the length prefix.
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::Oversized { claimed, max }) => {
                assert_eq!(claimed, u32::MAX as usize);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected Oversized error, got: {other:?}"),
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

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
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

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
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

        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_partial_read_reassembly() {
        // Simulate partial reads by using a reader that yields small chunks.
        let msg = ClientMessage::Input {
            data: vec![42; 500], // 500-byte input payload
        };
        let mut full_buf = Vec::new();
        write_message(&mut full_buf, &msg).expect("test precondition");

        // Wrap in a chunked reader that only yields 7 bytes at a time.
        let mut chunked = ChunkedReader::new(full_buf, 7);
        let decoded: ClientMessage =
            read_message(&mut chunked, MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    // ---- Malformed/oversized input ----

    #[test]
    fn oversized_frame_does_not_panic() {
        // Claim 4GB payload - should return Oversized error, not panic.
        let mut buf: Vec<u8> = 0xFFC00000u32.to_le_bytes().to_vec(); // ~4 GB claim
        buf.extend_from_slice(&[0; 8]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
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

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err());
        // Did not panic.
    }

    #[test]
    fn oversized_input_rejected_custom_max() {
        // Verify a custom (small) max_frame_size is enforced.
        let msg = ClientMessage::Input {
            data: vec![0x41; 1000],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice(), 64);
        // The encoded payload for 1000 bytes of input will be > 64 bytes.
        assert!(
            matches!(result, Err(FramingError::Oversized { .. })),
            "expected Oversized with small max_frame_size"
        );
    }

    // ---- FrameData ↔ ratatui Buffer conversion ----

    #[test]
    fn frame_data_roundtrip_through_ratatui_buffer() {
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
            shape: crate::protocol::CursorShapeParam::Default,
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

        // Convert back to ratatui buffer and compare.
        let restored = frame.to_ratatui_buffer().expect("should reconstruct");
        assert_eq!(restored.area, area);
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").symbol(),
            "H"
        );
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").fg,
            Color::Red
        );
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").modifier,
            Modifier::BOLD
        );
        assert_eq!(
            restored.cell((1, 0)).expect("test precondition").symbol(),
            "i"
        );
        assert_eq!(
            restored.cell((2, 0)).expect("test precondition").symbol(),
            "!"
        );
        assert_eq!(
            restored.cell((2, 0)).expect("test precondition").fg,
            Color::Rgb(255, 128, 0)
        );
    }

    #[test]
    fn frame_data_rejects_mismatched_cell_count() {
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "X".into(),
                    fg: WireColor::Reset,
                    bg: WireColor::Reset,
                    style: WireStyle::default(),
                    skip: false,
                    hyperlink: None,
                };
                5
            ], // 5 cells but 3×2 = 6 expected
            width: 3,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        assert!(frame.to_ratatui_buffer().is_none());
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
    fn wire_style_roundtrip_through_ratatui_modifier() {
        let all_mods = [
            Modifier::BOLD,
            Modifier::ITALIC,
            Modifier::REVERSED,
            Modifier::UNDERLINED,
            Modifier::DIM,
            Modifier::SLOW_BLINK,
            Modifier::CROSSED_OUT,
            Modifier::BOLD | Modifier::ITALIC,
            Modifier::BOLD | Modifier::UNDERLINED | Modifier::REVERSED,
            Modifier::empty(),
        ];
        for m in all_mods {
            let style = WireStyle::from_ratatui_modifier(m);
            assert_eq!(style.to_ratatui_modifier(), m, "roundtrip failed for {m:?}");
        }
    }

    #[test]
    fn underline_style_survives_ratatui_buffer_roundtrip() {
        // The ratatui buffer has no underline-shape field, so the adapter
        // preserves non-single underline styles in its temporary modifier.
        for underline in [
            crate::vt::UnderlineStyle::Double,
            crate::vt::UnderlineStyle::Curly,
            crate::vt::UnderlineStyle::Dotted,
            crate::vt::UnderlineStyle::Dashed,
        ] {
            let style = WireStyle {
                flags: WireStyleFlags::BOLD,
                underline,
            };
            let frame = FrameData {
                cells: vec![CellData {
                    symbol: "u".into(),
                    fg: WireColor::Reset,
                    bg: WireColor::Reset,
                    style,
                    skip: false,
                    hyperlink: None,
                }],
                width: 1,
                height: 1,
                cursor: None,
                hyperlinks: Vec::new(),
            };
            let buffer = frame.to_ratatui_buffer().expect("test precondition");
            let mut restored = frame.clone();
            restored.replace_from_ratatui_buffer_preserving_effects(&buffer, None);
            assert_eq!(restored.cells[0].style, style, "style {underline:?}");
        }
    }

    #[test]
    fn stale_ratatui_underline_style_is_dropped_from_ununderlined_cells() {
        let stale = Modifier::from_bits_retain(
            Modifier::BOLD.bits() | (3 << RATATUI_UNDERLINE_STYLE_SHIFT),
        );
        let style = WireStyle::from_ratatui_modifier(stale);
        assert_eq!(style.underline, crate::vt::UnderlineStyle::None);
        assert_eq!(style.to_ratatui_modifier(), Modifier::BOLD);
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

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
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
        let msg = ClientMessage::TerminalHello {
            geometry: super::TerminalGeometry::new(80, 24, 8, 16, false),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn write_message_rejects_oversized_payload() {
        // Input is variant 1 (one byte) followed by a 3-byte varint length for
        // payloads this size, so `data` of MAX_FRAME_SIZE - 4 bytes encodes to
        // exactly MAX_FRAME_SIZE.
        let envelope = 4;
        let at_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE - envelope],
        };
        assert_eq!(
            codec::encoded_len(&at_limit).expect("test precondition"),
            MAX_FRAME_SIZE
        );
        let mut buf = Vec::new();
        write_message(&mut buf, &at_limit).expect("a frame at the cap is accepted");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(decoded, at_limit);

        let over_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE - envelope + 1],
        };
        let mut buf = Vec::new();
        match write_message(&mut buf, &over_limit) {
            Err(FramingError::Oversized { claimed, max }) => {
                assert_eq!(claimed, MAX_FRAME_SIZE + 1);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected Oversized, got {other:?}"),
        }
        assert!(buf.is_empty(), "nothing is written for a rejected frame");
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
            read_message(&mut frame.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(decoded, msg);

        let over_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE],
        };
        assert!(matches!(
            encode_frame(&over_limit),
            Err(FramingError::Oversized { max, .. }) if max == MAX_FRAME_SIZE
        ));
    }

    // ---- Unix socketpair integration test ----

    #[test]
    fn framing_over_unix_socketpair() {
        use std::os::unix::net::UnixStream;

        let (mut a, mut b) = UnixStream::pair().expect("socketpair");

        let messages = vec![
            ClientMessage::TerminalHello {
                geometry: super::TerminalGeometry::new(200, 60, 8, 16, true),
            },
            ClientMessage::Input {
                data: b"hello world".to_vec(),
            },
            ClientMessage::Resize {
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
            let decoded: ClientMessage =
                read_message(&mut b, MAX_FRAME_SIZE).expect("test precondition");
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
