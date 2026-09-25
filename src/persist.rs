//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/shepr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.

mod io;
mod restore;
mod snapshot;
mod writer;

pub use self::io::{clear_history, load, load_history};
pub use self::restore::restore;
pub use self::snapshot::{
    capture, capture_history, DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot,
    SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};
pub(crate) use self::writer::SessionWriter;
