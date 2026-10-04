use super::*;
use alacritty_terminal::term::color::Colors;
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

type Palette = [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT];

/// What the host supplies to the terminal: its palette and default
/// foreground/background, which sit under the child's OSC 4/10/11 overrides
/// (alacritty's `colors` slots) and over the built-in defaults, so host theme
/// changes never go through the child's parser, plus the cell pitch and
/// colour scheme it reports.
pub(super) struct HostDefaults {
    palette: Palette,
    foreground: Option<RgbColor>,
    background: Option<RgbColor>,
    cell: Option<shepr_core::geometry::CellPx>,
    color_scheme: Option<ColorScheme>,
}

impl HostDefaults {
    pub(super) fn new() -> Self {
        Self {
            palette: default_palette(),
            foreground: None,
            background: None,
            cell: None,
            color_scheme: None,
        }
    }

    pub(super) fn palette(&self) -> Palette {
        self.palette
    }

    /// Returns whether the palette changed.
    pub(super) fn set_palette(&mut self, palette: &Palette) -> bool {
        let changed = self.palette != *palette;
        self.palette = *palette;
        changed
    }

    /// Returns whether either default changed.
    pub(super) fn set_colors(
        &mut self,
        foreground: Option<RgbColor>,
        background: Option<RgbColor>,
    ) -> bool {
        let changed = (self.foreground, self.background) != (foreground, background);
        self.foreground = foreground;
        self.background = background;
        changed
    }

    pub(super) fn cell(&self) -> Option<shepr_core::geometry::CellPx> {
        self.cell
    }

    pub(super) fn set_cell(&mut self, cell: Option<shepr_core::geometry::CellPx>) {
        self.cell = cell;
    }

    pub(super) fn color_scheme(&self) -> Option<ColorScheme> {
        self.color_scheme
    }

    pub(super) fn replace_color_scheme(
        &mut self,
        color_scheme: Option<ColorScheme>,
    ) -> Option<ColorScheme> {
        mem::replace(&mut self.color_scheme, color_scheme)
    }

    /// The geometry of `term` with the host's cell pitch.
    pub(super) fn geometry<T>(&self, term: &Term<T>) -> shepr_core::geometry::PaneGeometry {
        crate::handler::geometry_for_terminal(term.columns(), term.screen_lines(), self.cell)
    }

    /// The query the child asked about `target`, answered from its own
    /// overrides (`colors`), then these defaults, then the built-in palette.
    pub(super) fn color_query(
        &self,
        colors: &Colors,
        target: ColorQueryTarget,
        reply_form: crate::seq::ReplyForm,
    ) -> ColorQuery {
        let core_color = match target {
            ColorQueryTarget::Palette(index) => {
                let index = usize::from(index);
                Some(colors[index].map_or(self.palette[index], rgb_from_vte))
            }
            ColorQueryTarget::Foreground => colors[NamedColor::Foreground]
                .map(rgb_from_vte)
                .or(self.foreground),
            ColorQueryTarget::Background => colors[NamedColor::Background]
                .map(rgb_from_vte)
                .or(self.background),
            ColorQueryTarget::Cursor => colors[NamedColor::Cursor]
                .or(colors[NamedColor::Foreground])
                .map(rgb_from_vte)
                .or(self.foreground),
        };
        let child_override = match target {
            ColorQueryTarget::Foreground => colors[NamedColor::Foreground].is_some(),
            ColorQueryTarget::Background => colors[NamedColor::Background].is_some(),
            ColorQueryTarget::Palette(_) | ColorQueryTarget::Cursor => false,
        };
        ColorQuery {
            target,
            core_color,
            child_override,
            reply_form,
        }
    }

    pub(super) fn render_colors(&self, colors: &Colors) -> RenderColors {
        let mut palette = self.palette;
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
                self.foreground.is_some(),
            ),
            background_source: source(
                colors[NamedColor::Background].is_some(),
                self.background.is_some(),
            ),
            child_palette: std::array::from_fn(|index| colors[index].map(rgb_from_vte)),
            background: colors[NamedColor::Background]
                .map(rgb_from_vte)
                .or(self.background)
                .unwrap_or(DEFAULT_BACKGROUND),
            foreground: colors[NamedColor::Foreground]
                .map(rgb_from_vte)
                .or(self.foreground)
                .unwrap_or(DEFAULT_FOREGROUND),
            palette,
        }
    }
}

impl Terminal {
    pub(super) fn render_colors(&self) -> RenderColors {
        self.host.render_colors(self.emu.term.colors())
    }

    pub fn set_default_palette(
        &mut self,
        palette: &[RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT],
    ) {
        if self.host.set_palette(palette) {
            self.damage.bump_full();
        }
    }

    pub fn default_palette(&self) -> [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT] {
        self.host.palette()
    }

    /// Sets the host's default foreground/background (`None`: the built-in
    /// default). The child's own OSC 10/11 overrides stay on top of them, and
    /// OSC 110/111 fall back to them.
    pub fn set_default_colors(
        &mut self,
        foreground: Option<RgbColor>,
        background: Option<RgbColor>,
    ) {
        if self.host.set_colors(foreground, background) {
            self.damage.bump_full();
        }
    }

    /// The default colour the child set with OSC 10/11, if it has one.
    pub fn default_color_override(&self, color: DefaultColor) -> Option<RgbColor> {
        self.emu.term.colors()[named_default_color(color)].map(rgb_from_vte)
    }

    /// Drops the child's OSC 10/11 overrides, as OSC 110/111 would, so the
    /// host defaults show again. Goes through `Term`'s handler directly,
    /// never through the child's parser.
    pub fn reset_default_color_overrides(&mut self) {
        let mut changed = false;
        for color in [DefaultColor::Foreground, DefaultColor::Background] {
            if self.default_color_override(color).is_some() {
                Handler::reset_color(&mut self.emu.term, named_default_color(color) as usize);
                changed = true;
            }
        }
        if changed {
            self.damage.bump_full();
        }
    }
}
