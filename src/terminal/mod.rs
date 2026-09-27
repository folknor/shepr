mod history_read;
pub(crate) mod metadata_tokens;
mod runtime_registry;
pub mod state;
mod title;

pub use crate::pane::PaneRuntime as TerminalRuntime;
use crate::protocol::TerminalId;
pub(crate) use history_read::{ScreenSnapshot, UpwardMerge, merge_scrolled_up, snapshot_text};
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
