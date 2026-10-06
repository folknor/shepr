//! Capture of the live session: the structural snapshot and the cwd probes,
//! read from workspaces and pane runtimes on the event loop and handed to
//! whoever writes them.

use std::collections::HashMap;

use crate::pane::PaneRuntimeRegistry;
use crate::workspace::{PaneRecord, Workspace, WorkspaceSet};
use shepr_core::absolute_path::AbsolutePath;
use shepr_core::layout::PaneId;
use shepr_protocol::PanePublicNumber;

use super::actor::{PersistJob, SessionBundle};
use super::schema::{
    LayoutSnapshot, PaneSnapshot, SNAPSHOT_VERSION, SessionSnapshot, WorkspaceSnapshot,
};

/// Captures the current session for a save: the job clears the saved state
/// when no workspace remains, and otherwise writes one structural snapshot.
pub fn capture_job(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    host_theme: shepr_term::host::TerminalTheme,
) -> Result<SessionCapture, super::SaveError> {
    capture_job_with_workspace_capture(
        workspaces,
        terminal_runtimes,
        fallback_cwd,
        host_theme,
        capture_workspace,
    )
}

fn capture_job_with_workspace_capture(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    host_theme: shepr_term::host::TerminalTheme,
    capture_one: impl FnMut(
        usize,
        &Workspace,
        &PaneRuntimeRegistry,
        &AbsolutePath,
        &mut PendingCwds,
        &mut HashMap<SavedPaneRef, PaneId>,
    ) -> Result<WorkspaceSnapshot, super::SaveError>,
) -> Result<SessionCapture, super::SaveError> {
    if workspaces.is_empty() {
        return Ok(SessionCapture {
            job: PersistJob::Clear,
            pane_ids: HashMap::new(),
        });
    }
    let (snapshot, cwds, pane_ids) = capture_deferred_with_workspace_capture(
        workspaces,
        terminal_runtimes,
        fallback_cwd,
        host_theme,
        capture_one,
    )?;
    Ok(SessionCapture {
        job: PersistJob::Save(SessionBundle { snapshot, cwds }),
        pane_ids,
    })
}

/// What [`capture_job`] captured: the persister's job, and the live pane each
/// saved pane came from, keyed as the job's snapshot keys them.
pub struct SessionCapture {
    job: PersistJob,
    pane_ids: HashMap<SavedPaneRef, PaneId>,
}

impl SessionCapture {
    /// The job alone, for a save whose layout is not kept.
    pub fn into_job(self) -> PersistJob {
        self.job
    }

    /// The job and, when it writes a layout rather than clearing the saved
    /// session, that layout with its pane identities, so the same layout can
    /// be written again later ([`CapturedLayout::recapture`]).
    pub fn into_job_with_layout(self) -> (PersistJob, Option<CapturedLayout>) {
        let Self { job, pane_ids } = self;
        let layout = match &job {
            PersistJob::Save(bundle) => Some(CapturedLayout {
                snapshot: bundle.snapshot.clone(),
                pane_ids,
            }),
            PersistJob::Clear => None,
        };
        (job, layout)
    }
}

/// A layout a save wrote, with the live pane each of its saved panes came
/// from. Every saved pane is in the map: a saved pane is a record in a
/// workspace's tree when the layout is captured.
pub struct CapturedLayout {
    snapshot: SessionSnapshot,
    pane_ids: HashMap<SavedPaneRef, PaneId>,
}

impl CapturedLayout {
    /// A layout from its parts. Captures build it through
    /// [`SessionCapture::into_job_with_layout`]; this constructor is the seam
    /// a dependent crate's tests build or alter one through, since this
    /// crate's `cfg(test)` does not reach them. `pane_ids` must key the panes
    /// of `snapshot`; a pane missing from it makes [`Self::recapture`] fail.
    pub fn new(snapshot: SessionSnapshot, pane_ids: HashMap<SavedPaneRef, PaneId>) -> Self {
        Self { snapshot, pane_ids }
    }

    /// The layout as it was saved.
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    /// A save of this same layout with fresh cwd probes: a pane that still
    /// has a runtime contributes its current cwd, and one removed since the
    /// capture keeps the cwd the layout saved. `None` when a saved pane has
    /// no identity in the map.
    pub fn recapture(&self, terminal_runtimes: &PaneRuntimeRegistry) -> Option<PersistJob> {
        let cwds =
            capture_pending_cwds_for_snapshot(&self.snapshot, &self.pane_ids, terminal_runtimes)?;
        Some(PersistJob::Save(SessionBundle {
            snapshot: self.snapshot.clone(),
            cwds,
        }))
    }
}

/// Where a pane sits in a snapshot: the workspace's index in the snapshot and
/// the pane's public number there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SavedPaneRef {
    pub workspace: usize,
    pub pane: PanePublicNumber,
}

/// The live cwd probe reads a capture left for whoever writes the snapshot.
/// Reading a cwd is a /proc access per pane, which the event loop should not
/// pay per save, so a capture records the best cwd it knows without one and
/// hands over a [`PaneCwdProbe`](crate::pane::PaneCwdProbe) per runtime; [`resolve`]
/// applies the same OSC 7 and /proc arbitration used by live panes.
///
/// [`resolve`]: Self::resolve
#[derive(Default)]
pub struct PendingCwds {
    probes: Vec<(SavedPaneRef, crate::pane::PaneCwdProbe)>,
}

impl PendingCwds {
    /// Reads every probe and stores the best result in `snapshot`.
    pub fn resolve(self, snapshot: &mut SessionSnapshot) {
        for (saved, probe) in self.probes {
            let Some(cwd) = probe.read() else {
                continue;
            };
            // A probe reads a live observation; one that is not absolute is
            // not saved, and the pane keeps its best known cwd.
            let Ok(cwd) = AbsolutePath::new(cwd) else {
                continue;
            };
            if let Some(pane) = snapshot
                .workspaces
                .get_mut(saved.workspace)
                .and_then(|workspace| workspace.layout.pane_mut(saved.pane))
            {
                pane.cwd = cwd;
            }
        }
    }
}

/// Capture the current app state into a serializable snapshot, refreshing each
/// runtime's cwd now. A save uses [`capture_deferred`] instead.
pub fn capture(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    host_theme: shepr_term::host::TerminalTheme,
) -> Result<SessionSnapshot, super::SaveError> {
    let (mut snapshot, cwds, _) =
        capture_deferred(workspaces, terminal_runtimes, fallback_cwd, host_theme)?;
    cwds.resolve(&mut snapshot);
    Ok(snapshot)
}

/// Capture the current app state without reading any shell's /proc cwd: the
/// snapshot holds each pane's best known cwd, [`PendingCwds`] refreshes it where
/// the snapshot is written, and the map keys each saved pane to its live pane.
fn capture_deferred(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    host_theme: shepr_term::host::TerminalTheme,
) -> Result<(SessionSnapshot, PendingCwds, HashMap<SavedPaneRef, PaneId>), super::SaveError> {
    capture_deferred_with_workspace_capture(
        workspaces,
        terminal_runtimes,
        fallback_cwd,
        host_theme,
        capture_workspace,
    )
}

fn capture_deferred_with_workspace_capture(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    host_theme: shepr_term::host::TerminalTheme,
    mut capture_one: impl FnMut(
        usize,
        &Workspace,
        &PaneRuntimeRegistry,
        &AbsolutePath,
        &mut PendingCwds,
        &mut HashMap<SavedPaneRef, PaneId>,
    ) -> Result<WorkspaceSnapshot, super::SaveError>,
) -> Result<(SessionSnapshot, PendingCwds, HashMap<SavedPaneRef, PaneId>), super::SaveError> {
    let mut cwds = PendingCwds::default();
    let mut pane_ids = HashMap::new();
    let mut captured = Vec::with_capacity(workspaces.len());
    let mut captured_ids = Vec::with_capacity(workspaces.len());
    for workspace in workspaces.iter() {
        // Capture fails as a whole on inconsistency, so this is also the
        // workspace's position in the set.
        let snapshot_index = captured.len();
        let saved = capture_one(
            snapshot_index,
            workspace,
            terminal_runtimes,
            fallback_cwd,
            &mut cwds,
            &mut pane_ids,
        )?;
        captured_ids.push(workspace.id());
        captured.push(saved);
    }
    // The bookmark is a position in the set; the snapshot names it by where
    // that workspace was saved.
    let active = workspaces
        .bookmark()
        .and_then(|bookmark| captured_ids.iter().position(|id| *id == bookmark));
    let snapshot = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: host_theme.into(),
        workspaces: captured,
        active,
    };
    Ok((snapshot, cwds, pane_ids))
}

/// One workspace as saved, read through `Workspace` and its tree's read
/// methods. A disagreement is an error for the whole save: writing the other
/// workspaces would make their snapshot silently replace this workspace's
/// last good on-disk layout.
fn capture_workspace(
    workspace_index: usize,
    ws: &Workspace,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &AbsolutePath,
    cwds: &mut PendingCwds,
    pane_ids: &mut HashMap<SavedPaneRef, PaneId>,
) -> Result<WorkspaceSnapshot, super::SaveError> {
    let tree = ws.tree();
    let number_of = |pane| tree.pane(pane).map(PaneRecord::number);
    let (Some(focused), Some(root_pane)) = (number_of(tree.focused()), number_of(tree.root()))
    else {
        return Err(super::SaveError::CaptureInconsistent {
            workspace: ws.id().to_string(),
            detail: "focus or root pane has no record",
        });
    };
    let shape = tree.map_shape(|pane, record| {
        let number = record.number();
        let pane_ref = SavedPaneRef {
            workspace: workspace_index,
            pane: number,
        };
        let terminal = record.terminal();
        pane_ids.insert(pane_ref, pane);
        let runtime = terminal_runtimes.get(&pane);
        let cwd = crate::workspace::terminal_cwd(
            runtime,
            Some(terminal),
            crate::workspace::CwdPurpose::Save,
        )
        .unwrap_or_else(|| fallback_cwd.clone());
        if let Some(runtime) = runtime {
            cwds.probes.push((pane_ref, runtime.cwd_probe()));
        }
        PaneSnapshot {
            cwd,
            public_number: number,
            label: terminal.manual_label_value().cloned(),
            agent_session: terminal
                .ownership()
                .current_session_identity_for_persistence(),
            unusable_agent_session: None,
        }
    });
    let Some(shape) = shape else {
        return Err(super::SaveError::CaptureInconsistent {
            workspace: ws.id().to_string(),
            detail: "layout and pane records disagree",
        });
    };
    Ok(WorkspaceSnapshot {
        id: ws.id(),
        name: ws.name_label().clone(),
        next_public_pane_number: tree.next_number(),
        layout: LayoutSnapshot::from_shape(shape),
        zoomed: tree.zoomed(),
        focused,
        root_pane,
    })
}

/// Captures cwd probes for a previously captured session layout. A probe keeps
/// the best known cwd if its child has exited, and live panes keep their
/// checkpoint workspace and pane keys even if removals changed workspace indexes.
fn capture_pending_cwds_for_snapshot(
    snapshot: &SessionSnapshot,
    pane_ids: &HashMap<SavedPaneRef, PaneId>,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> Option<PendingCwds> {
    let mut cwds = PendingCwds::default();
    for (workspace_index, workspace) in snapshot.workspaces.iter().enumerate() {
        for pane in workspace.layout.panes() {
            let saved = SavedPaneRef {
                workspace: workspace_index,
                pane: pane.public_number,
            };
            let pane = pane_ids.get(&saved)?;
            if let Some(runtime) = terminal_runtimes.get(pane) {
                cwds.probes.push((saved, runtime.cwd_probe()));
            }
        }
    }
    Some(cwds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneRuntimeRegistry;
    use crate::terminal::TerminalState;
    use crate::workspace::{Workspace, WorkspaceSet};

    fn set_of(workspace: Workspace) -> WorkspaceSet {
        WorkspaceSet::restored(
            crate::workspace::WorkspaceIdAllocator::new(),
            vec![workspace],
            None,
        )
    }

    fn json(snapshot: &SessionSnapshot) -> serde_json::Value {
        serde_json::to_value(snapshot).expect("a snapshot serializes")
    }

    fn capture_of(workspaces: &WorkspaceSet) -> SessionCapture {
        capture_job(
            workspaces,
            &PaneRuntimeRegistry::default(),
            &AbsolutePath::root(),
            Default::default(),
        )
        .expect("fixture workspace trees capture consistently")
    }

    #[test]
    fn a_session_without_workspaces_captures_a_clear_and_keeps_no_layout() {
        let empty = WorkspaceSet::restored(
            crate::workspace::WorkspaceIdAllocator::new(),
            Vec::new(),
            None,
        );
        let (job, layout) = capture_of(&empty).into_job_with_layout();
        assert!(matches!(job, PersistJob::Clear));
        assert!(layout.is_none());
    }

    #[test]
    fn a_kept_layout_recaptures_the_snapshot_it_saved() {
        let mut workspace = Workspace::test_new("kept");
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        let workspaces = set_of(workspace);
        let (job, layout) = capture_of(&workspaces).into_job_with_layout();
        let PersistJob::Save(saved) = job else {
            panic!("a session with a workspace is saved");
        };
        let layout = layout.expect("a save keeps its layout");
        assert_eq!(json(layout.snapshot()), json(&saved.snapshot));

        let Some(PersistJob::Save(again)) = layout.recapture(&PaneRuntimeRegistry::default())
        else {
            panic!("every saved pane has its identity");
        };
        assert_eq!(json(&again.snapshot), json(&saved.snapshot));
    }

    #[test]
    fn a_layout_missing_a_pane_identity_recaptures_nothing() {
        let workspaces = set_of(Workspace::test_new("unpaired"));
        let (_, layout) = capture_of(&workspaces).into_job_with_layout();
        let layout = layout.expect("a save keeps its layout");
        let unpaired = CapturedLayout::new(layout.snapshot().clone(), HashMap::new());
        assert!(
            unpaired
                .recapture(&PaneRuntimeRegistry::default())
                .is_none()
        );
    }

    #[test]
    fn one_inconsistent_workspace_fails_the_whole_capture() {
        let first = Workspace::test_new("consistent");
        let broken = Workspace::test_new("inconsistent");
        let broken_id = broken.id();
        let workspaces = WorkspaceSet::restored(
            crate::workspace::WorkspaceIdAllocator::new(),
            vec![first, broken],
            None,
        );

        // Workspace constructors deliberately preserve the tree invariant, so
        // the failure `capture_workspace` reports is injected at the seam for
        // one workspace only. The others still capture; the job must not be
        // a save of the remaining workspaces, which would replace the last
        // good file without the broken one.
        let failed = capture_job_with_workspace_capture(
            &workspaces,
            &PaneRuntimeRegistry::default(),
            &AbsolutePath::root(),
            Default::default(),
            |index, workspace, runtimes, fallback, cwds, pane_ids| {
                if workspace.id() == broken_id {
                    return Err(super::super::SaveError::CaptureInconsistent {
                        workspace: workspace.id().to_string(),
                        detail: "layout and pane records disagree",
                    });
                }
                capture_workspace(index, workspace, runtimes, fallback, cwds, pane_ids)
            },
        );
        assert!(matches!(
            failed,
            Err(super::super::SaveError::CaptureInconsistent { .. })
        ));
    }

    #[test]
    fn invalid_live_hook_authority_falls_back_to_persisted_agent_session() {
        use shepr_agent::resume::AgentSessionRef;
        for (source, label, live_ref) in [
            (
                "shepr:codex",
                "codex",
                AgentSessionRef::path("/codex-session").expect("test session ref"),
            ),
            (
                "shepr:claude",
                "claude",
                AgentSessionRef::path("/session.jsonl").expect("test session ref"),
            ),
        ] {
            let saved_ref = shepr_agent::resume::AgentSessionRef::id("saved-session")
                .expect("test session ref");
            let mut terminal = TerminalState::new(AbsolutePath::root());
            terminal.seed_hook_authority_for_test(Some(crate::terminal::state::HookAuthority {
                origin: shepr_agent::ReportOrigin::parse(source, label).expect("test origin"),
                state: shepr_agent::AgentState::Working,
                reported_at: std::time::Instant::now(),
                session_ref: Some(live_ref),
            }));
            let expected = shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::new(shepr_agent::IntegrationTarget::Claude),
                shepr_agent::Agent::Claude,
                saved_ref.clone(),
            )
            .expect("test session is valid");
            terminal
                .ownership_mut()
                .set_persisted_agent_session(expected.clone());
            let workspace = Workspace::test_from_pane(
                crate::workspace::test_workspace_id(),
                Some("snapshot-session-fallback".into()),
                &AbsolutePath::root(),
                shepr_core::layout::PaneId::alloc(),
                terminal,
            );

            let snapshot = capture(
                &set_of(workspace),
                &PaneRuntimeRegistry::default(),
                &AbsolutePath::root(),
                Default::default(),
            )
            .expect("a one-pane workspace captures");

            let LayoutSnapshot::Pane(saved) = &snapshot.workspaces[0].layout else {
                panic!("a one-pane workspace saves one leaf");
            };
            assert_eq!(saved.agent_session.as_ref(), Some(&expected));
        }
    }
}
