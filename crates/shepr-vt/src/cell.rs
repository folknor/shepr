use super::*;
use crate::limits::MAX_UNICODE_CODEPOINT_WIDTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Palette(u8),
    Rgb(RgbColor),
}

/// The terminal underline shape carried by a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    pub(super) fn from_flags(flags: Flags) -> Self {
        if flags.contains(Flags::UNDERLINE) {
            Self::Single
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            Self::Double
        } else if flags.contains(Flags::UNDERCURL) {
            Self::Curly
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Self::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Self::Dashed
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg_color: Option<CellColor>,
    pub bg_color: Option<CellColor>,
    pub underline_color: Option<CellColor>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub underline: UnderlineStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderColors {
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub palette: [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWide {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

/// How a row joins its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowWrap {
    /// The row's text continues on the next row.
    pub soft_wrapped: bool,
    /// The row continues the previous row's text.
    pub wrap_continuation: bool,
}

pub(super) fn is_halfwidth_voiced_mark(character: char) -> bool {
    matches!(character, '\u{ff9e}' | '\u{ff9f}')
}

/// U+FF9E/U+FF9F on their own. unicode-width measures them as zero-width, but
/// the terminal core gives them a cell (as wcwidth does).
pub fn is_halfwidth_katakana_voiced_mark(symbol: &str) -> bool {
    let mut characters = symbol.chars();
    let Some(mark) = characters.next() else {
        return false;
    };
    characters.next().is_none() && is_halfwidth_voiced_mark(mark)
}

/// A halfwidth katakana letter followed by its voiced mark: two columns in the
/// terminal core, although unicode-width measures the pair as one.
pub fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
    let mut characters = symbol.chars();
    let Some(base) = characters.next() else {
        return false;
    };
    let Some(mark) = characters.next() else {
        return false;
    };
    characters.next().is_none()
        && ('\u{ff66}'..='\u{ff9d}').contains(&base)
        && is_halfwidth_voiced_mark(mark)
}

pub fn unicode_codepoint_width(character: char) -> u8 {
    if is_halfwidth_voiced_mark(character) {
        return 1;
    }
    u8::try_from(
        character
            .width()
            .unwrap_or(0)
            .min(usize::from(MAX_UNICODE_CODEPOINT_WIDTH)),
    )
    .unwrap_or(MAX_UNICODE_CODEPOINT_WIDTH)
}

/// A codepoint and any following zero-width codepoints stored in its cell.
///
/// This follows per-codepoint grid widths, not Unicode grapheme clusters.
/// Halfwidth voiced marks use the terminal-specific one-cell override in
/// [`unicode_codepoint_width`].
pub struct UnicodeDisplayUnits<'a> {
    text: &'a str,
    characters: std::str::CharIndices<'a>,
    next_character: Option<(usize, char, u8)>,
}

impl<'a> Iterator for UnicodeDisplayUnits<'a> {
    type Item = (&'a str, u8);

    fn next(&mut self) -> Option<Self::Item> {
        let (start, character, width) = match self.next_character.take() {
            Some(next) => next,
            None => {
                let (index, character) = self.characters.next()?;
                (index, character, unicode_codepoint_width(character))
            }
        };
        let first_len = character.len_utf8();
        let mut end = start + first_len;
        if !character.is_control() {
            for (index, following) in self.characters.by_ref() {
                let following_width = unicode_codepoint_width(following);
                if following.is_control() || following_width != 0 {
                    self.next_character = Some((index, following, following_width));
                    break;
                }
                end = index + following.len_utf8();
            }
        }
        let unit = &self.text[start..end];
        Some((unit, width))
    }
}

/// Iterate text by grid cells without allocating or using grapheme widths.
/// Each item starts with one codepoint and includes following zero-width
/// codepoints stored with it; a leading zero-width run has width zero.
pub fn unicode_display_units(text: &str) -> UnicodeDisplayUnits<'_> {
    UnicodeDisplayUnits {
        text,
        characters: text.char_indices(),
        next_character: None,
    }
}

/// Width of text under the terminal grid's per-codepoint and voiced-mark rules.
pub fn unicode_text_width(text: &str) -> usize {
    unicode_display_units(text).fold(0usize, |width, (_, unit_width)| {
        width.saturating_add(usize::from(unit_width))
    })
}

pub(super) fn cell_wide(cell: &Cell) -> CellWide {
    if cell.flags.contains(Flags::WIDE_CHAR) {
        CellWide::Wide
    } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        CellWide::SpacerTail
    } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        CellWide::SpacerHead
    } else {
        CellWide::Narrow
    }
}

fn cell_zerowidth(cell: &Cell) -> &[char] {
    cell.zerowidth().unwrap_or(&[])
}

pub(super) enum CellText<'a> {
    Empty,
    Grapheme { base: char, zerowidth: &'a [char] },
}

/// One classification for the text-facing cell adapters. Empty cells,
/// spacers and kitty graphics placeholders all represent a blank cell.
pub(super) fn cell_text(cell: &Cell) -> CellText<'_> {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        || u32::from(cell.c) == KITTY_UNICODE_PLACEHOLDER
    {
        return CellText::Empty;
    }
    let zerowidth = cell_zerowidth(cell);
    if (cell.c == ' ' || cell.c == '\t') && zerowidth.is_empty() {
        return CellText::Empty;
    }
    CellText::Grapheme {
        base: if cell.c == '\t' { ' ' } else { cell.c },
        zerowidth,
    }
}

/// The cell's text as readers show it, into `out`: the grapheme, or a single
/// space for a cell classified as empty.
pub(super) fn cell_text_into(cell: &Cell, out: &mut String) {
    out.clear();
    match cell_text(cell) {
        CellText::Empty => out.push(' '),
        CellText::Grapheme { base, zerowidth } => {
            out.push(base);
            out.extend(zerowidth.iter().copied());
        }
    }
}

fn cell_color(color: Color) -> Option<CellColor> {
    match color {
        Color::Named(named) => {
            let index = named as usize;
            (index < super::color::NAMED_COLOR_COUNT)
                .then(|| CellColor::Palette(u8::try_from(index).unwrap_or(u8::MAX)))
        }
        Color::Indexed(index) => Some(CellColor::Palette(index)),
        Color::Spec(rgb) => Some(CellColor::Rgb(RgbColor::from_vte(rgb))),
    }
}

fn resolve_cell_color(color: CellColor, colors: &RenderColors) -> RgbColor {
    match color {
        CellColor::Palette(index) => colors.palette[usize::from(index)],
        CellColor::Rgb(rgb) => rgb,
    }
}

pub(super) fn cell_style(cell: &Cell) -> CellStyle {
    let flags = cell.flags;
    CellStyle {
        fg_color: cell_color(cell.fg),
        bg_color: cell_color(cell.bg),
        underline_color: cell.underline_color().and_then(cell_color),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        faint: flags.contains(Flags::DIM),
        inverse: flags.contains(Flags::INVERSE),
        invisible: flags.contains(Flags::HIDDEN),
        strikethrough: flags.contains(Flags::STRIKEOUT),
        underline: UnderlineStyle::from_flags(flags),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBasicData {
    pub wide: CellWide,
    pub has_hyperlink: bool,
    pub has_styling: bool,
    pub style: CellStyle,
}

impl Default for CellBasicData {
    fn default() -> Self {
        Self {
            wide: CellWide::Narrow,
            has_hyperlink: false,
            has_styling: false,
            style: CellStyle::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct CellView<'a> {
    pub(super) cell: &'a Cell,
    pub(super) colors: &'a RenderColors,
}

impl CellView<'_> {
    pub fn basic_data(&self) -> CellBasicData {
        let cell = self.cell;
        let style = cell_style(cell);
        CellBasicData {
            wide: cell_wide(cell),
            has_hyperlink: cell.hyperlink().is_some(),
            has_styling: style != CellStyle::default(),
            style,
        }
    }

    pub fn wide(&self) -> CellWide {
        cell_wide(self.cell)
    }

    pub fn has_hyperlink(&self) -> bool {
        self.cell.hyperlink().is_some()
    }

    /// The cell's explicit foreground resolved to RGB; `None` for default.
    pub fn fg_color(&self) -> Option<RgbColor> {
        cell_color(self.cell.fg).map(|color| resolve_cell_color(color, self.colors))
    }

    /// The cell's explicit background resolved to RGB; `None` for default.
    pub fn bg_color(&self) -> Option<RgbColor> {
        cell_color(self.cell.bg).map(|color| resolve_cell_color(color, self.colors))
    }

    pub fn grapheme_text(&self) -> String {
        let mut text = String::new();
        self.grapheme_text_into(&mut text);
        text
    }

    /// Writes the cell's grapheme into `text` (empty for blank cells and spacers).
    pub fn grapheme_text_into(&self, text: &mut String) {
        text.clear();
        match cell_text(self.cell) {
            CellText::Empty => {}
            CellText::Grapheme { base, zerowidth } => {
                text.push(base);
                text.extend(zerowidth.iter().copied());
            }
        }
    }
}
