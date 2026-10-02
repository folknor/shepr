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
use shepr_mux::workspace::{PaneRemoval, PaneRemovalScope, Workspace, WorkspacePane};
use shepr_protocol::TerminalId;
use tokio::sync::{Notify, mpsc};

pub(crate) use shepr_test_fixtures::{AppPathsFixture, ValidatedServerConfigFixture};
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
    /// Feed `bytes` directly to the terminal parser and advance content
    /// revisions. This does not exercise PTY-reader effect dispatch.
    fn test_process_pty_bytes(&self, bytes: &[u8]);
    /// Arrange for a writer to try to land `bytes` while the next dirty-patch
    /// collection holds the terminal core. Once the collection has started,
    /// the writer tries to take the terminal core without waiting, reports
    /// through the handle whether it got it (which would let a write land
    /// mid-collection), then waits for the returned sender before it writes,
    /// blocking on the core if the first attempt failed.
    fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>);
    /// Breaks the terminal core the way a parser panic does: another thread
    /// panics while it holds the core, poisoning it.
    fn test_break_terminal_core(&self);
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
            let took_core = early.is_some();
            ready_tx.send(()).expect("test ready channel is open");
            release_rx.recv().expect("test releases the waiting writer");
            early.unwrap_or_else(|| writer.begin()).write(&bytes);
            took_core
        });
        (release_tx, handle)
    }

    fn test_break_terminal_core(&self) {
        let writer = self.output_writer();
        let outcome = std::thread::spawn(move || {
            let _core = writer.begin();
            panic!("break the terminal core for a test");
        })
        .join();
        assert!(outcome.is_err(), "the core holder panicked");
        assert!(self.terminal_core_broken(), "the panic broke the core");
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
    /// One pane, named `name`, rooted at `/`: a directory that
    /// exists on every host, so tests that launch the pane can, and that is
    /// neither the runner's cwd nor a git repository.
    fn test_new(name: &str) -> Self;
    /// Split the focused pane; returns the new pane.
    fn test_split(&mut self, direction: Direction) -> PaneId;
    /// A workspace whose raw pane ids and public pane numbers differ, so code
    /// that confuses them is caught.
    fn test_adversarial_identity_state() -> Self;
    fn assert_invariants_for_test(&self);
    fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRemoval>;
    fn resolved_identity_cwd(&self) -> Option<PathBuf>;
}

impl WorkspaceFixture for Workspace {
    fn test_new(name: &str) -> Self {
        let identity_cwd = PathBuf::from("/");
        Self::test_from_pane(
            Some(name.to_string()),
            &identity_cwd,
            PaneId::alloc(),
            WorkspacePane::new(PaneState::new(TerminalId::alloc())),
        )
    }

    fn test_split(&mut self, direction: Direction) -> PaneId {
        let mut layout = self.layout().clone();
        let new_id = layout.split_focused(direction);
        let number = self.next_public_pane_number();
        self.commit_new_pane(new_id, layout, TerminalId::alloc(), number, false)
            .expect("test split commits");
        new_id
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

        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane)
                .expect("test pane has a public pane number"),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        ws
    }

    fn assert_invariants_for_test(&self) {
        let mut terminal_ids = std::collections::HashSet::new();
        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;

        assert!(
            self.panes().contains_key(&self.root_pane()),
            "workspace {} root pane {:?} is missing from its panes",
            self.id,
            self.root_pane()
        );
        let layout_panes = self.layout().pane_ids();
        let layout_set: std::collections::HashSet<_> = layout_panes.iter().copied().collect();
        assert_eq!(
            layout_panes.len(),
            layout_set.len(),
            "workspace {} layout contains duplicate pane ids",
            self.id
        );
        assert!(
            layout_set.contains(&self.layout().focused()),
            "workspace {} focused pane {:?} is not in layout",
            self.id,
            self.layout().focused()
        );
        let pane_set: std::collections::HashSet<_> = self.panes().keys().copied().collect();
        assert_eq!(
            layout_set, pane_set,
            "workspace {} layout panes must exactly match pane records",
            self.id
        );
        assert!(
            !self.zoomed() || self.pane_count() > 1,
            "workspace {} is zoomed with a single pane",
            self.id
        );

        for (pane_id, pane) in self.panes() {
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
        source: &str,
        agent_label: &str,
        state: AgentState,
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
        source: &str,
        agent_label: &str,
        state: AgentState,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_at(
            source,
            agent_label,
            state,
            None,
            seq,
            shepr_mux::terminal::state::HookClockSample {
                monotonic: Instant::now(),
                wall: std::time::SystemTime::now(),
            },
        )
        .and_then(|mutation| mutation.effective_state_change)
    }
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
    use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
    use shepr_agent::agent::{AgentSource, IntegrationTarget};
    let session_id = identity.rsplit('\0').next().unwrap_or(identity);
    let session = PersistedAgentSession::new(
        AgentSource::Official(IntegrationTarget::Codex),
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
