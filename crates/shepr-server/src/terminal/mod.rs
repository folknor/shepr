mod history_read;
pub(crate) mod metadata_tokens;
pub mod state;
mod title;

pub(crate) use history_read::{
    ScreenSnapshot, TerminalReadSnapshot, UpwardMerge, merge_scrolled_up, snapshot_text,
};
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
