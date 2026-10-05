//! The preview itself: every colour the client derives for one theme, drawn
//! on the surfaces the client draws it on, with its WCAG contrast.
//!
//! Every cell is painted with an explicit truecolour background, the theme's
//! own background included, so a theme given on the command line previews
//! the same in any terminal.

use shepr_term::host::TerminalTheme;
use shepr_term::host_tint::{HostHue, HostPillColors, HostPillPalette, UiPalette};
use shepr_term::{NAMED_COLOR_COUNT, RgbColor};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

/// What the preview shows: the theme, where it came from, the local server's
/// hue (the client's accent) and the hues derived together for the sidebar.
pub(crate) struct Preview<'a> {
    pub(crate) theme: &'a TerminalTheme,
    pub(crate) source: &'a str,
    pub(crate) accent: HostHue,
    pub(crate) hues: &'a [HostHue],
}

impl Preview<'_> {
    /// The preview as terminal text, or `None` when the theme has no
    /// background, the case where the client derives nothing and draws with
    /// the terminal's own ANSI colours instead.
    pub(crate) fn render(&self) -> Option<String> {
        let background = self.theme.background?;
        let ui = UiPalette::derive(self.theme, self.accent)?;
        let pills = HostPillPalette::derive(self.theme, self.hues)?;
        let mut out = String::new();
        self.theme_section(&mut out, background);
        ui_tokens_section(&mut out, &ui, background);
        surfaces_section(&mut out, &ui, background);
        let focused_border = pills
            .colors(self.accent)
            .map_or(ui.accent, |colors| colors.accent);
        borders_section(&mut out, &ui, background, focused_border, self.accent);
        hosts_section(&mut out, &pills, self.hues, background);
        Some(out)
    }

    fn theme_section(&self, out: &mut String, background: RgbColor) {
        heading(out, &format!("Theme ({})", self.source));
        let foreground = self
            .theme
            .foreground
            .map_or_else(|| "not reported".to_owned(), hex);
        let polarity = match self.theme.foreground {
            Some(foreground)
                if background.relative_luminance() < foreground.relative_luminance() =>
            {
                "dark"
            }
            Some(_) => "light",
            None => "unknown polarity without a foreground",
        };
        out.push_str(&format!(
            "  background {}  foreground {foreground}  {polarity}\n",
            hex(background)
        ));
        out.push_str(
            "  ANSI colours as reported (the hues take a slot's angle when it is near the name):\n",
        );
        for row in [0..8_u8, 8..16] {
            out.push_str("  ");
            for index in row {
                match self
                    .theme
                    .palette
                    .get(usize::from(index))
                    .copied()
                    .flatten()
                {
                    Some(color) => out.push_str(&format!(
                        "{}{} {index:>2} {RESET} ",
                        bg(color),
                        fg(readable_on(color))
                    )),
                    None => out.push_str(&format!(" {index:>2}-- ")),
                }
            }
            out.push('\n');
        }
        if self.theme.palette[..NAMED_COLOR_COUNT]
            .iter()
            .all(Option::is_none)
        {
            out.push_str("  (none reported: every hue takes its named angle)\n");
        }
    }
}

/// Every token of the UI palette: a swatch, its value, its contrast against
/// the background, and sample text in it on the background.
fn ui_tokens_section(out: &mut String, ui: &UiPalette, background: RgbColor) {
    heading(
        out,
        "UI palette: swatch, value, contrast against the background, text on the background",
    );
    for (name, color) in ui_tokens(ui) {
        out.push_str(&format!(
            "  {}      {RESET} {name:<14} {} {:>5.2}  {}{} The quick brown fox {RESET}\n",
            bg(color),
            hex(color),
            color.contrast_with(background),
            bg(background),
            fg(color),
        ));
    }
}

/// Each surface the client draws text on, with every text tone and hue on it,
/// then a table of their contrasts.
fn surfaces_section(out: &mut String, ui: &UiPalette, background: RgbColor) {
    heading(out, "Text and hues on each surface");
    let surfaces = surfaces(ui, background);
    let texts = text_tokens(ui);
    for (surface_name, surface) in &surfaces {
        out.push_str(&format!(
            "  {}{} {surface_name:<14}",
            bg(*surface),
            fg(ui.text)
        ));
        for (name, color) in &texts {
            out.push_str(&format!("{} {}", fg(*color), sample_word(name)));
        }
        out.push_str(&format!(" {RESET}\n"));
    }

    out.push_str(&format!("\n  {:<10}", "contrast"));
    for (surface_name, _) in &surfaces {
        out.push_str(&format!("{surface_name:>14}"));
    }
    out.push('\n');
    for (name, color) in &texts {
        out.push_str(&format!("  {name:<10}"));
        for (_, surface) in &surfaces {
            out.push_str(&format!("{:>14.2}", color.contrast_with(*surface)));
        }
        out.push('\n');
    }
}

/// An unfocused and a focused pane side by side, with the border and
/// scrollbar colours the client gives the chrome roles a server draws.
fn borders_section(
    out: &mut String,
    ui: &UiPalette,
    background: RgbColor,
    focused_border: RgbColor,
    accent: HostHue,
) {
    heading(
        out,
        &format!(
            "Pane borders: unfocused, and focused in the presented machine's accent ({accent})"
        ),
    );
    let inner_width = 26;
    let lines = [
        "$ cargo build",
        "   Compiling shepr",
        "    Finished dev",
        "$",
    ];
    let pane = |focused: bool, row: usize| -> String {
        let (border, track, thumb, thumb_glyph) = if focused {
            (focused_border, ui.overlay0, ui.overlay1, "▐")
        } else {
            (ui.overlay0, ui.surface_dim, ui.overlay0, "▕")
        };
        let title = if focused { " focused " } else { " unfocused " };
        let last = lines.len() + 1;
        let mut cell = bg(background);
        if row == 0 {
            let rule = "─".repeat(inner_width + 1 - title.chars().count());
            cell.push_str(&format!("{}┌─{title}{rule}┐", fg(border)));
        } else if row == last {
            cell.push_str(&format!("{}└{}┘", fg(border), "─".repeat(inner_width + 2)));
        } else {
            let text = lines[row - 1];
            let (glyph, color) = if row <= 2 {
                (thumb_glyph, thumb)
            } else {
                ("▕", track)
            };
            cell.push_str(&format!(
                "{}│{} {text:<inner_width$}{}{glyph}{}│",
                fg(border),
                fg(ui.text),
                fg(color),
                fg(border)
            ));
        }
        cell.push_str(RESET);
        cell
    };
    for row in 0..=lines.len() + 1 {
        out.push_str(&format!(
            "  {}{}  {RESET}{}\n",
            pane(false, row),
            bg(background),
            pane(true, row)
        ));
    }
}

/// Each host hue's sidebar entries, plain and focused, with its colours and
/// their contrasts against the tints.
fn hosts_section(
    out: &mut String,
    pills: &HostPillPalette,
    hues: &[HostHue],
    background: RgbColor,
) {
    heading(
        out,
        "Sidebar host colours: entry, focused entry, values and contrasts",
    );
    let entry_width = 22;
    for hue in hues {
        let Some(colors) = pills.colors(*hue) else {
            continue;
        };
        let entry = |tint: RgbColor, first: bool| -> String {
            let text = if first {
                format!(
                    "{}{BOLD} 1 {RESET}{}{}{:<width$}",
                    fg(colors.accent),
                    bg(tint),
                    fg(colors.text),
                    format!("{hue} workspace"),
                    width = entry_width - 3
                )
            } else {
                format!("{}{:<entry_width$}", fg(colors.dim), "   main +2 -1  idle")
            };
            format!("{}{text}{RESET}", bg(tint))
        };
        for first in [true, false] {
            out.push_str(&format!(
                "  {}{}  {RESET}{}{}  {RESET}",
                entry(colors.tint, first),
                bg(background),
                entry(colors.tint_focused, first),
                bg(background),
            ));
            if first {
                out.push_str(&pill_values(&colors));
            } else {
                out.push_str(&pill_contrasts(&colors));
            }
            out.push('\n');
        }
    }
}

fn pill_values(colors: &HostPillColors) -> String {
    format!(
        "tint {} focused {} text {} dim {} accent {}",
        hex(colors.tint),
        hex(colors.tint_focused),
        hex(colors.text),
        hex(colors.dim),
        hex(colors.accent)
    )
}

/// The lower contrast of each colour against the two tints.
fn pill_contrasts(colors: &HostPillColors) -> String {
    let worst = |color: RgbColor| {
        color
            .contrast_with(colors.tint)
            .min(color.contrast_with(colors.tint_focused))
    };
    format!(
        "contrast on the tints: text {:.2}  dim {:.2}  accent {:.2}",
        worst(colors.text),
        worst(colors.dim),
        worst(colors.accent)
    )
}

fn ui_tokens(ui: &UiPalette) -> [(&'static str, RgbColor); 15] {
    [
        ("panel_bg", ui.panel_bg),
        ("active_row_bg", ui.active_row_bg),
        ("selection_bg", ui.selection_bg),
        ("surface0", ui.surface0),
        ("surface1", ui.surface1),
        ("surface_dim", ui.surface_dim),
        ("overlay0", ui.overlay0),
        ("overlay1", ui.overlay1),
        ("subtext0", ui.subtext0),
        ("text", ui.text),
        ("accent", ui.accent),
        ("mauve", ui.mauve),
        ("green", ui.green),
        ("yellow", ui.yellow),
        ("red", ui.red),
    ]
}

/// The backgrounds text is drawn on.
fn surfaces(ui: &UiPalette, background: RgbColor) -> [(&'static str, RgbColor); 6] {
    [
        ("background", background),
        ("panel_bg", ui.panel_bg),
        ("active_row_bg", ui.active_row_bg),
        ("selection_bg", ui.selection_bg),
        ("surface0", ui.surface0),
        ("surface1", ui.surface1),
    ]
}

/// The colours drawn as text or glyphs.
fn text_tokens(ui: &UiPalette) -> [(&'static str, RgbColor); 9] {
    [
        ("text", ui.text),
        ("subtext0", ui.subtext0),
        ("overlay1", ui.overlay1),
        ("overlay0", ui.overlay0),
        ("accent", ui.accent),
        ("mauve", ui.mauve),
        ("green", ui.green),
        ("yellow", ui.yellow),
        ("red", ui.red),
    ]
}

/// The sample a text token is shown with: the agent state for the state hues,
/// its own name otherwise.
fn sample_word(name: &str) -> String {
    match name {
        "green" => "● idle".to_owned(),
        "yellow" => "● working".to_owned(),
        "red" => "● blocked".to_owned(),
        "mauve" => "main".to_owned(),
        other => other.to_owned(),
    }
}

fn heading(out: &mut String, title: &str) {
    out.push_str(&format!("\n{BOLD}{title}{RESET}\n"));
}

/// Black or white, whichever reads better on `color`, for swatch labels.
fn readable_on(color: RgbColor) -> RgbColor {
    let black = RgbColor { r: 0, g: 0, b: 0 };
    let white = RgbColor {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    };
    if color.contrast_with(black) >= color.contrast_with(white) {
        black
    } else {
        white
    }
}

fn fg(color: RgbColor) -> String {
    format!("\x1b[38;2;{};{};{}m", color.r, color.g, color.b)
}

fn bg(color: RgbColor) -> String {
    format!("\x1b[48;2;{};{};{}m", color.r, color.g, color.b)
}

fn hex(color: RgbColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(background: Option<RgbColor>) -> TerminalTheme {
        TerminalTheme {
            background,
            foreground: Some(RgbColor {
                r: 0xcd,
                g: 0xd6,
                b: 0xf4,
            }),
            ..TerminalTheme::default()
        }
    }

    #[test]
    fn a_theme_without_a_background_has_no_preview() {
        let theme = theme(None);
        let preview = Preview {
            theme: &theme,
            source: "test",
            accent: HostHue::Blue,
            hues: &HostHue::ALL,
        };
        assert_eq!(preview.render(), None);
    }

    #[test]
    fn the_preview_shows_every_token_and_every_hue() {
        let theme = theme(Some(RgbColor {
            r: 0x1e,
            g: 0x1e,
            b: 0x2e,
        }));
        let preview = Preview {
            theme: &theme,
            source: "test",
            accent: HostHue::Green,
            hues: &HostHue::ALL,
        };
        let text = preview.render().expect("background known");
        let ui = UiPalette::derive(&theme, HostHue::Green).expect("background known");
        for (name, color) in ui_tokens(&ui) {
            assert!(text.contains(name), "{name}");
            assert!(text.contains(&hex(color)), "{name}");
        }
        for hue in HostHue::ALL {
            assert!(text.contains(&format!("{hue} workspace")), "{hue}");
        }
    }
}
