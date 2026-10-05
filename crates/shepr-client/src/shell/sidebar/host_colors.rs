//! Per-host colours of the expanded sidebar's workspace and agent entries.
//!
//! Every machine names a hue in `client.toml` (its `palette`); the local
//! server takes the hue of this host's own machine entry, or the default. The
//! colours are derived in `shepr_term::host_tint` from the colours the host
//! terminal reported, and rederived whenever it reports new ones.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use shepr_term::host_tint::{HostHue, HostPillColors, HostPillPalette};

use crate::endpoint::ClientEndpointId;
use crate::shell::palette::Palette;
use crate::shell::state::ClientShellState;

/// The hue of each endpoint, fixed for the life of the client like the
/// machines themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::shell) struct HostHues {
    local: HostHue,
    machines: HashMap<shepr_config::MachineLabel, HostHue>,
}

impl HostHues {
    pub(in crate::shell) fn from_validated_config(
        config: &shepr_config::ValidatedClientConfig,
    ) -> Self {
        Self {
            local: config.local_hue(),
            machines: config
                .machines()
                .iter()
                .map(|machine| (machine.label.clone(), machine.palette))
                .collect(),
        }
    }

    /// The local server's hue, the accent of everything the client draws.
    pub(in crate::shell) fn local(&self) -> HostHue {
        self.local
    }

    /// `endpoint`'s hue; `None` only for a machine the config does not name.
    pub(in crate::shell) fn hue_for(&self, endpoint: &ClientEndpointId) -> Option<HostHue> {
        match endpoint {
            ClientEndpointId::Local => Some(self.local),
            ClientEndpointId::Ssh(label) => self.machines.get(label).copied(),
        }
    }

    /// Every hue in use once, in a fixed order, for deriving them together.
    fn configured(&self) -> Vec<HostHue> {
        HostHue::ALL
            .into_iter()
            .filter(|hue| self.local == *hue || self.machines.values().any(|other| other == hue))
            .collect()
    }
}

impl ClientShellState {
    /// Rederives the palette and the per-host colours from the host
    /// terminal's colours as last reported. Returns whether they changed, so
    /// the caller repaints.
    pub(in crate::shell) fn refresh_host_colors(&mut self) -> bool {
        let palette = Palette::derive(&self.host_theme, self.config.host_hues.local());
        let pills = HostPillPalette::derive(&self.host_theme, &self.config.host_hues.configured());
        let changed = pills != self.host_pills || palette != self.palette;
        self.host_pills = pills;
        self.palette = palette;
        changed
    }

    /// The colour that identifies `endpoint`: its sidebar accent, which the
    /// focused pane's border takes while `endpoint` is presented. `None` for a
    /// machine the config does not name.
    pub(in crate::shell) fn host_accent(&self, endpoint: &ClientEndpointId) -> Option<Color> {
        PillLook::for_endpoint(&self.config.host_hues, self.host_pills.as_ref(), endpoint)
            .map(PillLook::accent)
    }
}

/// How the entries of an endpoint with a configured hue are drawn.
#[expect(
    variant_size_differences,
    reason = "the tinted variant is 15 bytes against 1; the value is Copy and lives on the stack per entry drawn, so boxing it would cost more than the size gap"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell::sidebar) enum PillLook {
    /// Truecolour derived from the terminal's reported background.
    Tinted(HostPillColors),
    /// The terminal reported no background: no tint, the terminal's own ANSI
    /// colour as the accent, and the second line drawn dim.
    Indexed { accent: u8 },
}

impl PillLook {
    /// The look of `endpoint`'s entries, or `None`, the palette's plain look,
    /// for a machine the config does not name.
    pub(in crate::shell::sidebar) fn for_endpoint(
        hues: &HostHues,
        pills: Option<&HostPillPalette>,
        endpoint: &ClientEndpointId,
    ) -> Option<Self> {
        let hue = hues.hue_for(endpoint)?;
        Some(pills.and_then(|pills| pills.colors(hue)).map_or(
            Self::Indexed {
                accent: hue.ansi_index(),
            },
            Self::Tinted,
        ))
    }

    /// The entry's background: the tint (the stronger one when focused), or
    /// nothing when the look has no tint.
    pub(in crate::shell::sidebar) fn background(self, focused: bool) -> Option<Color> {
        match self {
            Self::Tinted(colors) => Some(rgb(if focused {
                colors.tint_focused
            } else {
                colors.tint
            })),
            Self::Indexed { .. } => None,
        }
    }

    /// The accent foreground.
    pub(in crate::shell::sidebar) fn accent(self) -> Color {
        match self {
            Self::Tinted(colors) => rgb(colors.accent),
            Self::Indexed { accent } => Color::Indexed(accent),
        }
    }

    /// `style` for text on line `row` of an entry: the derived main colour on
    /// the first line and the dim colour below it, or, without a tint, the
    /// palette's colour with the second line dimmed. Modifiers such as bold are
    /// kept.
    pub(in crate::shell::sidebar) fn text(self, style: Style, row: usize) -> Style {
        match (self, row) {
            (Self::Tinted(colors), 0) => style.fg(rgb(colors.text)),
            (Self::Tinted(colors), _) => style.fg(rgb(colors.dim)),
            (Self::Indexed { .. }, 0) => style,
            (Self::Indexed { .. }, _) => style.add_modifier(Modifier::DIM),
        }
    }

    /// The separator between tokens on line `row`: dim text on a tint, the
    /// palette's separator colour otherwise.
    pub(in crate::shell::sidebar) fn separator(self, palette: &Palette, row: usize) -> Style {
        match self {
            Self::Tinted(colors) => Style::default().fg(rgb(colors.dim)),
            Self::Indexed { .. } => self.text(Style::default().fg(palette.overlay0), row),
        }
    }
}

fn rgb(color: shepr_term::RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(label: &str, palette: HostHue) -> shepr_config::MachineConfig {
        shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse(label).expect("test label"),
            ssh: shepr_config::SshTarget::parse(label).expect("test target"),
            palette,
        }
    }

    fn local_desk(machines: Vec<shepr_config::MachineConfig>) -> shepr_config::ClientConfig {
        shepr_config::ClientConfig {
            local: shepr_config::LocalConfig {
                label: Some(shepr_config::MachineLabel::parse("desk").expect("test label")),
            },
            machines,
            ..Default::default()
        }
    }

    fn hues() -> HostHues {
        use shepr_test_fixtures::ValidatedClientConfigFixture as _;
        let config = local_desk(vec![
            machine("desk", HostHue::Purple),
            machine("build", HostHue::Green),
            machine("ci", HostHue::Green),
        ]);
        HostHues::from_validated_config(&shepr_config::ValidatedClientConfig::test_from_config(
            config, None,
        ))
    }

    fn ssh(label: &str) -> ClientEndpointId {
        ClientEndpointId::Ssh(shepr_config::MachineLabel::parse(label).expect("test label"))
    }

    #[test]
    fn each_endpoint_gets_the_hue_it_was_configured_with() {
        let hues = hues();
        // The local server takes this host's own entry's hue.
        assert_eq!(hues.local(), HostHue::Purple);
        assert_eq!(
            hues.hue_for(&ClientEndpointId::Local),
            Some(HostHue::Purple)
        );
        assert_eq!(hues.hue_for(&ssh("build")), Some(HostHue::Green));
        assert_eq!(hues.hue_for(&ssh("unknown")), None);
        assert_eq!(hues.configured(), [HostHue::Green, HostHue::Purple]);
    }

    #[test]
    fn a_local_server_without_its_own_entry_takes_the_default_hue() {
        use shepr_test_fixtures::ValidatedClientConfigFixture as _;
        let hues = HostHues::from_validated_config(
            &shepr_config::ValidatedClientConfig::test_from_config(
                local_desk(vec![machine("build", HostHue::Green)]),
                None,
            ),
        );
        assert_eq!(hues.local(), shepr_config::DEFAULT_LOCAL_HUE);
    }

    #[test]
    fn without_derived_colours_an_endpoint_falls_back_to_its_ansi_colour() {
        let hues = hues();
        assert_eq!(
            PillLook::for_endpoint(&hues, None, &ssh("build")),
            Some(PillLook::Indexed { accent: 2 })
        );

        let look = PillLook::Indexed { accent: 2 };
        assert_eq!(look.background(true), None);
        assert_eq!(look.accent(), Color::Indexed(2));
        let base = Style::default().fg(Color::Red);
        assert_eq!(look.text(base, 0), base);
        assert_eq!(look.text(base, 1), base.add_modifier(Modifier::DIM));
    }

    #[test]
    fn host_colour_reports_rederive_the_colours_and_repaint() {
        use shepr_termio::input::raw_input::RawInputEvent;

        let config = local_desk(vec![machine("desk", HostHue::Green)]);
        let mut state = ClientShellState::new(
            crate::shell::config::ClientShellConfig::from_config(&config),
        );
        assert_eq!(state.host_pills, None);
        assert_eq!(state.palette, Palette::terminal(HostHue::Green));

        let foreground = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
            kind: shepr_term::host::DefaultColorKind::Foreground,
            color: shepr_term::RgbColor {
                r: 0xcd,
                g: 0xd6,
                b: 0xf4,
            },
        }]);
        // Nothing is derived until the background is known.
        assert!(!foreground.repaint);
        assert_eq!(state.host_pills, None);

        let background = shepr_term::RgbColor {
            r: 0x1e,
            g: 0x1e,
            b: 0x2e,
        };
        let outcome = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
            kind: shepr_term::host::DefaultColorKind::Background,
            color: background,
        }]);
        assert!(outcome.repaint);
        let dark = state
            .host_pills
            .as_ref()
            .and_then(|pills| pills.colors(HostHue::Green))
            .expect("a reported background derives the configured hue");
        let dark_palette = state.palette.clone();
        assert!(
            matches!(dark_palette.panel_bg, Color::Rgb(..)),
            "{dark_palette:?}"
        );

        // A palette reply outside the named slots changes nothing.
        let unrelated = state.handle_raw_events(vec![RawInputEvent::HostPaletteColors {
            colors: vec![(200, background)],
        }]);
        assert!(!unrelated.repaint);

        // A switch to a light background rederives them.
        let outcome = state.handle_raw_events(vec![
            RawInputEvent::HostDefaultColor {
                kind: shepr_term::host::DefaultColorKind::Background,
                color: shepr_term::RgbColor {
                    r: 0xfa,
                    g: 0xfa,
                    b: 0xfa,
                },
            },
            RawInputEvent::HostDefaultColor {
                kind: shepr_term::host::DefaultColorKind::Foreground,
                color: shepr_term::RgbColor {
                    r: 0x38,
                    g: 0x3a,
                    b: 0x42,
                },
            },
        ]);
        assert!(outcome.repaint);
        let light = state
            .host_pills
            .as_ref()
            .and_then(|pills| pills.colors(HostHue::Green))
            .expect("still derived");
        assert_ne!(dark, light);
        assert_ne!(dark_palette, state.palette);
    }

    /// The server names the focused border's role; the client draws it in the
    /// presented machine's sidebar accent, and redraws it when that changes.
    #[test]
    fn the_focused_pane_border_takes_the_presented_machines_accent() {
        use shepr_protocol::{ChromeRole, WireColor};
        use shepr_surface::ratatui_conversion::WireColorExt as _;
        use shepr_termio::input::raw_input::RawInputEvent;

        let config = local_desk(vec![machine("desk", HostHue::Green)]);
        let mut state = ClientShellState::new(
            crate::shell::config::ClientShellConfig::from_config(&config),
        );
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        let mut surface = crate::shell::tests::surface();
        surface.frame.cells_mut()[0].fg = WireColor::Chrome(ChromeRole::BorderFocused);
        state.receive_pane_surface_from(
            surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        let border_fg = |state: &mut ClientShellState| {
            let frame = state.compose(100, 20).expect("pane frame");
            let inner = state.pane_hits()[0].inner_rect;
            frame.cells()[usize::from(inner.y) * 100 + usize::from(inner.x)].fg
        };

        // No background reported: the hue's own ANSI colour.
        assert_eq!(border_fg(&mut state), WireColor::Indexed(2));

        let outcome = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
            kind: shepr_term::host::DefaultColorKind::Background,
            color: shepr_term::RgbColor {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e,
            },
        }]);
        assert!(outcome.repaint);
        let accent = state
            .host_accent(&crate::endpoint::ClientEndpointId::Local)
            .expect("the local server has a hue");
        assert!(matches!(accent, Color::Rgb(..)), "{accent:?}");
        assert_eq!(border_fg(&mut state), WireColor::from_ratatui(accent));
    }

    #[test]
    fn derived_colours_tint_the_entry_and_colour_its_lines() {
        let hues = hues();
        let theme = shepr_term::host::TerminalTheme {
            background: Some(shepr_term::RgbColor {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e,
            }),
            foreground: Some(shepr_term::RgbColor {
                r: 0xcd,
                g: 0xd6,
                b: 0xf4,
            }),
            ..Default::default()
        };
        let pills = HostPillPalette::derive(&theme, &hues.configured()).expect("background known");
        let Some(PillLook::Tinted(colors)) =
            PillLook::for_endpoint(&hues, Some(&pills), &ssh("build"))
        else {
            panic!("a reported background gives a tint");
        };
        let look = PillLook::Tinted(colors);
        assert_eq!(look.background(false), Some(rgb(colors.tint)));
        assert_eq!(look.background(true), Some(rgb(colors.tint_focused)));
        let bold = Style::default().add_modifier(Modifier::BOLD);
        assert_eq!(look.text(bold, 0), bold.fg(rgb(colors.text)));
        assert_eq!(look.text(bold, 1), bold.fg(rgb(colors.dim)));
        assert_eq!(look.accent(), rgb(colors.accent));
    }

    fn draw_workspace_entry(
        look: Option<PillLook>,
        focused: bool,
        selected: bool,
    ) -> ratatui::buffer::Buffer {
        let palette = Palette::test_dark();
        let area = ratatui::layout::Rect::new(0, 0, 20, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        crate::shell::sidebar::render_workspace_rows(
            &mut buffer,
            area,
            1,
            shepr_protocol::AgentStatus::Idle,
            &[Vec::new(), Vec::new()],
            crate::shell::sidebar::WorkspaceEntryState {
                focused,
                selected,
                navigating: false,
                dragged: false,
                look,
            },
            &palette,
        );
        buffer
    }

    #[test]
    fn a_tinted_workspace_entry_fills_its_area_and_numbers_in_the_accent() {
        let colors = HostPillColors {
            tint: shepr_term::RgbColor { r: 1, g: 2, b: 3 },
            tint_focused: shepr_term::RgbColor { r: 4, g: 5, b: 6 },
            text: shepr_term::RgbColor { r: 7, g: 8, b: 9 },
            dim: shepr_term::RgbColor {
                r: 10,
                g: 11,
                b: 12,
            },
            accent: shepr_term::RgbColor {
                r: 13,
                g: 14,
                b: 15,
            },
        };
        let look = Some(PillLook::Tinted(colors));

        let plain = draw_workspace_entry(look, false, false);
        for cell in &plain.content {
            assert_eq!(cell.bg, rgb(colors.tint));
        }
        assert_eq!(plain[(1, 0)].fg, rgb(colors.accent));

        let focused = draw_workspace_entry(look, true, false);
        for cell in &focused.content {
            assert_eq!(cell.bg, rgb(colors.tint_focused));
        }

        // The navigation cursor keeps the palette's own highlight and colours.
        let selected = draw_workspace_entry(look, false, true);
        let untinted = draw_workspace_entry(None, false, true);
        assert_eq!(selected, untinted);
    }

    #[test]
    fn an_indexed_workspace_entry_keeps_the_plain_background() {
        let indexed = draw_workspace_entry(Some(PillLook::Indexed { accent: 2 }), true, false);
        let plain = draw_workspace_entry(None, true, false);
        assert_eq!(indexed[(5, 0)].bg, plain[(5, 0)].bg);
        assert_eq!(indexed[(1, 0)].fg, Color::Indexed(2));
    }
}
