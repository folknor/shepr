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

//!
//! The files of this module, by job: `schema` is the on-disk schema, `capture`
//! reads the live session into it, `history` carries pane history between
//! saves and serializes it, `files` is path policy, publication and reading,
//! `recovery` makes and prunes recovery copies, and `writer` is the save
//! sequence over them.

mod actor;
mod capture;
mod error;
mod files;
mod history;
pub mod lock;
mod open;
mod recovery;
mod restore;
pub mod schema;
mod writer;

pub use self::actor::{PendingSave, PersistJob, SaveCompletion, SessionBundle, SessionPersister};
pub use self::capture::{
    PendingCwds, SavedPaneRef, capture, capture_deferred, capture_job,
    capture_pending_cwds_for_snapshot, capture_pending_history,
    capture_pending_history_for_snapshot,
};
pub use self::error::{SaveError, SaveRefusal};
pub use self::files::{
    SessionLoad, check_session_target, load, load_history, session_backup_directory,
};
pub use self::history::{HistoryCarry, HistoryDigest, PendingHistory, SessionHistory};
pub use self::lock::{DataDirLease, DataDirLeaseHeld};
pub use self::open::{
    OpenedRestore, OpenedSession, SessionOpenOptions, SessionOpenPolicy, SessionRestoreOutcome,
    SessionRestoreSummary, open_session,
};
pub use self::recovery::SessionBackupPolicy;
pub use self::restore::{RestoreLoss, RestoredSession, SessionRestorePlan, plan_restore};
pub use self::schema::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, WorkspaceSnapshot,
};
pub use self::writer::SessionWriter;

#[cfg(test)]
pub use self::capture::capture_history;
