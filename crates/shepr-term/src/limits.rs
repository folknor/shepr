//! Width bounds, child-facing encoder buffer sizes and the tuning of the
//! per-host sidebar colours (`host_tint`).

/// Maximum width in terminal cells for one Unicode codepoint. The cap matches
/// the widest category in Unicode display width, keeping cell accounting within
/// that model.
pub(crate) const MAX_UNICODE_CODEPOINT_WIDTH: u8 = 2;
/// Initial allocation for a UTF-8 mouse report.
///
/// The initial capacity fits a complete supported report, including its escape
/// prefix and encoded coordinates, without growing the common buffer.
pub(crate) const UTF8_MOUSE_REPORT_INITIAL_CAPACITY: usize = 16;

/// Initial allocation for the common kitty key encoding before optional text.
///
/// The initial capacity avoids growth for ordinary key sequences while
/// allowing the associated-text path to expand when needed.
pub(crate) const KITTY_KEY_SEQUENCE_INITIAL_CAPACITY: usize = 32;

// Per-host sidebar colours. Lightness and chroma are Oklch values: lightness
// runs 0..=1 and chroma is the distance from grey.

/// Lightness a host's tint moves away from the terminal background, toward the
/// foreground. Enough to see the tint on any background, little enough that
/// the entry still reads as part of the sidebar.
pub(crate) const HOST_TINT_LIGHTNESS_STEP: f64 = 0.05;
/// Lightness a focused entry's tint moves away from the background: twice the
/// plain tint's step, so focus reads as a stronger shade of the same colour.
pub(crate) const HOST_FOCUSED_TINT_LIGHTNESS_STEP: f64 = 0.10;
/// Lowest lightness of a tint on a dark background. A step from a near-black
/// background would stay too dark to show any colour.
pub(crate) const HOST_DARK_TINT_MIN_LIGHTNESS: f64 = 0.20;
/// Chroma of a tint on a dark background, the same for every hue so no host
/// stands out.
pub(crate) const HOST_TINT_CHROMA_DARK: f64 = 0.04;
/// Chroma of a tint on a light background, where pale colours turn garish at
/// lower chroma than dark ones.
pub(crate) const HOST_TINT_CHROMA_LIGHT: f64 = 0.033;
/// Chroma of the main text: a hint of the host's hue on near-foreground text.
pub(crate) const HOST_TEXT_CHROMA: f64 = 0.015;
/// Chroma of the dim text.
pub(crate) const HOST_DIM_CHROMA: f64 = 0.03;
/// Where the dim text sits between the tint (0) and the main text (1).
pub(crate) const HOST_DIM_MIX: f64 = 0.6;
/// Main text lightness on a dark background when the terminal reports no
/// foreground.
pub(crate) const HOST_DEFAULT_TEXT_LIGHTNESS_DARK: f64 = 0.90;
/// Main text lightness on a light background when the terminal reports no
/// foreground.
pub(crate) const HOST_DEFAULT_TEXT_LIGHTNESS_LIGHT: f64 = 0.25;
/// Without a reported foreground, a background at least this light is a light
/// theme.
pub(crate) const HOST_LIGHT_BACKGROUND_MIN_LIGHTNESS: f64 = 0.6;
/// Accent lightness on a dark background, shared by every host.
pub(crate) const HOST_ACCENT_LIGHTNESS_DARK: f64 = 0.75;
/// Accent lightness on a light background, shared by every host.
pub(crate) const HOST_ACCENT_LIGHTNESS_LIGHT: f64 = 0.55;
/// Share of the smallest in-gamut chroma across the configured hues that the
/// shared accent chroma takes, so no hue sits on the gamut edge.
pub(crate) const HOST_ACCENT_CHROMA_HEADROOM: f64 = 0.9;
/// Upper bound on the shared accent chroma, so a set of hues that all reach far
/// (greens and yellows) still gives a calm accent.
pub(crate) const HOST_ACCENT_CHROMA_MAX: f64 = 0.14;
/// Lightness a colour moves per step while it is pushed away from its
/// backgrounds to reach its contrast.
pub(crate) const HOST_CONTRAST_LIGHTNESS_STEP: f64 = 0.01;
/// WCAG contrast the main text keeps against both tints (the body-text level).
pub(crate) const HOST_TEXT_MIN_CONTRAST: f32 = 4.5;
/// WCAG contrast the dim text keeps against both tints.
pub(crate) const HOST_DIM_MIN_CONTRAST: f32 = 3.0;
/// WCAG contrast the accent keeps against both tints (the non-text level).
pub(crate) const HOST_ACCENT_MIN_CONTRAST: f32 = 3.0;
/// The most, in degrees, a host terminal's own ANSI colour may sit from a hue
/// name's angle for shepr to take its hue. Further off, the slot was
/// repurposed by the theme and the named angle is used instead.
pub(crate) const HOST_SLOT_HUE_MAX_DEVIATION: f64 = 40.0;
/// The least chroma a host terminal's ANSI colour needs to give a hue; a
/// greyer slot has no hue worth taking.
pub(crate) const HOST_SLOT_MIN_CHROMA: f64 = 0.03;
/// Halvings of the chroma search that maps a colour into the sRGB gamut. Each
/// halves the error; 24 put it far below one 8-bit step.
pub(crate) const HOST_GAMUT_BISECTION_STEPS: u32 = 24;
