//! Drawing a prompt's `shepr_termio::text_editor::TextEditor` into a buffer.

use shepr_termio::text_editor::TextEditor;

pub(in crate::shell) fn render(
    buffer: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    editor: &TextEditor,
    style: ratatui::style::Style,
) -> Option<shepr_protocol::CursorState> {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return None;
    }
    let (text, cursor) = editor.viewport(area.width);
    for x in area.x..area.right() {
        buffer[(x, area.y)].set_symbol(" ").set_style(style);
    }
    crate::shell::presentation::text::put_text(buffer, area.x, area.y, area.width, text, style);
    Some(shepr_protocol::CursorState {
        x: area.x + cursor,
        y: area.y,
        visible: true,
        shape: shepr_protocol::CursorShapeParam::Default,
    })
}

#[cfg(test)]
mod tests {
    use super::render;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use shepr_termio::text_editor::TextEditor;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn render_is_pure_and_places_the_cursor_at_the_viewport_column() {
        for text in [
            "abcdefghijklmnopqrstuvwxyz",
            "e\u{301}中\u{1F469}\u{200D}\u{1F4BB}xyz",
            "\u{301}abc",
        ] {
            let mut editor = TextEditor::from(text);
            // Walk the cursor back from the end one grapheme at a time.
            for _ in 0..=text.graphemes(true).count() {
                for width in [0, 1, 2, 3, 8, 80] {
                    let before = editor.clone();
                    let (_, col) = editor.viewport(width);
                    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 1));
                    let result = render(
                        &mut buffer,
                        Rect::new(0, 0, width, 1),
                        &editor,
                        Style::default(),
                    );
                    assert_eq!(result.map(|c| c.x), (width > 0).then_some(col));
                    assert_eq!(editor, before);
                }
                editor.handle_key(&shepr_term::key::TerminalKey::new(
                    crossterm::event::KeyCode::Left,
                    crossterm::event::KeyModifiers::NONE,
                ));
            }
        }
    }
}
