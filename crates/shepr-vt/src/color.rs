use super::*;
use vte::ansi::Handler;

/// Converts a vte colour to the shared RGB value.
pub(super) fn rgb_from_vte(value: Rgb) -> RgbColor {
    RgbColor {
        r: value.r,
        g: value.g,
        b: value.b,
    }
}

/// The OSC 4/10/11/12 target of a vte colour index.
pub(super) fn color_query_target(index: usize) -> Option<ColorQueryTarget> {
    match index {
        // xterm palette indexes cover every value in one byte.
        index if index <= usize::from(u8::MAX) => Some(ColorQueryTarget::Palette(
            u8::try_from(index).unwrap_or(u8::MAX),
        )),
        index if index == NamedColor::Foreground as usize => Some(ColorQueryTarget::Foreground),
        index if index == NamedColor::Background as usize => Some(ColorQueryTarget::Background),
        index if index == NamedColor::Cursor as usize => Some(ColorQueryTarget::Cursor),
        _ => None,
    }
}

/// The vte slot holding a default colour the child can override.
fn named_default_color(color: DefaultColor) -> NamedColor {
    match color {
        DefaultColor::Foreground => NamedColor::Foreground,
        DefaultColor::Background => NamedColor::Background,
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
                *slot = rgb_from_vte(color);
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
            child_palette: std::array::from_fn(|index| colors[index].map(rgb_from_vte)),
            background: colors[NamedColor::Background]
                .map(rgb_from_vte)
                .or(self.host_background)
                .unwrap_or(DEFAULT_BACKGROUND),
            foreground: colors[NamedColor::Foreground]
                .map(rgb_from_vte)
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
        self.term.colors()[named_default_color(color)].map(rgb_from_vte)
    }

    /// Drops the child's OSC 10/11 overrides, as OSC 110/111 would, so the
    /// host defaults show again. Goes through `Term`'s handler directly,
    /// never through the child's parser.
    pub fn reset_default_color_overrides(&mut self) {
        let mut changed = false;
        for color in [DefaultColor::Foreground, DefaultColor::Background] {
            if self.default_color_override(color).is_some() {
                Handler::reset_color(&mut self.term, named_default_color(color) as usize);
                changed = true;
            }
        }
        if changed {
            self.bump_full_damage();
        }
    }
}
