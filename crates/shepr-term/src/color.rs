use shepr_core::limits::PALETTE_COLOR_COUNT;

use crate::seq;

// limits-exempt: the xterm palette starts with its 16 named ANSI colors.
pub const NAMED_COLOR_COUNT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// Relative luminance in the range 0..=1, with sRGB channels linearized first.
    pub fn relative_luminance(self) -> f32 {
        fn channel(value: u8) -> f32 {
            let value = f32::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// Classifies the color for a child's light/dark color-scheme query.
    ///
    /// The boundary is half of the Rec. 601 weighted sum of the gamma-encoded
    /// channels, the rule children such as Neovim apply to the background
    /// they read through OSC 11, so a child asking for the scheme and one
    /// reading the background agree. Consumers that need readable
    /// foregrounds use [`Self::contrast_with`] instead of treating the scheme
    /// as a contrast test.
    pub fn appearance(self) -> ColorScheme {
        let weighted = u32::from(self.r) * 299 + u32::from(self.g) * 587 + u32::from(self.b) * 114;
        // The weights sum to 1000, so this is the 8-bit midpoint 128 scaled.
        if weighted >= 128_000 {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        }
    }

    /// Returns the WCAG contrast ratio between this color and `other`.
    pub fn contrast_with(self, other: Self) -> f32 {
        let self_luminance = self.relative_luminance();
        let other_luminance = other.relative_luminance();
        let lighter = self_luminance.max(other_luminance);
        let darker = self_luminance.min(other_luminance);
        (lighter + 0.05) / (darker + 0.05)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

impl ColorScheme {
    pub const fn report(self) -> &'static [u8] {
        match self {
            Self::Dark => seq::COLOR_SCHEME_DARK,
            Self::Light => seq::COLOR_SCHEME_LIGHT,
        }
    }
}

const NAMED: [(u8, u8, u8); NAMED_COLOR_COUNT] = [
    (0x1d, 0x1f, 0x21),
    (0xcc, 0x66, 0x66),
    (0xb5, 0xbd, 0x68),
    (0xf0, 0xc6, 0x74),
    (0x81, 0xa2, 0xbe),
    (0xb2, 0x94, 0xbb),
    (0x8a, 0xbe, 0xb7),
    (0xc5, 0xc8, 0xc6),
    (0x66, 0x66, 0x66),
    (0xd5, 0x4e, 0x53),
    (0xb9, 0xca, 0x4a),
    (0xe7, 0xc5, 0x47),
    (0x7a, 0xa6, 0xda),
    (0xc3, 0x97, 0xd8),
    (0x70, 0xc0, 0xb1),
    (0xea, 0xea, 0xea),
];

/// The built-in indexed color at `index`, before any host palette overrides.
pub fn default_palette_color(index: u8) -> RgbColor {
    let index = usize::from(index);
    if index < NAMED_COLOR_COUNT {
        let (r, g, b) = NAMED[index];
        return RgbColor { r, g, b };
    }

    // xterm's 6-by-6-by-6 color cube occupies indexes 16 through 231.
    if index < 232 {
        let offset = index - NAMED_COLOR_COUNT;
        let cube = |value: usize| -> u8 {
            if value == 0 {
                0
            } else {
                // `value` is a 0..=5 cube coordinate, so this is at most 255.
                u8::try_from(value * 40 + 55).unwrap_or(u8::MAX)
            }
        };
        return RgbColor {
            r: cube(offset / 36),
            g: cube((offset / 6) % 6),
            b: cube(offset % 6),
        };
    }

    // The remaining xterm indexes are grayscale values from 8 through 238.
    let value = u8::try_from((index - 232) * 10 + 8).unwrap_or(u8::MAX);
    RgbColor {
        r: value,
        g: value,
        b: value,
    }
}

/// The built-in indexed palette used until the host theme overrides it.
pub fn default_palette() -> [RgbColor; PALETTE_COLOR_COUNT] {
    std::array::from_fn(|index| default_palette_color(u8::try_from(index).unwrap_or(u8::MAX)))
}

/// Target of an OSC 4/10/11/12 colour query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorQueryTarget {
    Palette(u8),
    Foreground,
    Background,
    Cursor,
}

/// The default colours a child can override with OSC 10/11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultColor {
    Foreground,
    Background,
}
