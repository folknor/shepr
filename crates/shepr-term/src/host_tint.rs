//! The client's colours, derived from the host terminal's own colours: the
//! per-host sidebar colours ([`HostPillPalette`]) and the palette of everything
//! else it draws ([`UiPalette`]). There is no theme to pick; both follow the
//! terminal.
//!
//! A configured machine (or the local server) names a hue (`palette = "green"`
//! in `client.toml`), and the sidebar draws that host's entries in colours
//! derived here from the background and foreground the host terminal reported
//! (OSC 10/11) and, where it fits, the terminal's own ANSI colour of that name
//! (OSC 4). Every host gets a tint background, a stronger tint for its focused
//! entry, main and dim text for those tints, and an accent.
//!
//! The work is done in Oklch, the polar form of Oklab: lightness, chroma
//! (distance from grey) and hue angle, where equal steps look roughly equal.
//! Holding lightness and chroma fixed and changing only the hue gives colours
//! that are equally loud, which is what lets several hosts sit side by side
//! with none of them shouting. The steps are:
//!
//! - Polarity: the theme is dark when the background is darker than the
//!   foreground (or, with no foreground, darker than
//!   [`HOST_LIGHT_BACKGROUND_MIN_LIGHTNESS`]). Every derived colour moves away
//!   from the background, toward the foreground.
//! - Tints sit a fixed lightness step from the background with a small chroma
//!   shared by every hue; the focused tint takes twice the step.
//! - Main text keeps the foreground's lightness, dim text sits
//!   [`HOST_DIM_MIX`] of the way from the tint to it, and the accent has one
//!   lightness and one chroma for every configured host: the chroma is the
//!   smallest the configured hues can all show at that lightness, with some
//!   headroom.
//! - A colour outside the sRGB gamut keeps its lightness and hue and loses
//!   chroma until it fits (bisection), instead of being clipped per channel,
//!   which would shift its hue and lightness.
//! - Each text colour and the accent are then pushed away from the background
//!   until they reach their WCAG contrast against both tints, or the other way
//!   when lightness runs out first. On a mid-tone background, where no text
//!   reaches its contrast against tints moved toward the foreground, the tints
//!   move the other way instead, which puts both on one side of the middle
//!   where black or white text always does.
//!
//! The math is a few lines of Oklab conversion (Ottosson's published
//! matrices) over sRGB, so no colour crate is linked for it.

use serde::Deserialize;

use crate::RgbColor;
use crate::host::TerminalTheme;
use crate::limits::{
    HOST_ACCENT_CHROMA_HEADROOM, HOST_ACCENT_CHROMA_MAX, HOST_ACCENT_LIGHTNESS_DARK,
    HOST_ACCENT_LIGHTNESS_LIGHT, HOST_ACCENT_MIN_CONTRAST, HOST_CONTRAST_LIGHTNESS_STEP,
    HOST_DARK_TINT_MIN_LIGHTNESS, HOST_DEFAULT_TEXT_LIGHTNESS_DARK,
    HOST_DEFAULT_TEXT_LIGHTNESS_LIGHT, HOST_DIM_CHROMA, HOST_DIM_MIN_CONTRAST, HOST_DIM_MIX,
    HOST_FOCUSED_TINT_LIGHTNESS_STEP, HOST_GAMUT_BISECTION_STEPS,
    HOST_LIGHT_BACKGROUND_MIN_LIGHTNESS, HOST_SLOT_HUE_MAX_DEVIATION, HOST_SLOT_MIN_CHROMA,
    HOST_TEXT_CHROMA, HOST_TEXT_MIN_CONTRAST, HOST_TINT_CHROMA_DARK, HOST_TINT_CHROMA_LIGHT,
    HOST_TINT_LIGHTNESS_STEP, UI_ACTIVE_ROW_CONTRAST, UI_NEUTRAL_MAX_CHROMA, UI_OVERLAY0_CONTRAST,
    UI_OVERLAY1_CONTRAST, UI_PANEL_CONTRAST, UI_SELECTION_CONTRAST, UI_SUBTEXT_CONTRAST,
    UI_SURFACE_DIM_CONTRAST, UI_SURFACE0_CONTRAST, UI_SURFACE1_CONTRAST,
};

/// A hue name a host can be given in `client.toml` (`palette = "green"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostHue {
    Red,
    Orange,
    Yellow,
    Green,
    Cyan,
    Blue,
    Purple,
    Magenta,
}

impl HostHue {
    /// Every hue name, in the order the config documents them.
    pub const ALL: [Self; 8] = [
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Cyan,
        Self::Blue,
        Self::Purple,
        Self::Magenta,
    ];

    /// The name the config spells this hue with.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Cyan => "cyan",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Magenta => "magenta",
        }
    }

    /// The hue's Oklch angle in degrees. Red, yellow, green, cyan, blue and
    /// magenta sit near the angles of the sRGB primaries and secondaries;
    /// orange and purple sit between their neighbours.
    fn named_angle(self) -> f64 {
        match self {
            Self::Red => 25.0,
            Self::Orange => 60.0,
            Self::Yellow => 100.0,
            Self::Green => 145.0,
            Self::Cyan => 195.0,
            Self::Blue => 260.0,
            Self::Purple => 300.0,
            Self::Magenta => 335.0,
        }
    }

    /// The normal (not bright) ANSI slot the host terminal names after this
    /// hue. The bright slots are left alone: themes such as Solarized reuse
    /// them as greys.
    fn matching_slot(self) -> Option<u8> {
        match self {
            Self::Red => Some(1),
            Self::Green => Some(2),
            Self::Yellow => Some(3),
            Self::Blue => Some(4),
            Self::Magenta => Some(5),
            Self::Cyan => Some(6),
            Self::Orange | Self::Purple => None,
        }
    }

    /// The ANSI palette index that stands for this hue when no truecolour can
    /// be derived: its own slot, or the nearest one for orange and purple.
    /// Being the terminal's own colour, it fits the terminal's theme.
    pub const fn ansi_index(self) -> u8 {
        match self {
            Self::Red => 1,
            Self::Green => 2,
            Self::Yellow | Self::Orange => 3,
            Self::Blue => 4,
            Self::Magenta | Self::Purple => 5,
            Self::Cyan => 6,
        }
    }

    /// The angle used for this host: the terminal's own colour of this name
    /// when it reported one with a clear hue near the named angle, else the
    /// named angle.
    fn angle_in(self, theme: &TerminalTheme) -> f64 {
        let named = self.named_angle();
        self.matching_slot()
            .and_then(|slot| theme.palette.get(usize::from(slot)).copied().flatten())
            .map(Oklch::from_rgb)
            .filter(|slot| {
                slot.c >= HOST_SLOT_MIN_CHROMA
                    && hue_distance(slot.h, named) <= HOST_SLOT_HUE_MAX_DEVIATION
            })
            .map_or(named, |slot| slot.h)
    }
}

impl std::fmt::Display for HostHue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A colour in Oklch: lightness 0..=1, chroma from 0 (grey) up, and hue in
/// degrees 0..360.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

impl Oklch {
    pub const fn new(l: f64, c: f64, h: f64) -> Self {
        Self { l, c, h }
    }

    pub fn from_rgb(color: RgbColor) -> Self {
        let [l, a, b] = linear_rgb_to_oklab([
            srgb_to_linear(color.r),
            srgb_to_linear(color.g),
            srgb_to_linear(color.b),
        ]);
        Self {
            l,
            c: a.hypot(b),
            h: b.atan2(a).to_degrees().rem_euclid(360.0),
        }
    }

    /// The nearest 8-bit sRGB colour of the same lightness and hue: a colour
    /// outside the gamut keeps its lightness and hue and loses chroma until it
    /// fits.
    pub fn to_rgb(self) -> RgbColor {
        let l = self.l.clamp(0.0, 1.0);
        let requested = self.c.max(0.0);
        let chroma = if in_gamut(Self::new(l, requested, self.h).linear_rgb()) {
            requested
        } else {
            max_in_gamut_chroma(l, self.h, requested)
        };
        let [r, g, b] = Self::new(l, chroma, self.h).linear_rgb();
        RgbColor {
            r: encode_channel(r),
            g: encode_channel(g),
            b: encode_channel(b),
        }
    }

    fn linear_rgb(self) -> [f64; 3] {
        let (sin, cos) = self.h.to_radians().sin_cos();
        oklab_to_linear_rgb([self.l, self.c * cos, self.c * sin])
    }
}

/// The colours one host's sidebar entries are drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPillColors {
    /// Background of the host's entries.
    pub tint: RgbColor,
    /// Background of the host's focused entry: the same hue, further from the
    /// terminal background.
    pub tint_focused: RgbColor,
    /// First-line text.
    pub text: RgbColor,
    /// Second-line text.
    pub dim: RgbColor,
    /// The host's identifying colour, as loud as every other host's.
    pub accent: RgbColor,
}

/// The colours of every configured hue, derived together from one terminal
/// theme so the hosts share one accent lightness and chroma.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostPillPalette {
    entries: Vec<(HostHue, HostPillColors)>,
}

impl HostPillPalette {
    /// Derives the colours of `hues` from `theme`. `None` when the terminal
    /// reported no background: a tint chosen blind could clash with it, so the
    /// caller falls back to the terminal's own ANSI colours.
    pub fn derive(theme: &TerminalTheme, hues: &[HostHue]) -> Option<Self> {
        let polarity = Polarity::of(theme)?;
        let away = polarity.away();
        let angles = hues
            .iter()
            .map(|hue| (*hue, hue.angle_in(theme)))
            .collect::<Vec<_>>();
        let shared = SharedTargets {
            background_l: polarity.background.l,
            dark: polarity.dark,
            text_l: polarity.text.l,
            accent_l: polarity.accent_l(),
            accent_c: polarity.accent_chroma(&angles),
        };

        // The tints normally move toward the foreground. On a mid-tone
        // background (grey #a0a0a0 under white text) that can leave them
        // where no text colour reaches its contrast against both of them;
        // tints moved the other way put both on one side of the middle, where
        // black or white text always does.
        let toward_foreground = shared.entries(&angles, away);
        if toward_foreground.1 {
            return Some(Self {
                entries: toward_foreground.0,
            });
        }
        let toward_background = shared.entries(&angles, -away);
        Some(Self {
            entries: if toward_background.1 {
                toward_background.0
            } else {
                toward_foreground.0
            },
        })
    }

    /// The colours derived for `hue`, if it was one of the derived hues.
    pub fn colors(&self, hue: HostHue) -> Option<HostPillColors> {
        self.entries
            .iter()
            .find(|(entry, _)| *entry == hue)
            .map(|(_, colors)| *colors)
    }
}

/// Which side of the terminal's colours is the background, read from what it
/// reported: the ground both the host colours and the UI palette are derived on.
struct Polarity {
    background: Oklch,
    /// The reported foreground, or a default text lightness of the theme's
    /// polarity in grey.
    text: Oklch,
    /// The theme is dark: the background is darker than the foreground (or,
    /// with no foreground, darker than [`HOST_LIGHT_BACKGROUND_MIN_LIGHTNESS`]).
    dark: bool,
}

impl Polarity {
    /// `None` when the terminal reported no background.
    fn of(theme: &TerminalTheme) -> Option<Self> {
        let background = Oklch::from_rgb(theme.background?);
        let foreground = theme.foreground.map(Oklch::from_rgb);
        let dark = foreground.map_or(
            background.l < HOST_LIGHT_BACKGROUND_MIN_LIGHTNESS,
            |foreground| background.l < foreground.l,
        );
        let text = foreground.unwrap_or(Oklch::new(
            if dark {
                HOST_DEFAULT_TEXT_LIGHTNESS_DARK
            } else {
                HOST_DEFAULT_TEXT_LIGHTNESS_LIGHT
            },
            0.0,
            0.0,
        ));
        Some(Self {
            background,
            text,
            dark,
        })
    }

    /// +1 when colours move lighter away from the background, -1 darker.
    fn away(&self) -> f64 {
        if self.dark { 1.0 } else { -1.0 }
    }

    fn accent_l(&self) -> f64 {
        if self.dark {
            HOST_ACCENT_LIGHTNESS_DARK
        } else {
            HOST_ACCENT_LIGHTNESS_LIGHT
        }
    }

    /// The one chroma every accent of `angles` takes: the smallest the hues
    /// can all show at the accent lightness, with some headroom.
    fn accent_chroma(&self, angles: &[(HostHue, f64)]) -> f64 {
        // Beyond any chroma sRGB can show, so the search starts above every
        // hue's gamut edge.
        let chroma_ceiling = 0.4;
        let accent_l = self.accent_l();
        (angles
            .iter()
            .map(|(_, angle)| max_in_gamut_chroma(accent_l, *angle, chroma_ceiling))
            .fold(f64::INFINITY, f64::min)
            * HOST_ACCENT_CHROMA_HEADROOM)
            .min(HOST_ACCENT_CHROMA_MAX)
    }
}

/// The colours of everything the client draws, derived from the host
/// terminal's own colours: neutrals for surfaces and text, and the accent and
/// state hues. A host's configured hue is its accent here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPalette {
    /// Floating panels, overlays and menus.
    pub panel_bg: RgbColor,
    /// The active workspace and focused agent rows.
    pub active_row_bg: RgbColor,
    /// The navigate-mode cursor row.
    pub selection_bg: RgbColor,
    /// Selected and focused surfaces.
    pub surface0: RgbColor,
    /// Hovered and active surfaces.
    pub surface1: RgbColor,
    /// Separators and unfocused scrollbar tracks.
    pub surface_dim: RgbColor,
    /// Muted text.
    pub overlay0: RgbColor,
    /// Secondary text, a step brighter than `overlay0`.
    pub overlay1: RgbColor,
    /// Subdued text.
    pub subtext0: RgbColor,
    /// Main text.
    pub text: RgbColor,
    /// Highlights and the focused pane's border.
    pub accent: RgbColor,
    /// Branch names and special labels.
    pub mauve: RgbColor,
    /// Idle.
    pub green: RgbColor,
    /// Working.
    pub yellow: RgbColor,
    /// Blocked and errors.
    pub red: RgbColor,
}

impl UiPalette {
    /// The palette for `theme` with `accent` as its accent hue. `None` when the
    /// terminal reported no background: neutrals chosen blind could vanish
    /// into it, so the caller falls back to the terminal's own ANSI colours.
    ///
    /// Each neutral starts at the background's lightness, keeps a little of
    /// its tint, and moves toward the foreground until it reaches its WCAG
    /// contrast against the background, or the other way where lightness runs
    /// out (a near-white background under dark text). Text and the hues then
    /// keep their contrast against every surface they are drawn on. The hues
    /// share one lightness and chroma, as host accents do.
    ///
    /// On a mid-tone background (grey #5a5a5a under white text) surfaces moved
    /// toward the foreground can leave the text no room to reach its contrast
    /// against all of them; surfaces moved the other way put everything on
    /// one side of the middle, where black or white text always does.
    pub fn derive(theme: &TerminalTheme, accent: HostHue) -> Option<Self> {
        let polarity = Polarity::of(theme)?;
        let toward_foreground = Self::derive_toward(theme, accent, &polarity, polarity.away())?;
        if toward_foreground.1 {
            return Some(toward_foreground.0);
        }
        let toward_background = Self::derive_toward(theme, accent, &polarity, -polarity.away())?;
        Some(if toward_background.1 {
            toward_background.0
        } else {
            toward_foreground.0
        })
    }

    /// The palette with its surfaces moved in `surface_direction` (+1 lighter,
    /// -1 darker) from the background, and whether every text colour and hue
    /// reached its contrast against them.
    fn derive_toward(
        theme: &TerminalTheme,
        accent: HostHue,
        polarity: &Polarity,
        surface_direction: f64,
    ) -> Option<(Self, bool)> {
        let away = polarity.away();
        let background = theme.background?;
        let base = Oklch::new(
            polarity.background.l,
            polarity.background.c.min(UI_NEUTRAL_MAX_CHROMA),
            polarity.background.h,
        );
        let neutral =
            |contrast: f32| with_contrast(base, &[background], contrast, surface_direction).0;
        let panel_bg = neutral(UI_PANEL_CONTRAST);
        let active_row_bg = neutral(UI_ACTIVE_ROW_CONTRAST);
        let selection_bg = neutral(UI_SELECTION_CONTRAST);
        let surface0 = neutral(UI_SURFACE0_CONTRAST);
        let surface1 = neutral(UI_SURFACE1_CONTRAST);
        // Copy-search matches use surface1 as a background, so text and hues
        // must meet their contrast there too.
        let surfaces = [
            background,
            panel_bg,
            active_row_bg,
            selection_bg,
            surface0,
            surface1,
        ];
        let mut readable = true;
        let mut on_surfaces = |color: Oklch, contrast: f32| {
            let (rgb, met) = with_contrast(color, &surfaces, contrast, away);
            readable &= met;
            rgb
        };
        let text = on_surfaces(polarity.text, HOST_TEXT_MIN_CONTRAST);
        let overlay0 = on_surfaces(base, UI_OVERLAY0_CONTRAST);
        let overlay1 = on_surfaces(base, UI_OVERLAY1_CONTRAST);
        let subtext0 = on_surfaces(base, UI_SUBTEXT_CONTRAST);

        let angles = [
            accent,
            HostHue::Purple,
            HostHue::Green,
            HostHue::Yellow,
            HostHue::Red,
        ]
        .map(|hue| (hue, hue.angle_in(theme)));
        let accent_l = polarity.accent_l();
        let accent_c = polarity.accent_chroma(&angles);
        let [accent, mauve, green, yellow, red] = angles.map(|(_, angle)| {
            on_surfaces(
                Oklch::new(accent_l, accent_c, angle),
                HOST_ACCENT_MIN_CONTRAST,
            )
        });

        Some((
            Self {
                panel_bg,
                active_row_bg,
                selection_bg,
                surface0,
                surface1,
                surface_dim: neutral(UI_SURFACE_DIM_CONTRAST),
                overlay0,
                overlay1,
                subtext0,
                text,
                accent,
                mauve,
                green,
                yellow,
                red,
            },
            readable,
        ))
    }
}

/// What every host's colours share, decided once per derivation.
struct SharedTargets {
    background_l: f64,
    dark: bool,
    text_l: f64,
    accent_l: f64,
    accent_c: f64,
}

impl SharedTargets {
    /// Every host's colours with the tints moved in `tint_direction` (+1
    /// lighter, -1 darker) from the background, and whether every colour of
    /// every host reached its contrast.
    fn entries(
        &self,
        angles: &[(HostHue, f64)],
        tint_direction: f64,
    ) -> (Vec<(HostHue, HostPillColors)>, bool) {
        let mut tint_l =
            (self.background_l + tint_direction * HOST_TINT_LIGHTNESS_STEP).clamp(0.0, 1.0);
        let mut focused_l =
            (self.background_l + tint_direction * HOST_FOCUSED_TINT_LIGHTNESS_STEP).clamp(0.0, 1.0);
        if tint_direction > 0.0 && tint_l < HOST_DARK_TINT_MIN_LIGHTNESS {
            focused_l = (focused_l + HOST_DARK_TINT_MIN_LIGHTNESS - tint_l).min(1.0);
            tint_l = HOST_DARK_TINT_MIN_LIGHTNESS;
        }
        let tint_c = if self.dark {
            HOST_TINT_CHROMA_DARK
        } else {
            HOST_TINT_CHROMA_LIGHT
        };
        let away = if self.dark { 1.0 } else { -1.0 };
        let dim_l = tint_l + HOST_DIM_MIX * (self.text_l - tint_l);
        let mut readable = true;
        let entries = angles
            .iter()
            .map(|(hue, angle)| {
                let angle = *angle;
                let tint = Oklch::new(tint_l, tint_c, angle).to_rgb();
                let tint_focused = Oklch::new(focused_l, tint_c, angle).to_rgb();
                let tints = [tint, tint_focused];
                let mut pick = |color: Oklch, minimum: f32| {
                    let (rgb, met) = with_contrast(color, &tints, minimum, away);
                    readable &= met;
                    rgb
                };
                let colors = HostPillColors {
                    tint,
                    tint_focused,
                    text: pick(
                        Oklch::new(self.text_l, HOST_TEXT_CHROMA, angle),
                        HOST_TEXT_MIN_CONTRAST,
                    ),
                    dim: pick(
                        Oklch::new(dim_l, HOST_DIM_CHROMA, angle),
                        HOST_DIM_MIN_CONTRAST,
                    ),
                    accent: pick(
                        Oklch::new(self.accent_l, self.accent_c, angle),
                        HOST_ACCENT_MIN_CONTRAST,
                    ),
                };
                (*hue, colors)
            })
            .collect();
        (entries, readable)
    }
}

/// `color` as an 8-bit colour with `minimum` contrast against every one of
/// `backgrounds`, and whether it got there. The colour first moves away from
/// the terminal background (`away` is +1 on a dark theme, -1 on a light one);
/// when lightness runs out first, it moves the other way instead. When neither
/// way reaches the contrast, the result is the one with the better worst-case
/// contrast.
fn with_contrast(
    color: Oklch,
    backgrounds: &[RgbColor],
    minimum: f32,
    away: f64,
) -> (RgbColor, bool) {
    let forward = push_for_contrast(color, backgrounds, minimum, away);
    if worst_contrast(forward, backgrounds) >= minimum {
        return (forward, true);
    }
    let backward = push_for_contrast(color, backgrounds, minimum, -away);
    if worst_contrast(backward, backgrounds) >= minimum {
        return (backward, true);
    }
    let best = if worst_contrast(forward, backgrounds) >= worst_contrast(backward, backgrounds) {
        forward
    } else {
        backward
    };
    (best, false)
}

/// `color` moved in `direction` (+1 lighter, -1 darker) until it has
/// `minimum` contrast against every one of `backgrounds`, or until it reaches
/// the end of the lightness range.
fn push_for_contrast(
    mut color: Oklch,
    backgrounds: &[RgbColor],
    minimum: f32,
    direction: f64,
) -> RgbColor {
    loop {
        let rgb = color.to_rgb();
        if worst_contrast(rgb, backgrounds) >= minimum {
            return rgb;
        }
        let next = (color.l + direction * HOST_CONTRAST_LIGHTNESS_STEP).clamp(0.0, 1.0);
        let moved = (next - color.l).abs();
        if moved.is_nan() || moved < f64::EPSILON {
            return rgb;
        }
        color.l = next;
    }
}

/// The lowest WCAG contrast `color` has against any of `backgrounds`.
fn worst_contrast(color: RgbColor, backgrounds: &[RgbColor]) -> f32 {
    backgrounds
        .iter()
        .map(|background| color.contrast_with(*background))
        .fold(f32::INFINITY, f32::min)
}

/// The largest chroma up to `ceiling` that keeps lightness `l` and hue `h`
/// inside the sRGB gamut.
fn max_in_gamut_chroma(l: f64, h: f64, ceiling: f64) -> f64 {
    let mut inside = 0.0;
    let mut outside = ceiling;
    for _ in 0..HOST_GAMUT_BISECTION_STEPS {
        let middle = (inside + outside) / 2.0;
        if in_gamut(Oklch::new(l, middle, h).linear_rgb()) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    inside
}

/// Whether linear sRGB channels are displayable. The tolerance absorbs the
/// rounding of the conversion matrices, which would otherwise put white
/// itself just outside.
fn in_gamut(channels: [f64; 3]) -> bool {
    channels
        .iter()
        .all(|channel| (-1e-6..=1.0 + 1e-6).contains(channel))
}

/// The angle between two hues in degrees, 0..=180.
fn hue_distance(first: f64, second: f64) -> f64 {
    let difference = (first - second).rem_euclid(360.0);
    difference.min(360.0 - difference)
}

/// The sRGB transfer function, from an 8-bit channel to linear light.
fn srgb_to_linear(channel: u8) -> f64 {
    let encoded = f64::from(channel) / 255.0;
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light to an 8-bit sRGB channel, clamped into range.
fn encode_channel(linear: f64) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is clamped to 0..=255 and rounded first, and a NaN converts to 0"
    )]
    let channel = (encoded.clamp(0.0, 1.0) * 255.0).round() as u8;
    channel
}

/// Linear sRGB to Oklab, with Ottosson's published matrices.
fn linear_rgb_to_oklab([r, g, b]: [f64; 3]) -> [f64; 3] {
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    [
        0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
    ]
}

/// Oklab to linear sRGB, the inverse of [`linear_rgb_to_oklab`]. Channels
/// outside 0..=1 mean the colour is outside the gamut.
fn oklab_to_linear_rgb([l, a, b]: [f64; 3]) -> [f64; 3] {
    let long = (l + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
    let medium = (l - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
    let short = (l - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
    [
        4.076_741_662_1 * long - 3.307_711_591_3 * medium + 0.230_969_929_2 * short,
        -1.268_438_004_6 * long + 2.609_757_401_1 * medium - 0.341_319_396_5 * short,
        -0.004_196_086_3 * long - 0.703_418_614_7 * medium + 1.707_614_701_0 * short,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: u32) -> RgbColor {
        let [_, r, g, b] = hex.to_be_bytes();
        RgbColor { r, g, b }
    }

    fn theme(background: Option<u32>, foreground: Option<u32>) -> TerminalTheme {
        TerminalTheme {
            background: background.map(rgb),
            foreground: foreground.map(rgb),
            ..TerminalTheme::default()
        }
    }

    fn close(actual: f64, expected: f64, tolerance: f64) -> bool {
        (actual - expected).abs() <= tolerance
    }

    #[test]
    fn converts_reference_colours_to_known_oklch_values() {
        let white = Oklch::from_rgb(rgb(0xffffff));
        assert!(close(white.l, 1.0, 1e-3) && white.c < 1e-3, "{white:?}");
        let black = Oklch::from_rgb(rgb(0x000000));
        assert!(close(black.l, 0.0, 1e-6) && black.c < 1e-6, "{black:?}");
        // Published Oklch values of the sRGB primaries.
        let red = Oklch::from_rgb(rgb(0xff0000));
        assert!(close(red.l, 0.628, 2e-3), "{red:?}");
        assert!(close(red.c, 0.2577, 2e-3), "{red:?}");
        assert!(close(red.h, 29.23, 0.2), "{red:?}");
        let green = Oklch::from_rgb(rgb(0x00ff00));
        assert!(close(green.l, 0.8664, 2e-3), "{green:?}");
        assert!(close(green.h, 142.5, 0.2), "{green:?}");
        let blue = Oklch::from_rgb(rgb(0x0000ff));
        assert!(close(blue.l, 0.452, 2e-3), "{blue:?}");
        assert!(close(blue.c, 0.3132, 2e-3), "{blue:?}");
        assert!(close(blue.h, 264.05, 0.2), "{blue:?}");
    }

    #[test]
    fn in_gamut_colours_survive_a_round_trip() {
        for hex in [
            0x000000, 0xffffff, 0x1e1e2e, 0xcdd6f4, 0xfafafa, 0xff0000, 0x00ff00, 0x0000ff,
            0x808080, 0x123456, 0xfedcba, 0x50fa7b,
        ] {
            assert_eq!(Oklch::from_rgb(rgb(hex)).to_rgb(), rgb(hex), "{hex:06x}");
        }
    }

    #[test]
    fn gamut_mapping_keeps_lightness_and_hue_and_drops_chroma() {
        for hue in [25.0, 100.0, 145.0, 260.0, 335.0] {
            let requested = Oklch::new(0.75, 0.35, hue);
            let mapped = Oklch::from_rgb(requested.to_rgb());
            assert!(close(mapped.l, 0.75, 0.01), "{hue}: {mapped:?}");
            assert!(hue_distance(mapped.h, hue) < 2.0, "{hue}: {mapped:?}");
            assert!(mapped.c < 0.35, "{hue}: {mapped:?}");
            assert!(mapped.c > 0.05, "{hue}: {mapped:?}");
        }
    }

    #[test]
    fn hue_distance_wraps_around_the_circle() {
        assert!(close(hue_distance(350.0, 10.0), 20.0, 1e-9));
        assert!(close(hue_distance(10.0, 350.0), 20.0, 1e-9));
        assert!(close(hue_distance(0.0, 180.0), 180.0, 1e-9));
    }

    #[test]
    fn nothing_is_derived_without_a_reported_background() {
        assert_eq!(
            HostPillPalette::derive(&theme(None, Some(0xcdd6f4)), &HostHue::ALL),
            None
        );
    }

    fn assert_contrast(palette: &HostPillPalette, hues: &[HostHue], context: &str) {
        for hue in hues {
            let colors = palette.colors(*hue).expect("every hue is derived");
            for tint in [colors.tint, colors.tint_focused] {
                assert!(
                    colors.text.contrast_with(tint) >= HOST_TEXT_MIN_CONTRAST,
                    "{context} {hue}: {colors:?}"
                );
                assert!(
                    colors.dim.contrast_with(tint) >= HOST_DIM_MIN_CONTRAST,
                    "{context} {hue}: {colors:?}"
                );
                assert!(
                    colors.accent.contrast_with(tint) >= HOST_ACCENT_MIN_CONTRAST,
                    "{context} {hue}: {colors:?}"
                );
            }
        }
    }

    fn assert_readable(palette: &HostPillPalette, hues: &[HostHue]) {
        assert_contrast(palette, hues, "");
        for hue in hues {
            let colors = palette.colors(*hue).expect("every hue is derived");
            assert_ne!(colors.tint, colors.tint_focused, "{hue}");
        }
    }

    /// A mid-grey background under white text once gave tints so close to
    /// white that no text reached its contrast; the tints now move the other
    /// way and the text turns dark.
    #[test]
    fn mid_grey_backgrounds_stay_readable() {
        for (background, foreground) in [
            (0xa0a0a0, Some(0xffffff)),
            (0x808080, Some(0xffffff)),
            (0x808080, Some(0x000000)),
            (0x767676, None),
            (0x6e6e6e, Some(0x000000)),
            (0x909090, Some(0xeeeeee)),
        ] {
            let theme = theme(Some(background), foreground);
            let palette = HostPillPalette::derive(&theme, &HostHue::ALL).expect("background known");
            assert_readable(&palette, &HostHue::ALL);
        }
    }

    /// Every grey level, under white, black or no reported foreground, gives
    /// colours that meet their contrast.
    #[test]
    fn every_grey_background_meets_the_contrast_targets() {
        for level in (0..=0xff_u32).step_by(0x0f) {
            let background = (level << 16) | (level << 8) | level;
            for foreground in [Some(0xffffff), Some(0x000000), None] {
                let theme = theme(Some(background), foreground);
                let palette =
                    HostPillPalette::derive(&theme, &HostHue::ALL).expect("background known");
                assert_contrast(
                    &palette,
                    &HostHue::ALL,
                    &format!("background {background:06x}, foreground {foreground:?}"),
                );
            }
        }
    }

    #[test]
    fn contrast_search_turns_around_when_lightness_runs_out() {
        let tints = [rgb(0xa1b7a0), rgb(0xb0c7b0)];
        // White cannot reach 4.5 against these; moving lighter runs out at
        // white, so the search moves darker and succeeds.
        let (text, met) = with_contrast(Oklch::from_rgb(rgb(0xffffff)), &tints, 4.5, 1.0);
        assert!(met, "{text:?}");
        assert!(worst_contrast(text, &tints) >= 4.5, "{text:?}");
        assert!(Oklch::from_rgb(text).l < Oklch::from_rgb(tints[0]).l);
    }

    #[test]
    fn dark_theme_tints_lift_off_the_background_and_stay_readable() {
        let theme = theme(Some(0x1e1e2e), Some(0xcdd6f4));
        let palette = HostPillPalette::derive(&theme, &HostHue::ALL).expect("background known");
        assert_readable(&palette, &HostHue::ALL);
        let background = Oklch::from_rgb(rgb(0x1e1e2e));
        for hue in HostHue::ALL {
            let colors = palette.colors(hue).expect("derived");
            let tint = Oklch::from_rgb(colors.tint);
            let focused = Oklch::from_rgb(colors.tint_focused);
            assert!(tint.l > background.l, "{hue}: {tint:?}");
            assert!(focused.l > tint.l, "{hue}: {focused:?}");
        }
    }

    #[test]
    fn light_theme_tints_darken_from_the_background_and_stay_readable() {
        let theme = theme(Some(0xfafafa), Some(0x383a42));
        let palette = HostPillPalette::derive(&theme, &HostHue::ALL).expect("background known");
        assert_readable(&palette, &HostHue::ALL);
        let background = Oklch::from_rgb(rgb(0xfafafa));
        for hue in HostHue::ALL {
            let colors = palette.colors(hue).expect("derived");
            let tint = Oklch::from_rgb(colors.tint);
            assert!(tint.l < background.l, "{hue}: {tint:?}");
            assert!(
                Oklch::from_rgb(colors.tint_focused).l < tint.l,
                "{hue}: {colors:?}"
            );
        }
    }

    #[test]
    fn a_missing_foreground_still_gives_readable_text() {
        for background in [0x000000, 0x282c34, 0xffffff, 0xfdf6e3] {
            let theme = theme(Some(background), None);
            let palette = HostPillPalette::derive(&theme, &HostHue::ALL).expect("background known");
            assert_readable(&palette, &HostHue::ALL);
        }
    }

    #[test]
    fn a_black_background_still_gets_a_visible_tint() {
        let theme = theme(Some(0x000000), Some(0xffffff));
        let palette = HostPillPalette::derive(&theme, &[HostHue::Blue]).expect("background known");
        let tint = Oklch::from_rgb(palette.colors(HostHue::Blue).expect("derived").tint);
        assert!(tint.l >= HOST_DARK_TINT_MIN_LIGHTNESS - 0.01, "{tint:?}");
    }

    #[test]
    fn every_host_accent_shares_one_lightness_and_chroma() {
        let hues = [
            HostHue::Red,
            HostHue::Yellow,
            HostHue::Green,
            HostHue::Blue,
            HostHue::Magenta,
        ];
        let theme = theme(Some(0x1e1e2e), Some(0xcdd6f4));
        let palette = HostPillPalette::derive(&theme, &hues).expect("background known");
        let accents = hues
            .iter()
            .map(|hue| Oklch::from_rgb(palette.colors(*hue).expect("derived").accent))
            .collect::<Vec<_>>();
        for accent in &accents {
            assert!(
                close(accent.l, HOST_ACCENT_LIGHTNESS_DARK, 0.01),
                "{accents:?}"
            );
            assert!(close(accent.c, accents[0].c, 0.01), "{accents:?}");
        }
        for (accent, hue) in accents.iter().zip(hues) {
            assert!(
                hue_distance(accent.h, hue.named_angle()) < 3.0,
                "{hue}: {accent:?}"
            );
        }
    }

    #[test]
    fn the_terminal_own_colour_gives_the_hue_only_when_it_is_near_the_name() {
        let slot_hue = 125.0;
        let mut theme = theme(Some(0x1e1e2e), Some(0xcdd6f4));
        theme.palette[2] = Some(Oklch::new(0.7, 0.15, slot_hue).to_rgb());
        let accent_hue = |theme: &TerminalTheme| {
            let palette =
                HostPillPalette::derive(theme, &[HostHue::Green]).expect("background known");
            Oklch::from_rgb(palette.colors(HostHue::Green).expect("derived").accent).h
        };
        assert!(hue_distance(accent_hue(&theme), slot_hue) < 3.0);

        // A slot far from green (a theme that reuses it) or a grey one is ignored.
        theme.palette[2] = Some(Oklch::new(0.6, 0.15, 260.0).to_rgb());
        assert!(hue_distance(accent_hue(&theme), HostHue::Green.named_angle()) < 3.0);
        theme.palette[2] = Some(rgb(0x808080));
        assert!(hue_distance(accent_hue(&theme), HostHue::Green.named_angle()) < 3.0);
    }

    #[test]
    fn no_ui_palette_is_derived_without_a_reported_background() {
        assert_eq!(
            UiPalette::derive(&theme(None, Some(0xcdd6f4)), HostHue::Blue),
            None
        );
    }

    /// Every grey background, under white, black or no reported foreground,
    /// gives surfaces off the background, readable text and hues on every
    /// surface, and distinct steps.
    #[test]
    fn every_grey_background_gives_a_readable_ui_palette() {
        for level in (0..=0xff_u32).step_by(0x0f) {
            let background = (level << 16) | (level << 8) | level;
            for foreground in [Some(0xffffff), Some(0x000000), None] {
                let context = format!("background {background:06x}, foreground {foreground:?}");
                let ui = UiPalette::derive(&theme(Some(background), foreground), HostHue::Blue)
                    .expect("background known");
                let bg = rgb(background);
                for (surface, minimum) in [
                    (ui.panel_bg, UI_PANEL_CONTRAST),
                    (ui.active_row_bg, UI_ACTIVE_ROW_CONTRAST),
                    (ui.selection_bg, UI_SELECTION_CONTRAST),
                    (ui.surface0, UI_SURFACE0_CONTRAST),
                    (ui.surface1, UI_SURFACE1_CONTRAST),
                    (ui.surface_dim, UI_SURFACE_DIM_CONTRAST),
                ] {
                    assert!(surface.contrast_with(bg) >= minimum, "{context}: {ui:?}");
                }
                assert_ne!(ui.active_row_bg, ui.selection_bg, "{context}");
                let surfaces = [
                    bg,
                    ui.panel_bg,
                    ui.active_row_bg,
                    ui.selection_bg,
                    ui.surface0,
                    ui.surface1,
                ];
                for (color, minimum) in [
                    (ui.text, HOST_TEXT_MIN_CONTRAST),
                    (ui.subtext0, UI_SUBTEXT_CONTRAST),
                    (ui.overlay0, UI_OVERLAY0_CONTRAST),
                    (ui.overlay1, UI_OVERLAY1_CONTRAST),
                    (ui.accent, HOST_ACCENT_MIN_CONTRAST),
                    (ui.green, HOST_ACCENT_MIN_CONTRAST),
                    (ui.yellow, HOST_ACCENT_MIN_CONTRAST),
                    (ui.red, HOST_ACCENT_MIN_CONTRAST),
                    (ui.mauve, HOST_ACCENT_MIN_CONTRAST),
                ] {
                    assert!(
                        worst_contrast(color, &surfaces) >= minimum,
                        "{context}: {color:?} in {ui:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_ui_palette_follows_the_theme_polarity_and_hue() {
        let dark = UiPalette::derive(&theme(Some(0x1e1e2e), Some(0xcdd6f4)), HostHue::Green)
            .expect("background known");
        let background = Oklch::from_rgb(rgb(0x1e1e2e));
        assert!(Oklch::from_rgb(dark.panel_bg).l > background.l, "{dark:?}");
        assert!(
            Oklch::from_rgb(dark.surface1).l > Oklch::from_rgb(dark.surface0).l,
            "{dark:?}"
        );
        // The foreground the terminal reported is the text when it reads.
        assert_eq!(dark.text, rgb(0xcdd6f4));
        assert!(hue_distance(Oklch::from_rgb(dark.accent).h, HostHue::Green.named_angle()) < 3.0);

        let light = UiPalette::derive(&theme(Some(0xfafafa), Some(0x383a42)), HostHue::Blue)
            .expect("background known");
        let background = Oklch::from_rgb(rgb(0xfafafa));
        assert!(
            Oklch::from_rgb(light.panel_bg).l < background.l,
            "{light:?}"
        );
        assert!(
            Oklch::from_rgb(light.overlay0).l < Oklch::from_rgb(light.surface1).l,
            "{light:?}"
        );
    }

    #[test]
    fn every_hue_has_a_name_and_an_ansi_fallback() {
        for hue in HostHue::ALL {
            assert!(!hue.name().is_empty());
            assert!((1..=6).contains(&hue.ansi_index()), "{hue}");
            if let Some(slot) = hue.matching_slot() {
                assert_eq!(slot, hue.ansi_index(), "{hue}");
            }
        }
    }
}
