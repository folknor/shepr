//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Files live in the data directory passed by the runtime (per build
//! profile). The layout is `session.json`; optional pane
//! screen history is stored separately in `session-history.json`, and the
//! layout names the history it pairs with by the digest of its bytes. One server
//! at a time owns a data directory, enforced by a lease on `session.lock`
//! there (see `lock`). Within the server, the [`SessionPersister`] (see
//! `actor`) is the one owner of those files once restore has read them: it
//! holds the lease, the writer and the pane history carried between saves.
//! If a worker job panics, it stops writing but keeps the lease until the
//! server retires that persister, preventing another server from restoring
//! stale files while this one still owns live panes.

mod actor;
mod capture;
mod error;
mod io;
pub mod lock;
mod open;
mod restore;
pub mod snapshot;
mod writer;

pub use self::actor::{PendingSave, PersistJob, SaveCompletion, SessionBundle, SessionPersister};
pub use self::capture::capture_job;
pub use self::error::{SaveError, SaveRefusal};
pub use self::io::{
    HistoryDigest, SessionLoad, check_session_target, load, load_history, session_backup_directory,
};
pub use self::lock::{DataDirLease, DataDirLeaseHeld};
pub use self::open::{
    OpenedRestore, OpenedSession, SessionOpenOptions, SessionOpenPolicy, SessionRestoreOutcome,
    SessionRestoreSummary, open_session,
};
pub use self::restore::{RestoreLoss, RestoredSession, SessionRestorePlan, plan_restore};
pub use self::snapshot::{
    DirectionSnapshot, HistoryCarry, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot,
    WorkspaceSnapshot, capture,
};
pub use self::snapshot::{PendingCwds, capture_deferred};
pub use self::snapshot::{PendingHistory, capture_pending_history};
pub use self::writer::{SessionBackupPolicy, SessionWriter};

#[cfg(test)]
pub use self::snapshot::capture_history;
