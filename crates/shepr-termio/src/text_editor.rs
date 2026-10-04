//! A one-line text editor driven by host keys: grapheme-safe cursor motion,
//! emacs-style kill and yank, and the visible window of its text at a width.
//! It holds no presentation; the client draws `viewport` where it wants.

use crossterm::event::KeyEventKind;
use crossterm::event::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Unhandled,
    Handled,
    Changed,
}

impl EditOutcome {
    pub fn is_handled(self) -> bool {
        self != Self::Unhandled
    }

    pub fn changed(self) -> bool {
        self == Self::Changed
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct TextEditor {
    text: String,
    cursor: usize,
    replace_on_type: bool,
    killed: String,
}

impl std::fmt::Debug for TextEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextEditor([redacted])")
    }
}

impl std::ops::Deref for TextEditor {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for TextEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.text.fmt(f)
    }
}

impl From<&str> for TextEditor {
    fn from(text: &str) -> Self {
        Self::new(text, false)
    }
}

impl TextEditor {
    pub fn new(text: &str, replace_on_type: bool) -> Self {
        let mut editor = Self::default();
        editor.insert(text);
        editor.replace_on_type = replace_on_type;
        editor
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.replace_on_type = false;
    }

    fn repair_cursor(&mut self) {
        // Insertion/deletion can join clusters across the edit. Snap forward, never
        // leave an insertion offset inside the newly formed grapheme.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|index| *index >= self.cursor)
            .unwrap_or(self.text.len());
    }

    pub fn insert(&mut self, text: &str) -> bool {
        let mut normalized = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    normalized.push(' ');
                }
                '\n' | '\t' => normalized.push(' '),
                ch if !ch.is_control() => normalized.push(ch),
                _ => {}
            }
        }
        if normalized.is_empty() {
            return false;
        }
        let content_changed = !self.replace_on_type || self.text != normalized;
        if self.replace_on_type {
            self.clear();
        }
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.repair_cursor();
        content_changed
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }

    /// Word motion for the prompt editor. This edits text the user types into
    /// a prompt (names, paths, commands), not pane text, so it keeps its own
    /// rule rather than `shepr_term::word`: a word is a run of alphanumerics
    /// and `_`, any other non-space character is punctuation, and each run
    /// of a class is one step, as in a shell line editor. Pane text uses the
    /// terminal classifier because its separators and CJK punctuation are
    /// what terminal output looks like.
    fn word_boundary(&self, backward: bool) -> usize {
        let class = |grapheme: &str| {
            let ch = grapheme.chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                0
            } else if ch.is_alphanumeric() || ch == '_' {
                1
            } else {
                2
            }
        };
        let mut boundary = self.cursor;
        let mut run = 0;
        if backward {
            for (index, grapheme) in self.text[..self.cursor].grapheme_indices(true).rev() {
                let current = class(grapheme);
                if run != 0 && current != run {
                    break;
                }
                run = current;
                boundary = index;
            }
        } else {
            for (index, grapheme) in self.text[self.cursor..].grapheme_indices(true) {
                let current = class(grapheme);
                if run != 0 && current != run {
                    break;
                }
                run = current;
                boundary = self.cursor + index + grapheme.len();
            }
        }
        boundary
    }

    fn remove(&mut self, start: usize, end: usize, kill: bool) {
        self.replace_on_type = false;
        if start == end {
            return;
        }
        if kill {
            self.killed = self.text[start..end].to_owned();
        }
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.repair_cursor();
    }

    pub fn handle_key(&mut self, key: &shepr_term::key::TerminalKey) -> EditOutcome {
        if key.kind == KeyEventKind::Release {
            return EditOutcome::Unhandled;
        }
        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            return EditOutcome::Unhandled;
        }
        let previous_len = self.text.len();
        let mut content_changed = false;
        let shepr_term::key::KeyChord { code, modifiers } = key.chord().normalized();
        // Explicit text from the host is authoritative, including AltGr/composition.
        if let Some(text) = key
            .generated_text
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            content_changed = self.insert(text);
        } else {
            let ctrl = modifiers == KeyModifiers::CONTROL;
            let alt = modifiers == KeyModifiers::ALT;
            let plain = modifiers.is_empty();
            let movement = match code {
                KeyCode::Left if plain => Some(self.previous()),
                KeyCode::Char('b') if ctrl => Some(self.previous()),
                KeyCode::Right if plain => Some(self.next()),
                KeyCode::Char('f') if ctrl => Some(self.next()),
                KeyCode::Home if plain => Some(0),
                KeyCode::Char('a') if ctrl => Some(0),
                KeyCode::End if plain => Some(self.text.len()),
                KeyCode::Char('e') if ctrl => Some(self.text.len()),
                KeyCode::Char('b') if alt => Some(self.word_boundary(true)),
                KeyCode::Char('f') if alt => Some(self.word_boundary(false)),
                _ => None,
            };
            if let Some(position) = movement {
                self.cursor = position;
                self.replace_on_type = false;
            } else {
                match code {
                    KeyCode::Backspace | KeyCode::Char('h')
                        if code == KeyCode::Backspace && plain
                            || code == KeyCode::Char('h') && ctrl =>
                    {
                        if self.replace_on_type {
                            self.clear();
                        } else {
                            self.remove(self.previous(), self.cursor, false);
                        }
                    }
                    KeyCode::Delete if plain => self.remove(self.cursor, self.next(), false),
                    KeyCode::Char('d') if ctrl => self.remove(self.cursor, self.next(), false),
                    KeyCode::Char('u') if ctrl => self.remove(0, self.cursor, true),
                    KeyCode::Char('k') if ctrl => self.remove(self.cursor, self.text.len(), true),
                    KeyCode::Char('w') if ctrl => {
                        self.remove(self.word_boundary(true), self.cursor, true);
                    }
                    KeyCode::Backspace if ctrl || alt => {
                        self.remove(self.word_boundary(true), self.cursor, true);
                    }
                    KeyCode::Char('d') if alt => {
                        self.remove(self.cursor, self.word_boundary(false), true);
                    }
                    KeyCode::Char('y') if ctrl => {
                        content_changed = self.insert(&self.killed.clone());
                    }
                    KeyCode::Char(_) if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                        if let Some(ch) = crate::input::keybind_help_text_char(key) {
                            content_changed = self.insert(&ch.to_string());
                        }
                    }
                    _ => return EditOutcome::Unhandled,
                }
            }
        }
        if content_changed || self.text.len() != previous_len {
            EditOutcome::Changed
        } else {
            EditOutcome::Handled
        }
    }

    /// The part of the text a field `width` cells wide shows around the
    /// cursor, and the cursor's column in it.
    pub fn viewport(&self, width: u16) -> (&str, u16) {
        if width == 0 {
            return ("", 0);
        }
        let mut start = self.cursor;
        let mut cells = 0;
        for (index, grapheme) in self.text[..self.cursor].grapheme_indices(true).rev() {
            let next = cells + shepr_term::width::text_width(grapheme);
            if next >= usize::from(width) {
                break;
            }
            start = index;
            cells = next;
        }
        let mut end = self.cursor;
        let mut used = cells;
        for (index, grapheme) in self.text[self.cursor..].grapheme_indices(true) {
            used += shepr_term::width::text_width(grapheme);
            if used > usize::from(width) {
                break;
            }
            end = self.cursor + index + grapheme.len();
        }
        (
            &self.text[start..end],
            u16::try_from(cells).unwrap_or(u16::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventKind;
    use crossterm::event::{KeyCode, KeyModifiers};
    use unicode_segmentation::UnicodeSegmentation;

    use super::{EditOutcome, TextEditor};
    use shepr_term::key::TerminalKey;

    fn key(editor: &mut TextEditor, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let result = editor.handle_key(&TerminalKey::new(code, modifiers));
        assert_ne!(result, EditOutcome::Unhandled, "editor binding");
        assert!(
            editor.cursor == editor.len()
                || editor
                    .grapheme_indices(true)
                    .any(|(i, _)| i == editor.cursor)
        );
        result.changed()
    }

    #[test]
    fn every_binding_edits_at_the_cursor() {
        use KeyCode::*;
        let plain = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        for (code, modifiers, text, cursor, killed) in [
            (Left, plain, "one two", 3, ""),
            (Char('b'), ctrl, "one two", 3, ""),
            (Right, plain, "one two", 5, ""),
            (Char('f'), ctrl, "one two", 5, ""),
            (Home, plain, "one two", 0, ""),
            (Char('a'), ctrl, "one two", 0, ""),
            (End, plain, "one two", 7, ""),
            (Char('e'), ctrl, "one two", 7, ""),
            (Backspace, plain, "onetwo", 3, ""),
            (Char('h'), ctrl, "onetwo", 3, ""),
            (Delete, plain, "one wo", 4, ""),
            (Char('d'), ctrl, "one wo", 4, ""),
            (Char('b'), alt, "one two", 0, ""),
            (Char('f'), alt, "one two", 7, ""),
            (Char('u'), ctrl, "two", 0, "one "),
            (Char('k'), ctrl, "one ", 4, "two"),
            (Char('w'), ctrl, "two", 0, "one "),
            (Backspace, alt, "two", 0, "one "),
            (Backspace, ctrl, "two", 0, "one "),
            (Char('d'), alt, "one ", 4, "two"),
            (Char('y'), ctrl, "one two", 4, ""),
            (Char('X'), plain, "one Xtwo", 5, ""),
        ] {
            let mut editor = TextEditor::from("one two");
            editor.cursor = 4;
            let result = key(&mut editor, code, modifiers);
            assert_eq!(
                (editor.as_str(), editor.cursor, editor.killed.as_str()),
                (text, cursor, killed),
                "{code:?} {modifiers:?}"
            );
            assert_eq!(result, text != "one two");
            let mut empty = TextEditor::default();
            key(&mut empty, code, modifiers);
        }
    }

    #[test]
    fn suggestions_movement_kills_and_yank() {
        for (code, expected) in [
            (KeyCode::Left, "defaulxt"),
            (KeyCode::Right, "defaultx"),
            (KeyCode::Home, "xdefault"),
            (KeyCode::End, "defaultx"),
        ] {
            let mut editor = TextEditor::new("default", true);
            key(&mut editor, code, KeyModifiers::NONE);
            editor.insert("x");
            assert_eq!(editor.as_str(), expected);
        }
        let mut editor = TextEditor::new("default", true);
        editor.insert("new");
        assert_eq!(editor.as_str(), "new");
        for (replacement, changed) in [("default", false), ("another", true)] {
            let mut editor = TextEditor::new("default", true);
            let event = TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE)
                .with_generated_text(Some(replacement.into()));
            assert_eq!(
                editor.handle_key(&event),
                if changed {
                    EditOutcome::Changed
                } else {
                    EditOutcome::Handled
                }
            );
            assert_eq!(editor.as_str(), replacement);
            assert!(!editor.replace_on_type);
        }
        let mut editor = TextEditor::new("default", true);
        key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(editor.is_empty());
        let mut editor = TextEditor::new("one two", true);
        key(&mut editor, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(editor.as_str(), "one ");
        assert!(!editor.replace_on_type);
        key(&mut editor, KeyCode::Char('k'), KeyModifiers::CONTROL);
        key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        key(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        key(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(editor.as_str(), "onetwotwo");
        key(&mut editor, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(editor.killed, "onetwotwo");
        let mut other = TextEditor::default();
        key(&mut other, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert!(other.is_empty());
    }

    #[test]
    fn unicode_graphemes_and_boundary_changing_edits() {
        let mut editor = TextEditor::from("e\u{301}中\u{1F469}\u{200D}\u{1F4BB}");
        for expected in ["e\u{301}中", "e\u{301}", ""] {
            key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
            assert_eq!(editor.as_str(), expected);
        }
        let mut editor = TextEditor::from("\u{1F469}\u{1F4BB}");
        key(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        editor.insert("\u{200d}");
        assert_eq!(editor.cursor, editor.len());
        key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(editor.is_empty());
        let mut editor = TextEditor::from("\u{301}x");
        key(&mut editor, KeyCode::Home, KeyModifiers::NONE);
        editor.insert("e");
        assert_eq!(editor.cursor, "e\u{301}".len());
        key(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.as_str(), "e\u{301}");
    }

    #[test]
    fn words_distinguish_paths_punctuation_and_whitespace() {
        let mut editor = TextEditor::from("src/foo_bar.rs  e\u{301}中 \u{1F469}\u{200D}\u{1F4BB}");
        for expected in [
            "src/foo_bar.rs  e\u{301}中 ",
            "src/foo_bar.rs  ",
            "src/foo_bar.",
            "src/foo_bar",
            "src/",
            "src",
            "",
        ] {
            key(&mut editor, KeyCode::Char('w'), KeyModifiers::CONTROL);
            assert_eq!(editor.as_str(), expected);
        }
        let mut editor = TextEditor::from(" /tmp/foo_bar.rs");
        key(&mut editor, KeyCode::Home, KeyModifiers::NONE);
        for expected in [2, 5, 6, 13, 14, 16] {
            key(&mut editor, KeyCode::Char('f'), KeyModifiers::ALT);
            assert_eq!(editor.cursor, expected);
        }
    }

    #[test]
    fn insertion_normalizes_controls_and_respects_host_text() {
        let mut editor = TextEditor::from("ab");
        key(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        editor.insert("中\r\n\r\n\t\x00\x1b\u{7f}e\u{301}");
        assert_eq!(editor.as_str(), "a中   e\u{301}b");
        let event = TerminalKey::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )
        .with_generated_text(Some("β".into()));
        editor.handle_key(&event);
        assert_eq!(editor.as_str(), "a中   e\u{301}βb");
        let before = editor.clone();
        assert_eq!(
            editor.handle_key(&event.with_kind(KeyEventKind::Release)),
            EditOutcome::Unhandled
        );
        assert_eq!(editor, before);
        let repeat =
            TerminalKey::new(KeyCode::Left, KeyModifiers::NONE).with_kind(KeyEventKind::Repeat);
        assert!(editor.handle_key(&repeat).is_handled());
    }

    #[test]
    fn enter_and_escape_ignore_generated_text() {
        for code in [KeyCode::Enter, KeyCode::Esc] {
            let mut editor = TextEditor::new("default", true);
            let before = editor.clone();
            let event = TerminalKey::new(code, KeyModifiers::NONE)
                .with_generated_text(Some("printable".into()));
            assert_eq!(
                editor.handle_key(&event),
                EditOutcome::Unhandled,
                "{code:?}"
            );
            assert_eq!(editor, before, "{code:?}");
        }
    }

    #[test]
    fn viewport_is_pure_and_grapheme_safe() {
        for text in [
            "abcdefghijklmnopqrstuvwxyz",
            "e\u{301}中\u{1F469}\u{200D}\u{1F4BB}xyz",
            "\u{301}abc",
        ] {
            let mut editor = TextEditor::from(text);
            for cursor in text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain([text.len()])
            {
                editor.cursor = cursor;
                for width in [0, 1, 2, 3, 8, 80] {
                    let before = editor.clone();
                    let (visible, col) = editor.viewport(width);
                    assert!(shepr_term::width::text_width(visible) <= usize::from(width));
                    assert!(width == 0 || col < width);
                    assert_eq!(editor, before);
                }
            }
        }
        let mut editor = TextEditor::from("abcdef");
        assert_eq!(editor.viewport(4), ("def", 3));
        editor.cursor = 2;
        assert_eq!(editor.viewport(4), ("abcd", 2));
        editor.cursor = 0;
        assert_eq!(editor.viewport(4), ("abcd", 0));
    }
}
