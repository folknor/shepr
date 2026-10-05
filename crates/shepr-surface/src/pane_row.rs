//! The wide-glyph rule for a row of pane cells as it is emitted.
//!
//! A pane cell carries its terminal grid width. A wide glyph is a nonempty
//! `WideLead` followed by a `WideTail`; the client draws the lead
//! across both columns and skips the tail. A row cut narrower than its terminal
//! (a client view smaller than the pane's PTY), or one the emulator left with a
//! broken pair (deleting the lead, erasing only the tail), can hold a lead
//! without its tail or a tail without its lead. A lead drawn without its tail
//! spills into whatever column follows (a border, a gutter, the next pane); a
//! lone tail draws nothing and leaves stale content. Every place that emits pane
//! cells at some width normalizes the row at that width, so both halves of a
//! broken pair become one-column blanks.

use shepr_protocol::{CellData, GridCellWidth, WireStyleFlags};

fn is_lead(cell: &CellData) -> bool {
    cell.grid_width == GridCellWidth::WideLead && !cell.symbol.is_empty()
}

// Pair validity is a row property: changed spans may start with a tail whose
// lead remains in the baseline, so cell deserialization must not repair it.
fn is_tail(cell: &CellData) -> bool {
    cell.grid_width == GridCellWidth::WideTail
}

/// Turns one half of a broken wide pair into a one-column blank. Its colours
/// and reverse flag stay, so the visible background is the one it had; the
/// decorations a space would still show (underline, strikethrough) and its
/// link go, since the glyph they belonged to is gone.
pub fn blank_pane_cell(cell: &mut CellData) {
    cell.symbol.clear();
    cell.symbol.push(' ');
    cell.grid_width = GridCellWidth::One;
    cell.hyperlink = None;
    cell.style.underline = shepr_term::UnderlineStyle::None;
    // `WireStyleFlags` offers `toggle`, not a remove.
    if cell.style.flags.contains(WireStyleFlags::CROSSED_OUT) {
        cell.style.flags.toggle(WireStyleFlags::CROSSED_OUT);
    }
}

/// Visits the index of every cell [`normalize_pane_row`] would blank.
fn broken_cells(row: &[CellData], mut visit: impl FnMut(usize)) {
    let mut x = 0;
    while x < row.len() {
        let cell = &row[x];
        if is_lead(cell) {
            if row.get(x + 1).is_some_and(is_tail) {
                x += 2;
                continue;
            }
            visit(x);
        } else if is_tail(cell)
            || cell.symbol.is_empty()
            || cell.grid_width == GridCellWidth::WideLead
        {
            // A tail no lead claimed, an empty non-tail cell, or an empty lead,
            // none of which is half of a drawable pair.
            visit(x);
        }
        x += 1;
    }
}

/// Whether every wide glyph in `row` is a complete pair and every tail
/// belongs to one, so [`normalize_pane_row`] would change nothing.
pub fn pane_row_is_normalized(row: &[CellData]) -> bool {
    let mut normalized = true;
    broken_cells(row, |_| normalized = false);
    normalized
}

/// Blanks both halves of every broken wide pair in `row`, the pane cells of
/// one terminal row exactly as wide as they are emitted.
pub fn normalize_pane_row(row: &mut [CellData]) {
    let mut broken = Vec::new();
    broken_cells(row, |x| broken.push(x));
    for x in broken {
        blank_pane_cell(&mut row[x]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{CompactString, ToCompactString, WireColor, WireStyle};

    /// One pane row: `W` is a wide lead, `~` an empty tail, other chars narrow.
    fn row(text: &str) -> Vec<CellData> {
        text.chars()
            .map(|c| CellData {
                symbol: match c {
                    '~' => CompactString::default(),
                    'W' => "\u{754c}".into(),
                    c => c.to_compact_string(),
                },
                grid_width: match c {
                    'W' => GridCellWidth::WideLead,
                    '~' => GridCellWidth::WideTail,
                    _ => GridCellWidth::One,
                },
                fg: WireColor::Reset,
                bg: WireColor::Reset,
                style: WireStyle::default(),
                hyperlink: None,
            })
            .collect()
    }

    fn text(row: &[CellData]) -> String {
        row.iter()
            .map(|cell| match cell.symbol.as_str() {
                "" => "~".to_owned(),
                "\u{754c}" => "W".to_owned(),
                symbol => symbol.to_owned(),
            })
            .collect()
    }

    fn normalized(text_row: &str, width: usize) -> Vec<CellData> {
        let mut cells = row(text_row);
        cells.truncate(width);
        normalize_pane_row(&mut cells);
        cells
    }

    #[test]
    fn complete_pairs_are_kept_and_broken_halves_blanked() {
        assert_eq!(text(&normalized("aW~b", 4)), "aW~b");
        // The crop cuts the tail off.
        assert_eq!(text(&normalized("aW~b", 2)), "a ");
        // Deleting a lead leaves its tail at column zero.
        assert_eq!(text(&normalized("~bW~", 4)), " bW~");
        // Erasing only the tail leaves a lead followed by a narrow blank.
        assert_eq!(text(&normalized("W c", 3)), "  c");
        // A tail is claimed once: two in a row leave the second orphaned.
        assert_eq!(text(&normalized("W~~", 3)), "W~ ");
    }

    #[test]
    fn a_blank_keeps_the_background_and_drops_link_and_decorations() {
        let mut cells = row("W");
        cells[0].bg = WireColor::Blue;
        cells[0].fg = WireColor::Red;
        cells[0].hyperlink = Some(2);
        cells[0].style.underline = shepr_term::UnderlineStyle::Curly;
        cells[0].style.flags = WireStyleFlags::REVERSED.union(WireStyleFlags::CROSSED_OUT);
        normalize_pane_row(&mut cells);
        let cell = &cells[0];
        assert_eq!(cell.symbol, " ");
        assert_eq!(cell.grid_width, GridCellWidth::One);
        assert_eq!((cell.fg, cell.bg), (WireColor::Red, WireColor::Blue));
        assert_eq!(cell.hyperlink, None);
        assert_eq!(cell.style.underline, shepr_term::UnderlineStyle::None);
        assert_eq!(cell.style.flags, WireStyleFlags::REVERSED);
    }

    #[test]
    fn normalizing_is_idempotent_and_cropping_twice_matches_cropping_once() {
        let rows = ["aW~bW~c", "~W~W~~W", "WWW~ W~", "W~W~W~W", ""];
        for source in rows {
            let len = source.chars().count();
            for wide in 0..=len {
                let once = normalized(source, wide);
                let mut twice = once.clone();
                normalize_pane_row(&mut twice);
                assert_eq!(twice, once, "{source} at {wide}");
                assert!(pane_row_is_normalized(&once), "{source} at {wide}");
                for narrow in 0..=wide {
                    let mut recut = once[..narrow].to_vec();
                    normalize_pane_row(&mut recut);
                    assert_eq!(
                        recut,
                        normalized(source, narrow),
                        "{source} at {wide} then {narrow}"
                    );
                }
            }
        }
    }
}
