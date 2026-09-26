//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Files live in the session's data directory (`crate::session::data_dir`):
//! the config directory itself for the default session, `sessions/<name>/`
//! under it for a named one. The layout is `session.json`; optional pane
//! screen history is stored separately in `session-history.json`. One server
//! at a time owns a data directory, enforced by a lock on `session.lock`
//! there (see `lock`).

mod io;
pub(crate) mod lock;
mod restore;
mod snapshot;
mod writer;

pub use self::io::{load, load_history};
pub use self::restore::restore;
#[cfg(test)]
pub use self::snapshot::capture_history;
pub use self::snapshot::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, TabSnapshot,
    WorkspaceSnapshot, capture,
};
pub(crate) use self::snapshot::{PendingHistory, capture_pending_history};
pub(crate) use self::writer::SessionWriter;
