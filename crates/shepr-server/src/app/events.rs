use super::App;
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::PaneId;
use shepr_mux::events::AppEvent;
use std::time::Instant;

/// Events the pure data reducer can apply. Runtime removal, Git completion and
/// clipboard delivery stay at the App boundary, outside this type.
#[derive(Debug)]
pub(crate) enum StateEvent {
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    HookStateReported {
        pane_id: PaneId,
        sample: shepr_mux::terminal::state::HookClockSample,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        state: AgentState,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    },
    AgentSessionReported {
        pane_id: PaneId,
        sample: shepr_mux::terminal::state::HookClockSample,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
    },
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: shepr_mux::UsableCwd,
    },
}

/// The checkpoint decision `App::prepare_pane_exit` made for a pane exit. It
/// travels with the held event to `App::handle_prepared_pane_exit`, so the
/// exit is finished as it was decided even if the pane's core breaks between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreparedPaneExit {
    /// Removed without a checkpoint of its own.
    Unchecked,
    /// Checkpointed, and the checkpoint is already settled: removal can
    /// follow at once.
    Settled,
    /// Checkpointed, and held until the checkpoint of this generation is
    /// durable.
    Held(u64),
}

impl PreparedPaneExit {
    /// The checkpoint generation the exit is held for, if it is held.
    pub(crate) fn held_generation(self) -> Option<u64> {
        match self {
            Self::Held(generation) => Some(generation),
            Self::Unchecked | Self::Settled => None,
        }
    }

    fn checkpointed(self) -> bool {
        !matches!(self, Self::Unchecked)
    }
}

impl App {
    fn live_workspace_identity_cwd(&self, workspace_id: &str) -> Option<std::path::PathBuf> {
        let workspace = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == *workspace_id)?;
        let root_pane_cwd = workspace.cwd_for_pane(
            workspace.root_pane(),
            &self.state.terminals,
            &self.terminal_runtimes,
        );
        Some(workspace.resolved_identity_cwd_from_root_pane(root_pane_cwd))
    }

    fn handle_git_status_refreshed(
        &mut self,
        results: Vec<shepr_mux::git::WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, shepr_mux::git::GitStatusCacheEntry)>,
    ) -> bool {
        self.git_refresh.finish(self.clock.now, cache_updates);
        let results = results
            .into_iter()
            .map(|result| {
                let resolved_identity_cwd = self.live_workspace_identity_cwd(&result.workspace_id);
                (result, resolved_identity_cwd)
            })
            .collect();
        let changed = self.state.apply_workspace_git_statuses(results);
        if changed {
            self.state.mark_shell_projection_dirty();
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }

    /// Check the producer before publication, checkpointing, or forwarding.
    /// A discarded resume attempt can finish after a successor runtime starts.
    pub(crate) fn admit_runtime_event(&self, ev: AppEvent) -> Option<AppEvent> {
        match ev {
            AppEvent::Runtime {
                pane_id,
                generation,
                event,
            } => {
                let runtime = self
                    .state
                    .workspaces
                    .iter()
                    .enumerate()
                    .find_map(|(index, _)| {
                        self.state.runtime_for_pane_in_workspace(
                            &self.terminal_runtimes,
                            index,
                            pane_id,
                        )
                    });
                runtime.filter(|runtime| runtime.generation() == generation)?;
                self.admit_runtime_event(*event)
            }
            event => Some(event),
        }
    }

    pub(crate) fn handle_internal_event(&mut self, ev: AppEvent) {
        let _ = self.handle_internal_event_with_view_change(ev);
    }

    pub(crate) fn handle_internal_event_with_view_change(&mut self, ev: AppEvent) -> bool {
        self.handle_internal_event_inner(ev, None)
    }

    /// Publishes the process exit once, then asks the App's session policy
    /// whether the event must wait for a checkpoint before removal. The
    /// decision goes back with the event to `handle_prepared_pane_exit`.
    pub(crate) fn prepare_pane_exit(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        exit_reason: shepr_platform::ChildExitReason,
    ) -> PreparedPaneExit {
        self.publish_pane_process_exit(pane_id, exit_reason);
        if self.pane_exit_needs_checkpoint(pane_id, exit_reason)
            && self.state.prepare_pane_removal_by_id(pane_id).is_some()
        {
            self.request_pane_exit_checkpoint()
                .map_or(PreparedPaneExit::Settled, PreparedPaneExit::Held)
        } else {
            PreparedPaneExit::Unchecked
        }
    }

    /// Whether a pane's exit is checkpointed before the pane is removed: the
    /// exit reason asks for it, and the pane's terminal core is not broken,
    /// since a broken core has nothing new to give the checkpoint. A prepared
    /// exit decides this once and carries the answer to its removal; a core
    /// that breaks in between is still safe to save, because history capture
    /// leaves an unreadable terminal's cached history as it was.
    fn pane_exit_needs_checkpoint(
        &self,
        pane_id: shepr_core::layout::PaneId,
        exit_reason: shepr_platform::ChildExitReason,
    ) -> bool {
        exit_reason.requires_session_checkpoint()
            && !self.state.workspaces.iter().enumerate().any(|(index, _)| {
                self.state
                    .runtime_for_pane_in_workspace(&self.terminal_runtimes, index, pane_id)
                    .is_some_and(shepr_mux::pane::PaneRuntime::terminal_core_broken)
            })
    }

    /// Applies an event whose pane-exit publication and checkpoint decision
    /// have already been made by the App, finishing it as `prepared` decided.
    pub(crate) fn handle_prepared_pane_exit(
        &mut self,
        ev: AppEvent,
        prepared: PreparedPaneExit,
    ) -> bool {
        self.handle_internal_event_inner(ev, Some(prepared.checkpointed()))
    }

    /// `prepared_checkpoint` is a prepared pane exit's recorded checkpoint
    /// decision, or `None` for an event that was not prepared.
    fn handle_internal_event_inner(
        &mut self,
        ev: AppEvent,
        prepared_checkpoint: Option<bool>,
    ) -> bool {
        let pane_exit_prepared = prepared_checkpoint.is_some();
        let Some(ev) = self.admit_runtime_event(ev) else {
            return false;
        };
        if matches!(&ev, AppEvent::ClipboardWrite { .. }) {
            return false;
        }

        if let AppEvent::GitStatusRefreshed {
            results,
            cache_updates,
        } = ev
        {
            let changed = self.handle_git_status_refreshed(results, cache_updates);
            return changed;
        }
        if let AppEvent::PaneLaunchSettled {
            pane_id,
            settlement,
        } = ev
        {
            return self.handle_pane_launch_settled(pane_id, settlement);
        }

        // A detector tick can finish before the watcher publishes PaneDied.
        // Once the pane child is dead, only its exit reason can decide whether
        // to release the resume identity. Ignore all queued detector updates,
        // including the identity-clear tick following its process-exit report.
        if let AppEvent::StateChanged { pane_id, .. }
        | AppEvent::AgentProcessDetected { pane_id, .. } = &ev
            && self.state.workspaces.iter().enumerate().any(|(index, _)| {
                self.state
                    .runtime_for_pane_in_workspace(&self.terminal_runtimes, index, *pane_id)
                    .is_some_and(shepr_mux::pane::PaneRuntime::child_has_exited)
            })
        {
            return false;
        }

        let projection_before = self.state.shell_projection_revision;
        if let AppEvent::PaneDied {
            pane_id,
            exit_reason,
        } = &ev
            && !pane_exit_prepared
        {
            self.publish_pane_process_exit(*pane_id, *exit_reason);
        }

        let mut removed = false;
        let mut state_changed = false;
        let mut touched_pane = None;
        let session_was_dirty = self.state.session_dirty;
        let pane_removal_plan = if let AppEvent::PaneDied { pane_id, .. } = &ev {
            self.state.prepare_pane_removal_by_id(*pane_id)
        } else {
            None
        };
        let checkpointed_pane_exit = match &ev {
            AppEvent::PaneDied {
                pane_id,
                exit_reason,
            } if pane_removal_plan.is_some() => prepared_checkpoint
                .unwrap_or_else(|| self.pane_exit_needs_checkpoint(*pane_id, *exit_reason)),
            _ => false,
        };
        // The headless loop prepares and holds checkpointed exits before
        // applying them, so this only reports a direct caller that skipped
        // that step; the pane is still removed.
        if checkpointed_pane_exit && !pane_exit_prepared && !self.pane_exit_checkpoint_settled() {
            tracing::warn!("pane exit reached removal before its session checkpoint settled");
        }

        let terminal_cwd_reported = matches!(ev, AppEvent::TerminalCwdReported { .. });
        let mut detached_terminal_ids = Vec::new();
        if let AppEvent::PaneDied { pane_id, .. } = &ev {
            if let Some(plan) = pane_removal_plan {
                match self.state.commit_pane_removal(&plan) {
                    crate::app::actions::PaneRemovalCommit::Removed(outcome) => {
                        removed = true;
                        detached_terminal_ids = outcome.detached_terminal_ids;
                    }
                    crate::app::actions::PaneRemovalCommit::Stale => {
                        // The plan was made above in this same call, so a
                        // stale one means something in between changed the
                        // workspaces. Nothing was removed.
                        tracing::warn!(
                            pane = pane_id.raw(),
                            workspace_index = plan.workspace_index,
                            "PaneDied removal went stale; the dead pane stays in the layout"
                        );
                    }
                }
            }
        } else if let Some(event) = StateEvent::from_app_event(
            ev,
            shepr_mux::terminal::state::HookClockSample {
                monotonic: self.clock.now,
                wall: self.clock.wall_now,
            },
        ) {
            touched_pane = Some(event.pane_id());
            state_changed =
                self.state.handle_state_event(event) != super::actions::StateUpdate::Unchanged;
        }
        // A stale removal keeps the pane in the layout, so it has not completed
        // the checkpointed exit whose save state this method advances.
        if checkpointed_pane_exit && removed {
            self.finish_checkpointed_pane_exit_after_event(session_was_dirty);
        }
        if let Some(pane_id) = touched_pane {
            self.sync_pane_lifecycle_authority_detection_pause(pane_id);
        }
        let changed =
            removed || state_changed || self.state.shell_projection_revision != projection_before;
        if terminal_cwd_reported && changed {
            self.request_git_identity_refresh(self.clock.now);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }

        self.shutdown_detached_terminal_runtimes(&detached_terminal_ids);
        if removed {
            self.state.mark_shell_projection_dirty();
        }
        changed
    }

    fn publish_pane_process_exit(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        exit_reason: shepr_platform::ChildExitReason,
    ) {
        if self
            .state
            .publish_pane_process_exit_if_agent(pane_id, exit_reason)
        {
            self.sync_pane_lifecycle_authority_detection_pause(pane_id);
            self.state.mark_shell_projection_dirty();
        }
    }

    fn sync_pane_lifecycle_authority_detection_pause(&self, pane_id: PaneId) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.terminal_id(pane_id))
        else {
            return;
        };
        if let (Some(terminal), Some(runtime)) = (
            self.state.terminals.get(terminal_id),
            self.terminal_runtimes.get(terminal_id),
        ) {
            runtime.set_full_lifecycle_authority_active(
                terminal.full_lifecycle_hook_authority_active(),
            );
        }
    }

    /// Tells one pane it gained or lost terminal focus. Which panes hold focus
    /// is decided per client on the server (`sync_pane_focus`); a pane with no
    /// live runtime is skipped.
    pub(crate) fn send_pane_focus_event(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
        event: shepr_vt::FocusEvent,
    ) {
        let Some(runtime) = self.state.workspaces.get(ws_idx).and_then(|_| {
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        }) else {
            return;
        };
        runtime.try_send_focus_event(event);
    }
}

#[cfg(test)]
mod pane_exit_event_tests {
    use super::*;
    use crate::test_support::WorkspaceFixture as _;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;

    fn app_with_workspaces(names: &[&str]) -> App {
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.ensure_test_terminals();
        if !app.state.workspaces.is_empty() {
            app.state.set_bookmark_index(Some(0));
        }
        app
    }

    fn report_pane_exit(app: &mut App, pane_id: PaneId) {
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        });
    }

    #[test]
    fn pane_exit_removes_its_workspace_through_the_app_event_path() {
        let mut app = app_with_workspaces(&["a", "dying", "c"]);
        app.state.set_bookmark_index(Some(2));
        let pane_id = app.state.workspaces[1].root_pane();
        let terminal_id = app.state.workspaces[1]
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.session_dirty = false;

        report_pane_exit(&mut app, pane_id);

        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(
            app.state.workspaces[app.state.bookmark_index().expect("active")].display_name(),
            "c"
        );
        assert!(!app.state.terminals.contains_key(&terminal_id));
        assert!(app.state.session_dirty);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn pane_exit_clears_the_bookmark_when_it_removes_the_last_workspace() {
        let mut app = app_with_workspaces(&["only"]);
        let pane_id = app.state.workspaces[0].root_pane();

        report_pane_exit(&mut app, pane_id);

        assert!(app.state.workspaces.is_empty());
        assert_eq!(app.state.bookmark, None);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn pane_exit_keeps_a_workspace_that_still_has_a_pane() {
        let mut app = app_with_workspaces(&["test"]);
        let second_id = app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();

        report_pane_exit(&mut app, second_id);

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].panes().len(), 1);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn pane_exit_for_an_unknown_pane_is_a_noop() {
        let mut app = app_with_workspaces(&["test"]);
        let fake_id = shepr_test_fixtures::fixed_pane_id(9999);

        report_pane_exit(&mut app, fake_id);

        assert_eq!(app.state.workspaces.len(), 1);
        app.state.assert_invariants_for_test();
    }
}

#[cfg(test)]
mod runtime_generation_tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn agent_release_before_shell_death_preserves_exit_checkpoint_identity() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("kill-ordering");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        let session = shepr_agent::agent::resume::PersistedAgentSession::from_report(
            "shepr:codex",
            "codex",
            shepr_agent::agent::resume::AgentSessionRef::id("killed-agent").expect("session"),
        )
        .expect("official identity");
        let now = app.clock.now;
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_persisted_agent_session(session.clone());
        terminal.set_detected_agent_process_at(shepr_agent::agent::Agent::Codex, now);
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(shepr_agent::agent::Agent::Codex),
            state: shepr_agent::detect::AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at: now,
        });
        assert_eq!(
            app.state.terminals[&terminal_id].persisted_agent_session(),
            Some(&session)
        );
        app.prepare_pane_exit(pane_id, shepr_platform::ChildExitReason::Interrupted);
        // Preparation publishes the final exit before the checkpoint captures
        // the still-present pane; removal happens only after that checkpoint.
        assert!(app.state.workspaces[0].contains_pane(pane_id));
        assert_eq!(
            app.state.terminals[&terminal_id].current_session_identity_for_persistence(),
            Some(session),
        );
    }

    #[tokio::test]
    async fn discarded_and_replaced_runtimes_cannot_remove_a_restored_pane() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        let session = shepr_agent::agent::resume::PersistedAgentSession::from_report(
            "shepr:codex",
            "codex",
            shepr_agent::agent::resume::AgentSessionRef::id("restored").expect("session"),
        )
        .expect("persisted identity");
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_persisted_agent_session(session.clone());
        terminal.pending_agent_resume_plan =
            Some(test_codex_plan("restored", vec!["codex".into()]));
        let discarded = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = discarded.generation();
        drop(discarded);
        let died = || AppEvent::Runtime {
            pane_id,
            generation,
            event: Box::new(AppEvent::PaneDied {
                pane_id,
                exit_reason: shepr_platform::ChildExitReason::Interrupted,
            }),
        };
        assert!(!app.handle_internal_event_with_view_change(died()));
        assert!(app.state.workspaces[0].contains_pane(pane_id));
        assert_eq!(
            app.state.terminals[&terminal_id].persisted_agent_session(),
            Some(&session)
        );
        assert!(
            app.state.terminals[&terminal_id]
                .pending_agent_resume_plan
                .is_some()
        );

        let replacement = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let replacement_generation = replacement.generation();
        assert_ne!(generation, replacement_generation);
        app.insert_test_runtime(pane_id, replacement);
        // This also covers a checkpointed exit replayed after replacement:
        // its original envelope is checked again before any removal.
        assert!(!app.handle_prepared_pane_exit(died(), crate::app::PreparedPaneExit::Settled));
        assert!(app.state.workspaces[0].contains_pane(pane_id));
        assert!(
            app.admit_runtime_event(AppEvent::Runtime {
                pane_id,
                generation: replacement_generation,
                event: Box::new(AppEvent::PaneDied {
                    pane_id,
                    exit_reason: shepr_platform::ChildExitReason::Interrupted,
                }),
            })
            .is_some()
        );
    }

    #[tokio::test]
    async fn stale_runtime_cannot_forward_clipboard_or_update_detector_and_cwd() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        let stale = shepr_mux::events::RuntimeGeneration::alloc();
        for event in [
            AppEvent::ClipboardWrite {
                pane_id,
                content: b"stale".to_vec(),
            },
            AppEvent::AgentProcessDetected {
                pane_id,
                agent: Agent::Codex,
                observed_at: app.clock.now,
            },
            AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Codex),
                state: AgentState::Working,
                visible_blocker: false,
                process_exited: false,
                observed_at: app.clock.now,
            },
            AppEvent::TerminalCwdReported {
                pane_id,
                cwd: shepr_mux::UsableCwd::new("/".into()).expect("root"),
            },
        ] {
            assert!(
                app.admit_runtime_event(AppEvent::Runtime {
                    pane_id,
                    generation: stale,
                    event: Box::new(event),
                })
                .is_none()
            );
        }
        assert!(
            app.admit_runtime_event(AppEvent::Runtime {
                pane_id,
                generation,
                event: Box::new(AppEvent::ClipboardWrite {
                    pane_id,
                    content: Vec::new()
                }),
            })
            .is_some()
        );
    }
}
