//! A boot's session open: the one sequence from the data directory lease to
//! the restored session and the persister that owns its files from then on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Notify;

use crate::pane::{PaneLauncher, PaneRuntime};
use crate::workspace::WorkspaceChrome;
use crate::workspace::{WorkspaceIdAllocator, WorkspaceSet};
use shepr_core::layout::PaneId;

use super::actor::SessionPersister;
use super::files::{SessionLoad, load, session_backup_directory, session_path};
use super::lock::DataDirLease;
use super::recovery::SessionBackupPolicy;
use super::restore::{RestoredSession, plan_restore};

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
    pub geometry: WorkspaceChrome,
    pub launcher: &'a PaneLauncher,
    pub resume_agents_on_restore: bool,
    pub now: Instant,
}

/// The opened session, ready for the server to build its state from.
pub struct OpenedSession {
    /// The restored workspaces with the saved bookmark, empty when nothing
    /// was restored. The set owns the session's one workspace ID allocator,
    /// which restore moved past every saved ID before it issued any.
    pub workspaces: WorkspaceSet,
    /// The runtime of each restored pane whose shell launched, keyed by pane.
    pub terminal_runtimes: HashMap<PaneId, PaneRuntime>,
    /// The saved host theme; the default when no session was loaded.
    pub host_theme: shepr_term::host::TerminalTheme,
    /// The owner of the session's files from here on, holding the lease: a
    /// writing persister for [`SessionOpenPolicy::Persist`], a lease-only one
    /// for [`SessionOpenPolicy::Never`].
    pub persister: SessionPersister,
    /// Set when the saved session did not come back in full.
    pub restore_notice: Option<shepr_protocol::SessionRestoreNotice>,
}

/// How a loaded session's restore went, as the restore log reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionRestoreOutcome {
    /// Some saved data was dropped during restore.
    Partial,
    /// A valid session loaded with no workspaces.
    Empty,
    /// The saved session loaded and restored without loss.
    Restored,
}

impl SessionRestoreOutcome {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Empty => "empty",
            Self::Restored => "ok",
        }
    }
}

/// What the restore log says about a session that loaded; nothing is logged
/// for a missing or unusable one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionRestoreSummary {
    workspaces: usize,
    outcome: SessionRestoreOutcome,
}

/// Opens the session in the data directory `lease` guards. Under
/// [`SessionOpenPolicy::Persist`] it reads the saved layout, restores it
/// through the launcher, decides whether the saved file needs a recovery copy
/// before the first write and
/// whether clients must be told of a loss, and logs how the restore went.
/// Either way the lease moves to the returned persister, which fires
/// `save_finished` each time a submitted save ends.
pub fn open_session(
    lease: DataDirLease,
    options: &SessionOpenOptions<'_>,
    save_finished: Arc<Notify>,
) -> OpenedSession {
    let path = session_path(lease.directory());
    let (opened, summary) = open_and_summarize(lease, options, save_finished);
    if let Some(summary) = summary {
        log_restore(&path, summary);
    }
    opened
}

fn log_restore(path: &std::path::Path, summary: SessionRestoreSummary) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome = summary.outcome.as_log_value(),
        path = %path.display(),
        workspaces = summary.workspaces,
        "session restore evaluated"
    );
}

/// [`open_session`] without its log: the session, and the summary the log
/// reports.
fn open_and_summarize(
    lease: DataDirLease,
    options: &SessionOpenOptions<'_>,
    save_finished: Arc<Notify>,
) -> (OpenedSession, Option<SessionRestoreSummary>) {
    let mut backup_policy = SessionBackupPolicy::PreserveExisting;
    let mut host_theme = None;
    let mut restore_notice = None;
    let mut restore_summary = None;
    let mut restored = None;
    // The session's one workspace ID allocator. Restore moves it past every
    // saved ID before it issues any, and the workspace set then owns it.
    let mut workspace_ids = WorkspaceIdAllocator::new();

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
            SessionLoad::Loaded(snapshot) => {
                backup_policy = SessionBackupPolicy::NoBackupNeeded;
                host_theme = Some(snapshot.host_theme.to_theme());
                let restored_session = plan_restore(
                    &snapshot,
                    options.geometry,
                    options.resume_agents_on_restore,
                    options.now,
                    &mut workspace_ids,
                )
                .launch(options.launcher);
                let RestoredSession {
                    workspaces,
                    terminal_runtimes,
                    active,
                    restore_loss,
                } = restored_session;
                let restore_was_partial = restore_loss.is_some();
                if let Some(damage) = restore_loss {
                    backup_policy = SessionBackupPolicy::PreserveExisting;
                    tracing::warn!(
                        dropped_workspaces = damage.dropped_workspaces,
                        renamed_workspaces = damage.renamed_workspaces,
                        dropped_agent_sessions = damage.dropped_agent_sessions.len(),
                        "session restore dropped or repaired saved data; the saved session is backed up to session-backups before the first save"
                    );
                    restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                        loss: shepr_protocol::SessionRestoreLoss::Damaged(damage),
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
                restored = Some((workspaces, terminal_runtimes, active));
            }
        }
    }

    let persister = match options.policy {
        SessionOpenPolicy::Never => SessionPersister::lease_only(lease, save_finished),
        SessionOpenPolicy::Persist => SessionPersister::spawn(lease, backup_policy, save_finished),
    };
    // Nothing restored: an empty set with no bookmark and no runtimes.
    let (workspaces, terminal_runtimes, active) = restored.unwrap_or_default();

    let opened = OpenedSession {
        workspaces: WorkspaceSet::restored(workspace_ids, workspaces, active),
        terminal_runtimes,
        host_theme: host_theme.unwrap_or_default(),
        persister,
        restore_notice,
    };
    (opened, restore_summary)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use super::*;
    use crate::pane::PaneRuntimeRegistry;
    use crate::persist::schema::{
        DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SNAPSHOT_VERSION, SessionSnapshot,
        WorkspaceSnapshot,
    };
    use crate::persist::{SaveError, SaveRefusal, capture_job};
    use shepr_protocol::PanePublicNumber;

    /// A launcher whose shell can never start: every restored pane comes back
    /// without a runtime, refused before any fork, so these tests run no
    /// child and need no async runtime.
    fn refusing_launcher() -> PaneLauncher {
        let (events, _) = tokio::sync::mpsc::channel(1);
        let shell = shepr_test_support::fixture::resolved_shell("/__shepr_missing_open_shell__\0");
        PaneLauncher::new(
            crate::pane::PaneSpawnHandles {
                events,
                render_notify: Arc::new(Notify::new()),
                render_dirty: Arc::new(crate::render_signal::RenderSignal::new()),
                pane_teardowns: Arc::default(),
                socket_path: PathBuf::from("/run/user/1000/shepr-test.sock"),
            },
            crate::pane::PaneShellConfig::new(&shell, false),
            shepr_core::scrollback::ScrollbackBudget::new(0),
            None,
        )
    }

    fn geometry() -> WorkspaceChrome {
        WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, 80, 24),
            pane_gaps: false,
            pane_scrollbars: false,
        }
    }

    /// A data directory under a fresh scratch directory, with its lease.
    struct DataDir {
        _scratch: crate::test_support::ScratchDir,
        path: PathBuf,
    }

    impl DataDir {
        fn new(name: &str) -> (Self, DataDirLease) {
            let scratch = crate::test_support::ScratchDir::new(name);
            let lease = DataDirLease::acquire(&scratch.join("data")).expect("test session lease");
            // The lease's own canonical directory: the one open reads and
            // names in its notices.
            let path = lease.directory().to_path_buf();
            (
                Self {
                    _scratch: scratch,
                    path,
                },
                lease,
            )
        }

        fn session_file(&self) -> PathBuf {
            session_path(&self.path)
        }

        fn backups(&self) -> PathBuf {
            session_backup_directory(&self.path)
        }

        /// Writes `snapshot` as the saved session; returns the bytes written.
        fn write_session(&self, snapshot: &SessionSnapshot) -> Vec<u8> {
            let bytes = serde_json::to_vec(snapshot).expect("encode the saved session");
            std::fs::write(self.session_file(), &bytes).expect("write the saved session");
            bytes
        }
    }

    fn open(
        lease: DataDirLease,
        policy: SessionOpenPolicy,
    ) -> (OpenedSession, Option<SessionRestoreSummary>) {
        let launcher = refusing_launcher();
        open_and_summarize(
            lease,
            &SessionOpenOptions {
                policy,
                geometry: geometry(),
                launcher: &launcher,
                resume_agents_on_restore: false,
                now: Instant::now(),
            },
            Arc::new(Notify::new()),
        )
    }

    /// Saves the opened session as it stands, as the server's first save
    /// would.
    fn save(opened: &mut OpenedSession) -> Result<(), SaveError> {
        let job = capture_job(
            &opened.workspaces,
            &PaneRuntimeRegistry::new(),
            &shepr_core::absolute_path::AbsolutePath::root(),
            opened.host_theme,
        )
        .into_job();
        opened.persister.submit(job, SystemTime::now()).wait()
    }

    fn directory_files(directory: &Path) -> Vec<Vec<u8>> {
        let mut paths = std::fs::read_dir(directory)
            .expect("read backup directory")
            .map(|entry| entry.expect("backup directory entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .iter()
            .map(|path| std::fs::read(path).expect("read backup"))
            .collect()
    }

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("number")
    }

    fn pane(public_number: usize) -> PaneSnapshot {
        PaneSnapshot {
            cwd: shepr_core::absolute_path::AbsolutePath::root(),
            public_number: number(public_number),
            label: None,
            agent_session: None,
            unusable_agent_session: None,
        }
    }

    fn workspace(id: &str, name: &str, layout: LayoutSnapshot, next: usize) -> WorkspaceSnapshot {
        let first = layout.panes()[0].public_number;
        WorkspaceSnapshot {
            id: id.parse().expect("workspace id"),
            name: crate::terminal::Label::new(name).expect("test workspace name"),
            layout,
            next_public_pane_number: number(next),
            zoomed: false,
            focused: first,
            root_pane: first,
        }
    }

    fn session(workspaces: Vec<WorkspaceSnapshot>, active: Option<usize>) -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces,
            active,
        }
    }

    fn names(opened: &OpenedSession) -> Vec<&str> {
        opened
            .workspaces
            .iter()
            .map(crate::workspace::Workspace::name)
            .collect()
    }

    #[test]
    fn a_fresh_start_has_nothing_to_report() {
        let (_dir, lease) = DataDir::new("open-fresh-start");
        let (opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert!(opened.workspaces.is_empty());
        assert!(opened.terminal_runtimes.is_empty());
        assert_eq!(opened.restore_notice, None);
        assert_eq!(summary, None);
    }

    #[test]
    fn a_never_policy_reads_nothing_and_only_holds_the_lease() {
        let (dir, lease) = DataDir::new("open-never");
        let original = dir.write_session(&session(
            vec![workspace("w1", "saved", LayoutSnapshot::Pane(pane(1)), 2)],
            Some(0),
        ));
        let (mut opened, summary) = open(lease, SessionOpenPolicy::Never);
        assert!(opened.workspaces.is_empty(), "nothing is restored");
        assert_eq!(opened.restore_notice, None);
        assert_eq!(summary, None);
        assert!(matches!(
            save(&mut opened),
            Err(SaveError::Refused(SaveRefusal::LeaseOnly))
        ));
        assert_eq!(
            std::fs::read(dir.session_file()).expect("the saved session is untouched"),
            original
        );
        assert!(
            DataDirLease::acquire(&dir.path).is_err(),
            "the persister holds the lease"
        );
    }

    #[test]
    fn a_clean_restore_reports_its_workspaces_and_needs_no_backup() {
        let (dir, lease) = DataDir::new("open-clean");
        dir.write_session(&session(
            vec![
                workspace("w1", "first", LayoutSnapshot::Pane(pane(1)), 2),
                workspace("w2", "second", LayoutSnapshot::Pane(pane(1)), 2),
            ],
            Some(1),
        ));
        let (mut opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert_eq!(names(&opened), vec!["first", "second"]);
        assert_eq!(
            opened.workspaces.bookmark(),
            Some("w2".parse().expect("workspace id"))
        );
        assert_eq!(opened.restore_notice, None);
        assert_eq!(
            summary,
            Some(SessionRestoreSummary {
                workspaces: 2,
                outcome: SessionRestoreOutcome::Restored,
            })
        );
        save(&mut opened).expect("first save");
        assert!(
            !dir.backups().try_exists().expect("test stat"),
            "a session restored in full is not backed up"
        );
    }

    #[test]
    fn a_saved_session_without_workspaces_restores_as_empty() {
        let (dir, lease) = DataDir::new("open-empty");
        dir.write_session(&session(Vec::new(), None));
        let (opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert!(opened.workspaces.is_empty());
        assert_eq!(opened.restore_notice, None);
        assert_eq!(
            summary,
            Some(SessionRestoreSummary {
                workspaces: 0,
                outcome: SessionRestoreOutcome::Empty,
            })
        );
    }

    /// A session file that does not parse restores nothing, like a missing
    /// one, but unlike a missing one it is a whole saved session: clients are
    /// told, and the first save backs the file up before replacing it.
    #[test]
    fn an_unusable_session_file_is_reported_and_backed_up() {
        let (dir, lease) = DataDir::new("open-unusable");
        let original = b"{ this is not a session".to_vec();
        std::fs::write(dir.session_file(), &original).expect("test precondition");

        let (mut opened, summary) = open(lease, SessionOpenPolicy::Persist);
        let Some(shepr_protocol::SessionRestoreNotice {
            loss:
                shepr_protocol::SessionRestoreLoss::Unusable {
                    failure:
                        shepr_protocol::SessionRestoreFailure::Unparseable { line, category, .. },
                },
            backup_dir,
        }) = opened.restore_notice.clone()
        else {
            panic!(
                "an unusable session file is reported: {:?}",
                opened.restore_notice
            );
        };
        assert_eq!(line, 1);
        assert_eq!(category, shepr_protocol::SessionParseCategory::Syntax);
        assert_eq!(backup_dir.as_path(), dir.backups());
        assert_eq!(summary, None, "only a loaded session is summarised");

        save(&mut opened).expect("first save");
        assert_eq!(directory_files(&dir.backups()), vec![original]);
    }

    /// A restore that drops a saved workspace leaves that workspace only in the
    /// session file, so the first save copies the file to `session-backups` before
    /// replacing it. The copy is made once, not on every save.
    #[test]
    fn a_restore_that_drops_a_workspace_backs_up_the_saved_session_before_the_first_save() {
        let (dir, lease) = DataDir::new("open-dropped-workspace");
        // A saved split ratio out of range refuses the whole file at decode,
        // so the workspace-level defect here is two panes sharing one public
        // number, which drops only that workspace.
        let colliding = workspace(
            "w2",
            "colliding numbers",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: shepr_core::layout::SplitRatio::EVEN,
                first: Box::new(LayoutSnapshot::Pane(pane(1))),
                second: Box::new(LayoutSnapshot::Pane(pane(1))),
            },
            2,
        );
        let original = dir.write_session(&session(
            vec![
                workspace("w1", "healthy", LayoutSnapshot::Pane(pane(1)), 2),
                colliding,
            ],
            Some(0),
        ));

        let (mut opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert_eq!(
            names(&opened),
            vec!["healthy"],
            "the saved session loaded and only the invalid workspace was dropped"
        );
        // Every client of this boot is told, naming where the original goes.
        assert_eq!(
            opened.restore_notice,
            Some(shepr_protocol::SessionRestoreNotice {
                loss: shepr_protocol::SessionRestoreLoss::Damaged(
                    shepr_protocol::SessionRestoreDamage {
                        dropped_workspaces: 1,
                        ..Default::default()
                    }
                ),
                backup_dir: dir.backups().into(),
            })
        );
        assert_eq!(
            summary,
            Some(SessionRestoreSummary {
                workspaces: 1,
                outcome: SessionRestoreOutcome::Partial,
            })
        );

        save(&mut opened).expect("first save");
        assert_eq!(directory_files(&dir.backups()), vec![original.clone()]);
        let saved = crate::persist::schema::parse_session_file(
            &std::fs::read_to_string(dir.session_file()).expect("read the new session"),
        )
        .expect("parse the new session");
        assert_eq!(
            saved
                .workspaces
                .iter()
                .map(|workspace| workspace.name.as_str())
                .collect::<Vec<_>>(),
            vec!["healthy"]
        );

        save(&mut opened).expect("second save");
        assert_eq!(
            directory_files(&dir.backups()),
            vec![original],
            "a later save makes no second backup"
        );
    }

    /// Two saved workspaces claim one ID: the second is restored under a fresh
    /// ID and the restore reports damage, which is what makes the saved file
    /// worth keeping.
    #[test]
    fn restore_with_damage_backs_up_the_saved_session_before_the_first_save() {
        let (dir, lease) = DataDir::new("open-damaged");
        let original = dir.write_session(&session(
            vec![
                workspace("w1", "first", LayoutSnapshot::Pane(pane(1)), 2),
                workspace("w1", "repeat", LayoutSnapshot::Pane(pane(1)), 2),
            ],
            Some(0),
        ));

        let (mut opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert_eq!(opened.workspaces.len(), 2);
        assert_eq!(opened.workspaces.records().count(), 2);
        assert_eq!(
            opened
                .restore_notice
                .as_ref()
                .map(|notice| notice.loss.clone()),
            Some(shepr_protocol::SessionRestoreLoss::Damaged(
                shepr_protocol::SessionRestoreDamage {
                    renamed_workspaces: 1,
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            summary.map(|summary| summary.outcome),
            Some(SessionRestoreOutcome::Partial)
        );

        save(&mut opened).expect("first save");
        assert_eq!(directory_files(&dir.backups()), vec![original]);
    }

    /// A saved agent session this build cannot use costs only itself: its pane
    /// restores as a plain shell, the rest of the file comes back, and the
    /// drop is reported naming the pane and backed up before the first save.
    #[test]
    fn an_unusable_agent_session_is_dropped_reported_and_backed_up_before_save() {
        let (dir, lease) = DataDir::new("open-invalid-agent-session");
        let snapshot = session(
            vec![
                workspace("w1", "saved", LayoutSnapshot::Pane(pane(1)), 2),
                workspace("w2", "other", LayoutSnapshot::Pane(pane(1)), 2),
            ],
            Some(0),
        );
        let mut json = serde_json::to_value(snapshot).expect("encode snapshot");
        json["workspaces"][0]["layout"]["Pane"]["agent_session"] = serde_json::json!({
            "source": "shepr:codex", "agent": "removed-agent", "session_ref": {"id": "valuable-session"}
        });
        let original = serde_json::to_vec(&json).expect("encode damaged snapshot");
        std::fs::write(dir.session_file(), &original).expect("write damaged snapshot");

        let (mut opened, summary) = open(lease, SessionOpenPolicy::Persist);

        assert_eq!(names(&opened), vec!["saved", "other"]);
        let w1 = "w1".parse().expect("workspace ID");
        assert_eq!(
            opened
                .restore_notice
                .as_ref()
                .map(|notice| notice.loss.clone()),
            Some(shepr_protocol::SessionRestoreLoss::Damaged(
                shepr_protocol::SessionRestoreDamage {
                    dropped_agent_sessions: vec![shepr_protocol::PublicPaneId::new(&w1, number(1))],
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            summary.map(|summary| summary.outcome),
            Some(SessionRestoreOutcome::Partial)
        );
        save(&mut opened).expect("first save");
        assert_eq!(directory_files(&dir.backups()), vec![original]);
    }

    #[test]
    fn invalid_saved_tree_state_drops_only_its_workspace_and_backs_up() {
        for defect in ["focus", "root", "zoom"] {
            let (dir, lease) = DataDir::new(defect);
            let mut damaged = workspace("w2", "damaged", LayoutSnapshot::Pane(pane(1)), 2);
            match defect {
                "focus" => damaged.focused = number(9),
                "root" => damaged.root_pane = number(9),
                _ => damaged.zoomed = true,
            }
            let original = dir.write_session(&session(
                vec![
                    workspace("w1", "healthy", LayoutSnapshot::Pane(pane(1)), 2),
                    damaged,
                ],
                Some(0),
            ));
            let (mut opened, _) = open(lease, SessionOpenPolicy::Persist);
            assert_eq!(names(&opened), vec!["healthy"]);
            assert_eq!(
                opened
                    .restore_notice
                    .as_ref()
                    .map(|notice| notice.loss.clone()),
                Some(shepr_protocol::SessionRestoreLoss::Damaged(
                    shepr_protocol::SessionRestoreDamage {
                        dropped_workspaces: 1,
                        ..Default::default()
                    }
                )),
                "{defect}"
            );
            save(&mut opened).expect("first save");
            assert_eq!(directory_files(&dir.backups()), vec![original]);
        }
    }

    #[test]
    fn an_exhausted_duplicate_id_reports_a_workspace_drop_without_a_rename() {
        let (dir, lease) = DataDir::new("open-exhausted-duplicate");
        let id = shepr_protocol::WorkspaceId::from_number(usize::MAX)
            .expect("nonzero workspace ID")
            .to_string();
        let original = dir.write_session(&session(
            vec![
                workspace(&id, "first", LayoutSnapshot::Pane(pane(1)), 2),
                workspace(&id, "repeat", LayoutSnapshot::Pane(pane(1)), 2),
            ],
            Some(1),
        ));
        let (mut opened, _) = open(lease, SessionOpenPolicy::Persist);
        assert_eq!(names(&opened), vec!["first"]);
        assert_eq!(
            opened
                .restore_notice
                .as_ref()
                .map(|notice| notice.loss.clone()),
            Some(shepr_protocol::SessionRestoreLoss::Damaged(
                shepr_protocol::SessionRestoreDamage {
                    dropped_workspaces: 1,
                    ..Default::default()
                }
            ))
        );
        save(&mut opened).expect("first save");
        assert_eq!(directory_files(&dir.backups()), vec![original]);
    }

    #[test]
    fn a_failed_pane_launch_keeps_its_workspace_without_a_runtime() {
        let (dir, lease) = DataDir::new("open-refused-launch");
        dir.write_session(&session(
            vec![workspace("w1", "kept", LayoutSnapshot::Pane(pane(1)), 2)],
            Some(0),
        ));
        let (opened, summary) = open(lease, SessionOpenPolicy::Persist);
        assert_eq!(names(&opened), vec!["kept"]);
        assert!(opened.terminal_runtimes.is_empty());
        assert_eq!(opened.restore_notice, None, "a launch failure is no loss");
        assert_eq!(
            summary.map(|summary| summary.outcome),
            Some(SessionRestoreOutcome::Restored)
        );
    }
}
