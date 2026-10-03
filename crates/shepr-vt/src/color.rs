use super::*;
use vte::ansi::Handler;

// limits-exempt: the xterm palette starts with its 16 named ANSI colors.
pub(super) const NAMED_COLOR_COUNT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub(super) fn from_vte(value: Rgb) -> Self {
        Self {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }

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
pub fn default_palette() -> [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT] {
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

impl ColorQueryTarget {
    pub(super) fn from_index(index: usize) -> Option<Self> {
        match index {
            // xterm palette indexes cover every value in one byte.
            index if index <= usize::from(u8::MAX) => {
                Some(Self::Palette(u8::try_from(index).unwrap_or(u8::MAX)))
            }
            index if index == NamedColor::Foreground as usize => Some(Self::Foreground),
            index if index == NamedColor::Background as usize => Some(Self::Background),
            index if index == NamedColor::Cursor as usize => Some(Self::Cursor),
            _ => None,
        }
    }
}

/// The default colours a child can override with OSC 10/11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultColor {
    Foreground,
    Background,
}

impl DefaultColor {
    fn named(self) -> NamedColor {
        match self {
            Self::Foreground => NamedColor::Foreground,
            Self::Background => NamedColor::Background,
        }
    }
}

/// An OSC colour query the child sent. The pane decides how to answer it;
/// `core_color` is what the terminal itself would report (child override,
/// then host default, then built-in palette; `None` for a foreground or
/// background nobody has set), captured at the query's position in the
/// stream.
pub struct ColorQuery {
    pub(super) target: ColorQueryTarget,
    pub(super) core_color: Option<RgbColor>,
    pub(super) child_override: bool,
    pub(super) reply_form: crate::seq::ReplyForm,
}

impl ColorQuery {
    pub fn target(&self) -> ColorQueryTarget {
        self.target
    }

    pub fn core_color(&self) -> Option<RgbColor> {
        self.core_color
    }

    /// Whether a default-colour query was answered from the child's own OSC
    /// 10/11 override at the moment it was asked (always false for palette
    /// and cursor queries).
    pub fn child_override(&self) -> bool {
        self.child_override
    }

    /// Encode a reply in the form the query asked for (same OSC number and
    /// terminator).
    pub fn encode(&self, color: RgbColor) -> Vec<u8> {
        self.reply(color, self.reply_form)
    }

    /// Reply with an explicit terminator, for pane policies choosing their own form.
    pub fn reply(&self, color: RgbColor, form: crate::seq::ReplyForm) -> Vec<u8> {
        crate::seq::ColorReply(self.target, color, form)
            .to_string()
            .into_bytes()
    }
}

impl fmt::Debug for ColorQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColorQuery")
            .field("target", &self.target)
            .field("core_color", &self.core_color)
            .field("child_override", &self.child_override)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    pub(super) fn render_colors(&self) -> RenderColors {
        let colors = self.term.colors();
        let mut palette = self.default_palette;
        for (index, slot) in palette.iter_mut().enumerate() {
            if let Some(color) = colors[index] {
                *slot = RgbColor::from_vte(color);
            }
        }
        let source = |child: bool, host: bool| {
            if child {
                ColorSource::Child
            } else if host {
                ColorSource::Host
            } else {
                ColorSource::Builtin
            }
        };
        RenderColors {
            foreground_source: source(
                colors[NamedColor::Foreground].is_some(),
                self.host_foreground.is_some(),
            ),
            background_source: source(
                colors[NamedColor::Background].is_some(),
                self.host_background.is_some(),
            ),
            child_palette: std::array::from_fn(|index| colors[index].map(RgbColor::from_vte)),
            background: colors[NamedColor::Background]
                .map(RgbColor::from_vte)
                .or(self.host_background)
                .unwrap_or(DEFAULT_BACKGROUND),
            foreground: colors[NamedColor::Foreground]
                .map(RgbColor::from_vte)
                .or(self.host_foreground)
                .unwrap_or(DEFAULT_FOREGROUND),
            palette,
        }
    }
}

impl Terminal {
    pub fn set_default_palette(
        &mut self,
        palette: &[RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT],
    ) {
        if self.default_palette != *palette {
            self.default_palette = *palette;
            self.bump_full_damage();
        }
    }

    pub fn default_palette(&self) -> [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT] {
        self.default_palette
    }

    /// Sets the host's default foreground/background (`None`: the built-in
    /// default). The child's own OSC 10/11 overrides stay on top of them, and
    /// OSC 110/111 fall back to them.
    pub fn set_default_colors(
        &mut self,
        foreground: Option<RgbColor>,
        background: Option<RgbColor>,
    ) {
        if (self.host_foreground, self.host_background) != (foreground, background) {
            self.host_foreground = foreground;
            self.host_background = background;
            self.bump_full_damage();
        }
    }

    /// The default colour the child set with OSC 10/11, if it has one.
    pub fn default_color_override(&self, color: DefaultColor) -> Option<RgbColor> {
        self.term.colors()[color.named()].map(RgbColor::from_vte)
    }

    /// Drops the child's OSC 10/11 overrides, as OSC 110/111 would, so the
    /// host defaults show again. Goes through `Term`'s handler directly,
    /// never through the child's parser.
    pub fn reset_default_color_overrides(&mut self) {
        let mut changed = false;
        for color in [DefaultColor::Foreground, DefaultColor::Background] {
            if self.default_color_override(color).is_some() {
                Handler::reset_color(&mut self.term, color.named() as usize);
                changed = true;
            }
        }
        if changed {
            self.bump_full_damage();
        }
    }
}
