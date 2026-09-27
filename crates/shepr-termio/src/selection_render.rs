use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};
use shepr_config::theme::Palette;

fn panel_contrast_fg(p: &Palette) -> Color {
    if p.panel_bg == Color::Reset {
        p.surface_dim
    } else {
        p.panel_bg
    }
}

pub fn render_selection_highlight<P: PartialEq>(
    selection: Option<&shepr_vt::selection::Selection<P>>,
    buffer: &mut Buffer,
    pane_id: &P,
    inner: Rect,
    scroll_metrics: Option<shepr_protocol::ScrollMetrics>,
    p: &Palette,
    host_theme: crate::host_term::theme::TerminalTheme,
) {
    let Some(selection) =
        selection.filter(|selection| selection.is_visible() && &selection.pane_id == pane_id)
    else {
        return;
    };
    // Selection rows are absolute. Without the scroll origin, viewport rows
    // cannot be mapped back to them, so painting a fallback can mark the wrong
    // content when scrollback exists.
    let Some(scroll_metrics) = scroll_metrics else {
        return;
    };
    let style = automatic_selection_style(p, host_theme);
    // `inner` can extend past the buffer: the client composes pane surfaces whose
    // geometry was produced for a different layout (a resize or sidebar toggle racing
    // an in-flight surface, or the tab bar appearing when a second tab opens). Only
    // the visible part of `inner` is painted; `Buffer` indexing would panic.
    let visible = inner.intersection(buffer.area);
    if visible.is_empty() {
        return;
    }
    for screen_y in visible.top()..visible.bottom() {
        let y = screen_y - inner.y;
        let row = shepr_vt::ViewportRow(y);
        let absolute_row = scroll_metrics.absolute_row_at_viewport(row);
        for screen_x in visible.left()..visible.right() {
            let x = screen_x - inner.x;
            if selection.contains(shepr_vt::Point::new(absolute_row, x))
                && let Some(cell) = buffer.cell_mut((screen_x, screen_y))
            {
                cell.set_style(style);
            }
        }
    }
}

type Rgb = (u8, u8, u8);

pub fn automatic_selection_style(
    p: &Palette,
    host_theme: crate::host_term::theme::TerminalTheme,
) -> Style {
    let bg = automatic_selection_bg(p, host_theme);
    Style::reset().fg(selection_fg_for_bg(bg, p)).bg(bg)
}

pub fn automatic_selection_bg(
    p: &Palette,
    host_theme: crate::host_term::theme::TerminalTheme,
) -> Color {
    let fallback = selection_palette_background(p);
    let Some(background) = host_theme
        .background
        .map(|color| (color.r, color.g, color.b))
        .or(match fallback {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            _ => None,
        })
    else {
        return fallback;
    };

    let target = if relative_luminance(background) < 0.5 {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    };
    let selected = mix_rgb(background, target, 0.28);
    Color::Rgb(selected.0, selected.1, selected.2)
}

fn selection_palette_background(p: &Palette) -> Color {
    if p.panel_bg == Color::Reset {
        p.surface_dim
    } else {
        p.panel_bg
    }
}

fn selection_fg_for_bg(bg: Color, p: &Palette) -> Color {
    if let Color::Rgb(r, g, b) = bg {
        let luminance = relative_luminance((r, g, b));
        let black_contrast = (luminance + 0.05) / 0.05;
        let white_contrast = 1.05 / (luminance + 0.05);
        return if black_contrast > white_contrast {
            Color::Rgb(0, 0, 0)
        } else {
            Color::Rgb(255, 255, 255)
        };
    }

    color_to_rgb(bg)
        .map(|bg| {
            if relative_luminance(bg) < 0.5 {
                Color::White
            } else {
                Color::Black
            }
        })
        .unwrap_or_else(|| panel_contrast_fg(p))
}

fn mix_rgb(base: Rgb, target: Rgb, amount: f32) -> Rgb {
    fn channel(base: u8, target: u8, amount: f32) -> u8 {
        // amount is a mix fraction in [0, 1] and base/target are u8 channel
        // values, so the interpolated result stays within u8 range.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mixed =
            (f32::from(base) + (f32::from(target) - f32::from(base)) * amount).round() as u8;
        mixed
    }
    (
        channel(base.0, target.0, amount),
        channel(base.1, target.1, amount),
        channel(base.2, target.2, amount),
    )
}

pub fn relative_luminance(color: Rgb) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.0) + 0.7152 * channel(color.1) + 0.0722 * channel(color.2)
}

fn color_to_rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Reset => None,
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(_) => None,
    }
}
