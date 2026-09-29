mod history_read;
pub mod state;
mod title;

pub use history_read::{
    ScreenSnapshot, TerminalReadSnapshot, UpwardMerge, merge_scrolled_up, snapshot_text,
};
pub use state::{EffectiveStateChange, RestoreFailure, TerminalState, TerminalStateMutation};
pub(crate) use title::stripped_terminal_title;
