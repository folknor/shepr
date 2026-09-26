mod history_read;
mod id;
pub(crate) mod metadata_tokens;
mod rows;
mod runtime_registry;
pub mod state;
mod title;

pub use crate::pane::PaneRuntime as TerminalRuntime;
pub(crate) use history_read::{ScreenSnapshot, UpwardMerge, merge_scrolled_up, snapshot_text};
pub use id::TerminalId;
pub(crate) use rows::Point;
pub use rows::{AbsRow, ScreenRow, ViewportRow};
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
