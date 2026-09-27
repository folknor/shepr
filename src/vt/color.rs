use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub fn inferred_appearance(self) -> ColorScheme {
        let luminance = u32::from(self.r) * 299 + u32::from(self.g) * 587 + u32::from(self.b) * 114;
        if luminance >= 128_000 {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        }
    }
}

impl From<Rgb> for RgbColor {
    fn from(value: Rgb) -> Self {
        Self {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }
}

impl From<RgbColor> for Rgb {
    fn from(value: RgbColor) -> Self {
        Rgb {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }
}

/// The built-in 256-colour palette used until the host theme overrides it.
pub fn default_palette() -> [RgbColor; 256] {
    const NAMED: [(u8, u8, u8); 16] = [
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
    let mut palette = [RgbColor::default(); 256];
    for (slot, (r, g, b)) in palette.iter_mut().zip(NAMED) {
        *slot = RgbColor { r, g, b };
    }
    let cube = |value: usize| -> u8 {
        if value == 0 {
            0
        } else {
            // `value` is a 0..=5 cube coordinate, so `value * 40 + 55` maxes at 255.
            u8::try_from(value * 40 + 55).unwrap_or(u8::MAX)
        }
    };
    for (offset, slot) in palette[16..232].iter_mut().enumerate() {
        *slot = RgbColor {
            r: cube(offset / 36),
            g: cube((offset / 6) % 6),
            b: cube(offset % 6),
        };
    }
    for (offset, slot) in palette[232..256].iter_mut().enumerate() {
        // `offset` is 0..24 here, so `offset * 10 + 8` maxes at 238.
        let value = u8::try_from(offset * 10 + 8).unwrap_or(u8::MAX);
        *slot = RgbColor {
            r: value,
            g: value,
            b: value,
        };
    }
    palette
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
            0..=255 => Some(Self::Palette(u8::try_from(index).unwrap_or(u8::MAX))),
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
    pub(super) format: Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>,
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
        (*self.format)(color.into()).into_bytes()
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
    /// What the terminal reports for a colour query: the child's override
    /// first, then the host default, then (for the palette) the built-in
    /// table. `None` for a default colour nobody has set.
    pub(super) fn core_query_color(&self, target: ColorQueryTarget) -> Option<RgbColor> {
        let colors = self.term.colors();
        match target {
            ColorQueryTarget::Palette(index) => Some(self.effective_palette_color(index)),
            ColorQueryTarget::Foreground => colors[NamedColor::Foreground]
                .map(RgbColor::from)
                .or(self.host_foreground),
            ColorQueryTarget::Background => colors[NamedColor::Background]
                .map(RgbColor::from)
                .or(self.host_background),
            ColorQueryTarget::Cursor => colors[NamedColor::Cursor]
                .or(colors[NamedColor::Foreground])
                .map(RgbColor::from)
                .or(self.host_foreground),
        }
    }

    fn effective_palette_color(&self, index: u8) -> RgbColor {
        let index = usize::from(index);
        self.term.colors()[index]
            .map(RgbColor::from)
            .unwrap_or(self.default_palette[index])
    }

    pub(super) fn render_colors(&self) -> RenderColors {
        let colors = self.term.colors();
        let mut palette = self.default_palette;
        for (index, slot) in palette.iter_mut().enumerate() {
            if let Some(color) = colors[index] {
                *slot = color.into();
            }
        }
        RenderColors {
            background: colors[NamedColor::Background]
                .map(RgbColor::from)
                .or(self.host_background)
                .unwrap_or(DEFAULT_BACKGROUND),
            foreground: colors[NamedColor::Foreground]
                .map(RgbColor::from)
                .or(self.host_foreground)
                .unwrap_or(DEFAULT_FOREGROUND),
            palette,
        }
    }
}

impl Terminal {
    pub fn set_default_palette(&mut self, palette: &[RgbColor; 256]) {
        if self.default_palette != *palette {
            self.default_palette = *palette;
            self.bump_full_damage();
        }
    }

    pub fn default_palette(&self) -> [RgbColor; 256] {
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
        self.term.colors()[color.named()].map(RgbColor::from)
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
