use crate::{ClientHostAppearance, ClientHostColor, ClientHostDefaultColorKind};
use shepr_term::{ColorScheme as HostAppearance, DefaultColor as DefaultColorKind, RgbColor};

impl From<RgbColor> for ClientHostColor {
    fn from(color: RgbColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<ClientHostColor> for RgbColor {
    fn from(color: ClientHostColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<DefaultColorKind> for ClientHostDefaultColorKind {
    fn from(kind: DefaultColorKind) -> Self {
        match kind {
            DefaultColorKind::Foreground => Self::Foreground,
            DefaultColorKind::Background => Self::Background,
        }
    }
}

impl From<ClientHostDefaultColorKind> for DefaultColorKind {
    fn from(kind: ClientHostDefaultColorKind) -> Self {
        match kind {
            ClientHostDefaultColorKind::Foreground => Self::Foreground,
            ClientHostDefaultColorKind::Background => Self::Background,
        }
    }
}

impl From<HostAppearance> for ClientHostAppearance {
    fn from(appearance: HostAppearance) -> Self {
        match appearance {
            HostAppearance::Dark => Self::Dark,
            HostAppearance::Light => Self::Light,
        }
    }
}

impl From<ClientHostAppearance> for HostAppearance {
    fn from(appearance: ClientHostAppearance) -> Self {
        match appearance {
            ClientHostAppearance::Dark => Self::Dark,
            ClientHostAppearance::Light => Self::Light,
        }
    }
}
