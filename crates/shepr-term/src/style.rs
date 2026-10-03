/// The terminal underline shape carried by a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    pub const fn sgr_param(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Single => Some("4"),
            Self::Double => Some("4:2"),
            Self::Curly => Some("4:3"),
            Self::Dotted => Some("4:4"),
            Self::Dashed => Some("4:5"),
        }
    }
}
