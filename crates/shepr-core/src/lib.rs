pub mod absolute_path;
pub mod backoff;
pub mod chrome;
pub mod env;
pub mod geometry;
pub mod layout;
pub mod limits;
pub mod locks;
pub mod pathutil;
pub mod scrollback;
pub mod shell;
pub mod shell_quote;
pub mod socket_path;

pub mod workspace_label;

/// The text of a panic payload, or `fallback` when the payload is not a
/// string. Only string payloads are read: formatting any other type could run
/// arbitrary code, which a panic hook must not do.
pub fn panic_message<'a>(payload: &'a (dyn std::any::Any + Send), fallback: &'a str) -> &'a str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        fallback
    }
}

/// Declare a unit enum whose serde and operator spellings share one table.
/// The generated exhaustive Display match keeps every variant covered.
#[macro_export]
macro_rules! named_enum {
    ($(#[$enum_attr:meta])* $vis:vis enum $name:ident {
        $($(#[$variant_attr:meta])* $variant:ident => $spelling:literal),* $(,)?
    }) => {
        $(#[$enum_attr])*
        $vis enum $name {
            $($(#[$variant_attr])* #[serde(rename = $spelling)] $variant),*
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.label())
            }
        }

        impl $name {
            /// The variant's spelling, the same one serde and `Display` use.
            $vis const fn label(&self) -> &'static str {
                match self { $(Self::$variant => $spelling),* }
            }

            /// Every variant, generated from the spelling table, so a contract
            /// test can cover each spelling without listing the variants. Not
            /// gated on a test cfg: one written in this definition would put
            /// production lines below a test cfg in this file, which the
            /// skip-after-scopes check refuses.
            $vis const ALL: &'static [Self] = &[$(Self::$variant),*];
        }
    };
}
