mod id;
pub mod state;
mod title;

pub use id::allocate_terminal_id;
pub use state::{
    AgentResumeState, EffectiveStateChange, Label, PaneStartFailure, ResumeUnavailableReason,
    TerminalState,
};
pub(crate) use title::stripped_terminal_title;
