//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Files live in the data directory passed by the runtime (per build
//! profile). The layout is `session.json`; optional pane
//! screen history is stored separately in `session-history.json`. One server
//! at a time owns a data directory, enforced by a lease on `session.lock`
//! there (see `lock`). Within the server, the [`SessionPersister`] (see
//! `actor`) is the one owner of those files once restore has read them: it
//! holds the lease, the writer and the pane history carried between saves.

mod actor;
mod io;
pub mod lock;
mod restore;
pub mod snapshot;
mod writer;

pub use self::actor::{PendingSave, PersistJob, SaveCompletion, SessionBundle, SessionPersister};
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
