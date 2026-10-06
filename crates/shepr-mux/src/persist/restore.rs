use std::collections::{HashMap, HashSet};

use tracing::{error, warn};

use crate::pane::PaneRuntime;
use crate::terminal::{Label, PaneStartFailure, TerminalState};
use crate::workspace::{PaneTree, SavedTreeState, TreePlan, Workspace, WorkspaceChrome};
use shepr_agent::{AgentState, resume::PersistedAgentSession};
use shepr_core::absolute_path::AbsolutePath;
use shepr_core::layout::PaneId;
use shepr_protocol::{PublicPaneId, WorkspaceId};

use super::schema::PaneSnapshot;
use super::schema::{SessionSnapshot, WorkspaceSnapshot};

struct PaneRestoreStartup {
    restore_plan: Option<shepr_agent::resume::AgentResumePlan>,
    duplicate_agent_session: bool,
}

struct RestorePlanContext {
    chrome: WorkspaceChrome,
    now: std::time::Instant,
}

/// Validated state plus child launch descriptions. Building this plan requires
/// no runtime, PTY, channels, or filesystem probes.
pub(super) struct SessionRestorePlan {
    workspaces: Vec<Workspace>,
    active: Option<usize>,
    /// Every saved part dropped or repaired so far.
    damage: shepr_protocol::SessionRestoreDamage,
    launches: Vec<RestoredLaunch>,
    theme: shepr_term::host::TerminalTheme,
    now: std::time::Instant,
}

/// A running pane's launch, before the workspace around it exists to size it.
struct UnsizedLaunch {
    /// The workspace's index in the plan's `workspaces`.
    workspace: usize,
    pane_id: PaneId,
    public_id: PublicPaneId,
    saved_cwd: AbsolutePath,
    saved_label: Option<Label>,
    saved_agent_session: Option<PersistedAgentSession>,
}

impl UnsizedLaunch {
    fn sized(self, geometry: shepr_core::geometry::PaneGeometry) -> RestoredLaunch {
        RestoredLaunch {
            workspace: self.workspace,
            pane_id: self.pane_id,
            public_id: self.public_id,
            geometry,
            saved_cwd: self.saved_cwd,
            saved_label: self.saved_label,
            saved_agent_session: self.saved_agent_session,
        }
    }
}

struct RestoredLaunch {
    /// The workspace's index in the plan's `workspaces`.
    workspace: usize,
    pane_id: PaneId,
    public_id: PublicPaneId,
    geometry: shepr_core::geometry::PaneGeometry,
    saved_cwd: AbsolutePath,
    saved_label: Option<Label>,
    saved_agent_session: Option<PersistedAgentSession>,
}

impl SessionRestorePlan {
    pub(super) fn launch(mut self, launcher: &crate::pane::PaneLauncher) -> RestoredSession {
        let mut terminal_runtimes = HashMap::new();
        for launch in self.launches {
            let result = launcher.launch(crate::pane::PaneLaunchRequest {
                pane_id: launch.pane_id,
                public_id: launch.public_id,
                geometry: launch.geometry,
                cwd: &launch.saved_cwd,
                kind: crate::pane::LaunchKind::Restored,
                presentation: crate::pane::LaunchPresentation::Saved(self.theme),
            });
            match result {
                Ok(runtime) => {
                    terminal_runtimes.insert(launch.pane_id, runtime);
                }
                Err(err) => {
                    error!(
                        pane = %launch.public_id,
                        pane_id = %launch.pane_id,
                        error = %err,
                        "failed to restore pane"
                    );
                    // The planned terminal is replaced, in place, by one that
                    // keeps the saved state verbatim, including a saved agent
                    // session a running duplicate would have withdrawn. Only a
                    // pane with a resume plan reserves its session, and such a
                    // pane has no launch here, so the resumed-session set
                    // needs no rollback.
                    let terminal = restored_terminal(
                        &launch.saved_cwd,
                        launch.saved_label.as_ref(),
                        launch.saved_agent_session.as_ref(),
                        RestoredPaneStart::Unavailable(PaneStartFailure::shell_start_failed(&err)),
                        self.now,
                    );
                    let record = self
                        .workspaces
                        .get_mut(launch.workspace)
                        .and_then(|workspace| workspace.pane_mut(launch.pane_id));
                    match record {
                        Some(record) => record.replace_terminal(terminal),
                        None => error!(
                            pane = %launch.public_id,
                            "a pane whose launch failed is not in its restored workspace"
                        ),
                    }
                }
            }
        }
        RestoredSession {
            workspaces: self.workspaces,
            terminal_runtimes,
            active: self.active,
            restore_loss: (!self.damage.is_empty()).then_some(self.damage),
        }
    }
}

/// Everything a restore produces. Restore can drop saved workspaces (invalid
/// pane tree, or an unassignable duplicate ID), so saved indices into that
/// list no longer name the same item; `active` is already remapped onto
/// `workspaces` and must be used as it is, not re-derived from the snapshot by
/// clamping.
pub(super) struct RestoredSession {
    pub(super) workspaces: Vec<Workspace>,
    pub(super) terminal_runtimes: HashMap<PaneId, PaneRuntime>,
    /// The saved bookmarked workspace as an index into `workspaces`; if it was
    /// dropped, its nearest surviving neighbour. `None` if nothing was
    /// bookmarked or nothing survived.
    pub(super) active: Option<usize>,
    /// What saved data restore dropped or repaired, never empty when present.
    /// The caller preserves the source session file whenever it is present.
    pub(super) restore_loss: Option<shepr_protocol::SessionRestoreDamage>,
}

/// How a restored pane comes back. Every saved field is carried forward the
/// same way for all of them (`restored_terminal`); only what this decides
/// differs.
enum RestoredPaneStart {
    /// A fresh shell is running for the pane. `duplicate_agent_session`: the
    /// pane's saved agent session is resumed by an earlier pane of this
    /// restore, which owns it now. A named bool: the variant has no other
    /// field to swap it with, and its consumers read it by name.
    Running { duplicate_agent_session: bool },
    /// The pane waits for the event loop to type its agent's resume command
    /// into a fresh shell.
    PendingResume(shepr_agent::resume::AgentResumePlan),
    /// Nothing could be started (the reason is shown in the pane). The pane
    /// keeps its saved state verbatim so the next start can try again.
    Unavailable(PaneStartFailure),
}

// Plain validated data only: constructing these never launches children or
// reserves agent sessions. The whole snapshot is planned before execution.
struct WorkspaceRestorePlan<'a> {
    snapshot: &'a WorkspaceSnapshot,
    identity_cwd: AbsolutePath,
    /// The panes' tree, numbers and focus admitted.
    plan: TreePlan<&'a PaneSnapshot>,
}

/// Plan the complete saved session before any child is launched.
/// `workspace_ids` is the allocator of the workspace set the restored
/// workspaces join (`WorkspaceSet::restored` takes it over): it is moved past
/// every saved ID first, and a repeated saved ID takes a fresh one from it
/// when the number space permits; otherwise that duplicate workspace is
/// dropped.
pub(super) fn plan_restore(
    snapshot: &SessionSnapshot,
    chrome: WorkspaceChrome,
    resume_agents_on_restore: bool,
    now: std::time::Instant,
    workspace_ids: &mut crate::workspace::WorkspaceIdAllocator,
) -> SessionRestorePlan {
    let mut workspaces = Vec::new();
    let mut launches = Vec::new();
    let mut resumed_agent_sessions = HashSet::new();
    let host_theme = snapshot.host_theme.to_theme();
    // Where each saved workspace ended up, `None` for a dropped one.
    let mut restored_index = Vec::with_capacity(snapshot.workspaces.len());
    let plans: Vec<_> = snapshot.workspaces.iter().map(plan_workspace).collect();
    let mut damage = shepr_protocol::SessionRestoreDamage::default();
    // Before any allocation below, so a fresh ID is never one a saved
    // workspace owns.
    workspace_ids.reserve(snapshot.workspaces.iter().map(|ws| &ws.id));
    let mut used_ids = HashSet::new();
    let plan_context = RestorePlanContext { chrome, now };
    for (plan, saved) in plans.into_iter().zip(&snapshot.workspaces) {
        let saved_id = saved.id;
        let Some(plan) = plan else {
            damage.dropped_workspaces += 1;
            restored_index.push(None);
            continue;
        };
        let Some(workspace_id) = restored_workspace_id(saved_id, &mut used_ids, workspace_ids)
        else {
            // A duplicate at the end of the reserved number space has no
            // collision-free replacement. Keep the earlier workspace and drop
            // this one whole, counted like any other dropped workspace.
            warn!(
                workspace = %saved_id,
                "dropping saved workspace: duplicate ID has no available replacement"
            );
            damage.dropped_workspaces += 1;
            restored_index.push(None);
            continue;
        };
        let restored = restore_workspace(
            plan,
            workspace_id,
            workspaces.len(),
            &plan_context,
            resume_agents_on_restore.then_some(&mut resumed_agent_sessions),
        );
        if let Some((workspace, restored_launches, dropped_sessions)) = restored {
            if workspace_id != saved_id {
                damage.renamed_workspaces += 1;
                warn!(
                    workspace = %saved_id,
                    replacement = %workspace_id,
                    "reassigned duplicate saved workspace ID"
                );
            }
            damage.dropped_agent_sessions.extend(dropped_sessions);
            launches.extend(restored_launches);
            restored_index.push(Some(workspaces.len()));
            workspaces.push(workspace);
        } else {
            damage.dropped_workspaces += 1;
            restored_index.push(None);
        }
    }
    let active = snapshot.active.and_then(|active| {
        if active >= restored_index.len() {
            damage.repaired_bookmarks += 1;
            warn!(
                saved_index = active,
                saved_workspaces = restored_index.len(),
                "repairing out-of-range saved workspace bookmark"
            );
        }
        remap_saved_index(active, &restored_index)
    });
    SessionRestorePlan {
        workspaces,
        active,
        damage,
        launches,
        theme: host_theme,
        now,
    }
}

/// Where a saved index into a list lands once restore has dropped some of
/// its items. `restored[i]` is the new index of saved item `i`, `None` if it
/// was dropped. A surviving item keeps pointing at itself; a dropped one
/// resolves to the nearest survivor after it, else the nearest before it
/// (what closing the item would have selected). An index past the end of the
/// saved list (a hand-edited file) resolves to the last survivor. `None` only
/// when nothing survived.
fn remap_saved_index(saved: usize, restored: &[Option<usize>]) -> Option<usize> {
    let split = saved.min(restored.len());
    restored[split..]
        .iter()
        .flatten()
        .next()
        .or_else(|| restored[..split].iter().rev().flatten().next())
        .copied()
}

/// The ID a restored workspace gets: its saved ID (decoding admits only
/// canonical ones), unless an earlier workspace of the same file already took
/// it, in which case a fresh one if the caller's reservation left one. The
/// caller reserved every saved ID before restoring, so a fresh ID is past all
/// of them.
fn restored_workspace_id(
    saved: WorkspaceId,
    used_ids: &mut HashSet<WorkspaceId>,
    workspace_ids: &mut crate::workspace::WorkspaceIdAllocator,
) -> Option<WorkspaceId> {
    if used_ids.insert(saved) {
        return Some(saved);
    }
    workspace_ids.try_allocate()
}

/// The terminal state of one restored pane. Every saved `PaneSnapshot` field
/// is carried forward here, once, whichever way the pane comes back; `start`
/// only decides the parts that genuinely differ:
///
/// - cwd, label and launch argv: always kept.
/// - agent session: always kept, except by a running duplicate whose session
///   an earlier pane of this restore resumes.
fn restored_terminal(
    cwd: &AbsolutePath,
    label: Option<&Label>,
    agent_session: Option<&PersistedAgentSession>,
    start: RestoredPaneStart,
    now: std::time::Instant,
) -> TerminalState {
    let mut terminal = TerminalState::new(cwd.clone());
    if let Some(label) = label {
        terminal.set_manual_label(label.clone());
    }
    let duplicate_agent_session = matches!(
        start,
        RestoredPaneStart::Running {
            duplicate_agent_session: true
        }
    );
    if let Some(session) = restored_terminal_agent_session(agent_session, duplicate_agent_session) {
        terminal
            .ownership_mut()
            .set_persisted_agent_session(session);
    }
    match start {
        RestoredPaneStart::Running { .. } => {}
        RestoredPaneStart::PendingResume(plan) => {
            let plan_agent = plan.agent();
            terminal = terminal.with_pending_agent_resume_plan(plan);
            // Seeded so the sidebar shows the agent while its resume waits to
            // launch and while the resumed process starts. Once the shell
            // runs, the pane's detector holds back its "no agent" report
            // (`withhold_agent_absence` in `pane/agent_detection.rs`) until
            // it identifies the agent, which replaces the seed without a gap,
            // or until the hold expires. The seed does not outlive a failed
            // resume: at expiry the detector publishes a no-agent `Unknown`
            // update, which withdraws it. A resume that can never launch
            // leaves no detector, so abandoning it withdraws the seed
            // (`abandon_agent_resume`).
            let _ = terminal
                .ownership_mut()
                .set_detected_state_with_screen_signals_at(
                    Some(plan_agent),
                    AgentState::Idle,
                    false,
                    false,
                    now,
                );
        }
        RestoredPaneStart::Unavailable(reason) => {
            warn!(
                cwd = %cwd.display(),
                reason = ?reason,
                "preserving unavailable restored pane"
            );
            terminal.record_start_failure(reason);
        }
    }
    terminal
}

/// One saved workspace, or `None` when a layout defect leaves nothing usable
/// to restore.
///
/// A file that does not match the saved schema (a missing key, a wrong type,
/// a layout leaf without its pane record) never gets here: it fails to parse
/// and is refused whole, backed up as unusable. The snapshot types are part of
/// that schema: a workspace ID decodes only from its canonical spelling, a
/// pane number or next pane number only as nonzero, a split ratio only
/// when finite and within bounds, and a pane cwd only as an absolute path,
/// exactly what a shepr save writes. What does
/// get here parsed, so its defects are values the snapshot types can hold but
/// a restore cannot use, such as pane numbers that collide
/// or reach the next number. Such a defect drops this one workspace,
/// rather than refusing the whole session (which would lose every healthy
/// workspace for one bad value) or repairing it (which silently rewrites a
/// corrupt file). The workspace is not lost on disk: the workspace-drop case
/// in `RestoredSession::restore_loss` makes the first save back the original
/// file up before overwriting it. A saved pane label must decode as a
/// nonempty `Label` with no surrounding whitespace; empty or padded text fails
/// this strict schema and refuses the whole file before planning.
fn plan_workspace(snapshot: &WorkspaceSnapshot) -> Option<WorkspaceRestorePlan<'_>> {
    // A saved cwd is an `AbsolutePath`, so a relative one (which would resolve
    // against the server's own working directory in splits and cwd following)
    // never gets here: the file that held it failed to parse. A directory that
    // went missing is not a schema fact and stays the child's chdir to judge.
    let shape = snapshot.layout.to_shape();
    let saved = SavedTreeState {
        focus: snapshot.focused,
        root: snapshot.root_pane,
        zoomed: snapshot.zoomed,
        next_number: snapshot.next_public_pane_number,
    };
    let plan = match PaneTree::plan(shape, |pane| pane.public_number, saved) {
        Ok(plan) => plan,
        Err(rejection) => {
            warn!(
                workspace = %snapshot.id,
                ?rejection,
                "dropping saved workspace with invalid saved pane tree"
            );
            return None;
        }
    };
    let identity_cwd = plan.root_leaf().cwd.clone();
    Some(WorkspaceRestorePlan {
        snapshot,
        identity_cwd,
        plan,
    })
}

/// Builds the state of one planned workspace and the launches its shell panes
/// need; nothing is launched here. Every saved-file defect was found while
/// planning, before any agent session was reserved; what can still refuse
/// the workspace guards internal invariants only. Also returns the panes
/// whose saved agent session this build cannot use: each is restored as a
/// plain shell and its session dropped, logged here once.
/// `workspace_index` is where the workspace will sit in the plan's list.
fn restore_workspace(
    plan: WorkspaceRestorePlan<'_>,
    workspace_id: WorkspaceId,
    workspace_index: usize,
    plan_context: &RestorePlanContext,
    mut resumed_agent_sessions: Option<&mut HashSet<shepr_agent::resume::AgentResumeKey>>,
) -> Option<(Workspace, Vec<RestoredLaunch>, Vec<PublicPaneId>)> {
    let WorkspaceRestorePlan {
        snapshot,
        identity_cwd,
        plan,
    } = plan;
    let mut launches = Vec::new();
    let mut dropped_sessions = Vec::new();
    let built = plan.build(|pane_id, saved| {
        if let Some(unusable) = &saved.unusable_agent_session {
            let public_id = PublicPaneId::new(&workspace_id, saved.public_number);
            warn!(
                workspace = %workspace_id,
                pane = %public_id,
                agent = unusable.agent.as_deref().unwrap_or("unknown"),
                error = %unusable.error,
                "dropping saved agent session this build cannot use; the pane restores as a plain shell"
            );
            dropped_sessions.push(public_id);
        }
        // Nothing here looks at the saved directory: restore runs on the
        // server's startup path, and a stat of a directory on a hung mount
        // would hold the server before it serves anyone. The pane's launch
        // enters it by chdir (never falling back), and a directory that is
        // gone or unreadable settles that launch as a placeholder pane.
        let PaneRestoreStartup {
            restore_plan,
            duplicate_agent_session,
        } = pane_restore_startup(saved.agent_session.as_ref(), resumed_agent_sessions.as_deref_mut());

        if let Some(resume) = restore_plan {
            return restored_terminal(
                &saved.cwd,
                saved.label.as_ref(),
                saved.agent_session.as_ref(),
                RestoredPaneStart::PendingResume(resume),
                plan_context.now,
            );
        }

        // Planned as running; a launch that fails replaces this terminal with
        // an unavailable one under the same id (`SessionRestorePlan::launch`).
        // No detected-agent seeding: a pane with a resume plan took the
        // deferred branch above, so this shell has no agent until detection
        // or a hook reports one.
        let terminal = restored_terminal(
            &saved.cwd,
            saved.label.as_ref(),
            saved.agent_session.as_ref(),
            RestoredPaneStart::Running {
                duplicate_agent_session,
            },
            plan_context.now,
        );
        launches.push(UnsizedLaunch {
            workspace: workspace_index,
            pane_id,
            public_id: PublicPaneId::new(&workspace_id, saved.public_number),
            saved_cwd: saved.cwd.clone(),
            saved_label: saved.label.clone(),
            saved_agent_session: saved.agent_session.clone(),
        });
        terminal
    });
    let tree = match built {
        Ok(tree) => tree,
        Err(rejection) => {
            // Fresh IDs and a resolved focus rule this out, and planning
            // already refused every defect the saved data can have.
            error!(
                workspace = %workspace_id,
                ?rejection,
                "a planned workspace failed to build; dropping it"
            );
            return None;
        }
    };

    // Restore runs before any client has attached, so there is no cell
    // size to give the shell; the first client geometry pass supplies it.
    let spawn_sizes = plan_context
        .chrome
        .resume_panes(tree.layout(), tree.zoomed());
    let launches = launches
        .into_iter()
        .map(|launch| {
            let grid = match spawn_sizes
                .iter()
                .find(|pane| pane.chrome.id == launch.pane_id)
            {
                Some(pane) => shepr_core::geometry::PaneGeometry::with_cell(
                    pane.content.width,
                    pane.content.height,
                    None,
                ),
                None => {
                    crate::workspace::spawn_geometry(plan_context.chrome.sole_pane_size(), None)
                }
            };
            launch.sized(grid)
        })
        .collect();
    // The schema already refused a blank or padded name, with the whole file.
    let workspace = Workspace::from_tree(
        workspace_id,
        Some(snapshot.name.clone()),
        identity_cwd,
        tree,
    );
    Some((workspace, launches, dropped_sessions))
}

fn pane_restore_startup(
    session: Option<&PersistedAgentSession>,
    resumed_sessions: Option<&mut HashSet<shepr_agent::resume::AgentResumeKey>>,
) -> PaneRestoreStartup {
    let Some(resumed_sessions) = resumed_sessions else {
        return PaneRestoreStartup {
            restore_plan: None,
            duplicate_agent_session: false,
        };
    };
    let restore_plan = session.map(PersistedAgentSession::resume_plan);
    // Reserve the session so later panes in the same restore pass cannot
    // launch the same native agent session. A reserving pane always defers its
    // launch, so no restore-time spawn failure can leave a stale reservation.
    let duplicate_agent_session = restore_plan
        .as_ref()
        .is_some_and(|plan| !resumed_sessions.insert(plan.key().clone()));
    // The duplicate is accidental saved state: nothing resumes in this pane,
    // which starts as a plain shell.
    let restore_plan = restore_plan.filter(|_| !duplicate_agent_session);

    PaneRestoreStartup {
        restore_plan,
        duplicate_agent_session,
    }
}

fn restored_terminal_agent_session(
    session: Option<&PersistedAgentSession>,
    duplicate_agent_session: bool,
) -> Option<PersistedAgentSession> {
    if duplicate_agent_session {
        return None;
    }
    session.cloned()
}

#[cfg(test)]
use crate::events::AppEvent;
#[cfg(test)]
use crate::render_signal::RenderSignal;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::{Notify, mpsc};

#[cfg(test)]
fn restore(
    snapshot: &SessionSnapshot,
    chrome: WorkspaceChrome,
    scrollback_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    socket_path: &std::path::Path,
    resume_agents_on_restore: bool,
    events: &mpsc::Sender<AppEvent>,
    render_notify: &Arc<Notify>,
    render_dirty: &Arc<RenderSignal>,
    pane_teardowns: &Arc<crate::pane::PaneTeardownTracker>,
    now: std::time::Instant,
) -> RestoredSession {
    let launcher = crate::pane::PaneLauncher::new(
        crate::pane::PaneSpawnHandles {
            events: events.clone(),
            render_notify: Arc::clone(render_notify),
            render_dirty: Arc::clone(render_dirty),
            pane_teardowns: Arc::clone(pane_teardowns),
            socket_path: socket_path.to_path_buf(),
        },
        shell_config,
        shepr_core::scrollback::ScrollbackBudget::new(scrollback_bytes),
        None,
    );
    plan_restore(
        snapshot,
        chrome,
        resume_agents_on_restore,
        now,
        &mut crate::workspace::WorkspaceIdAllocator::new(),
    )
    .launch(&launcher)
}

#[cfg(test)]
mod tests {
    use shepr_test_support::fixture::resolved_shell as test_shell;
    use std::path::{Path, PathBuf};

    use super::super::schema::{
        DirectionSnapshot, LayoutSnapshot, SNAPSHOT_VERSION, SavedHostTheme,
    };
    use super::*;
    use crate::workspace::{PaneRecord, WorkspaceSet};
    use shepr_protocol::PanePublicNumber;

    std::thread_local! {
        static RESTORE_TEST_SCRATCH: crate::test_support::ScratchDir =
            crate::test_support::ScratchDir::new("restore-test-paths");
    }

    fn test_split_ratio(value: f32) -> shepr_core::layout::SplitRatio {
        shepr_core::layout::SplitRatio::new(value).expect("test split ratio is valid")
    }

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("nonzero literal")
    }

    fn persisted_test_session(
        source: &str,
        agent: shepr_agent::Agent,
        session_ref: shepr_agent::resume::AgentSessionRef,
    ) -> shepr_agent::resume::PersistedAgentSession {
        shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse(source).expect("bundled test source"),
            agent,
            session_ref,
        )
        .expect("test session is valid")
    }

    fn test_restore_now() -> std::time::Instant {
        std::time::Instant::now()
    }

    /// A resolved server socket for restored test panes; nothing listens on it.
    const TEST_SOCKET: &str = "/run/user/1000/shepr-test.sock";

    fn restore_test_path(name: &str) -> PathBuf {
        RESTORE_TEST_SCRATCH.with(|scratch| scratch.join(name))
    }

    /// A saved cwd from a path the test knows is absolute.
    fn abs(path: impl Into<PathBuf>) -> AbsolutePath {
        AbsolutePath::new(path).expect("test cwd is absolute")
    }

    fn test_session_path(name: &str) -> String {
        restore_test_path(name).display().to_string()
    }

    fn test_restore_shell() -> &'static str {
        shepr_test_support::fixture::idle_shell()
    }

    /// Workspaces laid out in `rows` by `cols` cells without scrollbars, so a
    /// workspace's only pane has that size less its border cells.
    fn test_geometry(rows: u16, cols: u16) -> WorkspaceChrome {
        WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, cols, rows),
            pane_gaps: false,
            pane_scrollbars: false,
        }
    }

    /// A pane snapshot whose saved directory does not exist.
    fn pane_snapshot(public_number: usize) -> PaneSnapshot {
        let cwd = restore_test_path("__shepr_missing_restore_directory__");
        assert!(!cwd.try_exists().expect("test stat"));
        PaneSnapshot {
            cwd: abs(cwd),
            public_number: number(public_number),
            label: None,
            agent_session: None,
            unusable_agent_session: None,
        }
    }

    fn leaf(public_number: usize) -> LayoutSnapshot {
        LayoutSnapshot::Pane(pane_snapshot(public_number))
    }

    /// A workspace restore drops: its two panes share one public number.
    fn rejected_workspace(id: &str, name: &str, public_number: usize) -> WorkspaceSnapshot {
        workspace_snapshot(id, name, split(leaf(public_number), leaf(public_number)))
    }

    fn split(first: LayoutSnapshot, second: LayoutSnapshot) -> LayoutSnapshot {
        LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: test_split_ratio(0.5),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// A workspace of `layout`; focus and root are its first pane and its
    /// next number follows the highest.
    fn workspace_snapshot(id: &str, name: &str, layout: LayoutSnapshot) -> WorkspaceSnapshot {
        let numbers: Vec<_> = layout
            .panes()
            .into_iter()
            .map(|pane| pane.public_number)
            .collect();
        let first = numbers[0];
        let highest = numbers.iter().map(|number| number.get()).max().unwrap_or(1);
        WorkspaceSnapshot {
            id: id.parse().expect("canonical workspace ID"),
            name: crate::terminal::Label::new(name).expect("test workspace name"),
            next_public_pane_number: number(highest + 1),
            layout,
            zoomed: false,
            focused: first,
            root_pane: first,
        }
    }

    fn one_pane_workspace(id: &str, name: &str, public_number: usize) -> WorkspaceSnapshot {
        workspace_snapshot(id, name, leaf(public_number))
    }

    fn session(workspaces: Vec<WorkspaceSnapshot>, active: Option<usize>) -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces,
            active,
        }
    }

    fn only_pane(workspace: &WorkspaceSnapshot) -> &PaneSnapshot {
        let LayoutSnapshot::Pane(pane) = &workspace.layout else {
            panic!("a one-pane workspace saves one leaf");
        };
        pane
    }

    fn only_pane_mut(workspace: &mut WorkspaceSnapshot) -> &mut PaneSnapshot {
        let LayoutSnapshot::Pane(pane) = &mut workspace.layout else {
            panic!("a one-pane workspace saves one leaf");
        };
        pane
    }

    fn root_terminal(workspace: &Workspace) -> &TerminalState {
        terminal_of(workspace, workspace.tree().root())
    }

    fn terminal_of(workspace: &Workspace, pane: PaneId) -> &TerminalState {
        workspace
            .tree()
            .pane(pane)
            .expect("the pane is in the workspace")
            .terminal()
    }

    fn numbers_in_layout_order(workspace: &Workspace) -> Vec<usize> {
        workspace
            .tree()
            .pane_ids()
            .into_iter()
            .filter_map(|pane| workspace.tree().pane(pane))
            .map(|record| record.number().get())
            .collect()
    }

    #[test]
    fn complete_restore_planning_needs_no_runtime_or_directory_access() {
        let cwd = Path::new("/__shepr_plan_missing_directory__");
        let snapshot = one_pane_session_in(cwd);
        let plan = plan_restore(
            &snapshot,
            test_geometry(12, 40),
            false,
            test_restore_now(),
            &mut crate::workspace::WorkspaceIdAllocator::new(),
        );
        assert_eq!(plan.workspaces.len(), 1);
        assert_eq!(plan.workspaces[0].tree().len(), 1);
        assert_eq!(plan.launches.len(), 1);
        let launch = &plan.launches[0];
        assert_eq!(launch.saved_cwd, cwd);
        assert_eq!(
            launch.geometry,
            shepr_core::geometry::PaneGeometry::cells_only(38, 10)
        );
        assert!(
            plan.workspaces[0].tree().pane(launch.pane_id).is_some(),
            "a planned launch names a pane of its workspace"
        );
        assert_eq!(launch.workspace, 0);
        assert_eq!(plan.active, Some(0));
    }

    #[test]
    fn complete_restore_plan_defers_one_resume_and_plans_duplicate_as_shell() {
        let cwd = Path::new("/__shepr_plan_agent_directory__");
        let mut snapshot = one_pane_session_in(cwd);
        let workspace = &mut snapshot.workspaces[0];
        let pane = only_pane_mut(workspace);
        pane.agent_session = Some(persisted_test_session(
            "shepr:codex",
            shepr_agent::Agent::Codex,
            shepr_agent::resume::AgentSessionRef::id("planned-session").expect("session id"),
        ));
        let mut duplicate = pane.clone();
        duplicate.public_number = number(2);
        let first = pane.clone();
        workspace.next_public_pane_number = number(3);
        workspace.layout = split(LayoutSnapshot::Pane(first), LayoutSnapshot::Pane(duplicate));
        let plan = plan_restore(
            &snapshot,
            test_geometry(12, 40),
            true,
            test_restore_now(),
            &mut crate::workspace::WorkspaceIdAllocator::new(),
        );
        let tree = plan.workspaces[0].tree();
        assert_eq!(tree.len(), 2);
        assert_eq!(
            tree.panes()
                .filter(|(_, record)| record.terminal().agent_resume().is_pending())
                .count(),
            1
        );
        assert_eq!(plan.launches.len(), 1);
        let shell = terminal_of(&plan.workspaces[0], plan.launches[0].pane_id);
        assert!(shell.ownership().persisted_agent_session().is_none());
    }

    /// Capture writes a restorable file: the layout, focus, root, zoom, public
    /// numbers (gaps and all) and each pane's saved fields come back.
    #[test]
    fn capture_then_restore_keeps_layout_records_focus_zoom_and_numbers() {
        let mut original = Workspace::test_new("round trip");
        let first = original.tree().root();
        let second = original.test_split(shepr_core::layout::Direction::Horizontal);
        let third = original.test_split(shepr_core::layout::Direction::Vertical);
        assert!(original.close_pane(second).is_some());
        let fourth = original.test_split(shepr_core::layout::Direction::Horizontal);
        for (pane, label) in [(first, "one"), (third, "three"), (fourth, "four")] {
            original
                .pane_mut(pane)
                .expect("a live pane")
                .terminal_mut()
                .set_manual_label(Label::new(label).expect("test label"));
        }
        assert!(original.focus_pane(third));
        assert!(original.set_zoomed(true));
        let id = original.id();
        let shape_before = original
            .tree()
            .map_shape(|_, record| record.number().get())
            .expect("a tree maps");
        let numbers_before = numbers_in_layout_order(&original);
        assert_eq!(numbers_before, vec![1, 3, 4]);
        let workspaces = WorkspaceSet::restored(
            crate::workspace::WorkspaceIdAllocator::new(),
            vec![original],
            Some(0),
        );

        let snapshot = crate::persist::capture(
            &workspaces,
            &crate::pane::PaneRuntimeRegistry::default(),
            &shepr_core::absolute_path::AbsolutePath::root(),
            Default::default(),
        )
        .expect("fixture workspace trees capture consistently");
        let plan = plan_restore(
            &snapshot,
            test_geometry(24, 80),
            false,
            test_restore_now(),
            &mut crate::workspace::WorkspaceIdAllocator::new(),
        );

        assert!(plan.damage.is_empty());
        assert_eq!(plan.active, Some(0));
        let restored = &plan.workspaces[0];
        assert_eq!(restored.id(), id);
        assert_eq!(restored.name(), "round trip");
        assert_eq!(
            restored
                .tree()
                .map_shape(|_, record| record.number().get())
                .expect("a tree maps"),
            shape_before
        );
        let focused = restored.tree().focused();
        assert_eq!(
            restored.tree().pane(focused).map(PaneRecord::number),
            Some(number(3))
        );
        assert_eq!(
            restored
                .tree()
                .pane(restored.tree().root())
                .map(PaneRecord::number),
            Some(number(1))
        );
        assert!(restored.tree().zoomed());
        assert_eq!(restored.tree().next_number(), number(5));
        let labels: Vec<_> = restored
            .tree()
            .pane_ids()
            .into_iter()
            .map(|pane| {
                terminal_of(restored, pane)
                    .manual_label()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                Some("one".to_owned()),
                Some("three".to_owned()),
                Some("four".to_owned())
            ]
        );
    }

    /// Restored panes keep every saved field whichever way they come back.
    #[tokio::test]
    async fn restored_panes_keep_saved_fields() {
        // (resume agents, saved cwd missing, shell missing)
        for (resume, missing_cwd, missing_shell) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let scratch = crate::test_support::ScratchDir::new("restore-saved-fields");
            let mut snapshot = one_pane_session_in(scratch.path());
            let pane = only_pane_mut(&mut snapshot.workspaces[0]);
            pane.label = Some(Label::new("keep me").expect("test label"));
            pane.agent_session = Some(persisted_test_session(
                "shepr:codex",
                shepr_agent::Agent::Codex,
                shepr_agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            ));
            if missing_cwd {
                pane.cwd = abs(pane.cwd.join("__shepr_missing_restore_directory__"));
                assert!(!pane.cwd.try_exists().expect("test stat"));
            }
            let saved_cwd = pane.cwd.clone();
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                workspaces,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                test_geometry(5, 40),
                4096,
                crate::pane::PaneShellConfig::new(
                    &test_shell(if missing_shell {
                        "/__shepr_missing_restore_shell__\0"
                    } else {
                        test_restore_shell()
                    }),
                    false,
                ),
                std::path::Path::new(TEST_SOCKET),
                resume,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
                &Arc::default(),
                test_restore_now(),
            );
            let case =
                format!("resume={resume} missing_cwd={missing_cwd} missing_shell={missing_shell}");
            // A deferred resume and a refused shell leave no runtime. A saved
            // directory that is gone leaves one whose shell never launches.
            let runtimeless = resume || missing_shell;
            assert_eq!(runtimes.is_empty(), runtimeless, "{case}");
            let mut runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
            let workspaces = WorkspaceSet::restored(
                crate::workspace::WorkspaceIdAllocator::new(),
                workspaces,
                Some(0),
            );
            let captured = crate::persist::capture(
                &workspaces,
                &runtimes,
                &shepr_core::absolute_path::AbsolutePath::root(),
                Default::default(),
            )
            .expect("fixture workspace trees capture consistently");
            let pane = only_pane(&captured.workspaces[0]);
            assert_eq!(
                pane.label.as_ref().map(Label::as_str),
                Some("keep me"),
                "{case}"
            );
            assert_eq!(pane.cwd, saved_cwd, "{case}");
            assert_eq!(
                pane.agent_session
                    .as_ref()
                    .map(|session| session.session_ref().value_str()),
                Some("codex-session"),
                "{case}"
            );
            for (_, runtime) in runtimes.drain() {
                drop(runtime);
            }
        }
    }

    #[test]
    fn saved_indices_follow_their_item_past_dropped_ones() {
        // Saved items 0 and 2 were dropped.
        let restored = [None, Some(0), None, Some(1)];
        assert_eq!(remap_saved_index(1, &restored), Some(0));
        assert_eq!(remap_saved_index(3, &restored), Some(1));
        // A dropped item resolves to the next survivor, else the previous one.
        assert_eq!(remap_saved_index(0, &restored), Some(0));
        assert_eq!(remap_saved_index(2, &restored), Some(1));
        assert_eq!(remap_saved_index(1, &[Some(0), None]), Some(0));
        // Past the end (a hand-edited file): the last survivor.
        assert_eq!(remap_saved_index(9, &restored), Some(1));
        assert_eq!(remap_saved_index(0, &[None, None]), None);
        assert_eq!(remap_saved_index(0, &[]), None);
    }

    #[test]
    fn an_out_of_range_saved_bookmark_is_reported_as_restore_damage() {
        let snapshot = session(
            vec![
                one_pane_workspace("w1", "first", 1),
                one_pane_workspace("w2", "last", 2),
            ],
            Some(9),
        );

        let plan = plan_restore(
            &snapshot,
            test_geometry(5, 40),
            false,
            test_restore_now(),
            &mut crate::workspace::WorkspaceIdAllocator::new(),
        );

        assert_eq!(plan.active, Some(1));
        assert_eq!(plan.damage.repaired_bookmarks, 1);
        assert!(!plan.damage.loses_data());
    }

    #[test]
    fn restore_plans_reject_collisions_and_exhaustion_before_execution() {
        let mut snap = workspace_snapshot("w1", "numbers", split(leaf(1), leaf(2)));
        // Both panes claim number 7, which is also past the next number.
        snap.layout = split(leaf(7), leaf(7));
        assert!(plan_workspace(&snap).is_none());
        // Distinct numbers, but one is not below the next number.
        snap.layout = split(leaf(7), leaf(usize::MAX));
        snap.next_public_pane_number = number(8);
        assert!(plan_workspace(&snap).is_none());
        // The same number twice, below the next number.
        snap.layout = split(leaf(3), leaf(3));
        snap.next_public_pane_number = number(8);
        assert!(plan_workspace(&snap).is_none());
        // A healthy workspace plans.
        // Focus and root name panes of the layout, which a plan now requires.
        snap.layout = split(leaf(3), leaf(4));
        snap.focused = number(3);
        snap.root_pane = number(3);
        assert!(plan_workspace(&snap).is_some());
    }

    #[test]
    fn restore_plans_derive_identity_cwd_from_the_root_pane() {
        let mut layout = split(leaf(1), leaf(2));
        layout.pane_mut(number(1)).expect("pane").cwd = abs("/root-pane");
        layout.pane_mut(number(2)).expect("pane").cwd = abs("/other-pane");
        let snap = workspace_snapshot("w1", "paths", layout);

        let plan = plan_workspace(&snap).expect("a healthy workspace plans");

        assert_eq!(plan.identity_cwd, PathBuf::from("/root-pane"));
        assert_eq!(plan.plan.root_leaf().public_number, number(1));
    }

    /// An injected NUL makes command encoding fail before any fork, so every pane
    /// comes back as a placeholder without a runtime.
    fn restore_runtimeless(snapshot: &SessionSnapshot) -> RestoredSession {
        let (events, _rx) = mpsc::channel(8);
        let restored = restore(
            snapshot,
            test_geometry(5, 40),
            0,
            crate::pane::PaneShellConfig::new(
                &test_shell("/__shepr_refused_restore_shell__\0"),
                false,
            ),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );
        assert!(restored.terminal_runtimes.is_empty());
        restored
    }

    /// Capture writes one number per pane, so two panes sharing one is a
    /// damaged file; the workspace is dropped like any other defect rather
    /// than renumbered, and the first save backs the original up.
    #[test]
    fn restore_drops_a_workspace_whose_panes_share_a_public_number() {
        let mut duplicated = workspace_snapshot("w1", "duplicated", split(leaf(1), leaf(2)));
        duplicated.layout = split(leaf(3), leaf(3));
        duplicated.next_public_pane_number = number(4);
        let snapshot = session(
            vec![duplicated, one_pane_workspace("w2", "healthy", 3)],
            Some(1),
        );

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(
            restored
                .restore_loss
                .expect("a workspace was dropped")
                .dropped_workspaces,
            1
        );
        let names: Vec<_> = restored.workspaces.iter().map(Workspace::name).collect();
        assert_eq!(names, vec!["healthy"]);
    }

    #[test]
    fn dropped_workspaces_do_not_shift_the_saved_bookmark() {
        let snapshot = session(
            vec![
                // Rejected: its panes share a public number.
                rejected_workspace("w1", "dropped", 1),
                one_pane_workspace("w2", "kept", 2),
                rejected_workspace("w3", "gone", 4),
                one_pane_workspace("w4", "active", 5),
            ],
            Some(3),
        );

        let restored = restore_runtimeless(&snapshot);

        let names: Vec<_> = restored.workspaces.iter().map(Workspace::name).collect();
        assert_eq!(names, vec!["kept", "active"]);
        assert_eq!(
            restored
                .restore_loss
                .expect("workspaces were dropped")
                .dropped_workspaces,
            2
        );
        assert_eq!(restored.active, Some(1));
    }

    #[test]
    fn a_dropped_active_workspace_falls_back_to_its_neighbour() {
        let snapshot = session(
            vec![
                one_pane_workspace("w1", "before", 1),
                rejected_workspace("w2", "dropped", 2),
            ],
            Some(1),
        );

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.active, Some(0));
    }

    #[test]
    fn a_saved_zoom_survives_restore_only_with_a_second_pane() {
        // (layout, focused, whether the workspace restores zoomed)
        for (layout, focused, zoomed) in [
            (split(leaf(1), split(leaf(2), leaf(3))), 2, true),
            (split(leaf(1), leaf(2)), 2, true),
            // One pane cannot be zoomed: the saved zoom is damage, and the
            // workspace is dropped rather than silently repaired.
            (leaf(1), 1, false),
        ] {
            let mut workspace = workspace_snapshot("w1", "ws", layout);
            workspace.zoomed = true;
            workspace.focused = number(focused);
            let snapshot = session(vec![workspace], Some(0));

            let restored = restore_runtimeless(&snapshot);

            if zoomed {
                assert!(restored.workspaces[0].tree().zoomed());
            } else {
                assert!(restored.workspaces.is_empty());
                assert_eq!(
                    restored
                        .restore_loss
                        .expect("the workspace was dropped")
                        .dropped_workspaces,
                    1
                );
            }
        }
    }

    #[test]
    fn restored_workspace_ids_are_unique() {
        // The ID the state's allocator would hand out next is also saved on
        // a workspace; a second workspace repeats that saved ID.
        let mut workspace_ids = crate::workspace::WorkspaceIdAllocator::new();
        let taken = crate::workspace::WorkspaceIdAllocator::new()
            .try_allocate()
            .expect("workspace id available")
            .to_string();
        let snapshot = session(
            vec![
                one_pane_workspace(&taken, "owner", 2),
                one_pane_workspace(&taken, "repeat", 3),
            ],
            Some(0),
        );

        let restored = plan_restore(
            &snapshot,
            test_geometry(5, 40),
            false,
            test_restore_now(),
            &mut workspace_ids,
        );

        let ids: Vec<_> = restored.workspaces.iter().map(Workspace::id).collect();
        assert_eq!(
            ids.len(),
            2,
            "a duplicate saved ID is replaced, not dropped"
        );
        assert_eq!(restored.damage.renamed_workspaces, 1);
        assert_eq!(
            ids[0].to_string(),
            taken,
            "the first owner of a saved ID keeps it"
        );
        assert_ne!(
            ids[1].to_string(),
            taken,
            "a duplicate saved ID is replaced"
        );
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        // A later new workspace does not reuse any restored ID either.
        let fresh = workspace_ids
            .try_allocate()
            .expect("workspace id available");
        assert!(!ids.contains(&fresh), "fresh workspace id reused: {ids:?}");
    }

    #[test]
    fn restore_drops_an_unassignable_duplicate_workspace_id() {
        let id = WorkspaceId::from_number(usize::MAX)
            .expect("nonzero workspace id")
            .to_string();
        let snapshot = session(
            vec![
                one_pane_workspace(&id, "owner", 1),
                one_pane_workspace(&id, "duplicate", 2),
            ],
            Some(1),
        );
        let mut workspace_ids = crate::workspace::WorkspaceIdAllocator::new();

        let restored = plan_restore(
            &snapshot,
            test_geometry(5, 40),
            false,
            test_restore_now(),
            &mut workspace_ids,
        );

        assert_eq!(restored.damage.renamed_workspaces, 0);
        assert_eq!(restored.damage.dropped_workspaces, 1);
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].id().to_string(), id);
        assert_eq!(restored.active, Some(0));
        assert_eq!(workspace_ids.try_allocate(), None);
    }

    /// Validation precedes every side effect: a workspace the plan refuses
    /// reserves no agent session, so a later pane with the same session
    /// resumes it instead of starting as a duplicate shell.
    #[test]
    fn a_rejected_workspace_reserves_no_agent_session() {
        let session_of = || {
            persisted_test_session(
                "shepr:codex",
                shepr_agent::Agent::Codex,
                shepr_agent::resume::AgentSessionRef::id("shared-session").expect("session id"),
            )
        };
        let with_session = |public_number: usize| {
            let mut pane = pane_snapshot(public_number);
            pane.agent_session = Some(session_of());
            LayoutSnapshot::Pane(pane)
        };
        // The first workspace repeats a public number and is dropped.
        let mut rejected =
            workspace_snapshot("w1", "rejected", split(with_session(1), with_session(1)));
        rejected.next_public_pane_number = number(2);
        let snapshot = session(
            vec![
                rejected,
                workspace_snapshot("w2", "accepted", with_session(1)),
            ],
            Some(0),
        );

        let plan = plan_restore(
            &snapshot,
            test_geometry(5, 40),
            true,
            test_restore_now(),
            &mut crate::workspace::WorkspaceIdAllocator::new(),
        );

        assert_eq!(plan.damage.dropped_workspaces, 1);
        assert_eq!(plan.workspaces.len(), 1);
        assert_eq!(plan.workspaces[0].name(), "accepted");
        assert!(
            root_terminal(&plan.workspaces[0])
                .agent_resume()
                .is_pending(),
            "the surviving pane resumes the session"
        );
        assert!(plan.launches.is_empty(), "it is not planned as a shell");
    }

    #[test]
    fn restore_plan_respects_opt_in_and_allowlist() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = persisted_test_session(
            "shepr:pi",
            shepr_agent::Agent::Pi,
            shepr_agent::resume::AgentSessionRef::path(pi_session_path.clone())
                .expect("test precondition"),
        );

        assert!(
            pane_restore_startup(Some(&session), None)
                .restore_plan
                .is_none()
        );
        assert_eq!(
            pane_restore_startup(Some(&session), Some(&mut HashSet::new()))
                .restore_plan
                .expect("test precondition")
                .args(),
            &["--session", pi_session_path.as_str()]
        );

        assert!(
            shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::parse("shepr:claude").expect("bundled source"),
                shepr_agent::Agent::Claude,
                shepr_agent::resume::AgentSessionRef::path(test_session_path("claude-session",))
                    .expect("test precondition"),
            )
            .is_none()
        );
    }

    #[test]
    fn pane_restore_startup_resumes_a_session_once_and_starts_duplicates_as_shells() {
        let session = persisted_test_session(
            "shepr:pi",
            shepr_agent::Agent::Pi,
            shepr_agent::resume::AgentSessionRef::path(test_session_path("pi-session.jsonl"))
                .expect("test precondition"),
        );
        let mut resumed = HashSet::new();

        let first = pane_restore_startup(Some(&session), Some(&mut resumed));
        let duplicate = pane_restore_startup(Some(&session), Some(&mut resumed));

        assert!(first.restore_plan.is_some());
        assert!(!first.duplicate_agent_session);
        assert!(duplicate.restore_plan.is_none());
        assert!(duplicate.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_plans_no_resume_when_resume_is_off() {
        let session = persisted_test_session(
            "shepr:pi",
            shepr_agent::Agent::Pi,
            shepr_agent::resume::AgentSessionRef::path(test_session_path("pi-session.jsonl"))
                .expect("test precondition"),
        );
        let startup = pane_restore_startup(Some(&session), None);

        assert!(startup.restore_plan.is_none());
        assert!(!startup.duplicate_agent_session);
    }

    #[test]
    fn restore_rehydrates_agent_session_metadata() {
        let session = persisted_test_session(
            "shepr:codex",
            shepr_agent::Agent::Codex,
            shepr_agent::resume::AgentSessionRef::id("codex-session").expect("test precondition"),
        );

        let preserved = restored_terminal_agent_session(Some(&session), false)
            .expect("restore should preserve metadata");
        assert_eq!(preserved.source().as_str(), "shepr:codex");
        assert_eq!(preserved.agent().label(), "codex");
        assert_eq!(preserved.session_ref().value_str(), "codex-session");
    }

    #[test]
    fn restore_does_not_rehydrate_duplicate_agent_session_metadata() {
        let session = persisted_test_session(
            "shepr:pi",
            shepr_agent::Agent::Pi,
            shepr_agent::resume::AgentSessionRef::path(test_session_path("pi-session.jsonl"))
                .expect("test precondition"),
        );
        let mut resumed = HashSet::new();
        assert!(!pane_restore_startup(Some(&session), Some(&mut resumed)).duplicate_agent_session);
        assert!(pane_restore_startup(Some(&session), Some(&mut resumed)).duplicate_agent_session);

        assert!(restored_terminal_agent_session(Some(&session), true).is_none());
    }

    #[tokio::test]
    async fn failed_cold_restore_preserves_panes_and_saved_directories() {
        for missing_shell in [false, true] {
            let mut snapshot: SessionSnapshot = serde_json::from_value(serde_json::json!({
                "version": SNAPSHOT_VERSION,
                "host_theme": SavedHostTheme::default(),
                "workspaces": [
                    {
                        "id": "w1",
                        "name": "a",
                        "next_public_pane_number": 2,
                        "layout": { "Pane": { "cwd": "/tmp/shepr-restore-test-a", "public_number": 1, "label": null } },
                        "zoomed": false,
                        "focused": 1,
                        "root_pane": 1
                    },
                    {
                        "id": "w2",
                        "name": "b",
                        "next_public_pane_number": 2,
                        "layout": { "Pane": { "cwd": "/tmp/shepr-restore-test-b", "public_number": 1, "label": null } },
                        "zoomed": false,
                        "focused": 1,
                        "root_pane": 1
                    }
                ],
                "active": 0
            }))
            .expect("test precondition");
            let scratch = crate::test_support::ScratchDir::new("restore-cold-cwd");
            let cwd = scratch.to_path_buf();
            let missing = cwd.join("__shepr_missing_restore_directory__");
            assert!(!missing.try_exists().expect("test stat"));
            for workspace in &mut snapshot.workspaces {
                only_pane_mut(workspace).cwd = abs(cwd.clone());
            }
            let failed = only_pane_mut(&mut snapshot.workspaces[0]);
            failed.cwd = abs(missing.clone());
            failed.label = Some(Label::new("keep my pane").expect("test label"));
            failed.agent_session = Some(persisted_test_session(
                "shepr:opencode",
                shepr_agent::Agent::OpenCode,
                shepr_agent::resume::AgentSessionRef::id("keep-my-session")
                    .expect("test precondition"),
            ));
            let (events, mut events_rx) = mpsc::channel(32);
            let RestoredSession {
                workspaces,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                test_geometry(24, 80),
                0,
                crate::pane::PaneShellConfig::new(
                    &test_shell(if missing_shell {
                        "/__shepr_missing_restore_shell__\0"
                    } else {
                        test_restore_shell()
                    }),
                    false,
                ),
                std::path::Path::new(TEST_SOCKET),
                false,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
                &Arc::default(),
                test_restore_now(),
            );
            let runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
            let workspaces = WorkspaceSet::restored(
                crate::workspace::WorkspaceIdAllocator::new(),
                workspaces,
                Some(0),
            );
            let captured = crate::persist::capture(
                &workspaces,
                &runtimes,
                &shepr_core::absolute_path::AbsolutePath::root(),
                Default::default(),
            )
            .expect("fixture workspace trees capture consistently");
            assert_eq!(
                captured.workspaces.len(),
                2,
                "a launch failure must not delete a workspace"
            );
            let pane = only_pane(&captured.workspaces[0]);
            assert_eq!(
                pane.cwd, missing,
                "fallback cwd must not replace saved intent"
            );
            assert_eq!(pane.label.as_ref().map(Label::as_str), Some("keep my pane"));
            assert_eq!(
                pane.agent_session
                    .as_ref()
                    .expect("test precondition")
                    .session_ref()
                    .value_str(),
                "keep-my-session"
            );
            let root = workspaces.as_slice()[0].tree().root();
            let terminal = terminal_of(&workspaces.as_slice()[0], root);
            if missing_shell {
                // The launch is refused before any fork.
                assert!(runtimes.get(&root).is_none());
                assert!(terminal.start_failure().is_some());
            } else {
                // The child's chdir finds the saved directory gone and the
                // launch settles as a failure, never in another directory.
                assert!(matches!(
                    launch_settlement(&mut events_rx, root).await,
                    crate::pane::LaunchSettlement {
                        kind: crate::pane::LaunchKind::Restored,
                        outcome: crate::pane::LaunchOutcome::Failed(
                            PaneStartFailure::DirectoryUnavailable { ref path, .. }
                        ),
                    } if *path == missing
                ));
                assert!(
                    !runtimes
                        .get(&root)
                        .is_some_and(crate::pane::PaneRuntime::launched),
                    "do not open a replacement shell elsewhere"
                );
            }
            let healthy = workspaces.as_slice()[1].tree().root();
            assert_eq!(runtimes.get(&healthy).is_some(), !missing_shell);
        }
    }

    /// The settlement of `pane`'s launch, from the restore's event channel.
    async fn launch_settlement(
        events: &mut mpsc::Receiver<crate::events::AppEvent>,
        pane: shepr_core::layout::PaneId,
    ) -> crate::pane::LaunchSettlement {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .expect("the launch settles")
                .expect("the event channel stays open");
            let crate::events::AppEvent::Runtime { pane_id, event, .. } = event else {
                continue;
            };
            if let crate::events::RuntimeEvent::PaneLaunchSettled { settlement } = *event
                && pane_id == pane
            {
                return settlement;
            }
        }
    }

    /// A launch that fails (no shell can be started) replaces the pane's
    /// planned terminal in place with one that keeps everything saved.
    #[test]
    fn a_failed_launch_keeps_the_saved_state_on_the_pane() {
        let mut snapshot = session(vec![one_pane_workspace("w1", "failed", 4)], Some(0));
        let pane = only_pane_mut(&mut snapshot.workspaces[0]);
        pane.label = Some(Label::new("keep me").expect("test label"));
        pane.agent_session = Some(persisted_test_session(
            "shepr:codex",
            shepr_agent::Agent::Codex,
            shepr_agent::resume::AgentSessionRef::id("codex-session").expect("test precondition"),
        ));
        let saved_cwd = pane.cwd.clone();

        let restored = restore_runtimeless(&snapshot);

        let workspace = &restored.workspaces[0];
        let record = workspace
            .tree()
            .pane(workspace.tree().root())
            .expect("the root pane is restored");
        assert_eq!(record.number(), number(4));
        let terminal = record.terminal();
        assert!(terminal.start_failure().is_some());
        assert_eq!(terminal.manual_label(), Some("keep me"));
        assert_eq!(terminal.cwd(), &saved_cwd);
        assert_eq!(
            terminal
                .ownership()
                .persisted_agent_session()
                .map(|session| session.session_ref().value_str()),
            Some("codex-session")
        );
        assert_eq!(workspace.tree().len(), 1);
    }

    #[tokio::test]
    async fn restore_carries_persisted_agent_session_metadata() {
        let scratch = crate::test_support::ScratchDir::new("restore-agent-metadata-cwd");
        let cwd = abs(scratch.to_path_buf());
        let mut snapshot = session(vec![one_pane_workspace("w1", "metadata", 1)], Some(0));
        let pane = only_pane_mut(&mut snapshot.workspaces[0]);
        pane.cwd = cwd;
        pane.label = Some(Label::new("reviewer").expect("test label"));
        pane.agent_session = Some(persisted_test_session(
            "shepr:opencode",
            shepr_agent::Agent::OpenCode,
            shepr_agent::resume::AgentSessionRef::id("opencode-session")
                .expect("test precondition"),
        ));
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(&test_shell(test_restore_shell()), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let terminal = root_terminal(&workspaces[0]);
        assert_eq!(terminal.manual_label(), Some("reviewer"));
        let session = terminal
            .ownership()
            .persisted_agent_session()
            .expect("persisted agent session should survive restore");
        assert_eq!(session.source().as_str(), "shepr:opencode");
        assert_eq!(session.agent().label(), "opencode");
        assert_eq!(session.session_ref().value_str(), "opencode-session");
    }

    #[tokio::test]
    async fn restore_keeps_each_panes_public_id() {
        let scratch = crate::test_support::ScratchDir::new("restore-public-id-cwd");
        let cwd = abs(scratch.to_path_buf());
        let mut layout = split(leaf(1), leaf(3));
        for public_number in [1, 3] {
            layout.pane_mut(number(public_number)).expect("pane").cwd = cwd.clone();
        }
        let mut workspace = workspace_snapshot("w1", "ids", layout);
        workspace.next_public_pane_number = number(4);
        let snapshot = session(vec![workspace], Some(0));
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(&test_shell(test_restore_shell()), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let mut public_numbers: Vec<_> = workspace
            .tree()
            .panes()
            .map(|(_, record)| record.number().get())
            .collect();
        public_numbers.sort_unstable();
        assert_eq!(public_numbers, vec![1, 3]);
        assert_eq!(workspace.tree().next_number().get(), 4);
        assert!(
            workspace.tree().pane_by_number(number(3)).is_some(),
            "a public ID resolves to its pane after the pane IDs were reallocated"
        );
    }

    #[test]
    fn a_saved_zero_pane_number_is_refused_at_decode() {
        let snap = one_pane_workspace("w1", "zero number", 10);
        let mut json = serde_json::to_value(snap).expect("snapshot JSON");
        json["layout"]["Pane"]["public_number"] = serde_json::json!(0);
        assert!(serde_json::from_value::<WorkspaceSnapshot>(json).is_err());
    }

    #[tokio::test]
    async fn cold_restore_with_gapped_public_pane_numbers_starts_a_plain_shell_without_an_agent() {
        let scratch = crate::test_support::ScratchDir::new("restore-public-pane-cwd");
        let cwd = abs(scratch.to_path_buf());
        let mut layout = split(leaf(4), leaf(7));
        for public_number in [4, 7] {
            layout.pane_mut(number(public_number)).expect("pane").cwd = cwd.clone();
        }
        let final_pane = layout.pane_mut(number(7)).expect("pane");
        final_pane.label = Some(Label::new("planner").expect("test label"));
        final_pane.agent_session = Some(persisted_test_session(
            "shepr:codex",
            shepr_agent::Agent::Codex,
            shepr_agent::resume::AgentSessionRef::id("codex-session").expect("test precondition"),
        ));
        let mut workspace = workspace_snapshot("w1", "gapped", layout);
        // Numbers 1 to 3 were public panes that are gone.
        workspace.next_public_pane_number = number(8);
        workspace.focused = number(7);
        workspace.root_pane = number(4);
        let snapshot = session(vec![workspace], Some(0));
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(&test_shell(test_restore_shell()), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let agent_pane = workspace.tree().focused();
        assert_eq!(
            workspace
                .tree()
                .pane(agent_pane)
                .map(|record| record.number().get()),
            Some(7)
        );
        assert!(
            terminal_of(workspace, agent_pane)
                .ownership()
                .effective_agent()
                .is_none()
        );
    }

    #[tokio::test]
    async fn native_agent_restore_defers_runtime_launch() {
        let scratch = crate::test_support::ScratchDir::new("restore-native-agent-cwd");
        let cwd = abs(scratch.to_path_buf());
        let mut snapshot = session(vec![one_pane_workspace("w1", "native", 1)], Some(0));
        let pane = only_pane_mut(&mut snapshot.workspaces[0]);
        pane.cwd = cwd;
        pane.agent_session = Some(persisted_test_session(
            "shepr:codex",
            shepr_agent::Agent::Codex,
            shepr_agent::resume::AgentSessionRef::id("codex-session").expect("test precondition"),
        ));
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminal_runtimes: runtimes,
            ..
        } = restore(
            &snapshot,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(&test_shell(test_restore_shell()), false),
            std::path::Path::new(TEST_SOCKET),
            true,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let terminal = root_terminal(&workspaces[0]);
        assert!(
            terminal.agent_resume().is_pending(),
            // The launch waits for the event loop, not for a client: once a
            // view exists it starts after a short wait for a host theme, at
            // the headless size when no client is attached (see the headless
            // test `headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client`).
            "restored native agent panes should defer resume to the event loop"
        );
        assert!(
            runtimes.is_empty(),
            "native agent restore should not spawn a fallback-size runtime during snapshot restore"
        );
    }

    /// Each restored shell starts at its own size in its workspace's layout, not
    /// at one size shared by every pane; a pane hidden behind a zoomed one
    /// starts at its tiled size.
    #[tokio::test]
    async fn restored_panes_start_at_their_own_layout_size() {
        for zoomed in [false, true] {
            let scratch = crate::test_support::ScratchDir::new("restore-pane-size");
            let mut layout = LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: test_split_ratio(0.25),
                first: Box::new(leaf(1)),
                second: Box::new(leaf(2)),
            };
            for public_number in [1, 2] {
                layout.pane_mut(number(public_number)).expect("pane").cwd =
                    abs(scratch.to_path_buf());
            }
            let mut workspace = workspace_snapshot("w1", "split", layout);
            workspace.zoomed = zoomed;
            workspace.focused = number(2);
            workspace.root_pane = number(1);
            workspace.next_public_pane_number = number(3);
            let snapshot = session(vec![workspace], Some(0));
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                workspaces,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                test_geometry(24, 80),
                0,
                crate::pane::PaneShellConfig::new(&test_shell(test_restore_shell()), false),
                std::path::Path::new(TEST_SOCKET),
                false,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
                &Arc::default(),
                test_restore_now(),
            );
            let workspace = &workspaces[0];
            assert_eq!(workspace.tree().zoomed(), zoomed);
            let size = |pane_id| {
                runtimes
                    .get(&pane_id)
                    .expect("restored runtime")
                    .grid_size()
            };
            let focused = workspace.tree().focused();
            let other = workspace.tree().root();
            assert_ne!(focused, other);
            let other_size = size(other);
            let (other_rows, other_cols) = (other_size.rows.get(), other_size.cols.get());
            let focused_size = size(focused);
            let (focused_rows, focused_cols) = (focused_size.rows.get(), focused_size.cols.get());
            // Every pane is 24 rows less its top and bottom border.
            assert_eq!((other_rows, focused_rows), (22, 22), "zoomed={zoomed}");
            // The first pane has a quarter of the width in the tiled layout.
            assert!(other_cols < 40, "zoomed={zoomed} cols={other_cols}");
            if zoomed {
                assert_eq!(focused_cols, 78);
            } else {
                // 80 columns less the outer left and right borders and the
                // one divider the two panes share.
                assert_eq!(other_cols + focused_cols, 77);
            }
            for (_, runtime) in runtimes {
                drop(runtime);
            }
        }
    }

    /// A session of one workspace whose only pane was saved in `cwd`.
    fn one_pane_session_in(cwd: &Path) -> SessionSnapshot {
        let mut pane = pane_snapshot(1);
        pane.cwd = abs(cwd);
        session(
            vec![workspace_snapshot("w1", "one", LayoutSnapshot::Pane(pane))],
            Some(0),
        )
    }
}
