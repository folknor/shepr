//! Capture of the live session: the structural snapshot, the cwd probes and
//! the history handles, read from workspaces and pane runtimes on the event
//! loop and handed to whoever writes them.

use std::collections::HashMap;
use std::path::Path;

use crate::pane::PaneRuntimeRegistry;
use crate::workspace::{PaneRecord, Workspace, WorkspaceSet};
use shepr_core::absolute_path::AbsolutePath;
use shepr_core::layout::PaneId;
use shepr_protocol::PanePublicNumber;

use super::actor::{PersistJob, SessionBundle};
use super::history::{PendingHistory, PendingPaneHistory};
use super::schema::{
    LayoutSnapshot, PaneSnapshot, SNAPSHOT_VERSION, SessionSnapshot, WorkspaceSnapshot,
};

/// Captures the current session, clearing its saved state when no workspace
/// remains and otherwise writing one structural snapshot with optional pane
/// history. The returned pane index uses the same pane keys as the snapshot.
pub fn capture_job(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &Path,
    host_theme: shepr_term::host::TerminalTheme,
    persist_pane_history: bool,
) -> (PersistJob, HashMap<SavedPaneRef, PaneId>) {
    if workspaces.is_empty() {
        return (PersistJob::Clear, HashMap::new());
    }
    let (snapshot, cwds, pane_ids) =
        capture_deferred(workspaces, terminal_runtimes, fallback_cwd, host_theme);
    let history =
        persist_pane_history.then(|| capture_pending_history(workspaces, terminal_runtimes));
    (
        PersistJob::Save(SessionBundle {
            snapshot,
            cwds,
            history,
        }),
        pane_ids,
    )
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
    fallback_cwd: &std::path::Path,
    host_theme: shepr_term::host::TerminalTheme,
) -> SessionSnapshot {
    let (mut snapshot, cwds, _) =
        capture_deferred(workspaces, terminal_runtimes, fallback_cwd, host_theme);
    cwds.resolve(&mut snapshot);
    snapshot
}

/// Capture the current app state without reading any shell's /proc cwd: the
/// snapshot holds each pane's best known cwd, [`PendingCwds`] refreshes it where
/// the snapshot is written, and the map keys each saved pane to its live pane.
pub fn capture_deferred(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    host_theme: shepr_term::host::TerminalTheme,
) -> (SessionSnapshot, PendingCwds, HashMap<SavedPaneRef, PaneId>) {
    let mut cwds = PendingCwds::default();
    let mut pane_ids = HashMap::new();
    let mut captured = Vec::with_capacity(workspaces.len());
    let mut captured_ids = Vec::with_capacity(workspaces.len());
    for workspace in workspaces.iter() {
        // Keyed by its place in the snapshot, which only differs from its
        // place in the set if a workspace failed to capture.
        let snapshot_index = captured.len();
        let Some(saved) = capture_workspace(
            snapshot_index,
            workspace,
            terminal_runtimes,
            fallback_cwd,
            &mut cwds,
            &mut pane_ids,
        ) else {
            continue;
        };
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
    (snapshot, cwds, pane_ids)
}

/// One workspace as saved, read through `Workspace` and its tree's read
/// methods. `None` only if the tree's layout and records disagreed, which its
/// constructors and mutators rule out.
fn capture_workspace(
    workspace_index: usize,
    ws: &Workspace,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    cwds: &mut PendingCwds,
    pane_ids: &mut HashMap<SavedPaneRef, PaneId>,
) -> Option<WorkspaceSnapshot> {
    let Ok(fallback_cwd) = AbsolutePath::new(fallback_cwd) else {
        tracing::error!(
            workspace = %ws.id(),
            fallback = %fallback_cwd.display(),
            "the fallback cwd is not absolute; workspace not saved"
        );
        return None;
    };
    let tree = ws.tree();
    let number_of = |pane| tree.pane(pane).map(PaneRecord::number);
    let (Some(focused), Some(root_pane)) = (number_of(tree.focused()), number_of(tree.root()))
    else {
        tracing::error!(workspace = %ws.id(), "workspace focus or root has no pane record; not saved");
        return None;
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
        }
    });
    let Some(shape) = shape else {
        tracing::error!(workspace = %ws.id(), "workspace layout and pane records disagree; not saved");
        return None;
    };
    Some(WorkspaceSnapshot {
        id: ws.id(),
        custom_name: ws.custom_name().map(str::to_owned),
        next_public_pane_number: tree.next_number(),
        layout: LayoutSnapshot::from_shape(shape),
        zoomed: tree.zoomed(),
        focused,
        root_pane,
    })
}

/// The event-loop half of a history capture; see `PendingHistory`. Takes no
/// terminal lock: a live pane contributes a handle to its terminal.
pub fn capture_pending_history(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> PendingHistory {
    PendingHistory::new(
        workspaces
            .iter()
            .map(|workspace| {
                workspace
                    .tree()
                    .panes()
                    .map(|(pane, record)| {
                        let runtime = terminal_runtimes.get(&pane);
                        (record.number(), pending_pane_history(pane, runtime))
                    })
                    .collect()
            })
            .collect(),
    )
}

/// A pane's own screen supersedes its carried history only once its shell
/// launched. Until then (a chdir on a hung mount can last indefinitely) the
/// runtime holds a PTY and no shell, and a save must not trade the history
/// carried for it for that empty screen; if the launch then fails, the pane is
/// left with the carried history.
fn pending_pane_history(
    pane: PaneId,
    runtime: Option<&crate::pane::PaneRuntime>,
) -> PendingPaneHistory {
    match runtime.filter(|runtime| runtime.launched()) {
        Some(runtime) => PendingPaneHistory::Live(pane, runtime.read().history_source()),
        None => PendingPaneHistory::Runtimeless(pane),
    }
}

/// Captures fresh history handles for a previously captured session layout.
/// Panes removed since that layout was saved use the persister's carried
/// history, while panes that still have runtimes contribute their current
/// history. The pane map must have been captured with `snapshot`.
pub fn capture_pending_history_for_snapshot(
    snapshot: &SessionSnapshot,
    pane_ids: &HashMap<SavedPaneRef, PaneId>,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> Option<PendingHistory> {
    let mut workspaces = Vec::with_capacity(snapshot.workspaces.len());
    for (workspace_index, workspace) in snapshot.workspaces.iter().enumerate() {
        let mut numbers: Vec<_> = workspace
            .layout
            .panes()
            .into_iter()
            .map(|pane| pane.public_number)
            .collect();
        numbers.sort_unstable();
        let mut panes = Vec::with_capacity(numbers.len());
        for number in numbers {
            let saved = SavedPaneRef {
                workspace: workspace_index,
                pane: number,
            };
            let pane = *pane_ids.get(&saved)?;
            let runtime = terminal_runtimes.get(&pane);
            panes.push((number, pending_pane_history(pane, runtime)));
        }
        workspaces.push(panes);
    }
    Some(PendingHistory::new(workspaces))
}

/// Captures cwd probes for a previously captured session layout. A probe keeps
/// the best known cwd if its child has exited, and live panes keep their
/// checkpoint workspace and pane keys even if removals changed workspace indexes.
pub fn capture_pending_cwds_for_snapshot(
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

/// Both halves of a history capture in one call. Saves split them across the
/// event loop and the persister's thread instead.
#[cfg(test)]
pub fn capture_history(
    workspaces: &WorkspaceSet,
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &mut super::history::HistoryCarry,
) -> super::schema::SessionHistorySnapshot {
    capture_pending_history(workspaces, terminal_runtimes).resolve(carry)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
                &PaneRuntimeRegistry::new(),
                PathBuf::from("/").as_path(),
                Default::default(),
            );

            let LayoutSnapshot::Pane(saved) = &snapshot.workspaces[0].layout else {
                panic!("a one-pane workspace saves one leaf");
            };
            assert_eq!(saved.agent_session.as_ref(), Some(&expected));
        }
    }
}
