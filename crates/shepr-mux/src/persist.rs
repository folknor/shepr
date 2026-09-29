//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Files live in the session data directory passed by the runtime:
//! the state directory itself for the default session, `sessions/<name>/`
//! under it for a named one. The layout is `session.json`; optional pane
//! screen history is stored separately in `session-history.json`. One server
//! at a time owns a data directory, enforced by a lease on `session.lock`
//! there (see `lock`).

mod io;
pub mod lock;
mod restore;
pub mod snapshot;
mod writer;

pub use self::io::{load, load_history};
pub use self::lock::{DataDirLease, DataDirLeaseHeld};
pub use self::restore::restore;
pub use self::snapshot::{
    DirectionSnapshot, HistoryCarry, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot,
    TabSnapshot, WorkspaceSnapshot, capture,
};
pub use self::snapshot::{PendingHistory, capture_pending_history};
pub use self::writer::SessionWriter;

#[cfg(test)]
pub use self::snapshot::capture_history;
