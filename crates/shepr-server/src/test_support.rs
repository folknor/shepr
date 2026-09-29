//! Fixtures for this crate's unit tests, built on the public API (and the
//! seams) of the crates they stand in for. Fixtures several crates share live
//! in `shepr-test-fixtures`; these are the ones only this crate's tests use.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::{Direction, PaneId};
use shepr_mux::pane::{PaneRuntime, PaneRuntimeRegistry, PaneState};
use shepr_mux::terminal::{EffectiveStateChange, TerminalState};
use shepr_mux::workspace::{ExistingPane, PaneRemoval, PaneRemovalScope, Tab, TabPane, Workspace};
use shepr_protocol::TerminalId;
use tokio::sync::{Notify, mpsc};

pub(crate) use shepr_test_fixtures::{AppPathsFixture, ValidatedConfigFixture};
pub(crate) use shepr_test_support::{IsolatedEnv, ScratchDir};

/// Pane runtimes with no child: what the pane writes to its child arrives on
/// the returned receiver (`shepr_test_fixtures::ChannelChildIo`).
pub(crate) trait PaneRuntimeFixture: Sized {
    fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self;
    fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self;
    fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>);
    /// Feed `bytes` to the terminal as the child's output.
    fn test_process_pty_bytes(&self, bytes: &[u8]);
    /// Arrange for a writer to try to land `bytes` while the next dirty-patch
    /// collection holds the terminal core. The writer tries the content write
    /// lock once the collection has started, reports through the handle
    /// whether it got it (announcing a new revision mid-collection), then
    /// waits for the returned sender before it writes.
    fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>);
    /// `(rows, cols)`.
    fn current_size(&self) -> (u16, u16);
}

impl PaneRuntimeFixture for PaneRuntime {
    fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (io, rx) = shepr_test_fixtures::ChannelChildIo::new(channel_capacity);
        (
            Self::with_child_io(
                cols,
                rows,
                scrollback_limit_bytes,
                bytes,
                Box::new(io),
                Arc::new(Notify::new()),
            ),
            rx,
        )
    }

    fn test_process_pty_bytes(&self, bytes: &[u8]) {
        self.output_writer().begin().write(bytes);
    }

    fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
        let writer = self.output_writer();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        self.on_next_dirty_collection(Box::new(move || {
            start_tx.send(()).expect("test start channel is open");
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test ready signal arrives within timeout");
        }));
        let handle = std::thread::spawn(move || {
            start_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test start signal arrives within timeout");
            let early = writer.try_begin();
            let announced = early.is_some();
            ready_tx.send(()).expect("test ready channel is open");
            release_rx.recv().expect("test releases the waiting writer");
            early.unwrap_or_else(|| writer.begin()).write(&bytes);
            announced
        });
        (release_tx, handle)
    }

    fn current_size(&self) -> (u16, u16) {
        let grid = self.grid_size();
        (grid.rows.get(), grid.cols.get())
    }
}

pub(crate) trait PaneRuntimeRegistryFixture {
    /// Take every runtime out of the registry.
    fn drain(&mut self) -> std::collections::hash_map::IntoIter<TerminalId, PaneRuntime>;
}

impl PaneRuntimeRegistryFixture for PaneRuntimeRegistry {
    fn drain(&mut self) -> std::collections::hash_map::IntoIter<TerminalId, PaneRuntime> {
        std::mem::take(self).into_iter()
    }
}

/// Workspaces built without spawning a pane.
pub(crate) trait WorkspaceFixture: Sized {
    /// One tab with one pane, named `name`, rooted at `/`: a directory that
    /// exists on every host, so tests that launch the pane can, and that is
    /// neither the runner's cwd nor a git repository.
    fn test_new(name: &str) -> Self;
    /// Split the active tab's focused pane; returns the new pane.
    fn test_split(&mut self, direction: Direction) -> PaneId;
    /// Append a one-pane tab; returns its index.
    fn test_add_tab(&mut self, name: Option<&str>) -> usize;
    /// A workspace whose tab positions, public tab numbers, raw pane ids and
    /// public pane numbers all differ, so code that confuses them is caught.
    fn test_adversarial_identity_state() -> Self;
    fn assert_invariants_for_test(&self);
    fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRemoval>;
    fn resolved_identity_cwd(&self) -> Option<PathBuf>;
}

impl WorkspaceFixture for Workspace {
    fn test_new(name: &str) -> Self {
        let identity_cwd = PathBuf::from("/");
        let existing = ExistingPane {
            pane_id: PaneId::alloc(),
            pane: TabPane::new(PaneState::new(TerminalId::alloc())),
        };
        Self::from_existing_pane(Some(name.to_string()), None, &identity_cwd, existing)
    }

    fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab_index = self.active_tab_index();
        let mut layout = self.active_tab().layout().clone();
        let new_id = layout.split_focused(direction);
        self.commit_new_pane(tab_index, new_id, layout, TerminalId::alloc(), false)
            .expect("test split commits");
        new_id
    }

    fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let mut pane = TabPane::new(PaneState::new(TerminalId::alloc()));
        pane.public_number = self.next_public_pane_number;
        let tab = Tab::single_pane(name.map(str::to_string), self.next_public_tab_number, pane);
        self.commit_new_tab(tab)
            .expect("a test tab takes the workspace's next identities")
            .tab_index
    }

    fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert_eq!(
            ws.close_pane(removed_pane).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(None);
        let final_tab = ws.test_add_tab(None);
        let survivor_root = ws.tabs()[survivor_tab].root_pane();
        let final_root = ws.tabs()[final_tab].root_pane();
        assert!(ws.close_tab(removed_tab).is_some());
        assert!(ws.move_tab(0, ws.tabs().len()));
        ws.switch_tab(
            ws.find_tab_index_for_pane(survivor_root)
                .expect("survivor tab should still exist"),
        );

        assert_ne!(
            ws.active_tab_index() + 1,
            ws.active_tab().number(),
            "adversarial active tab must distinguish position from public tab number"
        );
        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane)
                .expect("test pane has a public pane number"),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        assert_eq!(ws.find_tab_index_for_pane(final_root), Some(1));
        ws
    }

    fn assert_invariants_for_test(&self) {
        let tabs = self.tabs();
        let mut tab_numbers = std::collections::HashSet::new();
        let mut max_tab_number = 0usize;
        let mut live_panes = std::collections::HashSet::new();
        let mut terminal_ids = std::collections::HashSet::new();
        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;

        for (tab_idx, tab) in tabs.iter().enumerate() {
            assert!(
                tab.number() > 0,
                "workspace {} tab {} has invalid public tab number 0",
                self.id,
                tab_idx
            );
            assert!(
                tab_numbers.insert(tab.number()),
                "workspace {} has duplicate public tab number {}",
                self.id,
                tab.number()
            );
            max_tab_number = max_tab_number.max(tab.number());
            assert!(
                tab.panes().contains_key(&tab.root_pane()),
                "workspace {} tab {} root pane {:?} is missing from tab panes",
                self.id,
                tab_idx,
                tab.root_pane()
            );

            let layout_panes = tab.layout().pane_ids();
            let layout_set: std::collections::HashSet<_> = layout_panes.iter().copied().collect();
            assert_eq!(
                layout_panes.len(),
                layout_set.len(),
                "workspace {} tab {} layout contains duplicate pane ids",
                self.id,
                tab_idx
            );
            assert!(
                layout_set.contains(&tab.layout().focused()),
                "workspace {} tab {} focused pane {:?} is not in layout",
                self.id,
                tab_idx,
                tab.layout().focused()
            );
            let pane_set: std::collections::HashSet<_> = tab.panes().keys().copied().collect();
            assert_eq!(
                layout_set, pane_set,
                "workspace {} tab {} layout panes must exactly match pane records",
                self.id, tab_idx
            );

            for (pane_id, pane) in tab.panes() {
                assert!(
                    live_panes.insert(*pane_id),
                    "workspace {} pane {:?} appears in more than one tab",
                    self.id,
                    pane_id
                );
                assert!(
                    pane.public_number > 0,
                    "workspace {} pane {:?} has invalid public pane number 0",
                    self.id,
                    pane_id
                );
                assert!(
                    pane_numbers.insert(pane.public_number),
                    "workspace {} duplicate public pane number {} for pane {:?}",
                    self.id,
                    pane.public_number,
                    pane_id
                );
                max_pane_number = max_pane_number.max(pane.public_number);
                assert!(
                    terminal_ids.insert(pane.attached_terminal_id.clone()),
                    "workspace {} terminal {} is attached to multiple panes",
                    self.id,
                    pane.attached_terminal_id
                );
            }
        }

        assert!(
            self.next_public_tab_number > 0,
            "workspace {} next_public_tab_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_tab_number > max_tab_number,
            "workspace {} next_public_tab_number {} must be greater than max live public tab number {}",
            self.id,
            self.next_public_tab_number,
            max_tab_number
        );

        assert!(
            self.next_public_pane_number > 0,
            "workspace {} next_public_pane_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_pane_number > max_pane_number,
            "workspace {} next_public_pane_number {} must be greater than max live public pane number {}",
            self.id,
            self.next_public_pane_number,
            max_pane_number
        );
    }

    fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRemoval> {
        let plan = self.prepare_pane_removal(pane_id)?;
        self.remove_pane(&plan)
    }

    fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }
}

pub(crate) trait TerminalStateFixture {
    /// Screen detection reporting `agent` in `fallback_state` now, with no
    /// visible blocker and the process running.
    fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange>;
    /// A hook report arriving now, without a session reference.
    fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange>;
}

impl TerminalStateFixture for TerminalState {
    fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            false,
            false,
            Instant::now(),
        )
        .effective_state_change
    }

    fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_at(
            source,
            agent_label,
            state,
            message,
            None,
            seq,
            Instant::now(),
        )
        .and_then(|mutation| mutation.effective_state_change)
    }
}

pub(crate) trait GitStatusRefreshDemandFixture {
    /// Every Git status field.
    const ALL: Self;
}

impl GitStatusRefreshDemandFixture for shepr_mux::git::GitStatusRefreshDemand {
    const ALL: Self = Self {
        branch: true,
        ahead_behind: true,
    };
}

/// A canonical workspace ID that no live test workspace holds, for public
/// pane IDs a pane kept from a workspace it has left. Workspace IDs come from
/// a process-wide counter that tests never drive to the top of the number
/// space, so this one is never allocated.
pub(crate) fn retired_workspace_id() -> shepr_protocol::WorkspaceId {
    shepr_protocol::WorkspaceId::from_number(usize::MAX).expect("nonzero public number")
}

/// A workspace ID from its canonical spelling (`w<number>`).
pub(crate) fn test_workspace_id(id: &str) -> shepr_protocol::WorkspaceId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical workspace id"))
}

/// A Codex resume plan for the session named by the last NUL-separated field
/// of `identity`, launching `argv` instead of the real resume command.
pub(crate) fn test_codex_plan(
    identity: &str,
    argv: Vec<String>,
) -> shepr_agent::agent::resume::AgentResumePlan {
    use shepr_agent::agent::AgentSource;
    use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
    let session_id = identity.rsplit('\0').next().unwrap_or(identity);
    let session = PersistedAgentSession::new(
        AgentSource::Official(Agent::Codex),
        Agent::Codex,
        AgentSessionRef::id(session_id).expect("test session id is valid"),
    )
    .expect("test session is a Codex session");
    let mut plan =
        shepr_agent::agent::resume::plan(&session).expect("a Codex session has a resume plan");
    plan.argv = argv;
    plan
}

/// An API reply as the JSON a client would read.
pub(crate) trait TestResponseJson {
    fn test_json(&self) -> String;
}

impl TestResponseJson for String {
    fn test_json(&self) -> String {
        self.clone()
    }
}

impl TestResponseJson for shepr_api::error::ApiResult {
    fn test_json(&self) -> String {
        shepr_api::error::encode_result("test".into(), self.clone())
    }
}

pub(crate) fn test_json(response: &impl TestResponseJson) -> String {
    response.test_json()
}

/// An API reply read as the success or error it is expected to be; the other
/// is a broken test and panics.
pub(crate) trait TestReply {
    fn success(&self) -> shepr_api::schema::SuccessResponse;
    fn error(&self) -> shepr_api::schema::ErrorResponse;
}

impl TestReply for shepr_api::error::ApiResult {
    fn success(&self) -> shepr_api::schema::SuccessResponse {
        shepr_api::schema::SuccessResponse {
            id: String::new(),
            result: self.clone().expect("expected successful API result"),
        }
    }

    fn error(&self) -> shepr_api::schema::ErrorResponse {
        shepr_api::schema::ErrorResponse {
            id: String::new(),
            error: self.clone().expect_err("expected API error").into_body(),
        }
    }
}

impl TestReply for String {
    fn success(&self) -> shepr_api::schema::SuccessResponse {
        serde_json::from_str(self).expect("expected successful API response")
    }

    fn error(&self) -> shepr_api::schema::ErrorResponse {
        serde_json::from_str(self).expect("expected API error response")
    }
}

pub(crate) fn test_success(response: &impl TestReply) -> shepr_api::schema::SuccessResponse {
    response.success()
}

pub(crate) fn test_error(response: &impl TestReply) -> shepr_api::schema::ErrorResponse {
    response.error()
}
