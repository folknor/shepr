use ratatui::{
    layout::Rect,
    style::{Color, Style},
};
use shepr_config::theme::Palette;
use shepr_vt::{ColorScheme, RgbColor};

fn panel_background(p: &Palette) -> Color {
    if p.panel_bg == Color::Reset {
        p.surface_dim
    } else {
        p.panel_bg
    }
}

/// Reports the style of every selected cell to `patch_cell(x, y, style)`, in screen
/// coordinates, and does not know the surface those cells live in: `inner` can reach past
/// it (the client composes pane surfaces produced for another layout), so the sink must
/// ignore positions it does not have. The style is meant to be applied like
/// `Cell::set_style`.
pub fn render_selection_highlight<P: PartialEq>(
    selection: Option<&shepr_vt::selection::Selection<P>>,
    pane_id: &P,
    inner: Rect,
    scroll_metrics: Option<crate::scroll::ScrollMetrics>,
    p: &Palette,
    host_theme: crate::host_term::theme::TerminalTheme,
    patch_cell: &mut impl FnMut(u16, u16, Style),
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
    for screen_y in inner.top()..inner.bottom() {
        let y = screen_y - inner.y;
        let row = shepr_vt::ViewportRow(y);
        let absolute_row = scroll_metrics.absolute_row_at_viewport(row);
        for screen_x in inner.left()..inner.right() {
            let x = screen_x - inner.x;
            if selection.contains(shepr_vt::Point::new(absolute_row, x)) {
                patch_cell(screen_x, screen_y, style);
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
    Style::reset()
        .fg(selection_fg_for_bg(bg, p, &host_theme))
        .bg(bg)
}

pub fn automatic_selection_bg(
    p: &Palette,
    host_theme: crate::host_term::theme::TerminalTheme,
) -> Color {
    let fallback = panel_background(p);
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

    // This asks the same dark/light question as the child-facing scheme query:
    // move a dark surface toward white and a light one toward black. The text
    // foreground below is picked by its actual contrast ratio instead.
    let target = match rgb_color(background).appearance() {
        ColorScheme::Dark => (255, 255, 255),
        ColorScheme::Light => (0, 0, 0),
    };
    let selected = mix_rgb(background, target, 0.28);
    Color::Rgb(selected.0, selected.1, selected.2)
}

fn selection_fg_for_bg(
    bg: Color,
    p: &Palette,
    host_theme: &crate::host_term::theme::TerminalTheme,
) -> Color {
    if let Color::Rgb(r, g, b) = bg {
        return contrast_foreground(rgb_color((r, g, b)), true, host_theme);
    }

    color_to_rgb(bg, host_theme).map_or_else(
        || panel_background(p),
        |background| contrast_foreground(background, false, host_theme),
    )
}

fn contrast_foreground(
    background: RgbColor,
    rgb_output: bool,
    host_theme: &crate::host_term::theme::TerminalTheme,
) -> Color {
    const RGB_BLACK: RgbColor = RgbColor { r: 0, g: 0, b: 0 };
    const RGB_WHITE: RgbColor = RgbColor {
        r: 255,
        g: 255,
        b: 255,
    };
    let (black_rgb, white_rgb, black, white) = if rgb_output {
        (
            RGB_BLACK,
            RGB_WHITE,
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
        )
    } else {
        (
            host_theme.palette_color(0),
            host_theme.palette_color(15),
            Color::Black,
            Color::White,
        )
    };

    if background.contrast_with(black_rgb) > background.contrast_with(white_rgb) {
        black
    } else {
        white
    }
}

fn mix_rgb(base: Rgb, target: Rgb, amount: f32) -> Rgb {
    fn channel(base: u8, target: u8, amount: f32) -> u8 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "amount is a mix fraction in [0, 1] and base/target are u8 channel \
                      values, so the interpolated result stays within u8 range"
        )]
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
    rgb_color(color).relative_luminance()
}

fn color_to_rgb(
    color: Color,
    host_theme: &crate::host_term::theme::TerminalTheme,
) -> Option<RgbColor> {
    let index = match color {
        Color::Reset => return None,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(index) => index,
        Color::Rgb(r, g, b) => return Some(rgb_color((r, g, b))),
    };
    Some(host_theme.palette_color(index))
}

fn rgb_color((r, g, b): Rgb) -> RgbColor {
    RgbColor { r, g, b }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier, Style};
    use shepr_config::theme::Palette;
    use shepr_vt::selection::Selection;

    use super::{
        automatic_selection_bg, automatic_selection_style, relative_luminance,
        render_selection_highlight,
    };

    fn zero_origin_metrics(viewport_rows: usize) -> Option<crate::ScrollMetrics> {
        Some(crate::ScrollMetrics::new(
            0,
            0,
            viewport_rows,
            shepr_vt::AbsRow(0),
        ))
    }

    fn sink(buffer: &mut Buffer) -> impl FnMut(u16, u16, Style) + '_ {
        |x, y, style| {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_style(style);
            }
        }
    }

    #[test]
    fn selection_highlight_uses_one_uniform_style() {
        let palette = Palette::catppuccin();
        let host_theme = crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 12,
                g: 14,
                b: 16,
            }),
            ..Default::default()
        };
        let expected_style = automatic_selection_style(&palette, host_theme);
        let pane_id = 1_u8;
        let selection = Some(Selection::range(
            pane_id,
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
        ));
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 1));
        buffer[(0, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(10, 220, 120))
                .bg(Color::Black),
        );
        buffer[(1, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(220, 180, 40))
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        buffer[(2, 0)].set_style(Style::default().fg(Color::Blue).bg(Color::Reset));

        render_selection_highlight(
            selection.as_ref(),
            &pane_id,
            ratatui::layout::Rect::new(0, 0, 4, 1),
            zero_origin_metrics(1),
            &palette,
            host_theme,
            &mut sink(&mut buffer),
        );

        let first = buffer[(0, 0)].style();
        let second = buffer[(1, 0)].style();
        let third = buffer[(2, 0)].style();
        // A cell reports its modifiers, not the reset the style carried, so
        // compare what the highlight sets.
        for cell in [first, second, third] {
            assert_eq!(cell.fg, expected_style.fg);
            assert_eq!(cell.bg, expected_style.bg);
            assert_eq!(cell.add_modifier, expected_style.add_modifier);
        }
        assert!(!second.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn selection_highlight_clips_pane_rect_larger_than_buffer() {
        let palette = Palette::catppuccin();
        let host_theme = crate::host_term::theme::TerminalTheme::default();
        let expected = automatic_selection_style(&palette, host_theme);
        let pane_id = 1_u8;
        let selection = Some(Selection::range(
            pane_id,
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
            shepr_vt::Point::new(shepr_vt::AbsRow(2), 3),
        ));
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 2));

        render_selection_highlight(
            selection.as_ref(),
            &pane_id,
            ratatui::layout::Rect::new(1, 1, 4, 3),
            zero_origin_metrics(3),
            &palette,
            host_theme,
            &mut sink(&mut buffer),
        );

        for x in 1..4 {
            assert_eq!(buffer[(x, 1)].style().bg, expected.bg, "column {x}");
        }
        assert_ne!(buffer[(0, 1)].style().bg, expected.bg);
        for x in 0..4 {
            assert_ne!(buffer[(x, 0)].style().bg, expected.bg, "row 0 column {x}");
        }

        render_selection_highlight(
            selection.as_ref(),
            &pane_id,
            ratatui::layout::Rect::new(10, 10, 4, 3),
            zero_origin_metrics(3),
            &palette,
            host_theme,
            &mut sink(&mut buffer),
        );

        let mut unmapped = Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 2));
        render_selection_highlight(
            selection.as_ref(),
            &pane_id,
            ratatui::layout::Rect::new(0, 0, 4, 2),
            None,
            &palette,
            host_theme,
            &mut sink(&mut unmapped),
        );
        assert_eq!(
            unmapped,
            Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 2))
        );
    }

    #[test]
    fn automatic_selection_background_uses_host_background() {
        let bg = automatic_selection_bg(
            &Palette::terminal(),
            crate::host_term::theme::TerminalTheme {
                foreground: Some(crate::host_term::theme::RgbColor {
                    r: 230,
                    g: 230,
                    b: 230,
                }),
                background: Some(crate::host_term::theme::RgbColor {
                    r: 12,
                    g: 14,
                    b: 16,
                }),
                ..Default::default()
            },
        );

        let Color::Rgb(r, g, b) = bg else {
            panic!("selection background should resolve to rgb");
        };
        assert!(relative_luminance((r, g, b)) > relative_luminance((12, 14, 16)));
    }

    #[test]
    fn automatic_selection_rgb_style_is_readable_with_or_without_host_background() {
        for (background, selected_bg, selected_fg) in [
            ((239, 241, 245), (172, 174, 176), (0, 0, 0)),
            ((26, 27, 38), (90, 91, 99), (255, 255, 255)),
            ((45, 53, 59), (104, 110, 114), (255, 255, 255)),
        ] {
            let mut palette = Palette::catppuccin();
            let (r, g, b) = background;
            palette.panel_bg = Color::Rgb(r, g, b);
            let expected = Style::reset()
                .bg(Color::Rgb(selected_bg.0, selected_bg.1, selected_bg.2))
                .fg(Color::Rgb(selected_fg.0, selected_fg.1, selected_fg.2));

            assert_eq!(
                automatic_selection_style(&palette, Default::default()),
                expected
            );
            assert_eq!(
                automatic_selection_style(
                    &Palette::terminal(),
                    crate::host_term::theme::TerminalTheme {
                        background: Some(crate::host_term::theme::RgbColor { r, g, b }),
                        ..Default::default()
                    },
                ),
                expected
            );
        }
    }

    #[test]
    fn automatic_selection_preserves_symbolic_palette_fallbacks() {
        let mut palette = Palette::terminal();
        assert_eq!(
            automatic_selection_style(&palette, Default::default()),
            Style::reset().fg(Color::White).bg(Color::DarkGray)
        );
        for fallback in [Color::Blue, Color::White, Color::Indexed(42), Color::Reset] {
            palette.surface_dim = fallback;
            assert_eq!(
                automatic_selection_bg(&palette, Default::default()),
                fallback
            );
        }
    }
}
