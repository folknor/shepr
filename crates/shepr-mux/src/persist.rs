//! Session persistence - save/restore workspaces, layouts, and working directories.
//!
//! Files live in the data directory passed by the runtime (per build
//! profile). The layout is `session.json`. One server at a time owns a data
//! directory, enforced by a lease on `session.lock` there (see `lock`).
//! Within the server, the [`SessionPersister`] (see `actor`) is the one owner
//! of that file once restore has read it: it holds the lease and the writer.
//! If a worker job panics, it stops writing but keeps the lease until the
//! server retires that persister, preventing another server from restoring
//! stale files while this one still owns live panes.
//!
mod actor;
mod capture;
mod error;
mod files;
mod lock;
mod open;
mod recovery;
mod restore;
pub mod schema;
mod writer;

pub use self::actor::{PendingSave, PersistJob, SaveCompletion, SessionBundle, SessionPersister};
pub use self::capture::{
    CapturedLayout, PendingCwds, SavedPaneRef, SessionCapture, capture, capture_job,
};
pub use self::error::{SaveError, SaveRefusal};
pub use self::files::{SessionLoad, check_session_target, load, session_path};
pub use self::lock::DataDirLease;
pub use self::open::{OpenedSession, SessionOpenOptions, open_session};
pub use self::recovery::SessionBackupPolicy;
