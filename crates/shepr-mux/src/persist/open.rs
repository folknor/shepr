use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Notify;

use crate::pane::{PaneLauncher, PaneRuntime};
use crate::terminal::TerminalState;
use crate::workspace::PaneGeometry;
use crate::workspace::Workspace;
use shepr_protocol::TerminalId;

use super::actor::SessionPersister;
use super::lock::DataDirLease;
use super::restore::RestoredSession;
use super::snapshot::HistoryCarry;
use super::writer::SessionBackupPolicy;
use super::{SessionLoad, load, load_history, plan_restore, session_backup_directory};

/// Whether a server restores and saves the session, or only holds its
/// directory lease while running without persistence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOpenPolicy {
    Never,
    Persist,
}

/// Runtime inputs needed to restore saved panes.
pub struct SessionOpenOptions<'a> {
    pub policy: SessionOpenPolicy,
    pub pane_history: bool,
    pub geometry: PaneGeometry,
    pub launcher: &'a PaneLauncher,
    pub resume_agents_on_restore: bool,
    pub now: Instant,
}

/// Everything session opening produces before the app state is assembled.
pub struct OpenedSession {
    pub policy: SessionOpenPolicy,
    pub restored: Option<OpenedRestore>,
    pub restored_host_theme: Option<shepr_termio::host_term::theme::TerminalTheme>,
    pub persister: SessionPersister,
    pub restore_notice: Option<shepr_protocol::SessionRestoreNotice>,
    pub restore_summary: Option<SessionRestoreSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRestoreOutcome {
    /// Some saved data was dropped during restore.
    Partial,
    /// A valid session loaded with no workspaces.
    Empty,
    /// The saved session loaded and restored without loss.
    Restored,
}

impl SessionRestoreOutcome {
    pub fn as_log_value(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Empty => "empty",
            Self::Restored => "ok",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionRestoreSummary {
    pub workspaces: usize,
    pub outcome: SessionRestoreOutcome,
}

/// The restored app data after its carried history has moved to the persister.
pub struct OpenedRestore {
    pub workspaces: Vec<Workspace>,
    pub terminals: std::collections::HashMap<TerminalId, TerminalState>,
    pub terminal_runtimes: std::collections::HashMap<TerminalId, PaneRuntime>,
    pub active: Option<usize>,
}

/// Reads and restores a session, decides whether its source needs a recovery
/// copy before the first write, and transfers the lease to its persister.
pub fn open_session(
    lease: DataDirLease,
    options: &SessionOpenOptions<'_>,
    save_finished: Arc<Notify>,
) -> OpenedSession {
    let mut backup_policy = SessionBackupPolicy::PreserveExisting;
    let mut restored_host_theme = None;
    let mut restore_notice = None;
    let mut restore_summary = None;
    let mut restored = None;
    let mut history_carry = HistoryCarry::default();

    if options.policy == SessionOpenPolicy::Persist {
        let backup_dir =
            || shepr_protocol::RemotePath::from(session_backup_directory(lease.directory()));
        match load(&lease) {
            SessionLoad::Missing => {}
            SessionLoad::Unusable(failure) => {
                restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                    loss: shepr_protocol::SessionRestoreLoss::Unusable { failure },
                    backup_dir: backup_dir(),
                });
            }
            SessionLoad::Loaded {
                snapshot,
                history_digest,
            } => {
                backup_policy = SessionBackupPolicy::NoBackupNeeded;
                restored_host_theme = Some(snapshot.host_theme.to_theme());
                let history = options
                    .pane_history
                    .then(|| load_history(&lease, history_digest.as_ref()))
                    .flatten();
                let restored_session = plan_restore(
                    &snapshot,
                    history.as_ref(),
                    options.geometry,
                    options.resume_agents_on_restore,
                    options.now,
                )
                .launch(options.launcher);
                let RestoredSession {
                    workspaces,
                    terminals,
                    terminal_runtimes,
                    active,
                    history_carry: restored_history,
                    restore_loss,
                } = restored_session;
                history_carry = restored_history;
                let restore_was_partial = restore_loss.is_some();
                if let Some(loss) = restore_loss {
                    backup_policy = SessionBackupPolicy::PreserveExisting;
                    tracing::warn!(
                        dropped_workspaces = loss.dropped_workspaces(),
                        restore_damage = loss.panes_pruned(),
                        "session restore discarded saved data; the saved session is backed up to session-backups before the first save"
                    );
                    restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                        loss: loss.into_notice_loss(),
                        backup_dir: backup_dir(),
                    });
                }
                let outcome = if restore_was_partial {
                    SessionRestoreOutcome::Partial
                } else if workspaces.is_empty() {
                    SessionRestoreOutcome::Empty
                } else {
                    SessionRestoreOutcome::Restored
                };
                restore_summary = Some(SessionRestoreSummary {
                    workspaces: workspaces.len(),
                    outcome,
                });
                restored = Some(OpenedRestore {
                    workspaces,
                    terminals,
                    terminal_runtimes,
                    active,
                });
            }
        }
    }

    let persister = match options.policy {
        SessionOpenPolicy::Never => SessionPersister::lease_only(lease, save_finished),
        SessionOpenPolicy::Persist => {
            SessionPersister::spawn(lease, backup_policy, history_carry, save_finished)
        }
    };

    OpenedSession {
        policy: options.policy,
        restored,
        restored_host_theme,
        persister,
        restore_notice,
        restore_summary,
    }
}
