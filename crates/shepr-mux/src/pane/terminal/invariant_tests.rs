//! Pane terminal behavior checks that span parsing, reads, rendering and history.
use super::*;

struct Harness {
    pane: PaneTerminal,
    width: u16,
    height: u16,
    effects: Effects,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Effects {
    replies: Vec<u8>,
    clipboard: Vec<Vec<u8>>,
    cwd: Vec<std::path::PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    geometry: (u16, u16),
    cells: Vec<CellData>,
    screen_text: String,
    links: Vec<((u16, u16), String, String)>,
    cursor: TerminalCursorState,
    input: InputState,
    visible: String,
    recent: TerminalReadSnapshot,
    detection: String,
    title: Option<String>,
}

impl Harness {
    fn new(width: u16, height: u16) -> Self {
        let terminal = shepr_vt::Terminal::new(width, height, 256);
        Self {
            pane: PaneTerminal::new(terminal),
            width,
            height,
            effects: Effects::default(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let result = self
            .pane
            .process_pty_bytes(shepr_test_fixtures::fixed_pane_id(1), bytes);
        for reply in result.terminal_responses {
            self.effects.replies.extend_from_slice(&reply);
        }
        self.effects.clipboard.extend(result.clipboard_writes);
        self.effects.cwd.extend(result.reported_cwd);
    }

    fn resize(&mut self, width: u16, height: u16) {
        for reply in self.pane.resize(shepr_core::geometry::PaneGeometry::new(
            width, height, 8, 16,
        )) {
            self.effects.replies.extend_from_slice(&reply);
        }
        self.width = width;
        self.height = height;
    }

    fn full_frame(&self) -> FrameData {
        // A full render into a blank frame: the independent oracle the
        // incremental dirty rows are compared against.
        let mut frame = FrameData::blank(self.width, self.height);
        self.pane
            .render_into(&mut frame, Rect::new(0, 0, self.width, self.height));
        frame
    }

    fn full_cells(&self) -> Vec<CellData> {
        self.full_frame().cells
    }

    /// Every linked cell as its position, symbol and target.
    fn links(&self) -> Vec<((u16, u16), String, String)> {
        let frame = self.full_frame();
        let mut links = Vec::new();
        for (index, cell) in frame.cells.iter().enumerate() {
            let Some(link) = cell.hyperlink else { continue };
            let x = u16::try_from(index % usize::from(frame.width)).expect("test precondition");
            let y = u16::try_from(index / usize::from(frame.width)).expect("test precondition");
            links.push((
                (x, y),
                cell.symbol.clone(),
                frame.hyperlinks[usize::try_from(link).expect("test precondition")].clone(),
            ));
        }
        links
    }

    fn cursor(&self) -> Option<TerminalCursorState> {
        current_cursor_state(
            &mut shepr_vt::lock_terminal_core(&self.pane.core).expect("test precondition"),
        )
    }

    fn screen_text(&self) -> String {
        let core = shepr_vt::lock_terminal_core(&self.pane.core).expect("test precondition");
        let terminal = &core.terminal;
        let last_row = terminal.total_rows().saturating_sub(1);
        terminal
            .read_text_screen(
                Point::new(ScreenRow(0), 0),
                Point::new(ScreenRow(last_row), terminal.cols().saturating_sub(1)),
            )
            .expect("test precondition")
    }

    fn observe(&self) -> Observation {
        Observation {
            geometry: (self.width, self.height),
            cells: self.full_cells(),
            screen_text: self.screen_text(),
            links: self.links(),
            cursor: self.cursor().expect("test precondition"),
            input: self.pane.input_state().expect("test precondition"),
            visible: self.pane.visible_text(),
            recent: self.pane.recent_text_snapshot(32),
            detection: self.pane.detection_text(),
            title: self.pane.terminal_title(),
        }
    }
}

#[test]
fn primary_screen_replay_honors_ed3_for_droid_at_chunk_boundaries() {
    // PTY output is parsed the same whatever process wrote it, so no child
    // process is needed to prove ED3 handling.
    for clear in ["\x1b[3J", "\x1b[?3J"] {
        let prefix = format!("\x1b[?2026h\x1b[2J{clear}\x1b[H");
        let frame = |label: &str| {
            let mut bytes = format!("{prefix}welcome\r\n");
            for row in 0..55 {
                bytes.push_str(&format!("{label}-{row:02}\r\n"));
            }
            bytes.push_str("\x1b[?2026l");
            bytes
        };
        let old = frame("old");
        let new = frame("new");
        // Whole writes catch the old filter; bytewise writes and every split
        // within the erase prefix prove that PTY chunking cannot change ED3.
        for chunk_size in [usize::MAX, 1, 7] {
            for split in 0..=prefix.len() {
                let mut harness = Harness::new(80, 24);
                for bytes in old.as_bytes().chunks(chunk_size) {
                    harness.write(bytes);
                }
                assert!(
                    harness
                        .pane
                        .recent_text_snapshot(256)
                        .text
                        .contains("old-00")
                );
                harness.write(&new.as_bytes()[..split]);
                for bytes in new.as_bytes()[split..].chunks(chunk_size) {
                    harness.write(bytes);
                }
                let recent = harness.pane.recent_text_snapshot(256).text;
                assert!(recent.contains("new-54"), "redraw must complete");
                assert_eq!(
                    recent.matches("welcome").count(),
                    1,
                    "{clear:?}, split {split}, chunk {chunk_size}: {recent}"
                );
                assert!(!recent.contains("old-"), "{recent}");
                assert!(harness.pane.visible_text().contains("new-54"));
                harness.pane.scroll_up(256);
                let top = harness.pane.visible_text();
                assert!(top.contains("welcome") && top.contains("new-00"), "{top}");
                assert!(!top.contains("old-"), "{top}");
            }
        }
    }
}

#[test]
fn erase_display_preserves_screen_and_history_boundaries() {
    for clear in [b"\x1b[3J".as_slice(), b"\x1b[?3J"] {
        let mut harness = Harness::new(80, 24);
        for row in 0..55 {
            harness.write(format!("history-{row:02}\r\n").as_bytes());
        }
        assert!(
            harness
                .pane
                .recent_text_snapshot(256)
                .text
                .contains("history-00")
        );
        let primary = harness.pane.recent_text_snapshot(256);
        let visible = harness.pane.visible_text();

        harness.write(b"\x1b[?1049h\x1b[2J\x1b[Halternate");
        harness.write(clear);
        assert!(harness.pane.visible_text().contains("alternate"));
        harness.write(b"\x1b[?1049l");
        assert_eq!(harness.pane.recent_text_snapshot(256), primary);
        assert_eq!(harness.pane.visible_text(), visible);

        // ED2 clears only the display, not prior shell output in scrollback.
        harness.write(b"\x1b[2J\x1b[Hprompt");
        assert_eq!(harness.pane.visible_text().trim(), "prompt");
        assert!(
            harness
                .pane
                .recent_text_snapshot(256)
                .text
                .contains("history-00")
        );
        harness.write(clear);
        // ED3 clears history without erasing the current display.
        assert_eq!(harness.pane.visible_text().trim(), "prompt");
        assert_eq!(harness.pane.recent_text_snapshot(256).text.trim(), "prompt");
    }
}

#[test]
fn short_streams_are_invariant_at_every_byte_boundary() {
    let fixtures: &[&[u8]] = &[
        "a界e\u{301}\u{1F1EF}\u{1F1F5}!".as_bytes(),
        b"a\x1b[31;1mB\x1b[0m\x1b[2;3HZ\x1b[6n\x1b[?2004h",
        b"\x1b]8;;https://example.test/a\x1b\\link\x1b]8;;\x1b\\!",
        b"\x1b]52;c;aGk=\x07\x1b]2;fragmentation\x1b\\\x07",
        b"\x1bP+q5463\x1b\\\x1bP+q6E6F7065\x9c\x1b[6n",
    ];
    for bytes in fixtures {
        let mut whole = Harness::new(16, 4);
        whole.write(bytes);
        let expected = whole.observe();
        for split in 0..=bytes.len() {
            let mut fragmented = Harness::new(16, 4);
            fragmented.write(&bytes[..split]);
            fragmented.write(&bytes[split..]);
            assert_eq!(
                fragmented.observe(),
                expected,
                "split {split}, bytes {bytes:?}"
            );
            assert_eq!(
                fragmented.effects, whole.effects,
                "effects at split {split}"
            );
        }
        let mut bytewise = Harness::new(16, 4);
        for byte in *bytes {
            bytewise.write(std::slice::from_ref(byte));
        }
        assert_eq!(bytewise.observe(), expected);
        assert_eq!(bytewise.effects, whole.effects);
    }
}

const MIXED: &str = "ab\x1b[1;38;2;12;34;56m界e\u{301}\x1b[0m\x1b]8;;https://example.test/reflow\x1b\\\u{1F1EF}\u{1F1F5}xyz\x1b]8;;\x1b\\\r\nnext カタカナ end";

#[test]
fn mixed_reflow_reads_are_stable_and_chunk_independent() {
    let mut whole = Harness::new(12, 5);
    let mut fragmented = Harness::new(12, 5);
    whole.write(MIXED.as_bytes());
    for chunk in MIXED.as_bytes().chunks(3) {
        fragmented.write(chunk);
    }
    for (step, (width, height)) in [(12, 5), (8, 4), (17, 6), (9, 5), (12, 5)]
        .into_iter()
        .enumerate()
    {
        whole.resize(width, height);
        fragmented.resize(width, height);
        let expected = whole.observe();
        assert_eq!(whole.observe(), expected, "reads must not mutate semantics");
        assert_eq!(fragmented.observe(), expected);
        // alacritty reflows bottom-anchored and keeps the blank row below the
        // cursor, so at 8x4 the linked rows scroll into history. The link must
        // survive every reflow, visible or not.
        assert!(
            whole
                .pane
                .recent_unwrapped_ansi_snapshot(64)
                .text
                .contains("https://example.test/reflow"),
            "reflow at {width}x{height} must retain the link"
        );
        if step == 0 {
            assert!(
                !expected.links.is_empty(),
                "fixture must show a visible link before any reflow"
            );
        }
    }
}

#[test]
fn incremental_rows_reconstruct_full_render() {
    let mut incremental = Harness::new(12, 5);
    let mut full = Harness::new(12, 5);
    let mut retained = vec![CellData::blank(); 60];
    // Independent terminals: full render must not consume incremental dirty state.
    for bytes in [
        b"".as_slice(),
        "ab\x1b[1;31m界e\u{301}\x1b[0m\r\nnext".as_bytes(),
        b"\x1b[2;2HZ",
        b"\x1b[1;1HA\x1b[5;8HB",
        b"\x1b]4;1;rgb:12/34/56\x07",
        b"\x1b[3;4H",
        b"\x1b[?1049hALT",
        b"\x1b[?1049l",
    ] {
        incremental.write(bytes);
        full.write(bytes);
        match incremental.pane.collect_dirty_patch(12, 5) {
            TerminalDirtyPatchOutcome::Clean => {}
            TerminalDirtyPatchOutcome::Patch(patch) => {
                for (row, cells) in patch.rows {
                    assert_eq!(cells.len(), 12);
                    let start = usize::from(row) * 12;
                    retained[start..start + 12].clone_from_slice(&cells);
                }
            }
            TerminalDirtyPatchOutcome::Fallback => {
                panic!("bounded text fixture unexpectedly fell back")
            }
        }
        assert_eq!(retained, full.full_cells(), "after {bytes:?}");
        assert_eq!(incremental.cursor(), full.cursor());
    }
}

#[test]
fn sparse_dirty_patches_preserve_coordinates_and_clipped_rows() {
    for height in [3, 6] {
        let mut terminal = Harness::new(8, 6);
        terminal.write(b"\x1b[2;3H");
        terminal.pane.collect_dirty_patch(8, 6);
        assert!(matches!(
            terminal.pane.collect_dirty_patch(8, 6),
            TerminalDirtyPatchOutcome::Clean
        ));
        terminal.write(b"\x1b[2;3HX\x1b[5;4HY");
        let TerminalDirtyPatchOutcome::Patch(patch) = terminal.pane.collect_dirty_patch(8, height)
        else {
            panic!("expected sparse patch");
        };
        let expected_rows = if height == 3 { vec![1] } else { vec![1, 4] };
        assert_eq!(
            patch.rows.iter().map(|(y, _)| *y).collect::<Vec<_>>(),
            expected_rows
        );
        assert!(patch.rows.iter().all(|(_, cells)| cells.len() == 8));

        let core = shepr_vt::lock_terminal_core(&terminal.pane.core).expect("test precondition");
        for row in core.render_state.iter_rows() {
            assert_eq!(row.is_dirty(), height == 3 && row.y() == 4);
        }
        assert_eq!(core.render_state.rows(), 6);
    }
}

#[test]
fn dirty_patch_fallback_keeps_previously_collected_rows_dirty() {
    let mut terminal = Harness::new(8, 6);
    terminal.write(b"\x1b[2;3H");
    terminal.pane.collect_dirty_patch(8, 6);
    let hook_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_ran_from_collection = std::sync::Arc::clone(&hook_ran);
    terminal.pane.on_next_dirty_collection(Box::new(move || {
        hook_ran_from_collection.store(true, std::sync::atomic::Ordering::Release);
    }));
    terminal.write(b"\x1b[2;3HX\x1b[5;4H\x1b]8;;https://example.test\x1b\\Y\x1b]8;;\x1b\\");
    assert!(matches!(
        terminal.pane.collect_dirty_patch(8, 6),
        TerminalDirtyPatchOutcome::Fallback
    ));
    assert!(
        !hook_ran.load(std::sync::atomic::Ordering::Acquire),
        "a fallback discards the collection and leaves its hook pending"
    );
    let core = shepr_vt::lock_terminal_core(&terminal.pane.core).expect("test precondition");
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "`RowView::y` takes `&self`, so it doesn't coerce to the `FnMut(RowView)` \
                  that `map` wants here; the closure below is not actually redundant"
    )]
    let dirty: Vec<_> = core
        .render_state
        .iter_rows()
        .filter(|row| row.is_dirty())
        .map(|row| row.y())
        .collect();
    assert_eq!(dirty, vec![1, 4]);
}

#[test]
fn complete_history_replay_supports_plain_append() {
    // ANSI history is text restoration, not a parser/cursor snapshot. End at a
    // non-wrapping printable cell with SGR and OSC8 closed; no pending tab/CSI.
    let mut source = Harness::new(24, 4);
    source.write(b"\x1b[31mred\x1b[0m\r\nplain");
    let ansi = source.pane.recent_unwrapped_ansi_snapshot(32).text;
    let mut restored = Harness::new(24, 4);
    restored.pane.seed_history_ansi(&ansi);
    assert_eq!(
        restored.pane.recent_text_snapshot(32),
        source.pane.recent_text_snapshot(32)
    );
    // Establish the documented live-output boundary explicitly instead of
    // requiring history formatting to restore arbitrary cursor/SGR state.
    // `source` still sits at the end of the unterminated "plain" line, so it
    // needs its own line break before the live write; `restored` was already
    // seeded with a trailing CRLF, so writing another one here would produce
    // an extra blank line that never existed in `source`.
    source.write(b"\x1b[0m\r\nappended");
    restored.write(b"\x1b[0mappended");
    assert_eq!(
        restored.pane.recent_text_snapshot(32),
        source.pane.recent_text_snapshot(32)
    );
    assert!(restored.effects.clipboard.is_empty());
    assert!(restored.effects.replies.is_empty());
}
