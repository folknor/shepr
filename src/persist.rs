//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/shepr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.

mod io;
mod restore;
mod snapshot;
mod writer;

pub use self::io::{load, load_history};
pub use self::restore::restore;
pub use self::snapshot::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, TabSnapshot,
    WorkspaceSnapshot, capture, capture_history,
};
pub(crate) use self::writer::SessionWriter;
