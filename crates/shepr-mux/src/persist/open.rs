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
    pub restore_summary: Option<(usize, &'static str)>,
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
    let mut protect_unloaded = options.policy == SessionOpenPolicy::Persist;
    let mut restored_host_theme = None;
    let mut restore_notice = None;
    let mut restore_summary = None;
    let mut restored = None;
    let mut history_carry = HistoryCarry::default();

    if options.policy == SessionOpenPolicy::Persist {
        let backup_dir = || {
            session_backup_directory(lease.directory())
                .display()
                .to_string()
        };
        match load(&lease) {
            SessionLoad::Missing => {}
            SessionLoad::Unusable(reason) => {
                restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                    loss: shepr_protocol::SessionRestoreLoss::Unusable { reason },
                    backup_dir: backup_dir(),
                });
            }
            SessionLoad::Loaded {
                snapshot,
                history_digest,
            } => {
                protect_unloaded = false;
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
                    restore_damage,
                    dropped_workspaces,
                } = restored_session;
                history_carry = restored_history;
                let loss =
                    shepr_protocol::SessionRestoreLoss::partial(dropped_workspaces, restore_damage);
                let restore_was_partial = loss.is_some();
                if let Some(loss) = loss {
                    protect_unloaded = true;
                    tracing::warn!(
                        dropped_workspaces,
                        restore_damage,
                        "session restore discarded saved data; the saved session is backed up to session-backups before the first save"
                    );
                    restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                        loss,
                        backup_dir: backup_dir(),
                    });
                }
                let outcome = if restore_was_partial {
                    "partial"
                } else if workspaces.is_empty() {
                    "empty"
                } else {
                    "ok"
                };
                restore_summary = Some((workspaces.len(), outcome));
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
            SessionPersister::spawn(lease, protect_unloaded, history_carry, save_finished)
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
