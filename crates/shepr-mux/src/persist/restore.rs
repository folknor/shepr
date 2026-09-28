use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{Notify, mpsc};
use tracing::{error, warn};

use crate::events::AppEvent;
use crate::pane::PaneRuntime;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalState;
use crate::workspace::Workspace;
use shepr_agent::detect::AgentState;
use shepr_core::layout::{Node, PaneId, TileLayout};
use shepr_protocol::TerminalId;

use super::snapshot::{
    HistoryCarry, PaneAgentSessionSnapshot, PaneHistorySnapshot, TabHistorySnapshot,
    WorkspaceHistorySnapshot,
};
use super::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, TabSnapshot,
    WorkspaceSnapshot,
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
    scrollback_limit_bytes: usize,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
    shell_config: crate::pane::PaneShellConfig<'a>,
    resume_agents_on_restore: bool,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
    history_carry: &'a HistoryCarry,
}

/// Everything a restore produces. Restore can drop saved workspaces (no tab
/// survived) and tabs (no pane survived), so saved indices into those lists
/// no longer name the same item; `active` and `selected` are already remapped
/// onto `workspaces` and must be used as they are, not re-derived from the
/// snapshot by clamping.
pub struct RestoredSession {
    pub workspaces: Vec<Workspace>,
    pub terminals: HashMap<TerminalId, TerminalState>,
    pub terminal_runtimes: HashMap<TerminalId, PaneRuntime>,
    /// The saved active workspace as an index into `workspaces`; if it was
    /// dropped, its nearest surviving neighbour. `None` if nothing was active
    /// or nothing survived.
    pub active: Option<usize>,
    /// The saved selected workspace as an index into `workspaces`, remapped
    /// the same way; 0 when nothing survived.
    pub selected: usize,
    /// Saved history of the panes that came back without a runtime. Every
    /// later history capture of this session must be given it.
    pub history_carry: HistoryCarry,
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
    Unavailable(String),
}

type RestoredWorkspace = (
    Workspace,
    Vec<TerminalState>,
    HashMap<TerminalId, PaneRuntime>,
);
type RestoredTab = (
    crate::workspace::Tab,
    Vec<TerminalState>,
    HashMap<TerminalId, PaneRuntime>,
    HashMap<PaneId, u32>,
);
/// Restore workspaces from a snapshot. Each pane gets a fresh shell in its saved cwd.
pub fn restore(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    login_shell: bool,
    resume_agents_on_restore: bool,
    events: &mpsc::Sender<AppEvent>,
    render_notify: &Arc<Notify>,
    render_dirty: &Arc<RenderSignal>,
) -> RestoredSession {
    restore_with_imports(
        snapshot,
        history,
        rows,
        cols,
        scrollback_limit_bytes,
        crate::pane::PaneShellConfig::new(default_shell, login_shell),
        resume_agents_on_restore,
        events,
        render_notify,
        render_dirty,
    )
}

fn restore_with_imports(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    resume_agents_on_restore: bool,
    events: &mpsc::Sender<AppEvent>,
    render_notify: &Arc<Notify>,
    render_dirty: &Arc<RenderSignal>,
) -> RestoredSession {
    let history = history.filter(|history| {
        let matches = history.layout_fingerprint.is_some()
            && history.layout_fingerprint == super::snapshot::layout_fingerprint(snapshot);
        if !matches {
            tracing::warn!("Ignoring pane history without a matching session layout");
        }
        matches
    });
    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut terminal_runtimes = HashMap::new();
    let mut resumed_agent_sessions = HashSet::new();
    let history_carry = HistoryCarry::default();
    let host_theme = snapshot.host_theme.to_theme();
    // Where each saved workspace ended up, `None` for a dropped one.
    let mut restored_index = Vec::with_capacity(snapshot.workspaces.len());
    let saved_ids: HashSet<&str> = snapshot
        .workspaces
        .iter()
        .filter_map(|ws| ws.id.as_deref())
        .collect();
    let mut used_ids = HashSet::new();
    for (idx, ws_snap) in snapshot.workspaces.iter().enumerate() {
        let workspace_id = restored_workspace_id(ws_snap.id.as_deref(), &saved_ids, &mut used_ids);
        let runtime_context = RestoreRuntimeContext {
            scrollback_limit_bytes,
            host_theme,
            shell_config,
            resume_agents_on_restore,
            events: events.clone(),
            render_notify: Arc::clone(render_notify),
            render_dirty: Arc::clone(render_dirty),
            history_carry: &history_carry,
        };
        let restored = restore_workspace(
            ws_snap,
            workspace_id,
            history.and_then(|history| history.workspaces.get(idx)),
            rows,
            cols,
            &runtime_context,
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
            restored_index.push(None);
        }
    }
    crate::workspace::reserve_workspace_ids(&workspaces);
    let active = snapshot
        .active
        .and_then(|active| remap_saved_index(active, &restored_index));
    let selected = remap_saved_index(snapshot.selected, &restored_index).unwrap_or(0);
    RestoredSession {
        workspaces,
        terminals,
        terminal_runtimes,
        active,
        selected,
        history_carry,
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

/// The ID a restored workspace gets. A saved ID is kept unless an earlier
/// workspace of the same file already took it (a hand-edited or damaged
/// file). A workspace without a usable ID gets a fresh one, which must not be
/// any other saved workspace's ID either: the process-wide ID counter only
/// moves past the saved IDs once restore finishes, so a fresh ID could
/// otherwise be one a later saved workspace already owns. Every candidate the
/// loop skips is a distinct saved or used ID, so it ends.
fn restored_workspace_id(
    saved: Option<&str>,
    saved_ids: &HashSet<&str>,
    used_ids: &mut HashSet<String>,
) -> String {
    if let Some(id) = saved.filter(|id| !id.is_empty())
        && used_ids.insert(id.to_string())
    {
        return id.to_string();
    }
    loop {
        let id = crate::workspace::generate_workspace_id();
        if !saved_ids.contains(id.as_str()) && used_ids.insert(id.clone()) {
            return id;
        }
    }
}

fn restore_workspace(
    snap: &WorkspaceSnapshot,
    workspace_id: String,
    history: Option<&WorkspaceHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    resumed_agent_sessions: &mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
) -> Option<RestoredWorkspace> {
    let mut tabs = Vec::new();
    // Where each saved tab ended up, `None` for a dropped one.
    let mut restored_tab_index = Vec::with_capacity(snap.tabs.len());
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    let mut next_public_pane_number = snap
        .public_pane_numbers
        .values()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_pane_number);
    let public_pane_numbers_by_old_raw =
        &assign_public_pane_numbers(snap, &mut next_public_pane_number);
    let public_pane_ids_by_old_raw: HashMap<u32, String> = public_pane_numbers_by_old_raw
        .iter()
        .map(|(old_raw, public_number)| {
            (
                *old_raw,
                shepr_protocol::PublicPaneId::new(workspace_id.as_str(), *public_number)
                    .to_string(),
            )
        })
        .collect();
    let mut next_public_tab_number = snap
        .public_tab_numbers
        .iter()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_tab_number);

    for (idx, tab_snap) in snap.tabs.iter().enumerate() {
        let tab_number = snap.public_tab_numbers.get(idx).copied().unwrap_or(idx + 1);
        let restored_tab = restore_tab(
            tab_snap,
            history.and_then(|history| history.tabs.get(idx)),
            tab_number,
            rows,
            cols,
            runtime_context,
            resumed_agent_sessions,
            &public_pane_ids_by_old_raw,
        );
        let Some((mut tab, restored_terminals, restored_runtimes, reverse_id_map)) = restored_tab
        else {
            restored_tab_index.push(None);
            continue;
        };
        restored_tab_index.push(Some(tabs.len()));
        if let Some(public_tab_number) = snap.public_tab_numbers.get(idx).copied() {
            tab.number = public_tab_number;
        }
        next_public_tab_number = next_public_tab_number.max(tab.number + 1);
        for (pane_id, pane) in &mut tab.panes {
            let public_number = public_pane_numbers_by_old_raw
                .get(
                    &reverse_id_map
                        .get(pane_id)
                        .copied()
                        .unwrap_or(pane_id.raw()),
                )
                .copied()
                .unwrap_or_else(|| {
                    let number = next_public_pane_number;
                    next_public_pane_number += 1;
                    number
                });
            pane.public_number = public_number;
            next_public_pane_number = next_public_pane_number.max(public_number + 1);
        }
        terminals.extend(restored_terminals);
        terminal_runtimes.extend(restored_runtimes);
        tabs.push(tab);
    }

    // `None` exactly when no tab survived; the workspace is dropped then.
    let active_tab = remap_saved_index(snap.active_tab, &restored_tab_index)?;

    let workspace = Workspace::from_restored_tabs(
        workspace_id,
        snap.custom_name.clone(),
        snap.identity_cwd.clone(),
        tabs,
        active_tab,
        next_public_pane_number,
        next_public_tab_number,
    )?;
    Some((workspace, terminals, terminal_runtimes))
}

/// The terminal state of one restored pane. Every saved `PaneSnapshot` field
/// is carried forward here, once, whichever way the pane comes back; `start`
/// only decides the parts that genuinely differ:
///
/// - cwd, label and launch argv: always kept.
/// - agent session: always kept, except by a running duplicate whose session
///   an earlier pane of this restore resumes.
/// - agent name: a running shell is a plain shell, so it carries none until
///   detection or a hook reports an agent. A pending resume keeps a managed
///   agent's name (its resumed process will own it, and the name is released
///   if that process never appears). An unavailable pane keeps
///   whatever name it had, so a later save writes it back unchanged.
fn restored_terminal(
    pane: &super::snapshot::PaneSnapshot,
    start: RestoredPaneStart,
) -> TerminalState {
    let mut terminal = TerminalState::new(TerminalId::alloc(), pane.cwd.clone());
    if let Some(label) = pane.label.clone() {
        terminal.set_manual_label(label);
    }
    terminal.launch_argv = pane.launch_argv.clone();
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
    let managed_agent = pane
        .managed_agent_kind
        .as_deref()
        .and_then(shepr_agent::detect::parse_canonical_agent_label);
    match start {
        RestoredPaneStart::Running { .. } => {}
        RestoredPaneStart::PendingResume(plan) => {
            let resumed_agent = Some(plan.agent);
            terminal = terminal.with_pending_agent_resume_plan(plan);
            if let (Some(name), Some(agent)) = (pane.agent_name.clone(), managed_agent) {
                // No process exists yet, so the name is held in a phase that
                // saves persist but nothing reconciles. Launching the resume
                // gives it a deadline: if the typed command fails (binary not
                // found, say) and the agent never shows up, the name is
                // released instead of sticking to a plain shell.
                terminal.restore_managed_agent_for_resume(name, agent);
            }
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
            if let Some(agent) = resumed_agent {
                let _ = terminal.set_detected_state_with_screen_signals_at(
                    Some(agent),
                    AgentState::Idle,
                    false,
                    false,
                    std::time::Instant::now(),
                );
            }
        }
        RestoredPaneStart::Unavailable(reason) => {
            warn!(
                cwd = %pane.cwd.display(),
                reason = %reason,
                "preserving unavailable restored pane"
            );
            terminal.restore_error = Some(reason);
            match (pane.agent_name.clone(), managed_agent) {
                (Some(name), Some(agent)) => terminal.restore_managed_agent(name, agent),
                (Some(name), None) => terminal.set_agent_name(name),
                (None, _) => {}
            }
        }
    }
    terminal
}

fn restore_tab(
    snap: &TabSnapshot,
    history: Option<&TabHistorySnapshot>,
    number: usize,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    resumed_agent_sessions: &mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
    public_pane_ids_by_old_raw: &HashMap<u32, String>,
) -> Option<RestoredTab> {
    let (node, id_map) = restore_node_remapped(&snap.layout);
    let reverse_id_map: HashMap<PaneId, u32> = id_map
        .iter()
        .map(|(&old_id, &new_id)| (new_id, old_id))
        .collect();
    let pane_ids = collect_pane_ids(&node);

    let mut panes = HashMap::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    for id in &pane_ids {
        let old_id = reverse_id_map.get(id);
        // A layout leaf with no saved pane (a repeated ID, or an entry missing
        // from `panes`) has nothing to restore. Inventing one would open a
        // shell in the server's own working directory and then save that
        // directory as if it had been the user's; drop the leaf and let the
        // pruning below collapse its split.
        let Some(saved_pane) = old_id.and_then(|old_id| snap.panes.get(old_id)) else {
            warn!(
                tab = ?snap.custom_name,
                pane_id = ?old_id,
                "saved layout names a pane with no saved state; dropping it"
            );
            continue;
        };
        let saved_history =
            old_id.and_then(|old_id| history.and_then(|history| history.panes.get(old_id)));

        if !saved_pane.cwd.is_dir() {
            let terminal = restored_terminal(
                saved_pane,
                RestoredPaneStart::Unavailable(
                    "Saved directory is unavailable. Restore the directory and restart this session."
                        .into(),
                ),
            );
            runtime_context
                .history_carry
                .carry_restored(&terminal.id, saved_history);
            panes.insert(
                *id,
                crate::workspace::TabPane::new(PaneState::new(terminal.id.clone())),
            );
            terminals.push(terminal);
            continue;
        }

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
        let public_pane_id = old_pane_id
            .and_then(|old_id| public_pane_ids_by_old_raw.get(&old_id))
            .map(String::as_str);
        let launch_env = public_pane_id
            .and_then(|pane_id| pane_id.parse::<shepr_protocol::PublicPaneId>().ok())
            .map(|pane_id| PaneLaunchEnv::from_extra(Vec::new()).with_pane_id(pane_id))
            .unwrap_or_default();
        if let Some(plan) = restore_plan {
            let terminal = restored_terminal(saved_pane, RestoredPaneStart::PendingResume(plan));
            // Native resume owns what this pane shows once it runs, so the
            // saved screen is not replayed. Until a runtime exists, though,
            // saves must keep writing it: the resume waits for the event loop
            // and is spaced out per agent (`startup_per_agent_delay_ms`), so
            // later panes can wait a while, or it can fail outright (missing
            // cwd or shell), and neither may cost the pane its saved history.
            runtime_context
                .history_carry
                .carry_restored(&terminal.id, saved_history);
            panes.insert(
                *id,
                crate::workspace::TabPane::new(PaneState::new(terminal.id.clone())),
            );
            terminals.push(terminal);
            continue;
        }

        let runtime_result = PaneRuntime::spawn_with_initial_history(
            *id,
            rows,
            cols,
            &saved_pane.cwd,
            runtime_context.scrollback_limit_bytes,
            runtime_context.host_theme,
            None,
            runtime_context.shell_config,
            &launch_env,
            initial_history_ansi,
            &runtime_context.events,
            &runtime_context.render_notify,
            &runtime_context.render_dirty,
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
                );
                panes.insert(
                    *id,
                    crate::workspace::TabPane::new(PaneState::new(terminal.id.clone())),
                );
                terminal_runtimes.insert(terminal.id.clone(), runtime);
                terminals.push(terminal);
            }
            Err(e) => {
                // Nothing to roll back in the resumed-session set: only a pane
                // with a resume plan reserves its session, and such a pane
                // took the deferred branch above without spawning anything.
                error!(
                    tab = ?snap.custom_name,
                    pane_id = id.raw(),
                    err = %e,
                    "failed to restore pane"
                );
                let terminal = restored_terminal(
                    saved_pane,
                    RestoredPaneStart::Unavailable(format!(
                        "Could not start the saved shell: {e}. Fix the shell configuration and restart this session."
                    )),
                );
                runtime_context
                    .history_carry
                    .carry_restored(&terminal.id, saved_history);
                panes.insert(
                    *id,
                    crate::workspace::TabPane::new(PaneState::new(terminal.id.clone())),
                );
                terminals.push(terminal);
            }
        }
    }

    if panes.is_empty() {
        warn!(
            tab = ?snap.custom_name,
            "no panes could be restored for tab, dropping it"
        );
        return None;
    }

    let surviving: HashSet<PaneId> = panes.keys().copied().collect();
    let Some(node) = prune_restored_node(node, &surviving) else {
        warn!(
            tab = ?snap.custom_name,
            "restored tab lost all panes after pruning missing layout nodes"
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
    // internal invariant broke. The tab is dropped like the other unusable
    // tabs above, loudly, rather than guessed back into shape.
    let layout = match TileLayout::from_saved(node, focus) {
        Ok(layout) => layout,
        Err(error) => {
            error!(
                tab = ?snap.custom_name,
                ?error,
                "restored tab failed layout validation after remapping; dropping it"
            );
            return None;
        }
    };

    Some((
        crate::workspace::Tab {
            custom_name: snap.custom_name.clone(),
            number,
            root_pane,
            layout,
            panes,
            // Pruning can leave a single pane, which is never zoomed, or drop
            // the zoomed (focused) pane, and zooming whichever pane focus
            // fell back to would show one the user never zoomed.
            zoomed: snap.zoomed && pane_ids.len() > 1 && saved_focus_survived,
        },
        terminals,
        terminal_runtimes,
        reverse_id_map,
    ))
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

#[cfg(test)]
fn take_restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
    resumed_agent_sessions: &mut HashSet<shepr_agent::agent::resume::AgentResumeKey>,
) -> Option<shepr_agent::agent::resume::AgentResumePlan> {
    restore_plan_for_snapshot(session, resume_agents_on_restore)
        .filter(|plan| resumed_agent_sessions.insert(plan.dedupe_key.clone()))
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

/// Restore a layout tree, remapping every pane ID to a fresh globally unique one.
/// Returns the new tree and a map of old_raw_id → new PaneId.
///
/// The session file is plain JSON and may be hand-edited or damaged, so the
/// tree is sanitized the way live layout edits are: split ratios go through
/// the same clamp as live splits and resizes, and a saved pane ID that appears
/// more than once maps only its first leaf. Later copies get a fresh ID with
/// no saved pane behind it, and `restore_tab` drops such leaves instead of
/// inventing a pane for them.
pub(super) fn restore_node_remapped(snap: &LayoutSnapshot) -> (Node, HashMap<u32, PaneId>) {
    let mut id_map = HashMap::new();
    let node = remap_inner(snap, &mut id_map);
    (node, id_map)
}

fn remap_inner(snap: &LayoutSnapshot, id_map: &mut HashMap<u32, PaneId>) -> Node {
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
            Node::Pane(new_id)
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_node = remap_inner(first, id_map);
            let second_node = remap_inner(second, id_map);
            let dir = match direction {
                DirectionSnapshot::Horizontal => Direction::Horizontal,
                DirectionSnapshot::Vertical => Direction::Vertical,
            };
            Node::Split {
                direction: dir,
                ratio: shepr_core::layout::valid_split_ratio(*ratio),
                first: Box::new(first_node),
                second: Box::new(second_node),
            }
        }
    }
}

/// Public pane numbers for every saved pane of a workspace, keyed by saved
/// pane ID. Every restored pane needs its public ID before its shell starts:
/// the ID goes into the shell's SHEPR identity environment, which agent hooks
/// use to report back. A saved pane without a number (a hand-edited or
/// damaged file) gets the next free one, in layout order.
fn assign_public_pane_numbers(
    snap: &WorkspaceSnapshot,
    next_public_pane_number: &mut usize,
) -> HashMap<u32, usize> {
    let mut numbers = snap.public_pane_numbers.clone();
    for tab_snap in &snap.tabs {
        let mut layout_panes = Vec::new();
        collect_snapshot_pane_ids(&tab_snap.layout, &mut layout_panes);
        for old_raw in layout_panes {
            if tab_snap.panes.contains_key(&old_raw) && !numbers.contains_key(&old_raw) {
                numbers.insert(old_raw, *next_public_pane_number);
                *next_public_pane_number += 1;
            }
        }
    }
    numbers
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
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .expect("test precondition")
            .join(name)
            .display()
            .to_string()
    }

    fn test_restore_shell() -> &'static str {
        "/bin/sh"
    }

    #[test]
    fn capture_and_restore_node_round_trip() {
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: shepr_core::layout::SplitRatio::clamped(0.5),
            first: Box::new(Node::Pane(PaneId::from_raw(0))),
            second: Box::new(Node::Split {
                direction: Direction::Vertical,
                ratio: shepr_core::layout::SplitRatio::clamped(0.3),
                first: Box::new(Node::Pane(PaneId::from_raw(1))),
                second: Box::new(Node::Pane(PaneId::from_raw(2))),
            }),
        };

        let snap = super::super::snapshot::capture_node(&node);
        let (restored, id_map) = restore_node_remapped(&snap);

        assert_eq!(id_map.len(), 3);
        let ids = collect_pane_ids(&restored);
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<u32> = ids.iter().map(|id| id.raw()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn restored_split_ratios_are_clamped_like_live_splits() {
        for (saved, expected) in [
            (f32::NAN, 0.5),
            (f32::INFINITY, 0.5),
            (5.0, 0.9),
            (-1.0, 0.1),
        ] {
            let snap = LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: saved,
                first: Box::new(LayoutSnapshot::Pane(0)),
                second: Box::new(LayoutSnapshot::Pane(1)),
            };
            let (node, _) = restore_node_remapped(&snap);
            let Node::Split { ratio, .. } = node else {
                panic!("expected split");
            };
            assert_eq!(ratio.get(), expected, "saved ratio {saved}");
        }
    }

    #[test]
    fn repeated_saved_pane_maps_only_its_first_leaf() {
        let snap = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(4)),
            second: Box::new(LayoutSnapshot::Pane(4)),
        };
        let (node, id_map) = restore_node_remapped(&snap);
        let ids = collect_pane_ids(&node);
        assert_eq!(ids.len(), 2);
        assert_eq!(id_map.len(), 1);
        assert_eq!(id_map.get(&4), ids.first());
    }

    #[tokio::test]
    async fn restore_drops_layout_leaves_without_saved_state() {
        let (mut snapshot, _) = snapshot_with_saved_pane_history();
        let cwd = snapshot.workspaces[0].tabs[0].panes[&0].cwd.clone();
        // Pane 0 appears twice and pane 7 has no entry in `panes`.
        snapshot.workspaces[0].tabs[0].layout = LayoutSnapshot::Split {
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
            5,
            40,
            4096,
            test_restore_shell(),
            false,
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
        );
        let tab = &workspaces[0].tabs()[0];
        assert_eq!(tab.layout.pane_ids(), vec![tab.root_pane]);
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(terminals.len(), 1);
        let mut runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
        let captured = crate::persist::capture(
            &workspaces,
            &terminals,
            &runtimes,
            std::path::Path::new("/"),
            Some(0),
            0,
            Default::default(),
        );
        let panes = &captured.workspaces[0].tabs[0].panes;
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
    async fn restored_panes_keep_launch_argv_and_runtimeless_history() {
        // (resume agents, saved cwd missing, shell missing)
        for (resume, missing_cwd, missing_shell) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let (mut snapshot, mut history) = snapshot_with_saved_pane_history();
            let pane = snapshot.workspaces[0].tabs[0]
                .panes
                .get_mut(&0)
                .expect("test precondition");
            pane.label = Some("keep me".into());
            pane.launch_argv = Some(vec!["just".into(), "dev".into()]);
            pane.agent_session = Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: "shepr:codex".into(),
                agent: shepr_agent::agent::Agent::Codex,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            });
            if missing_cwd {
                pane.cwd = pane.cwd.join("__shepr_missing_restore_directory__");
                assert!(!pane.cwd.exists());
            }
            let saved_cwd = pane.cwd.clone();
            history.layout_fingerprint = super::super::snapshot::layout_fingerprint(&snapshot);
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                workspaces,
                terminals,
                terminal_runtimes: runtimes,
                history_carry,
                ..
            } = restore(
                &snapshot,
                Some(&history),
                5,
                40,
                4096,
                if missing_shell {
                    "__shepr_missing_restore_shell__"
                } else {
                    test_restore_shell()
                },
                false,
                resume,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
            );
            let case =
                format!("resume={resume} missing_cwd={missing_cwd} missing_shell={missing_shell}");
            let runtimeless = resume || missing_cwd || missing_shell;
            assert_eq!(runtimes.is_empty(), runtimeless, "{case}");
            let mut runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
            let captured = crate::persist::capture(
                &workspaces,
                &terminals,
                &runtimes,
                std::path::Path::new("/"),
                Some(0),
                0,
                Default::default(),
            );
            let pane = captured.workspaces[0].tabs[0]
                .panes
                .values()
                .next()
                .expect("test precondition");
            assert_eq!(
                pane.launch_argv.as_deref(),
                Some(["just".to_string(), "dev".to_string()].as_slice()),
                "{case}"
            );
            assert_eq!(pane.label.as_deref(), Some("keep me"), "{case}");
            assert_eq!(pane.cwd, saved_cwd, "{case}");
            assert_eq!(
                pane.agent_session
                    .as_ref()
                    .map(|session| session.session_ref.value_str()),
                Some("codex-session"),
                "{case}"
            );

            if runtimeless {
                let saved = crate::persist::capture_history(
                    &captured,
                    &workspaces,
                    &runtimes,
                    &history_carry,
                );
                let pane_history = saved.workspaces[0].tabs[0]
                    .panes
                    .values()
                    .next()
                    .expect("a pane without a runtime keeps its saved history");
                assert!(pane_history.ansi.contains("RESTORED_HISTORY"));

                // Once the pane runs, its live screen supersedes the carried one
                // for good.
                let tab = &workspaces[0].tabs()[0];
                let terminal_id = tab.terminal_id(tab.root_pane).expect("test precondition");
                runtimes.insert(
                    terminal_id.clone(),
                    crate::pane::PaneRuntime::test_with_scrollback_bytes(
                        20,
                        3,
                        4096,
                        b"LIVE_SCREEN\r\n",
                    ),
                );
                let saved = crate::persist::capture_history(
                    &captured,
                    &workspaces,
                    &runtimes,
                    &history_carry,
                );
                let live = &saved.workspaces[0].tabs[0].panes[&tab.root_pane.raw()];
                assert!(live.ansi.contains("LIVE_SCREEN"));
                assert!(!live.ansi.contains("RESTORED_HISTORY"));
                // Should the pane lose its runtime again, what it keeps is its
                // own last screen, never the restored history.
                runtimes.remove(terminal_id);
                let saved = crate::persist::capture_history(
                    &captured,
                    &workspaces,
                    &runtimes,
                    &history_carry,
                );
                let kept = &saved.workspaces[0].tabs[0].panes[&tab.root_pane.raw()];
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

    /// A pane snapshot whose saved directory does not exist, so restore keeps
    /// it without starting a shell.
    fn runtimeless_pane() -> super::super::snapshot::PaneSnapshot {
        let cwd = std::env::current_dir()
            .expect("test precondition")
            .join("__shepr_missing_restore_directory__");
        assert!(!cwd.exists());
        super::super::snapshot::PaneSnapshot {
            cwd,
            label: None,
            agent_name: None,
            managed_agent_kind: None,
            agent_session: None,
            launch_argv: None,
        }
    }

    /// A tab with one kept pane per ID in `panes`; `layout` may name IDs
    /// without a saved pane, which restore drops.
    fn tab_snapshot(name: &str, layout: LayoutSnapshot, panes: &[u32]) -> TabSnapshot {
        TabSnapshot {
            custom_name: Some(name.into()),
            layout,
            panes: panes.iter().map(|id| (*id, runtimeless_pane())).collect(),
            zoomed: false,
            focused: None,
            root_pane: None,
        }
    }

    fn workspace_snapshot(
        id: Option<&str>,
        name: &str,
        tabs: Vec<TabSnapshot>,
        active_tab: usize,
    ) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.map(str::to_string),
            custom_name: Some(name.into()),
            identity_cwd: PathBuf::from("/"),
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs,
            active_tab,
        }
    }

    fn restore_runtimeless(snapshot: &SessionSnapshot) -> RestoredSession {
        let (events, _rx) = mpsc::channel(8);
        let restored = restore(
            snapshot,
            None,
            5,
            40,
            0,
            test_restore_shell(),
            false,
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
        );
        assert!(restored.terminal_runtimes.is_empty());
        restored
    }

    #[test]
    fn dropped_workspaces_and_tabs_do_not_shift_the_saved_selection() {
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                // No tab survives: the layout names a pane with no saved state.
                workspace_snapshot(
                    Some("w1"),
                    "dropped",
                    vec![tab_snapshot("gone", LayoutSnapshot::Pane(1), &[])],
                    0,
                ),
                workspace_snapshot(
                    Some("w2"),
                    "selected",
                    vec![tab_snapshot("only", LayoutSnapshot::Pane(2), &[2])],
                    0,
                ),
                workspace_snapshot(
                    Some("w3"),
                    "active",
                    vec![
                        tab_snapshot("first", LayoutSnapshot::Pane(3), &[3]),
                        tab_snapshot("gone", LayoutSnapshot::Pane(4), &[]),
                        tab_snapshot("wanted", LayoutSnapshot::Pane(5), &[5]),
                    ],
                    2,
                ),
            ],
            active: Some(2),
            selected: 1,
        };

        let restored = restore_runtimeless(&snapshot);

        let names: Vec<_> = restored
            .workspaces
            .iter()
            .map(|ws| ws.custom_name.as_deref())
            .collect();
        assert_eq!(names, vec![Some("selected"), Some("active")]);
        assert_eq!(restored.active, Some(1));
        assert_eq!(restored.selected, 0);
        let active = &restored.workspaces[1];
        assert_eq!(active.tabs().len(), 2);
        assert_eq!(
            active.tabs()[active.active_tab].custom_name.as_deref(),
            Some("wanted")
        );
    }

    #[test]
    fn a_dropped_active_workspace_falls_back_to_its_neighbour() {
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace_snapshot(
                    Some("w1"),
                    "before",
                    vec![tab_snapshot("t", LayoutSnapshot::Pane(1), &[1])],
                    0,
                ),
                workspace_snapshot(
                    Some("w2"),
                    "dropped",
                    vec![tab_snapshot("gone", LayoutSnapshot::Pane(2), &[])],
                    0,
                ),
            ],
            active: Some(1),
            selected: 1,
        };

        let restored = restore_runtimeless(&snapshot);

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.active, Some(0));
        assert_eq!(restored.selected, 0);
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
            let mut tab = tab_snapshot("t", split(1, 2, 3), panes);
            tab.zoomed = true;
            tab.focused = Some(focused);
            let snapshot = SessionSnapshot {
                version: super::super::snapshot::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![workspace_snapshot(Some("w1"), "ws", vec![tab], 0)],
                active: Some(0),
                selected: 0,
            };

            let restored = restore_runtimeless(&snapshot);

            let tab = &restored.workspaces[0].tabs()[0];
            assert_eq!(tab.zoomed, zoomed, "panes={panes:?} focused={focused}");
        }
    }

    #[test]
    fn restored_workspace_ids_are_unique() {
        // The ID the counter would hand out next is also saved on a later
        // workspace; a third workspace repeats that saved ID.
        let probe = crate::workspace::generate_workspace_id();
        let next =
            crate::workspace::public_workspace_number(&probe).expect("test precondition") + 1;
        let taken = format!("w{}", shepr_protocol::encode_public_number(next));
        let tab = |id: u32| vec![tab_snapshot("t", LayoutSnapshot::Pane(id), &[id])];
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace_snapshot(None, "unsaved id", tab(1), 0),
                workspace_snapshot(Some(&taken), "owner", tab(2), 0),
                workspace_snapshot(Some(&taken), "repeat", tab(3), 0),
                workspace_snapshot(Some(""), "empty id", tab(4), 0),
            ],
            active: Some(0),
            selected: 0,
        };

        let restored = restore_runtimeless(&snapshot);

        let ids: Vec<_> = restored.workspaces.iter().map(|ws| ws.id.clone()).collect();
        assert_eq!(ids.len(), 4);
        assert_eq!(ids[1], taken, "the first owner of a saved ID keeps it");
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        assert!(ids.iter().all(|id| !id.is_empty()));
        // A later new workspace does not reuse any restored ID either.
        let fresh = crate::workspace::generate_workspace_id();
        assert!(
            !ids.contains(&fresh.into()),
            "fresh workspace id reused: {ids:?}"
        );
    }

    #[test]
    fn prune_restored_node_collapses_missing_branch() {
        let keep = PaneId::from_raw(11);
        let missing = PaneId::from_raw(12);
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
        let first = PaneId::from_raw(21);
        let second = PaneId::from_raw(22);
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
            source: "shepr:pi".into(),
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
            source: "shepr:claude".into(),
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
            source: "shepr:pi".into(),
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
            source: "shepr:pi".into(),
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
            source: "shepr:pi".into(),
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
            source: "shepr:pi".into(),
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
            source: "shepr:letta".into(),
            agent: shepr_agent::agent::Agent::Letta,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::id("letta-session")
                .expect("test precondition"),
        };

        let preserved = restored_terminal_agent_session(Some(&session), false)
            .expect("restore should preserve metadata");
        assert_eq!(preserved.source, "shepr:letta");
        assert_eq!(preserved.agent, "letta");
        assert_eq!(preserved.session_ref.value(), "letta-session");
    }

    #[test]
    fn restore_does_not_rehydrate_duplicate_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "shepr:pi".into(),
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
                        "tabs": [
                            {
                                "layout": { "Pane": 1 },
                                "panes": { "1": { "cwd": "/tmp/shepr-restore-test-a" } },
                                "zoomed": false,
                                "focused": 1,
                                "root_pane": 1
                            },
                            {
                                "layout": { "Pane": 2 },
                                "panes": { "2": { "cwd": "/tmp/shepr-restore-test-a" } },
                                "zoomed": false,
                                "focused": 2,
                                "root_pane": 2
                            }
                        ],
                        "active_tab": 0
                    },
                    {
                        "id": "workspace-b",
                        "identity_cwd": "/tmp/shepr-restore-test-b",
                        "tabs": [
                            {
                                "layout": { "Pane": 3 },
                                "panes": { "3": { "cwd": "/tmp/shepr-restore-test-b" } },
                                "zoomed": false,
                                "focused": 3,
                                "root_pane": 3
                            }
                        ],
                        "active_tab": 0
                    }
                ],
                "active": 0,
                "selected": 0
            }))
            .expect("test precondition");
            let cwd = std::env::current_dir().expect("test precondition");
            let missing = cwd.join("__shepr_missing_restore_directory__");
            assert!(!missing.exists());
            for workspace in &mut snapshot.workspaces {
                workspace.identity_cwd = cwd.clone();
                for tab in &mut workspace.tabs {
                    for pane in tab.panes.values_mut() {
                        pane.cwd = cwd.clone();
                    }
                }
            }
            let failed = snapshot.workspaces[0].tabs[0]
                .panes
                .get_mut(&1)
                .expect("test precondition");
            failed.cwd = missing.clone();
            failed.label = Some("keep my pane".into());
            failed.agent_session = Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: "shepr:opencode".into(),
                agent: shepr_agent::agent::Agent::OpenCode,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("keep-my-session")
                    .expect("test precondition"),
            });
            let (events, _rx) = mpsc::channel(32);
            let RestoredSession {
                workspaces,
                terminals,
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                None,
                24,
                80,
                0,
                if missing_shell {
                    "__shepr_missing_restore_shell__"
                } else {
                    test_restore_shell()
                },
                false,
                false,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
            );
            let runtimes = crate::pane::PaneRuntimeRegistry::from(runtimes);
            let captured = crate::persist::capture(
                &workspaces,
                &terminals,
                &runtimes,
                std::path::Path::new("/"),
                Some(0),
                0,
                Default::default(),
            );
            assert_eq!(
                captured.workspaces.len(),
                2,
                "a launch failure must not delete a workspace"
            );
            assert_eq!(captured.workspaces[0].tabs.len(), 2);
            let pane = captured.workspaces[0].tabs[0]
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
            let root = workspaces[0].tabs()[0].root_pane;
            let terminal_id = workspaces[0].tabs()[0]
                .terminal_id(root)
                .expect("test precondition");
            assert!(
                runtimes.get(terminal_id).is_none(),
                "do not open a replacement shell elsewhere"
            );
            let healthy = workspaces[1].tabs()[0]
                .terminal_id(workspaces[1].tabs()[0].root_pane)
                .expect("test precondition");
            assert_eq!(runtimes.get(healthy).is_some(), !missing_shell);
            assert!(terminals[terminal_id].restore_error.is_some());
        }
    }

    #[tokio::test]
    async fn restore_carries_persisted_agent_session_metadata() {
        let cwd = std::env::current_dir().expect("test precondition");
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: Some("reviewer".into()),
                            agent_name: Some("reviewer".into()),
                            managed_agent_kind: Some("opencode".into()),
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "shepr:opencode".into(),
                                agent: shepr_agent::agent::Agent::OpenCode,
                                session_ref: shepr_agent::agent::resume::AgentSessionRef::id(
                                    "opencode-session",
                                )
                                .expect("test precondition"),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
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
            24,
            80,
            0,
            test_restore_shell(),
            false,
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("restored terminal should exist");
        assert_eq!(terminal.agent_name, None);
        assert_eq!(terminal.manual_label.as_deref(), Some("reviewer"));
        let session = terminal
            .persisted_agent_session
            .as_ref()
            .expect("persisted agent session should survive restore");
        assert_eq!(session.source, "shepr:opencode");
        assert_eq!(session.agent, "opencode");
        assert_eq!(session.session_ref.value(), "opencode-session");
    }

    #[tokio::test]
    async fn restore_preserves_public_id_mapping_after_pane_id_remap() {
        let cwd = std::env::current_dir().expect("test precondition");
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                public_pane_numbers: HashMap::from([(10, 1), (20, 3)]),
                next_public_pane_number: 4,
                public_tab_numbers: vec![5],
                next_public_tab_number: 6,
                tabs: vec![TabSnapshot {
                    custom_name: None,
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
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                        (
                            20,
                            super::super::snapshot::PaneSnapshot {
                                cwd: cwd.clone(),
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                    ]),
                    zoomed: false,
                    focused: Some(10),
                    root_pane: Some(10),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
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
            24,
            80,
            0,
            test_restore_shell(),
            false,
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let mut public_numbers: Vec<_> = workspace
            .tabs()
            .iter()
            .flat_map(|tab| tab.panes.values().map(|pane| pane.public_number))
            .collect();
        public_numbers.sort_unstable();
        assert_eq!(public_numbers, vec![1, 3]);
        assert_eq!(workspace.next_public_pane_number, 4);
        assert_eq!(workspace.tabs()[0].number, 5);
        assert_eq!(workspace.next_public_tab_number, 6);
    }

    #[test]
    fn every_saved_pane_gets_a_public_number_before_its_shell_starts() {
        let pane = || super::super::snapshot::PaneSnapshot {
            cwd: PathBuf::from("/"),
            label: None,
            agent_name: None,
            managed_agent_kind: None,
            agent_session: None,
            launch_argv: None,
        };
        let tab = |layout: LayoutSnapshot, panes: &[u32]| TabSnapshot {
            custom_name: None,
            layout,
            panes: panes.iter().map(|id| (*id, pane())).collect(),
            zoomed: false,
            focused: None,
            root_pane: None,
        };
        let snap = WorkspaceSnapshot {
            id: Some("w1".into()),
            custom_name: None,
            identity_cwd: PathBuf::from("/"),
            // Only pane 10 kept its number; 30 and 20 lost theirs.
            public_pane_numbers: HashMap::from([(10, 4)]),
            next_public_pane_number: 5,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![
                tab(
                    LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(30)),
                        second: Box::new(LayoutSnapshot::Split {
                            direction: DirectionSnapshot::Vertical,
                            ratio: 0.5,
                            first: Box::new(LayoutSnapshot::Pane(10)),
                            // A leaf with no saved pane is dropped by restore
                            // and needs no number.
                            second: Box::new(LayoutSnapshot::Pane(99)),
                        }),
                    },
                    &[10, 30],
                ),
                tab(LayoutSnapshot::Pane(20), &[20]),
            ],
            active_tab: 0,
        };
        let mut next = 5;

        let numbers = assign_public_pane_numbers(&snap, &mut next);

        assert_eq!(numbers, HashMap::from([(10, 4), (30, 5), (20, 6)]));
        assert_eq!(next, 7);
    }

    #[tokio::test]
    async fn cold_restore_with_gapped_public_tab_numbers_drops_unmanaged_agent_name() {
        let cwd = std::env::current_dir().expect("test precondition");
        let pane_snap = |id: &str| {
            (
                id.parse::<u32>().expect("test precondition"),
                super::super::snapshot::PaneSnapshot {
                    cwd: cwd.clone(),
                    label: None,
                    agent_name: None,
                    managed_agent_kind: None,
                    agent_session: None,
                    launch_argv: None,
                },
            )
        };
        let final_pane = super::super::snapshot::PaneSnapshot {
            cwd: cwd.clone(),
            label: Some("planner".into()),
            agent_name: Some("planner".into()),
            managed_agent_kind: None,
            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: "shepr:codex".into(),
                agent: shepr_agent::agent::Agent::Codex,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            }),
            launch_argv: None,
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                public_pane_numbers: HashMap::from([(10, 1), (11, 2), (12, 3), (13, 4)]),
                next_public_pane_number: 5,
                public_tab_numbers: vec![1, 3, 4, 5],
                next_public_tab_number: 6,
                tabs: vec![
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(10),
                        panes: HashMap::from([pane_snap("10")]),
                        zoomed: false,
                        focused: Some(10),
                        root_pane: Some(10),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(11),
                        panes: HashMap::from([pane_snap("11")]),
                        zoomed: false,
                        focused: Some(11),
                        root_pane: Some(11),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(12),
                        panes: HashMap::from([pane_snap("12")]),
                        zoomed: false,
                        focused: Some(12),
                        root_pane: Some(12),
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(13),
                        panes: HashMap::from([(13, final_pane)]),
                        zoomed: false,
                        focused: Some(13),
                        root_pane: Some(13),
                    },
                ],
                active_tab: 3,
            }],
            active: Some(0),
            selected: 0,
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
            24,
            80,
            0,
            test_restore_shell(),
            false,
            false,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        assert_eq!(workspace.active_tab, 3);
        assert_eq!(workspace.tabs()[3].number, 5);
        let agent_pane = workspace.tabs()[3].root_pane;
        let terminal_id = &workspace.tabs()[3].panes[&agent_pane].attached_terminal_id;
        assert!(terminals[terminal_id].agent_name.is_none());
        assert_eq!(terminals[terminal_id].managed_agent_kind(), None);
        assert!(terminals[terminal_id].effective_agent_label().is_none());
    }

    #[tokio::test]
    async fn native_agent_restore_defers_runtime_launch() {
        let cwd = std::env::current_dir().expect("test precondition");
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: None,
                            agent_name: None,
                            managed_agent_kind: None,
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "shepr:codex".into(),
                                agent: shepr_agent::agent::Agent::Codex,
                                session_ref: shepr_agent::agent::resume::AgentSessionRef::id(
                                    "codex-session",
                                )
                                .expect("test precondition"),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
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
            24,
            80,
            0,
            test_restore_shell(),
            false,
            true,
            &events,
            &Arc::new(Notify::new()),
            &Arc::new(RenderSignal::new()),
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

    #[tokio::test]
    async fn restore_seeds_saved_pane_history_into_runtime() {
        let (snapshot, history) = snapshot_with_saved_pane_history();
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
            5,
            40,
            4096,
            test_restore_shell(),
            false,
            false,
            &events,
            &render_notify,
            &render_dirty,
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        let restored_text = runtime.recent_unwrapped_text(10);
        assert!(
            restored_text
                .contains("RESTORED_HISTORY \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} LINK"),
            "styled Unicode and hyperlink text should survive history replay"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    #[tokio::test]
    async fn restore_without_history_snapshot_keeps_pane_contents_empty() {
        let (snapshot, _history) = snapshot_with_saved_pane_history();
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
            5,
            40,
            4096,
            test_restore_shell(),
            false,
            false,
            &events,
            &render_notify,
            &render_dirty,
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    #[tokio::test]
    async fn restore_rejects_history_from_another_layout_or_without_provenance() {
        for missing_fingerprint in [false, true] {
            let (snapshot, history) = snapshot_with_saved_pane_history();
            let mut value = serde_json::to_value(history).expect("test precondition");
            let fields = value.as_object_mut().expect("test precondition");
            if missing_fingerprint {
                fields.remove("layout_fingerprint");
            } else {
                // History recorded against a different layout: the fingerprint
                // covers only layout structure and pane ids, so name another.
                fields.insert(
                    "layout_fingerprint".into(),
                    serde_json::Value::String("0".repeat(64)),
                );
            }
            let history = serde_json::from_value(value).expect("test precondition");
            let (events, _rx) = mpsc::channel(8);
            let RestoredSession {
                terminal_runtimes: runtimes,
                ..
            } = restore(
                &snapshot,
                Some(&history),
                5,
                80,
                4096,
                test_restore_shell(),
                false,
                false,
                &events,
                &Arc::new(Notify::new()),
                &Arc::new(RenderSignal::new()),
            );
            let runtime = runtimes.values().next().expect("test precondition");
            assert!(
                !runtime
                    .recent_unwrapped_text(10)
                    .contains("RESTORED_HISTORY"),
                "screen history must belong to the exact saved layout"
            );
            for (_, runtime) in runtimes {
                drop(runtime);
            }
        }
    }

    fn snapshot_with_saved_pane_history() -> (SessionSnapshot, SessionHistorySnapshot) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut panes = HashMap::new();
        panes.insert(
            0,
            super::super::snapshot::PaneSnapshot {
                cwd: cwd.clone(),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        let mut history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![super::super::snapshot::TabHistorySnapshot {
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
            }],
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
        };
        history.layout_fingerprint = super::super::snapshot::layout_fingerprint(&snapshot);
        (snapshot, history)
    }
}
