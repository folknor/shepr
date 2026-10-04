//! Drawing primitives the overlays share: bordered panels, centred popups, buttons and rows
//! of buttons.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::limits::{
    MIN_OVERLAY_POPUP_HEIGHT, MIN_OVERLAY_POPUP_WIDTH, OVERLAY_POPUP_HORIZONTAL_MARGIN,
    OVERLAY_POPUP_VERTICAL_MARGIN,
};
use crate::shell::presentation::text::{display_width, put_text};

/// The area inside a panel's border, or `None` when the panel is too small to have one.
pub(super) fn panel_inner(a: Rect) -> Option<Rect> {
    if a.width < 2 || a.height < 2 {
        return None;
    }
    Some(Rect::new(a.x + 1, a.y + 1, a.width - 2, a.height - 2))
}

pub(super) fn panel(b: &mut Buffer, a: Rect, c: Color, bg: Color) -> Option<Rect> {
    let inner = panel_inner(a)?;
    let background = Style::default().bg(bg).remove_modifier(Modifier::DIM);
    let border = Style::default().fg(c).bg(bg).remove_modifier(Modifier::DIM);
    // A panel is opaque: every cell is reset before anything is drawn, so nothing already in
    // the buffer (pane attributes, an earlier draw) leaks into the popup.
    for y in a.y..a.bottom() {
        for x in a.x..a.right() {
            if let Some(cell) = b.cell_mut((x, y)) {
                cell.reset();
            }
            set_cell(b, x, y, " ", background);
        }
    }
    for x in a.x..a.right() {
        let top = if x == a.x {
            "┌"
        } else if x + 1 == a.right() {
            "┐"
        } else {
            "─"
        };
        set_cell(b, x, a.y, top, border);
        let bottom = if x == a.x {
            "└"
        } else if x + 1 == a.right() {
            "┘"
        } else {
            "─"
        };
        set_cell(b, x, a.bottom() - 1, bottom, border);
    }
    for y in a.y + 1..a.bottom() - 1 {
        set_cell(b, a.x, y, "│", border);
        set_cell(b, a.right() - 1, y, "│", border);
    }
    Some(inner)
}

/// Writes one cell, skipping positions outside the buffer. Menus are placed from pointer
/// positions and popups from the frame size; `Buffer` indexing would panic on any rect that
/// reaches past the frame.
pub(super) fn set_cell(b: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = b.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// A popup of at most `w` by `h` centred in `a`, shrunk to leave the overlay margins free, or
/// `None` when what is left is below the smallest popup drawn.
pub(super) fn popup(a: Rect, w: u16, h: u16) -> Option<Rect> {
    let w = w.min(a.width.saturating_sub(OVERLAY_POPUP_HORIZONTAL_MARGIN));
    let h = h.min(a.height.saturating_sub(OVERLAY_POPUP_VERTICAL_MARGIN));
    if w < MIN_OVERLAY_POPUP_WIDTH || h < MIN_OVERLAY_POPUP_HEIGHT {
        return None;
    }
    Some(Rect::new(
        a.x + (a.width - w) / 2,
        a.y + (a.height - h) / 2,
        w,
        h,
    ))
}

pub(super) fn button(b: &mut Buffer, r: Rect, t: &str, s: Style) {
    b.set_style(r, s);
    let w = display_width(t).min(r.width);
    put_text(b, r.x + (r.width - w) / 2, r.y, w, t, s);
}

pub(super) fn row(i: Rect, ws: &[u16], gap: u16, off: u16) -> Vec<Rect> {
    let total = ws.iter().sum::<u16>()
        + gap * u16::try_from(ws.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let mut x = i.x + i.width.saturating_sub(total) / 2;
    ws.iter()
        .map(|w| {
            let r = Rect::new(
                x,
                i.y + off.min(i.height.saturating_sub(1)),
                (*w).min(i.width.saturating_sub(x - i.x)),
                1,
            );
            x += *w + gap;
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};

    use super::panel;

    #[test]
    fn panel_resets_every_cell_so_nothing_leaks_into_the_popup() {
        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let attributed = Style::default()
            .fg(Color::Red)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED | Modifier::DIM);
        for y in 0..5 {
            for x in 0..8 {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol("x").set_style(attributed);
                }
            }
        }
        let inner = panel(
            &mut buffer,
            Rect::new(1, 1, 6, 3),
            Color::Green,
            Color::Black,
        )
        .expect("panel fits");
        assert_eq!(inner, Rect::new(2, 2, 4, 1));
        for y in 1..4 {
            for x in 1..7 {
                let cell = &buffer[(x, y)];
                let border = x == 1 || x == 6 || y == 1 || y == 3;
                assert_eq!(cell.bg, Color::Black, "({x}, {y})");
                assert_eq!(cell.modifier, Modifier::empty(), "({x}, {y})");
                if border {
                    assert_eq!(cell.fg, Color::Green, "({x}, {y})");
                } else {
                    assert_eq!(cell.symbol(), " ", "({x}, {y})");
                    assert_eq!(cell.fg, Color::Reset, "({x}, {y})");
                }
            }
        }
        // Cells outside the panel are untouched.
        assert_eq!(buffer[(0, 0)].symbol(), "x");
    }
}
