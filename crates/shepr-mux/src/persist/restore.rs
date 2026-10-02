use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use tracing::{error, warn};

use crate::events::AppEvent;
use crate::pane::PaneRuntime;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{RestoreFailure, TerminalState};
use crate::workspace::Workspace;
use shepr_agent::detect::AgentState;
use shepr_core::layout::{Direction, InvalidSavedLayout, Node, PaneId, SplitRatio, TileLayout};
use shepr_protocol::{TerminalId, WorkspaceId};

use super::snapshot::{
    HistoryCarry, PaneAgentSessionSnapshot, PaneHistorySnapshot, WorkspaceHistorySnapshot,
};
use super::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, WorkspaceSnapshot,
};

struct AgentRestoreState<'a> {
    enabled: bool,
    resumed_sessions: &'a mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
}

struct PaneRestoreStartup<'a> {
    restore_plan: Option<shepr_agent::agent::resume::AgentResumePlan>,
    initial_history_ansi: Option<&'a str>,
    duplicate_agent_session: bool,
}

struct RestoreRuntimeContext<'a> {
    /// The area each workspace is laid out in, with the pane chrome that decides
    /// every pane's size.
    geometry: crate::workspace::PaneGeometry,
    scrollback_limit_bytes: usize,
    now: std::time::Instant,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
    shell_config: crate::pane::PaneShellConfig<'a>,
    socket_path: &'a std::path::Path,
    resume_agents_on_restore: bool,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
    pane_teardowns: Arc<crate::pane::PaneTeardownTracker>,
}

/// Everything a restore produces. Restore can drop saved workspaces (invalid
/// layout, or no pane survived), so saved indices into that list no longer
/// name the same item; `active` is already remapped onto `workspaces` and
/// must be used as it is, not re-derived from the snapshot by clamping.
pub struct RestoredSession {
    pub workspaces: Vec<Workspace>,
    pub terminals: HashMap<TerminalId, TerminalState>,
    pub terminal_runtimes: HashMap<TerminalId, PaneRuntime>,
    /// The saved bookmarked workspace as an index into `workspaces`; if it was
    /// dropped, its nearest surviving neighbour. `None` if nothing was
    /// bookmarked or nothing survived.
    pub active: Option<usize>,
    /// Saved history of the panes that came back without a runtime. The
    /// session's persister takes it; every later history capture of this
    /// session is resolved against it.
    pub history_carry: HistoryCarry,
    /// Restore discarded pane data or layout leaves, or replaced a malformed
    /// or repeated workspace ID. This is separate from `dropped_workspaces`:
    /// the caller must also preserve the source file when a workspace only
    /// partly came back.
    pub restore_damage: bool,
    /// Saved workspaces restore dropped (invalid layout, or no pane
    /// survived). The first save of this session overwrites the file those
    /// workspaces are still in, so a nonzero count tells the caller to back
    /// the file up first.
    pub dropped_workspaces: usize,
}

/// How a restored pane comes back. Every saved field is carried forward the
/// same way for all of them (`restored_terminal`); only what this decides
/// differs.
enum RestoredPaneStart {
    /// A fresh shell is running for the pane. `duplicate_agent_session`: the
    /// pane's saved agent session is resumed by an earlier pane of this
    /// restore, which owns it now.
    Running { duplicate_agent_session: bool },
    /// The pane waits for the event loop to type its agent's resume command
    /// into a fresh shell.
    PendingResume(shepr_agent::agent::resume::AgentResumePlan),
    /// Nothing could be started (the reason is shown in the pane). The pane
    /// keeps its saved state verbatim so the next start can try again.
    Unavailable(RestoreFailure),
}

type RestoredWorkspace = (
    Workspace,
    Vec<TerminalState>,
    HashMap<TerminalId, PaneRuntime>,
);

// Plain validated data only: constructing these never launches children or
// reserves agent sessions. The whole snapshot is planned before execution.
struct WorkspaceRestorePlan {
    snapshot: WorkspaceSnapshot,
    layout: TileLayout,
    reverse_id_map: HashMap<PaneId, u32>,
    pane_ids: Vec<PaneId>,
    root_pane: PaneId,
    zoomed: bool,
    numbers: HashMap<u32, usize>,
    next_number: usize,
    restore_damage: bool,
}

/// Restore workspaces from a snapshot. Each pane gets a fresh shell in its
/// saved cwd, started at its own size in its workspace laid out by `geometry`.
#[expect(
    clippy::too_many_arguments,
    reason = "restore threads geometry, launch policy and every handle a spawned pane reports through"
)]
pub fn restore(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    geometry: crate::workspace::PaneGeometry,
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    socket_path: &std::path::Path,
    resume_agents_on_restore: bool,
    events: &mpsc::Sender<AppEvent>,
    render_notify: &Arc<Notify>,
    render_dirty: &Arc<RenderSignal>,
    pane_teardowns: &Arc<crate::pane::PaneTeardownTracker>,
    now: std::time::Instant,
) -> RestoredSession {
    // `history` is the one `load_history` found to be the history this
    // layout's own save serialized (its digest), so its keys are this
    // snapshot's workspace positions and pane IDs.
    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut terminal_runtimes = HashMap::new();
    let mut resumed_agent_sessions = HashSet::new();
    let mut history_carry = HistoryCarry::default();
    let host_theme = snapshot.host_theme.to_theme();
    // Where each saved workspace ended up, `None` for a dropped one.
    let mut restored_index = Vec::with_capacity(snapshot.workspaces.len());
    let plans: Vec<_> = snapshot.workspaces.iter().map(plan_workspace).collect();
    let mut restore_damage = plans.iter().flatten().any(|plan| plan.restore_damage);
    let saved_ids = snapshot
        .workspaces
        .iter()
        .map(|ws| {
            ws.id
                .as_deref()
                .and_then(|id| id.parse::<WorkspaceId>().ok())
        })
        .collect::<Vec<_>>();
    // Before any allocation below, so a fresh ID is never one a saved
    // workspace owns.
    crate::workspace::reserve_workspace_ids(saved_ids.iter().flatten());
    let mut used_ids = HashSet::new();
    let mut seen_saved_ids = HashSet::new();
    let mut dropped_workspaces = 0;
    for ((idx, plan), saved_id) in plans.into_iter().enumerate().zip(saved_ids) {
        if let Some(raw_id) = snapshot.workspaces[idx].id.as_deref() {
            match raw_id.parse::<WorkspaceId>() {
                Ok(id) => {
                    if !seen_saved_ids.insert(id) {
                        restore_damage = true;
                    }
                }
                Err(_) => restore_damage = true,
            }
        }
        let Some(plan) = plan else {
            dropped_workspaces += 1;
            restored_index.push(None);
            continue;
        };
        let workspace_id = restored_workspace_id(saved_id, &mut used_ids);
        let runtime_context = RestoreRuntimeContext {
            geometry,
            scrollback_limit_bytes,
            now,
            host_theme,
            shell_config,
            socket_path,
            resume_agents_on_restore,
            events: events.clone(),
            render_notify: Arc::clone(render_notify),
            render_dirty: Arc::clone(render_dirty),
            pane_teardowns: Arc::clone(pane_teardowns),
        };
        let restored = restore_workspace(
            plan,
            workspace_id,
            history.and_then(|history| history.workspaces.get(idx)),
            &runtime_context,
            &mut history_carry,
            &mut resumed_agent_sessions,
        );
        if let Some((workspace, restored_terminals, restored_runtimes)) = restored {
            for terminal in restored_terminals {
                terminals.insert(terminal.id.clone(), terminal);
            }
            terminal_runtimes.extend(restored_runtimes);
            restored_index.push(Some(workspaces.len()));
            workspaces.push(workspace);
        } else {
            dropped_workspaces += 1;
            restored_index.push(None);
        }
    }
    let active = snapshot
        .active
        .and_then(|active| remap_saved_index(active, &restored_index));
    RestoredSession {
        workspaces,
        terminals,
        terminal_runtimes,
        active,
        history_carry,
        restore_damage,
        dropped_workspaces,
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

/// The ID a restored workspace gets. A saved ID is kept if it is canonical
/// (`saved` is `None` otherwise) and no earlier workspace of the same file
/// already took it (a hand-edited or damaged file can break either). A
/// workspace without a usable ID gets a fresh one: the caller reserved every
/// saved ID before restoring, so a fresh ID is past all of them.
fn restored_workspace_id(
    saved: Option<WorkspaceId>,
    used_ids: &mut HashSet<WorkspaceId>,
) -> WorkspaceId {
    if let Some(id) = saved
        && used_ids.insert(id.clone())
    {
        return id;
    }
    crate::workspace::generate_workspace_id()
}

/// The terminal state of one restored pane. Every saved `PaneSnapshot` field
/// is carried forward here, once, whichever way the pane comes back; `start`
/// only decides the parts that genuinely differ:
///
/// - cwd, label and launch argv: always kept.
/// - agent session: always kept, except by a running duplicate whose session
///   an earlier pane of this restore resumes.
fn restored_terminal(
    pane: &super::snapshot::PaneSnapshot,
    start: RestoredPaneStart,
    now: std::time::Instant,
) -> TerminalState {
    let mut terminal = TerminalState::new(TerminalId::alloc(), pane.cwd.clone());
    if let Some(label) = pane.label.clone() {
        terminal.set_manual_label(label);
    }
    let duplicate_agent_session = matches!(
        start,
        RestoredPaneStart::Running {
            duplicate_agent_session: true
        }
    );
    if let Some(session) =
        restored_terminal_agent_session(pane.agent_session.as_ref(), duplicate_agent_session)
    {
        terminal.set_persisted_agent_session(session);
    }
    match start {
        RestoredPaneStart::Running { .. } => {}
        RestoredPaneStart::PendingResume(plan) => {
            let plan_agent = plan.agent;
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
            let _ = terminal.set_detected_state_with_screen_signals_at(
                Some(plan_agent),
                AgentState::Idle,
                false,
                false,
                now,
            );
        }
        RestoredPaneStart::Unavailable(reason) => {
            warn!(
                cwd = %pane.cwd.display(),
                reason = ?reason,
                "preserving unavailable restored pane"
            );
            terminal.restore_error = Some(reason);
        }
    }
    terminal
}

/// The `(rows, cols)` a restored pane's shell starts at: its own rect in the
/// workspace's layout, which is what the first view computation gives it. A
/// child reads its window size at startup, so any other size would reach it
/// first and be corrected only by the first resize. A pane hidden behind a
/// zoomed one gets its tiled size, which it has again once the workspace is
/// unzoomed.
fn restored_pane_size(
    geometry: &crate::workspace::PaneGeometry,
    layout: &TileLayout,
    zoomed: bool,
    pane: PaneId,
) -> (u16, u16) {
    zoomed
        .then(|| geometry.pane_size(layout, true, pane))
        .flatten()
        .or_else(|| geometry.pane_size(layout, false, pane))
        .unwrap_or_else(|| geometry.sole_pane_size())
}

/// One saved workspace, or `None` when a layout defect or a missing pane
/// leaves nothing usable to restore.
fn plan_workspace(original: &WorkspaceSnapshot) -> Option<WorkspaceRestorePlan> {
    let mut snap = original.clone();
    let mut restore_damage = false;
    // shepr only ever saves absolute cwds, so a relative one is a damaged
    // value, not a directory that went missing. It is dropped with its pane
    // rather than kept as an unavailable pane: a relative cwd in live terminal
    // state would reach splits and cwd following, which would resolve it
    // against the server's own working directory.
    let saved_pane_count = snap.panes.len();
    snap.panes.retain(|_, pane| {
        let valid = pane.cwd.is_absolute();
        if !valid {
            warn!(cwd = %pane.cwd.display(), "dropping saved pane with relative cwd");
        }
        valid
    });
    restore_damage |= snap.panes.len() != saved_pane_count;
    // An invalid saved split ratio drops this one workspace, like every other
    // per-workspace restore defect below, rather than refusing the whole
    // session (which would lose every healthy workspace for one bad number) or
    // clamping it (which silently repairs a corrupt file). The workspace is
    // not lost on disk: a nonzero `RestoredSession::dropped_workspaces` makes
    // the first save back the original file up before overwriting it.
    // Pane IDs are allocated here, before any shell starts; a workspace
    // dropped later only leaves gaps in the ID space.
    let (node, id_map) = match restore_node_remapped(&snap.layout) {
        Ok(restored) => restored,
        Err(error) => {
            error!(
                workspace = ?snap.id,
                ?error,
                "saved workspace layout is invalid; dropping workspace"
            );
            return None;
        }
    };
    let reverse_id_map: HashMap<PaneId, u32> = id_map
        .iter()
        .map(|(&old_id, &new_id)| (new_id, old_id))
        .collect();

    // A layout leaf with no saved pane (a repeated ID, or an entry missing
    // from `panes`) has nothing to restore. Inventing one would open a shell
    // in the server's own working directory and then save that directory as
    // if it had been the user's; the leaf is dropped and pruning collapses
    // its split. That happens before any pane starts, so every shell starts
    // at its size in the layout the workspace ends up with.
    let layout_pane_ids = collect_pane_ids(&node);
    let layout_pane_count = layout_pane_ids.len();
    let mut surviving = HashSet::new();
    for id in layout_pane_ids {
        let old_id = reverse_id_map.get(&id);
        if old_id.is_some_and(|old_id| snap.panes.contains_key(old_id)) {
            surviving.insert(id);
        } else {
            warn!(
                workspace = ?snap.id,
                pane_id = ?old_id,
                "saved layout names a pane with no saved state; dropping it"
            );
        }
    }
    restore_damage |= surviving.len() != layout_pane_count;
    let Some(node) = prune_restored_node(node, &surviving) else {
        warn!(
            workspace = ?snap.id,
            "no panes could be restored for workspace, dropping it"
        );
        return None;
    };
    let pane_ids = collect_pane_ids(&node);
    let saved_focus_survived = snap
        .focused
        .and_then(|old_id| id_map.get(&old_id))
        .is_some_and(|pane_id| surviving.contains(pane_id));
    // A stale saved focus falls back to the first surviving leaf before the
    // checked layout constructor is called.
    let focus = resolve_restored_pane(snap.focused, &id_map, &surviving, &pane_ids)?;
    let root_pane = resolve_restored_pane(snap.root_pane, &id_map, &surviving, &pane_ids)?;
    // Every leaf got a fresh `PaneId::alloc` in `restore_node_remapped` and
    // focus was just resolved to a surviving leaf, so the saved-file defects
    // `from_saved` checks for cannot reach it; a rejection here means an
    // internal invariant broke. The workspace is dropped like the other
    // unusable ones above, loudly, rather than guessed back into shape.
    let layout = match TileLayout::from_saved(node, focus) {
        Ok(layout) => layout,
        Err(error) => {
            error!(
                workspace = ?snap.id,
                ?error,
                "restored workspace failed layout validation after remapping; dropping it"
            );
            return None;
        }
    };
    // Pruning can leave a single pane, which is never zoomed, or drop the
    // zoomed (focused) pane, and zooming whichever pane focus fell back to
    // would show one the user never zoomed.
    let zoomed = snap.zoomed && pane_ids.len() > 1 && saved_focus_survived;

    // Assign only surviving panes; stale entries cannot exhaust numbering or
    // collide with a pane that will actually be restored.
    let planned_pane_count = snap.panes.len();
    snap.panes
        .retain(|old, _| id_map.get(old).is_some_and(|id| surviving.contains(id)));
    restore_damage |= snap.panes.len() != planned_pane_count;
    let max = snap
        .panes
        .values()
        .filter_map(|pane| pane.public_number)
        .max()
        .unwrap_or(0);
    let Some(mut next) = max.checked_add(1) else {
        warn!(workspace = ?snap.id, "saved public pane number space exhausted; dropping workspace");
        return None;
    };
    next = next.max(snap.next_public_pane_number).max(1);
    let Some(numbers) = assign_public_pane_numbers(&snap, &mut next) else {
        warn!(workspace = ?snap.id, "saved public pane number space exhausted; dropping workspace");
        return None;
    };
    if !Workspace::valid_public_numbers(numbers.values().copied(), next) {
        warn!(workspace = ?snap.id, "dropping saved workspace with invalid public pane numbers");
        return None;
    }
    if !snap.identity_cwd.is_absolute() {
        snap.identity_cwd = snap.panes.get(reverse_id_map.get(&root_pane)?)?.cwd.clone();
    }
    Some(WorkspaceRestorePlan {
        snapshot: snap,
        layout,
        reverse_id_map,
        pane_ids,
        root_pane,
        zoomed,
        numbers,
        next_number: next,
        restore_damage,
    })
}

/// Starts the panes of one planned workspace. Every saved-file defect was
/// found while planning; the `None` returns left here guard internal
/// invariants only.
fn restore_workspace(
    plan: WorkspaceRestorePlan,
    workspace_id: WorkspaceId,
    history: Option<&WorkspaceHistorySnapshot>,
    runtime_context: &RestoreRuntimeContext<'_>,
    history_carry: &mut HistoryCarry,
    resumed_agent_sessions: &mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
) -> Option<RestoredWorkspace> {
    let WorkspaceRestorePlan {
        snapshot: snap,
        layout,
        reverse_id_map,
        pane_ids,
        root_pane,
        zoomed,
        numbers: public_pane_numbers_by_old_raw,
        next_number: next_public_pane_number,
        restore_damage: _,
    } = plan;
    let public_pane_ids_by_old_raw: HashMap<_, _> = public_pane_numbers_by_old_raw
        .iter()
        .map(|(&old, &number)| {
            (
                old,
                shepr_protocol::PublicPaneId::new(&workspace_id, number),
            )
        })
        .collect();
    let mut panes = HashMap::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    for id in &pane_ids {
        let old_id = reverse_id_map.get(id);
        let Some(saved_pane) = old_id.and_then(|old_id| snap.panes.get(old_id)) else {
            // Pruned above: every remaining leaf has a saved pane.
            continue;
        };
        let saved_history =
            old_id.and_then(|old_id| history.and_then(|history| history.panes.get(old_id)));

        // Nothing here looks at the saved directory: restore runs on the
        // server's startup path, and a stat of a directory on a hung mount
        // would hold the server before it serves anyone. The pane's launch
        // enters it by chdir (never falling back), and a directory that is
        // gone or unreadable settles that launch as a placeholder pane.
        let PaneRestoreStartup {
            restore_plan,
            initial_history_ansi,
            duplicate_agent_session,
        } = {
            let mut agent_restore = AgentRestoreState {
                enabled: runtime_context.resume_agents_on_restore,
                resumed_sessions: resumed_agent_sessions,
            };
            pane_restore_startup(
                saved_pane.agent_session.as_ref(),
                saved_history,
                &mut agent_restore,
            )
        };

        let old_pane_id = reverse_id_map.get(id).copied();
        let Some(pane_id) = old_pane_id
            .and_then(|old_id| public_pane_ids_by_old_raw.get(&old_id))
            .cloned()
        else {
            error!(
                workspace = %workspace_id,
                pane = ?id,
                "restored pane has no assigned public identity; dropping workspace"
            );
            return None;
        };
        let launch_env =
            PaneLaunchEnv::from_extra(Vec::new(), runtime_context.socket_path.to_path_buf())
                .with_pane_id(pane_id);
        if let Some(plan) = restore_plan {
            let terminal = restored_terminal(
                saved_pane,
                RestoredPaneStart::PendingResume(plan),
                runtime_context.now,
            );
            // Native resume owns what this pane shows once it runs, so the
            // saved screen is not replayed. Until a runtime exists, though,
            // saves must keep writing it: the resume waits for the event loop
            // and is spaced out per agent (`startup_per_agent_delay_ms`), so
            // later panes can wait a while, or it can fail outright (missing
            // cwd or shell), and neither may cost the pane its saved history.
            history_carry.carry_restored(&terminal.id, saved_history);
            panes.insert(
                *id,
                crate::workspace::WorkspacePane::new(PaneState::new(terminal.id.clone())),
            );
            terminals.push(terminal);
            continue;
        }

        // Restore runs before any client has attached, so there is no cell
        // size to give the shell; the first client geometry pass supplies it.
        let (rows, cols) = restored_pane_size(&runtime_context.geometry, &layout, zoomed, *id);
        let runtime_result = PaneRuntime::spawn_with_initial_history(
            *id,
            crate::workspace::spawn_geometry(rows, cols, None),
            &saved_pane.cwd,
            runtime_context.scrollback_limit_bytes,
            runtime_context.host_theme,
            None,
            runtime_context.shell_config.require_cwd(),
            &launch_env,
            initial_history_ansi,
            &runtime_context.events,
            &runtime_context.render_notify,
            &runtime_context.render_dirty,
            &runtime_context.pane_teardowns,
        );

        match runtime_result {
            Ok(runtime) => {
                // No detected-agent seeding here: a pane with a resume plan
                // took the deferred branch above, so this shell has no agent
                // until detection or a hook reports one.
                let terminal = restored_terminal(
                    saved_pane,
                    RestoredPaneStart::Running {
                        duplicate_agent_session,
                    },
                    runtime_context.now,
                );
                // Its saves keep the saved screen until the shell launches,
                // and keep it if the launch fails.
                history_carry.carry_restored(&terminal.id, saved_history);
                panes.insert(
                    *id,
                    crate::workspace::WorkspacePane::new(PaneState::new(terminal.id.clone())),
                );
                terminal_runtimes.insert(terminal.id.clone(), runtime);
                terminals.push(terminal);
            }
            Err(e) => {
                // Nothing to roll back in the resumed-session set: only a pane
                // with a resume plan reserves its session, and such a pane
                // took the deferred branch above without spawning anything.
                error!(
                    workspace = %workspace_id,
                    pane_id = id.raw(),
                    error = %e,
                    "failed to restore pane"
                );
                let terminal = restored_terminal(
                    saved_pane,
                    RestoredPaneStart::Unavailable(RestoreFailure::shell_start_failed(&e)),
                    runtime_context.now,
                );
                history_carry.carry_restored(&terminal.id, saved_history);
                panes.insert(
                    *id,
                    crate::workspace::WorkspacePane::new(PaneState::new(terminal.id.clone())),
                );
                terminals.push(terminal);
            }
        }
    }

    // Every surviving pane is a saved layout pane, which
    // `assign_public_pane_numbers` numbered and counted into
    // `next_public_pane_number`; a pane without one means that invariant broke.
    for (pane_id, pane) in &mut panes {
        let Some(&public_number) = reverse_id_map
            .get(pane_id)
            .and_then(|old_raw| public_pane_numbers_by_old_raw.get(old_raw))
        else {
            error!(
                workspace = %workspace_id,
                pane_id = pane_id.raw(),
                "restored pane has no public number; dropping workspace"
            );
            return None;
        };
        pane.public_number = public_number;
    }

    let workspace = Workspace::from_restored(
        workspace_id,
        snap.custom_name.clone(),
        snap.identity_cwd.clone(),
        root_pane,
        layout,
        panes,
        zoomed,
        next_public_pane_number,
    )?;
    Some((workspace, terminals, terminal_runtimes))
}

fn pane_restore_startup<'a>(
    session: Option<&PaneAgentSessionSnapshot>,
    history: Option<&'a PaneHistorySnapshot>,
    agent_restore: &mut AgentRestoreState<'_>,
) -> PaneRestoreStartup<'a> {
    // Native agent resume owns the conversation history. If a pane has a
    // resumable agent session and resume is enabled, do not replay saved pane
    // presentation history into that terminal, even when this pane is a
    // duplicate suppressed by session de-duplication.
    let restore_plan =
        session.and_then(|session| restore_plan_for_snapshot(session, agent_restore.enabled));
    let has_native_agent_restore = restore_plan.is_some();
    // Reserve the session so later panes in the same restore pass cannot
    // launch the same native agent session. A reserving pane always defers its
    // launch, so no restore-time spawn failure can leave a stale reservation.
    let duplicate_agent_session = restore_plan.as_ref().is_some_and(|plan| {
        !agent_restore
            .resumed_sessions
            .insert(plan.dedupe_key.clone())
    });
    let restore_plan = if duplicate_agent_session {
        // The duplicate is accidental saved state. Nothing resumes in this
        // pane; it starts as a plain shell, so dropping its old agent screen
        // is acceptable and avoids showing a conversation it cannot own.
        None
    } else {
        restore_plan
    };

    PaneRestoreStartup {
        restore_plan,
        initial_history_ansi: if has_native_agent_restore {
            None
        } else {
            history.map(|history| history.ansi.as_str())
        },
        duplicate_agent_session,
    }
}

fn restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
) -> Option<shepr_agent::agent::resume::AgentResumePlan> {
    if !resume_agents_on_restore {
        return None;
    }
    let persisted = persisted_agent_session_from_snapshot(session)?;
    shepr_agent::agent::resume::plan(&persisted)
}

fn persisted_agent_session_from_snapshot(
    session: &PaneAgentSessionSnapshot,
) -> Option<shepr_agent::agent::resume::PersistedAgentSession> {
    shepr_agent::agent::resume::session_ref_from_snapshot(
        &session.source,
        session.agent,
        &session.session_ref,
    )
}

fn restored_terminal_agent_session(
    session: Option<&PaneAgentSessionSnapshot>,
    duplicate_agent_session: bool,
) -> Option<shepr_agent::agent::resume::PersistedAgentSession> {
    if duplicate_agent_session {
        return None;
    }
    session.and_then(persisted_agent_session_from_snapshot)
}

pub(super) fn prune_restored_node(node: Node, surviving: &HashSet<PaneId>) -> Option<Node> {
    match node {
        Node::Pane(id) => surviving.contains(&id).then_some(Node::Pane(id)),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first = prune_restored_node(*first, surviving);
            let second = prune_restored_node(*second, surviving);
            match (first, second) {
                (Some(first), Some(second)) => Some(Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            }
        }
    }
}

pub(super) fn resolve_restored_pane(
    saved_old_id: Option<u32>,
    id_map: &HashMap<u32, PaneId>,
    surviving: &HashSet<PaneId>,
    pane_ids: &[PaneId],
) -> Option<PaneId> {
    saved_old_id
        .and_then(|old_id| id_map.get(&old_id).copied())
        .filter(|pane_id| surviving.contains(pane_id))
        .or_else(|| pane_ids.first().copied())
}

/// Restore a layout tree, validating split ratios and remapping pane IDs.
/// Returns the new tree and a map of old_raw_id → new PaneId, or the saved
/// layout defect that prevented restoration.
///
/// The session file is plain JSON and may be hand-edited or damaged.
/// Invalid split ratios reject the saved layout rather than being clamped.
/// A saved pane ID that appears more than once maps only its first leaf.
/// Later copies get a fresh ID with
/// no saved pane behind it, and `restore_workspace` drops such leaves instead of
/// inventing a pane for them.
pub(super) fn restore_node_remapped(
    snap: &LayoutSnapshot,
) -> Result<(Node, HashMap<u32, PaneId>), InvalidSavedLayout> {
    let mut id_map = HashMap::new();
    let node = remap_inner(snap, &mut id_map)?;
    Ok((node, id_map))
}

fn remap_inner(
    snap: &LayoutSnapshot,
    id_map: &mut HashMap<u32, PaneId>,
) -> Result<Node, InvalidSavedLayout> {
    match snap {
        LayoutSnapshot::Pane(old_id) => {
            let new_id = PaneId::alloc();
            if id_map.contains_key(old_id) {
                warn!(
                    pane_id = old_id,
                    "saved layout repeats a pane; dropping the repeat"
                );
            } else {
                id_map.insert(*old_id, new_id);
            }
            Ok(Node::Pane(new_id))
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let ratio = SplitRatio::new(*ratio).ok_or(InvalidSavedLayout::InvalidSplitRatio)?;
            let first_node = remap_inner(first, id_map)?;
            let second_node = remap_inner(second, id_map)?;
            let dir = match direction {
                DirectionSnapshot::Horizontal => Direction::Horizontal,
                DirectionSnapshot::Vertical => Direction::Vertical,
            };
            Ok(Node::Split {
                direction: dir,
                ratio,
                first: Box::new(first_node),
                second: Box::new(second_node),
            })
        }
    }
}

/// Public pane numbers for every saved pane of a workspace, keyed by saved
/// pane ID. Every restored pane needs its public ID before its shell starts:
/// the ID goes into the shell's SHEPR identity environment, which agent hooks
/// use to report back. A saved pane without a number, or with the number zero
/// that no public ID can carry (a hand-edited or damaged file), gets the next
/// free one, in layout order.
fn assign_public_pane_numbers(
    snap: &WorkspaceSnapshot,
    next_public_pane_number: &mut usize,
) -> Option<HashMap<u32, usize>> {
    let mut numbers: HashMap<u32, usize> = snap
        .panes
        .iter()
        .filter_map(|(old_raw, pane)| {
            pane.public_number
                .filter(|number| *number > 0)
                .map(|number| (*old_raw, number))
        })
        .collect();
    let mut layout_panes = Vec::new();
    collect_snapshot_pane_ids(&snap.layout, &mut layout_panes);
    for old_raw in layout_panes {
        if snap.panes.contains_key(&old_raw) && !numbers.contains_key(&old_raw) {
            numbers.insert(old_raw, *next_public_pane_number);
            *next_public_pane_number = next_public_pane_number.checked_add(1)?;
        }
    }
    Some(numbers)
}

fn collect_snapshot_pane_ids(layout: &LayoutSnapshot, ids: &mut Vec<u32>) {
    match layout {
        LayoutSnapshot::Pane(id) => ids.push(*id),
        LayoutSnapshot::Split { first, second, .. } => {
            collect_snapshot_pane_ids(first, ids);
            collect_snapshot_pane_ids(second, ids);
        }
    }
}

pub(super) fn collect_pane_ids(node: &Node) -> Vec<PaneId> {
    let mut ids = Vec::new();
    collect_ids_inner(node, &mut ids);
    ids
}

fn collect_ids_inner(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids_inner(first, ids);
            collect_ids_inner(second, ids);
        }
    }
}

#[cfg(test)]
fn take_restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
    resumed_agent_sessions: &mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
) -> Option<shepr_agent::agent::resume::AgentResumePlan> {
    restore_plan_for_snapshot(session, resume_agents_on_restore)
        .filter(|plan| resumed_agent_sessions.insert(plan.dedupe_key.clone()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    std::thread_local! {
        static RESTORE_TEST_SCRATCH: crate::test_support::ScratchDir =
            crate::test_support::ScratchDir::new("restore-test-paths");
    }

    fn test_restore_now() -> std::time::Instant {
        std::time::Instant::now()
    }

    /// A resolved server socket for restored test panes; nothing listens on it.
    const TEST_SOCKET: &str = "/run/user/1000/shepr-test.sock";

    fn restore_test_path(name: &str) -> PathBuf {
        RESTORE_TEST_SCRATCH.with(|scratch| scratch.join(name))
    }

    fn test_session_path(name: &str) -> String {
        restore_test_path(name).display().to_string()
    }

    fn test_restore_shell() -> &'static str {
        shepr_test_support::fixture::idle_shell()
    }

    /// Workspaces laid out in `rows` by `cols` cells with no pane chrome, so a
    /// workspace's only pane is exactly that size.
    fn test_geometry(rows: u16, cols: u16) -> crate::workspace::PaneGeometry {
        crate::workspace::PaneGeometry {
            area: ratatui::layout::Rect::new(0, 0, cols, rows),
            pane_borders: shepr_config::PaneBordersConfig::Off,
            pane_gaps: false,
            pane_outer_borders: false,
            pane_scrollbars: false,
        }
    }

    #[test]
    fn capture_and_restore_node_round_trip() {
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: shepr_core::layout::SplitRatio::clamped(0.5),
            first: Box::new(Node::Pane(shepr_test_fixtures::fixed_pane_id(3))),
            second: Box::new(Node::Split {
                direction: Direction::Vertical,
                ratio: shepr_core::layout::SplitRatio::clamped(0.3),
                first: Box::new(Node::Pane(shepr_test_fixtures::fixed_pane_id(1))),
                second: Box::new(Node::Pane(shepr_test_fixtures::fixed_pane_id(2))),
            }),
        };

        let snap = super::super::snapshot::capture_node(&node);
        let (restored, id_map) = restore_node_remapped(&snap).expect("valid snapshot ratios");

        assert_eq!(id_map.len(), 3);
        let ids = collect_pane_ids(&restored);
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<u32> = ids.iter().map(|id| id.raw()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn restored_split_ratios_reject_invalid_saved_values() {
        for saved in [f32::NAN, f32::INFINITY, 5.0, -1.0] {
            let snap = LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: saved,
                first: Box::new(LayoutSnapshot::Pane(0)),
                second: Box::new(LayoutSnapshot::Pane(1)),
            };
            assert!(
                matches!(
                    restore_node_remapped(&snap),
                    Err(InvalidSavedLayout::InvalidSplitRatio)
                ),
                "saved ratio {saved}"
            );
        }

        let snap = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(0)),
            second: Box::new(LayoutSnapshot::Pane(1)),
        };
        let (node, _) = restore_node_remapped(&snap).expect("valid saved ratio");
        let Node::Split { ratio, .. } = node else {
            panic!("expected split");
        };
        assert_eq!(ratio.get(), 0.5);
    }

    #[test]
    fn repeated_saved_pane_maps_only_its_first_leaf() {
        let snap = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(4)),
            second: Box::new(LayoutSnapshot::Pane(4)),
        };
        let (node, id_map) = restore_node_remapped(&snap).expect("valid snapshot ratios");
        let ids = collect_pane_ids(&node);
        assert_eq!(ids.len(), 2);
        assert_eq!(id_map.len(), 1);
        assert_eq!(id_map.get(&4), ids.first());
    }

    #[tokio::test]
    async fn restore_drops_layout_leaves_without_saved_state() {
        let scratch = crate::test_support::ScratchDir::new("restore-drop-layout-leaves");
        let (mut snapshot, _) = snapshot_with_saved_pane_history(scratch.path());
        let cwd = snapshot.workspaces[0].panes[&0].cwd.clone();
        // Pane 0 appears twice and pane 7 has no entry in `panes`.
        snapshot.workspaces[0].layout = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(0)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(7)),
                second: Box::new(LayoutSnapshot::Pane(0)),
            }),
        };
        let (events, _rx) = mpsc::channel(8);
        let RestoredSession {
            workspaces,
            terminals,
            terminal_runtimes: runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(5, 40),
            4096,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );
        let workspace = &workspaces[0];
        assert_eq!(workspace.layout().pane_ids(), vec![workspace.root_pane()]);
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(terminals.len(), 1);
        let mut runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
        let captured = crate::persist::capture(
            &workspaces,
            &terminals,
            &runtimes,
            std::path::Path::new("/"),
            Some(0),
            Default::default(),
        );
        let panes = &captured.workspaces[0].panes;
        assert_eq!(panes.len(), 1);
        assert!(panes.values().all(|pane| pane.cwd == cwd));
        for (_, runtime) in runtimes.drain() {
            drop(runtime);
        }
    }

    /// Restored panes keep every saved field whichever way they come back, and
    /// a pane without a runtime keeps its saved screen history in later saves
    /// until a runtime of its own replaces it.
    #[tokio::test]
    async fn restored_panes_keep_saved_fields_and_runtimeless_history() {
        // (resume agents, saved cwd missing, shell missing)
        for (resume, missing_cwd, missing_shell) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let scratch = crate::test_support::ScratchDir::new("restore-runtime-history");
            let (mut snapshot, history) = snapshot_with_saved_pane_history(scratch.path());
            let pane = snapshot.workspaces[0]
                .panes
                .get_mut(&0)
                .expect("test precondition");
            pane.label = Some("keep me".into());
            pane.agent_session = Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: shepr_agent::agent::AgentSource::parse("shepr:codex"),
                agent: shepr_agent::agent::Agent::Codex,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            });
            if missing_cwd {
                pane.cwd = pane.cwd.join("__shepr_missing_restore_directory__");
                assert!(!pane.cwd.try_exists().expect("test stat"));
            }
            let saved_cwd = pane.cwd.clone();
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                workspaces,
                terminals,
                terminal_runtimes: runtimes,
                mut history_carry,
                ..
            } = restore(
                &snapshot,
                Some(&history),
                test_geometry(5, 40),
                4096,
                crate::pane::PaneShellConfig::new(
                    if missing_shell {
                        "__shepr_missing_restore_shell__"
                    } else {
                        test_restore_shell()
                    },
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
            // directory that is gone leaves one whose shell never launches,
            // which keeps its carried history just the same.
            let runtimeless = resume || missing_shell;
            assert_eq!(runtimes.is_empty(), runtimeless, "{case}");
            let carried = runtimeless || missing_cwd;
            let mut runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
            let captured = crate::persist::capture(
                &workspaces,
                &terminals,
                &runtimes,
                std::path::Path::new("/"),
                Some(0),
                Default::default(),
            );
            let pane = captured.workspaces[0]
                .panes
                .values()
                .next()
                .expect("test precondition");
            assert_eq!(pane.label.as_deref(), Some("keep me"), "{case}");
            assert_eq!(pane.cwd, saved_cwd, "{case}");
            assert_eq!(
                pane.agent_session
                    .as_ref()
                    .map(|session| session.session_ref.value_str()),
                Some("codex-session"),
                "{case}"
            );

            if carried {
                let saved =
                    crate::persist::capture_history(&workspaces, &runtimes, &mut history_carry);
                let pane_history = saved.workspaces[0]
                    .panes
                    .values()
                    .next()
                    .expect("a pane without a runtime keeps its saved history");
                assert!(pane_history.ansi.contains("RESTORED_HISTORY"));

                // Once the pane runs, its live screen supersedes the carried one
                // for good.
                let root_pane = workspaces[0].root_pane();
                let terminal_id = workspaces[0]
                    .terminal_id(root_pane)
                    .expect("test precondition");
                runtimes.insert(
                    terminal_id.clone(),
                    crate::pane::PaneRuntime::test_with_scrollback_bytes(
                        20,
                        3,
                        4096,
                        b"LIVE_SCREEN\r\n",
                    ),
                );
                let saved =
                    crate::persist::capture_history(&workspaces, &runtimes, &mut history_carry);
                let live = &saved.workspaces[0].panes[&root_pane.raw()];
                assert!(live.ansi.contains("LIVE_SCREEN"));
                assert!(!live.ansi.contains("RESTORED_HISTORY"));
                // Should the pane lose its runtime again, what it keeps is its
                // own last screen, never the restored history.
                runtimes.remove(terminal_id);
                let saved =
                    crate::persist::capture_history(&workspaces, &runtimes, &mut history_carry);
                let kept = &saved.workspaces[0].panes[&root_pane.raw()];
                assert!(kept.ansi.contains("LIVE_SCREEN"));
                assert!(!kept.ansi.contains("RESTORED_HISTORY"));
            }
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

    /// A pane snapshot whose saved directory does not exist.
    fn runtimeless_pane() -> super::super::snapshot::PaneSnapshot {
        let cwd = restore_test_path("__shepr_missing_restore_directory__");
        assert!(!cwd.try_exists().expect("test stat"));
        super::super::snapshot::PaneSnapshot {
            cwd,
            public_number: None,
            label: None,
            agent_session: None,
        }
    }

    /// A workspace with one kept pane per ID in `panes`; `layout` may name IDs
    /// without a saved pane, which restore drops.
    fn workspace_snapshot(
        id: Option<&str>,
        name: &str,
        layout: LayoutSnapshot,
        panes: &[u32],
    ) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.map(str::to_string),
            custom_name: Some(name.into()),
            identity_cwd: PathBuf::from("/"),
            next_public_pane_number: 0,
            layout,
            panes: panes.iter().map(|id| (*id, runtimeless_pane())).collect(),
            zoomed: false,
            focused: None,
            root_pane: None,
        }
    }

    #[test]
    fn restore_plans_reject_collisions_and_exhaustion_before_execution() {
        let mut snap = workspace_snapshot(
            Some("w1"),
            "numbers",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            },
            &[1, 2],
        );
        for pane in snap.panes.values_mut() {
            pane.public_number = Some(7);
        }
        assert!(plan_workspace(&snap).is_none());
        snap.panes.get_mut(&2).expect("pane").public_number = None;
        snap.next_public_pane_number = usize::MAX;
        assert!(plan_workspace(&snap).is_none());
        snap.next_public_pane_number = 8;
        snap.panes.get_mut(&1).expect("pane").public_number = Some(usize::MAX);
        assert!(plan_workspace(&snap).is_none());
    }

    #[test]
    fn restore_plans_prune_relative_panes_and_recover_identity_cwd() {
        let mut snap = workspace_snapshot(
            Some("w1"),
            "paths",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            },
            &[1, 2],
        );
        snap.identity_cwd = PathBuf::from("relative");
        snap.panes.get_mut(&1).expect("pane").cwd = PathBuf::from("relative");
        snap.panes.get_mut(&2).expect("pane").cwd = PathBuf::from("/surviving");
        let plan = plan_workspace(&snap).expect("healthy pane survives");
        assert_eq!(plan.snapshot.panes.len(), 1);
        assert_eq!(plan.snapshot.identity_cwd, PathBuf::from("/surviving"));
        assert_eq!(plan.pane_ids.len(), 1);
        assert!(plan.restore_damage);
    }

    /// Restores with a shell the launch refuses before any fork, so every pane
    /// comes back as a placeholder without a runtime.
    fn restore_runtimeless(snapshot: &SessionSnapshot) -> RestoredSession {
        let (events, _rx) = mpsc::channel(8);
        let restored = restore(
            snapshot,
            None,
            test_geometry(5, 40),
            0,
            crate::pane::PaneShellConfig::new("__shepr_refused_restore_shell__", false),
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

    /// An invalid saved ratio drops only its own workspace: the rest of the
    /// session restores, the saved bookmarked workspace still resolves, and the
    /// drop is counted so the caller backs the saved file up before the first
    /// save.
    #[test]
    fn restore_drops_only_the_workspace_with_an_invalid_split_ratio() {
        let invalid_workspace = |id: &str, name: &str, ratio: f32| {
            workspace_snapshot(
                Some(id),
                name,
                LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Horizontal,
                    ratio,
                    first: Box::new(LayoutSnapshot::Pane(1)),
                    second: Box::new(LayoutSnapshot::Pane(2)),
                },
                &[1, 2],
            )
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                invalid_workspace("w1", "out of range", 1.0),
                workspace_snapshot(Some("w2"), "healthy", LayoutSnapshot::Pane(3), &[3]),
                invalid_workspace("w3", "not finite", f32::NAN),
            ],
            active: Some(2),
        };

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(restored.dropped_workspaces, 2);
        assert_eq!(restored.workspaces.len(), 1);
        let workspace = &restored.workspaces[0];
        assert_eq!(workspace.custom_name.as_deref(), Some("healthy"));
        assert_eq!(restored.active, Some(0));
        assert_eq!(restored.terminals.len(), 1);
    }

    /// Capture writes one number per pane, so two panes sharing one is a
    /// damaged file; the workspace is dropped like any other defect rather
    /// than renumbered, and the first save backs the original up.
    #[test]
    fn restore_drops_a_workspace_whose_panes_share_a_public_number() {
        let mut duplicated = workspace_snapshot(
            Some("w1"),
            "duplicated",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            },
            &[1, 2],
        );
        for pane in duplicated.panes.values_mut() {
            pane.public_number = Some(3);
        }
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                duplicated,
                workspace_snapshot(Some("w2"), "healthy", LayoutSnapshot::Pane(3), &[3]),
            ],
            active: Some(1),
        };

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(restored.dropped_workspaces, 1);
        let names: Vec<_> = restored
            .workspaces
            .iter()
            .map(|ws| ws.custom_name.as_deref())
            .collect();
        assert_eq!(names, vec![Some("healthy")]);
    }

    #[test]
    fn dropped_workspaces_do_not_shift_the_saved_bookmark() {
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                // Nothing survives: the layout names a pane with no saved state.
                workspace_snapshot(Some("w1"), "dropped", LayoutSnapshot::Pane(1), &[]),
                workspace_snapshot(Some("w2"), "kept", LayoutSnapshot::Pane(2), &[2]),
                workspace_snapshot(Some("w3"), "gone", LayoutSnapshot::Pane(4), &[]),
                workspace_snapshot(Some("w4"), "active", LayoutSnapshot::Pane(5), &[5]),
            ],
            active: Some(3),
        };

        let restored = restore_runtimeless(&snapshot);

        let names: Vec<_> = restored
            .workspaces
            .iter()
            .map(|ws| ws.custom_name.as_deref())
            .collect();
        assert_eq!(names, vec![Some("kept"), Some("active")]);
        assert_eq!(restored.dropped_workspaces, 2);
        assert_eq!(restored.active, Some(1));
    }

    #[test]
    fn a_dropped_active_workspace_falls_back_to_its_neighbour() {
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace_snapshot(Some("w1"), "before", LayoutSnapshot::Pane(1), &[1]),
                workspace_snapshot(Some("w2"), "dropped", LayoutSnapshot::Pane(2), &[]),
            ],
            active: Some(1),
        };

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.active, Some(0));
    }

    #[test]
    fn zoom_does_not_survive_pruning_to_one_pane_or_losing_the_zoomed_pane() {
        let split = |first: u32, second: u32, third: u32| LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(first)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(second)),
                second: Box::new(LayoutSnapshot::Pane(third)),
            }),
        };
        // (saved panes, focused, expected zoom)
        for (panes, focused, zoomed) in [
            (&[1, 2, 3][..], 2, true),
            // Pruned to a single pane.
            (&[1][..], 1, false),
            // The zoomed pane itself is gone.
            (&[1, 3][..], 2, false),
            // Another pane is gone; the zoomed one stays zoomed.
            (&[1, 2][..], 2, true),
        ] {
            let mut workspace = workspace_snapshot(Some("w1"), "ws", split(1, 2, 3), panes);
            workspace.zoomed = true;
            workspace.focused = Some(focused);
            let snapshot = SessionSnapshot {
                version: super::super::snapshot::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![workspace],
                active: Some(0),
            };

            let restored = restore_runtimeless(&snapshot);

            let workspace = &restored.workspaces[0];
            assert_eq!(
                workspace.zoomed(),
                zoomed,
                "panes={panes:?} focused={focused}"
            );
            workspace.assert_invariants_for_test();
        }
    }

    #[test]
    fn restored_workspace_ids_are_unique() {
        // The ID the counter would hand out next is also saved on a later
        // workspace; a third workspace repeats that saved ID.
        let probe = crate::workspace::generate_workspace_id();
        let taken = WorkspaceId::from_number(probe.number() + 1)
            .expect("test precondition")
            .to_string();
        let workspace = |id: Option<&str>, name: &str, pane: u32| {
            workspace_snapshot(id, name, LayoutSnapshot::Pane(pane), &[pane])
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace(None, "unsaved id", 1),
                workspace(Some(&taken), "owner", 2),
                workspace(Some(&taken), "repeat", 3),
                workspace(Some(""), "empty id", 4),
                workspace(Some("ws_1"), "non-canonical id", 5),
            ],
            active: Some(0),
        };

        let restored = restore_runtimeless(&snapshot);

        let ids: Vec<_> = restored.workspaces.iter().map(|ws| ws.id.clone()).collect();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids[1], taken, "the first owner of a saved ID keeps it");
        assert_ne!(ids[4], "ws_1", "a non-canonical saved ID is replaced");
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        // A later new workspace does not reuse any restored ID either.
        let fresh = crate::workspace::generate_workspace_id();
        assert!(!ids.contains(&fresh), "fresh workspace id reused: {ids:?}");
    }

    #[test]
    fn prune_restored_node_collapses_missing_branch() {
        let keep = shepr_test_fixtures::fixed_pane_id(11);
        let missing = shepr_test_fixtures::fixed_pane_id(12);
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: shepr_core::layout::SplitRatio::clamped(0.5),
            first: Box::new(Node::Pane(keep)),
            second: Box::new(Node::Pane(missing)),
        };
        let surviving = std::collections::HashSet::from([keep]);

        let pruned = prune_restored_node(node, &surviving).expect("remaining pane should survive");

        assert!(matches!(pruned, Node::Pane(id) if id == keep));
    }

    #[test]
    fn resolve_restored_pane_prefers_surviving_saved_id_and_falls_back_to_first_remaining() {
        let first = shepr_test_fixtures::fixed_pane_id(21);
        let second = shepr_test_fixtures::fixed_pane_id(22);
        let id_map = HashMap::from([(0_u32, first), (1_u32, second)]);
        let surviving = std::collections::HashSet::from([first]);
        let pane_ids = vec![first];

        assert_eq!(
            resolve_restored_pane(Some(0), &id_map, &surviving, &pane_ids),
            Some(first)
        );
        assert_eq!(
            resolve_restored_pane(Some(1), &id_map, &surviving, &pane_ids),
            Some(first)
        );
    }

    #[test]
    fn restore_plan_respects_opt_in_and_allowlist() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(pi_session_path.clone())
                .expect("test precondition"),
        };

        assert!(restore_plan_for_snapshot(&session, false).is_none());
        assert_eq!(
            restore_plan_for_snapshot(&session, true)
                .expect("test precondition")
                .argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );

        let unsupported_path = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:claude"),
            agent: shepr_agent::agent::Agent::Claude,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "claude-session",
            ))
            .expect("test precondition"),
        };
        assert!(restore_plan_for_snapshot(&unsupported_path, true).is_none());
    }

    #[test]
    fn restore_plan_selection_suppresses_duplicates() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(pi_session_path.clone())
                .expect("test precondition"),
        };
        let mut resumed = HashSet::new();

        assert!(take_restore_plan_for_snapshot(&session, false, &mut resumed).is_none());
        assert!(resumed.is_empty());

        let first = take_restore_plan_for_snapshot(&session, true, &mut resumed)
            .expect("first restore should get a plan");
        assert_eq!(
            first.argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "pi-session.jsonl",
            ))
            .expect("test precondition"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_some());
        assert!(startup.initial_history_ansi.is_none());
        assert!(!startup.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_duplicate_native_agent_session() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "pi-session.jsonl",
            ))
            .expect("test precondition"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let first = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);
        let duplicate = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(first.restore_plan.is_some());
        assert!(first.initial_history_ansi.is_none());
        assert!(duplicate.restore_plan.is_none());
        assert!(duplicate.initial_history_ansi.is_none());
        assert!(duplicate.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_keeps_history_without_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "pi-session.jsonl",
            ))
            .expect("test precondition"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: false,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_none());
        assert_eq!(startup.initial_history_ansi, Some("RESTORED_HISTORY\r\n"));
        assert!(!startup.duplicate_agent_session);
        assert!(resumed.is_empty());
    }

    #[test]
    fn restore_rehydrates_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:codex"),
            agent: shepr_agent::agent::Agent::Codex,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                .expect("test precondition"),
        };

        let preserved = restored_terminal_agent_session(Some(&session), false)
            .expect("restore should preserve metadata");
        assert_eq!(preserved.source.as_str(), "shepr:codex");
        assert_eq!(preserved.agent.label(), "codex");
        assert_eq!(preserved.session_ref.value(), "codex-session");
    }

    #[test]
    fn restore_does_not_rehydrate_duplicate_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::parse("shepr:pi"),
            agent: shepr_agent::agent::Agent::Pi,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "pi-session.jsonl",
            ))
            .expect("test precondition"),
        };
        let mut resumed = HashSet::new();
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_some());
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());

        assert!(restored_terminal_agent_session(Some(&session), true).is_none());
    }

    #[tokio::test]
    async fn failed_cold_restore_preserves_panes_and_saved_directories() {
        for missing_shell in [false, true] {
            let mut snapshot: SessionSnapshot = serde_json::from_value(serde_json::json!({
                "version": super::super::snapshot::SNAPSHOT_VERSION,
                "workspaces": [
                    {
                        "id": "workspace-a",
                        "identity_cwd": "/tmp/shepr-restore-test-a",
                        "layout": { "Pane": 1 },
                        "panes": { "1": { "cwd": "/tmp/shepr-restore-test-a" } },
                        "zoomed": false,
                        "focused": 1,
                        "root_pane": 1
                    },
                    {
                        "id": "workspace-b",
                        "identity_cwd": "/tmp/shepr-restore-test-b",
                        "layout": { "Pane": 3 },
                        "panes": { "3": { "cwd": "/tmp/shepr-restore-test-b" } },
                        "zoomed": false,
                        "focused": 3,
                        "root_pane": 3
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
                workspace.identity_cwd = cwd.clone();
                for pane in workspace.panes.values_mut() {
                    pane.cwd = cwd.clone();
                }
            }
            let failed = snapshot.workspaces[0]
                .panes
                .get_mut(&1)
                .expect("test precondition");
            failed.cwd = missing.clone();
            failed.label = Some("keep my pane".into());
            failed.agent_session = Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: shepr_agent::agent::AgentSource::parse("shepr:opencode"),
                agent: shepr_agent::agent::Agent::OpenCode,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("keep-my-session")
                    .expect("test precondition"),
            });
            let (events, mut events_rx) = mpsc::channel(32);
            let RestoredSession {
                workspaces,
                terminals,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                None,
                test_geometry(24, 80),
                0,
                crate::pane::PaneShellConfig::new(
                    if missing_shell {
                        "__shepr_missing_restore_shell__"
                    } else {
                        test_restore_shell()
                    },
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
            let captured = crate::persist::capture(
                &workspaces,
                &terminals,
                &runtimes,
                std::path::Path::new("/"),
                Some(0),
                Default::default(),
            );
            assert_eq!(
                captured.workspaces.len(),
                2,
                "a launch failure must not delete a workspace"
            );
            let pane = captured.workspaces[0]
                .panes
                .values()
                .next()
                .expect("test precondition");
            assert_eq!(
                pane.cwd, missing,
                "fallback cwd must not replace saved intent"
            );
            assert_eq!(pane.label.as_deref(), Some("keep my pane"));
            assert_eq!(
                pane.agent_session
                    .as_ref()
                    .expect("test precondition")
                    .session_ref
                    .value_str(),
                "keep-my-session"
            );
            let root = workspaces[0].root_pane();
            let terminal_id = workspaces[0].terminal_id(root).expect("test precondition");
            if missing_shell {
                // The launch is refused before any fork.
                assert!(runtimes.get(terminal_id).is_none());
                assert!(terminals[terminal_id].restore_error.is_some());
            } else {
                // The child's chdir finds the saved directory gone and the
                // launch settles as a failure, never in another directory.
                assert!(matches!(
                    launch_settlement(&mut events_rx, root).await,
                    crate::pane::LaunchSettlement::Failed(RestoreFailure::DirectoryUnavailable {
                        ref path
                    }) if *path == missing
                ));
                assert!(
                    !runtimes
                        .get(terminal_id)
                        .is_some_and(crate::pane::PaneRuntime::launched),
                    "do not open a replacement shell elsewhere"
                );
            }
            let healthy = workspaces[1]
                .terminal_id(workspaces[1].root_pane())
                .expect("test precondition");
            assert_eq!(runtimes.get(healthy).is_some(), !missing_shell);
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
            let crate::events::AppEvent::Runtime { event, .. } = event else {
                continue;
            };
            if let crate::events::AppEvent::PaneLaunchSettled {
                pane_id,
                settlement,
            } = *event
                && pane_id == pane
            {
                return settlement;
            }
        }
    }

    #[tokio::test]
    async fn restore_carries_persisted_agent_session_metadata() {
        let scratch = crate::test_support::ScratchDir::new("restore-agent-metadata-cwd");
        let cwd = scratch.to_path_buf();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                next_public_pane_number: 0,
                layout: LayoutSnapshot::Pane(0),
                panes: HashMap::from([(
                    0,
                    super::super::snapshot::PaneSnapshot {
                        cwd,
                        public_number: None,
                        label: Some("reviewer".into()),
                        agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                            source: shepr_agent::agent::AgentSource::parse("shepr:opencode"),
                            agent: shepr_agent::agent::Agent::OpenCode,
                            session_ref: shepr_agent::agent::resume::AgentSessionRef::id(
                                "opencode-session",
                            )
                            .expect("test precondition"),
                        }),
                    },
                )]),
                zoomed: false,
                focused: Some(0),
                root_pane: Some(0),
            }],
            active: Some(0),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces: _workspaces,
            terminals,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("restored terminal should exist");
        assert_eq!(terminal.manual_label.as_deref(), Some("reviewer"));
        let session = terminal
            .persisted_agent_session()
            .expect("persisted agent session should survive restore");
        assert_eq!(session.source.as_str(), "shepr:opencode");
        assert_eq!(session.agent.label(), "opencode");
        assert_eq!(session.session_ref.value(), "opencode-session");
    }

    #[tokio::test]
    async fn restore_preserves_public_id_mapping_after_pane_id_remap() {
        let scratch = crate::test_support::ScratchDir::new("restore-public-id-cwd");
        let cwd = scratch.to_path_buf();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                next_public_pane_number: 4,
                layout: LayoutSnapshot::Split {
                    direction: super::super::snapshot::DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(10)),
                    second: Box::new(LayoutSnapshot::Pane(20)),
                },
                panes: HashMap::from([
                    (
                        10,
                        super::super::snapshot::PaneSnapshot {
                            cwd: cwd.clone(),
                            public_number: Some(1),
                            label: None,
                            agent_session: None,
                        },
                    ),
                    (
                        20,
                        super::super::snapshot::PaneSnapshot {
                            cwd: cwd.clone(),
                            public_number: Some(3),
                            label: None,
                            agent_session: None,
                        },
                    ),
                ]),
                zoomed: false,
                focused: Some(10),
                root_pane: Some(10),
            }],
            active: Some(0),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminals: _terminals,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
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
            .panes()
            .values()
            .map(|pane| pane.public_number)
            .collect();
        public_numbers.sort_unstable();
        assert_eq!(public_numbers, vec![1, 3]);
        assert_eq!(workspace.next_public_pane_number, 4);
    }

    #[test]
    fn every_saved_pane_gets_a_public_number_before_its_shell_starts() {
        let pane = |public_number| super::super::snapshot::PaneSnapshot {
            cwd: PathBuf::from("/"),
            public_number,
            label: None,
            agent_session: None,
        };
        let snap = WorkspaceSnapshot {
            id: Some("w1".into()),
            custom_name: None,
            identity_cwd: PathBuf::from("/"),
            next_public_pane_number: 5,
            layout: LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(30)),
                second: Box::new(LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Vertical,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(10)),
                    second: Box::new(LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Vertical,
                        ratio: 0.5,
                        // A leaf with no saved pane is dropped by restore
                        // and needs no number.
                        first: Box::new(LayoutSnapshot::Pane(99)),
                        second: Box::new(LayoutSnapshot::Pane(20)),
                    }),
                }),
            },
            // Only pane 10 kept its number; 30 and 20 lost theirs.
            panes: HashMap::from([(10, pane(Some(4))), (30, pane(None)), (20, pane(None))]),
            zoomed: false,
            focused: None,
            root_pane: None,
        };
        let mut next = 5;

        let numbers = assign_public_pane_numbers(&snap, &mut next).expect("available numbers");

        assert_eq!(numbers, HashMap::from([(10, 4), (30, 5), (20, 6)]));
        assert_eq!(next, 7);
    }

    #[test]
    fn a_saved_zero_pane_number_is_replaced_instead_of_aborting() {
        let snap = WorkspaceSnapshot {
            id: Some("w1".into()),
            custom_name: None,
            identity_cwd: PathBuf::from("/"),
            next_public_pane_number: 1,
            layout: LayoutSnapshot::Pane(10),
            panes: HashMap::from([(
                10,
                super::super::snapshot::PaneSnapshot {
                    cwd: PathBuf::from("/"),
                    public_number: Some(0),
                    label: None,
                    agent_session: None,
                },
            )]),
            zoomed: false,
            focused: None,
            root_pane: None,
        };
        let mut next = 1;

        let numbers = assign_public_pane_numbers(&snap, &mut next).expect("available numbers");

        assert_eq!(numbers, HashMap::from([(10, 1)]));
        assert_eq!(next, 2);
    }

    #[tokio::test]
    async fn cold_restore_with_gapped_public_pane_numbers_starts_a_plain_shell_without_an_agent() {
        let scratch = crate::test_support::ScratchDir::new("restore-public-pane-cwd");
        let cwd = scratch.to_path_buf();
        let pane_snap = |id: u32, public_number: usize| {
            (
                id,
                super::super::snapshot::PaneSnapshot {
                    cwd: cwd.clone(),
                    public_number: Some(public_number),
                    label: None,
                    agent_session: None,
                },
            )
        };
        let final_pane = super::super::snapshot::PaneSnapshot {
            cwd: cwd.clone(),
            public_number: Some(7),
            label: Some("planner".into()),
            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: shepr_agent::agent::AgentSource::parse("shepr:codex"),
                agent: shepr_agent::agent::Agent::Codex,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            }),
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                // Numbers 1 to 3 were public panes that are gone.
                next_public_pane_number: 8,
                layout: LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(10)),
                    second: Box::new(LayoutSnapshot::Pane(13)),
                },
                panes: HashMap::from([pane_snap(10, 4), (13, final_pane)]),
                zoomed: false,
                focused: Some(13),
                root_pane: Some(10),
            }],
            active: Some(0),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces,
            terminals,
            terminal_runtimes: _runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let agent_pane = workspace.focused_pane_id();
        assert_eq!(workspace.public_pane_number(agent_pane), Some(7));
        let terminal_id = workspace
            .terminal_id(agent_pane)
            .expect("restored agent pane");
        assert!(terminals[terminal_id].effective_agent_label().is_none());
    }

    #[tokio::test]
    async fn native_agent_restore_defers_runtime_launch() {
        let scratch = crate::test_support::ScratchDir::new("restore-native-agent-cwd");
        let cwd = scratch.to_path_buf();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                next_public_pane_number: 0,
                layout: LayoutSnapshot::Pane(0),
                panes: HashMap::from([(
                    0,
                    super::super::snapshot::PaneSnapshot {
                        cwd,
                        public_number: None,
                        label: None,
                        agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                            source: shepr_agent::agent::AgentSource::parse("shepr:codex"),
                            agent: shepr_agent::agent::Agent::Codex,
                            session_ref: shepr_agent::agent::resume::AgentSessionRef::id(
                                "codex-session",
                            )
                            .expect("test precondition"),
                        }),
                    },
                )]),
                zoomed: false,
                focused: Some(0),
                root_pane: Some(0),
            }],
            active: Some(0),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let RestoredSession {
            workspaces: _workspaces,
            terminals,
            terminal_runtimes: runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(24, 80),
            0,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            true,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
            &Arc::default(),
            test_restore_now(),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("native agent restore should create terminal state");
        assert!(
            terminal.pending_agent_resume_plan.is_some(),
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
            let pane = || super::super::snapshot::PaneSnapshot {
                cwd: scratch.to_path_buf(),
                public_number: None,
                label: None,
                agent_session: None,
            };
            let snapshot = SessionSnapshot {
                version: super::super::snapshot::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![WorkspaceSnapshot {
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.25,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
                    panes: HashMap::from([(0, pane()), (1, pane())]),
                    zoomed,
                    focused: Some(1),
                    root_pane: Some(0),
                    ..workspace_snapshot(Some("w1"), "split", LayoutSnapshot::Pane(0), &[])
                }],
                active: Some(0),
            };
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                workspaces,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                None,
                test_geometry(24, 80),
                0,
                crate::pane::PaneShellConfig::new(test_restore_shell(), false),
                std::path::Path::new(TEST_SOCKET),
                false,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
                &Arc::default(),
                test_restore_now(),
            );
            let workspace = &workspaces[0];
            assert_eq!(workspace.zoomed(), zoomed);
            let size = |pane_id| {
                let terminal = workspace.terminal_id(pane_id).expect("test precondition");
                runtimes
                    .get(terminal)
                    .expect("restored runtime")
                    .current_size()
            };
            let focused = workspace.layout().focused();
            let other = workspace.root_pane();
            assert_ne!(focused, other);
            let (other_rows, other_cols) = size(other);
            let (focused_rows, focused_cols) = size(focused);
            assert_eq!((other_rows, focused_rows), (24, 24), "zoomed={zoomed}");
            // The first pane has a quarter of the width in the tiled layout.
            assert!(other_cols < 40, "zoomed={zoomed} cols={other_cols}");
            if zoomed {
                assert_eq!(focused_cols, 80);
            } else {
                assert_eq!(other_cols + focused_cols, 80);
            }
            for (_, runtime) in runtimes {
                drop(runtime);
            }
        }
    }

    #[tokio::test]
    async fn restore_seeds_saved_pane_history_into_runtime() {
        let scratch = crate::test_support::ScratchDir::new("restore-seed-pane-history");
        let (snapshot, history) = snapshot_with_saved_pane_history(scratch.path());
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());

        let RestoredSession {
            workspaces: _workspaces,
            terminals: _terminals,
            terminal_runtimes: runtimes,
            ..
        } = restore(
            &snapshot,
            Some(&history),
            test_geometry(5, 40),
            4096,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &render_notify,
            &render_dirty,
            &Arc::default(),
            test_restore_now(),
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        assert!(
            !runtime
                .agent_detection_inputs()
                .screen_text
                .contains("RESTORED_HISTORY"),
            "saved display history must not become live detection evidence"
        );
        let restored_text = runtime.recent_unwrapped_text(10);
        assert!(
            restored_text
                .contains("RESTORED_HISTORY \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} LINK"),
            "styled Unicode and hyperlink text should survive history replay"
        );
    }

    #[tokio::test]
    async fn restore_without_history_snapshot_keeps_pane_contents_empty() {
        let scratch = crate::test_support::ScratchDir::new("restore-without-pane-history");
        let (snapshot, _history) = snapshot_with_saved_pane_history(scratch.path());
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());

        let RestoredSession {
            workspaces: _workspaces,
            terminals: _terminals,
            terminal_runtimes: runtimes,
            ..
        } = restore(
            &snapshot,
            None,
            test_geometry(5, 40),
            4096,
            crate::pane::PaneShellConfig::new(test_restore_shell(), false),
            std::path::Path::new(TEST_SOCKET),
            false,
            &events,
            &render_notify,
            &render_dirty,
            &Arc::default(),
            test_restore_now(),
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        assert!(
            !runtime
                .recent_unwrapped_text(10)
                .contains("RESTORED_HISTORY"),
            "pane history should not restore unless a history snapshot is supplied"
        );
    }

    fn snapshot_with_saved_pane_history(cwd: &Path) -> (SessionSnapshot, SessionHistorySnapshot) {
        let cwd = cwd.to_path_buf();
        let mut panes = HashMap::new();
        panes.insert(
            0,
            super::super::snapshot::PaneSnapshot {
                cwd: cwd.clone(),
                public_number: None,
                label: None,
                agent_session: None,
            },
        );
        let history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceHistorySnapshot {
                panes: HashMap::from([(
                    0,
                    super::super::snapshot::PaneHistorySnapshot {
                        ansi: concat!(
                            "\x1b[31mRESTORED_HISTORY \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\x1b[0m ",
                            "\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\"
                        )
                        .to_string(),
                    },
                )]),
            }],
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd,
                next_public_pane_number: 0,
                layout: LayoutSnapshot::Pane(0),
                panes,
                zoomed: false,
                focused: Some(0),
                root_pane: Some(0),
            }],
            active: Some(0),
        };
        (snapshot, history)
    }
}
