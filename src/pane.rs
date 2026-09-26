use std::cell::Cell;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use bytes::Bytes;
use ratatui::{Frame, layout::Rect};
#[cfg(test)]
use tokio::sync::watch;
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};

use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::pty::PtyCommand;
use crate::pty::actor::{
    PtyIoActor, PtyIoActorConfig, PtyIoActorHandle, PtyReadResult, ReaderExit,
};
use crate::render_signal::RenderSignal;

mod agent_detection;
mod cursor;
mod cwd;
mod osc;
mod state;
mod terminal;

/// Time allowed for a restored agent to appear after its resume launch.
pub(crate) const MANAGED_AGENT_RESUME_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

use self::agent_detection::{
    AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_RECHECK, AGENT_STARTUP_GRACE_WINDOW,
    DetectionPublishDecision, DetectionScreenReadDecision, DetectionScreenReadInput,
    PendingIdleConfirmation, ScreenDetectionPublishInput, codex_prompt_ready,
    decide_detection_screen_read, decide_screen_detection_publish,
    detection_update_for_publish_with_osc, mark_detection_content_changed,
    observe_detection_content_change, withhold_agent_absence,
};
use self::cwd::UsableCwd;
use self::terminal::{GhosttyPaneTerminal, PaneTerminal};
pub(crate) use self::terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalReadSnapshot, TerminalSearchDirection,
    TerminalSearchWindow, TerminalTextPoint, TerminalWordMotion,
};
pub use self::{
    state::PaneState,
    terminal::{ScrollMetrics, TerminalCursorState},
};

pub(crate) struct TerminalDirtyPatchSnapshot {
    pub patch: TerminalDirtyPatchOutcome,
    pub content_revision: u64,
    pub scroll_metrics: Option<ScrollMetrics>,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
}

const RELEASE_REACQUIRE_SUPPRESSION: std::time::Duration = std::time::Duration::from_secs(1);
pub(crate) const PANE_TERM: &str = "xterm-256color";
const PANE_COLORTERM: &str = "truecolor";

fn apply_pane_terminal_env(cmd: &mut PtyCommand) {
    // Each pane is rendered by shepr's own terminal layer, not the outer terminal
    // that launched the app. Advertising the inherited TERM leaks the host terminal
    // identity into shells and across SSH, which breaks redraw and cursor movement
    // when the remote side lacks matching terminfo entries.
    cmd.env("TERM", PANE_TERM);
    cmd.env("COLORTERM", PANE_COLORTERM);
    cmd.env("TERM_PROGRAM", "shepr");
    cmd.env("TERM_PROGRAM_VERSION", crate::build_info::version());
    // Host handles refer to the outer terminal, never to this pane.
    for key in [
        "ITERM_SESSION_ID",
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "WT_SESSION",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
    ] {
        cmd.env_remove(key);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaneLaunchEnv {
    extra: Vec<(String, String)>,
    identity: PaneLaunchIdentity,
    agent_absence_startup_hold: bool,
    api_socket_path: std::path::PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum PaneLaunchIdentity {
    #[default]
    Inherit,
    Managed {
        workspace_id: String,
        tab_id: String,
        pane_id: String,
    },
}

impl PaneLaunchEnv {
    pub(crate) fn from_extra(extra: Vec<(String, String)>) -> Self {
        Self {
            extra,
            identity: PaneLaunchIdentity::Inherit,
            agent_absence_startup_hold: false,
            api_socket_path: std::path::PathBuf::new(),
        }
    }

    pub(crate) fn with_api_socket_path(mut self, path: std::path::PathBuf) -> Self {
        self.api_socket_path = path;
        self
    }

    pub(crate) fn for_agent_resume(mut self) -> Self {
        self.agent_absence_startup_hold = true;
        self
    }

    pub(crate) fn with_identity(
        mut self,
        workspace_id: String,
        tab_id: String,
        pane_id: String,
    ) -> Self {
        self.identity = PaneLaunchIdentity::Managed {
            workspace_id,
            tab_id,
            pane_id,
        };
        self
    }
}

fn apply_pane_launch_env(cmd: &mut PtyCommand, launch_env: &PaneLaunchEnv) {
    crate::platform::ssh_agent::apply_pane_env(cmd, &launch_env.api_socket_path);
    // A new pane is not a child agent of the process that started the server.
    // Explicit launch env below can opt back into an intentional child session.
    // `SHEPR_AGENT` is the detector's per-process agent hint, checked before
    // name-based identification; inherited from the server it would label every
    // plain shell in every pane as that agent.
    for key in [
        "SHEPR_AGENT",
        "CODEX_THREAD_ID",
        "OMPCODE",
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ] {
        cmd.env_remove(key);
    }
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    cmd.env(crate::SHEPR_ENV_VAR, crate::SHEPR_ENV_VALUE);
    crate::integration::apply_pane_base_env(cmd, &launch_env.api_socket_path);
    match &launch_env.identity {
        PaneLaunchIdentity::Inherit => {}
        PaneLaunchIdentity::Managed {
            workspace_id,
            tab_id,
            pane_id,
        } => {
            cmd.env(crate::integration::SHEPR_WORKSPACE_ID_ENV_VAR, workspace_id);
            cmd.env(crate::integration::SHEPR_TAB_ID_ENV_VAR, tab_id);
            cmd.env(crate::integration::SHEPR_PANE_ID_ENV_VAR, pane_id);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingAgentRelease {
    agent: Agent,
    until: std::time::Instant,
}

fn active_pending_release(
    pending_release: &Mutex<Option<PendingAgentRelease>>,
    now: std::time::Instant,
) -> Option<Agent> {
    let mut pending_release = pending_release.lock().ok()?;
    match *pending_release {
        Some(pending) if now < pending.until => Some(pending.agent),
        Some(_) => {
            *pending_release = None;
            None
        }
        None => None,
    }
}

async fn publish_state_changed_event(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Option<Agent>,
    state: AgentState,
    visible_blocker: bool,
    process_exited: bool,
    observed_at: std::time::Instant,
) {
    // This runs on the async detector task, not the PTY reader thread.
    // Waiting for queue space here preserves correctness-critical state transitions
    // without blocking pane I/O.
    if let Err(e) = state_events
        .send(AppEvent::StateChanged {
            pane_id,
            agent,
            state,
            visible_blocker,
            process_exited,
            observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            err = %e,
            "failed to deliver StateChanged event"
        );
    }
}

async fn publish_agent_process_detected_event(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Agent,
    observed_at: std::time::Instant,
) {
    if let Err(e) = state_events
        .send(AppEvent::AgentProcessDetected {
            pane_id,
            agent,
            observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            err = %e,
            "failed to deliver AgentProcessDetected event"
        );
    }
}

async fn publish_codex_prompt_observation(
    state_events: &mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Option<Agent>,
    content: &str,
    detection: Option<&crate::detect::AgentDetection>,
    process_exited: bool,
    last_ready: &mut bool,
) {
    let ready = agent == Some(Agent::Codex)
        && !process_exited
        && detection.is_some_and(|detection| detection.state == AgentState::Unknown)
        && codex_prompt_ready(content);
    if ready == *last_ready {
        return;
    }
    *last_ready = ready;
    if let Err(err) = state_events
        .send(AppEvent::CodexPromptObserved { pane_id, ready })
        .await
    {
        warn!(pane = pane_id.raw(), %err, "failed to deliver Codex prompt observation");
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentDetectionPublishUpdate {
    state: AgentState,
    visible_idle: bool,
    visible_blocker: bool,
    visible_working: bool,
    process_exited: bool,
}

async fn apply_agent_detection_publish_update(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Option<Agent>,
    update: AgentDetectionPublishUpdate,
    observed_at: std::time::Instant,
    state: &mut AgentState,
    last_visible_idle: &mut bool,
    last_visible_blocker: &mut bool,
    last_visible_working: &mut bool,
    last_visible_signal_refresh: &mut Option<std::time::Instant>,
    foreground_shell_exit_reported: &mut bool,
) {
    *state = update.state;
    *last_visible_idle = update.visible_idle;
    *last_visible_blocker = update.visible_blocker;
    *last_visible_working = update.visible_working;
    *last_visible_signal_refresh = if update.visible_blocker || update.visible_working {
        Some(observed_at)
    } else {
        None
    };
    if update.process_exited {
        *foreground_shell_exit_reported = true;
    }
    publish_state_changed_event(
        state_events,
        pane_id,
        agent,
        update.state,
        update.visible_blocker,
        update.process_exited,
        observed_at,
    )
    .await;
}

const AGENT_MISS_CONFIRMATION_ATTEMPTS: u8 = 6;
const PROCESS_RECHECK_IDENTIFIED: std::time::Duration = std::time::Duration::from_secs(5);
const PROCESS_RECHECK_MISSING_FOREGROUND_GROUP: std::time::Duration =
    std::time::Duration::from_secs(30);
const PROCESS_ACQUISITION_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
const PROCESS_ACQUISITION_FAST_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);
const PROCESS_ACQUISITION_FAST_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);
const PROCESS_ACQUISITION_SLOW_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);
const PROCESS_ACQUISITION_IDLE_RESET: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct AgentDetectionPresence {
    current_agent: Option<Agent>,
    consecutive_misses: u8,
}

fn absolute_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    crate::platform::process_cwd(pid).filter(|cwd| cwd.is_absolute())
}

fn usable_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    absolute_process_cwd(pid)
        .and_then(UsableCwd::new)
        .map(UsableCwd::into_path_buf)
}

fn foreground_member_cwd_different_from_shell(
    shell_pid: u32,
    shell_cwd: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let job = crate::detect::foreground_job(shell_pid)?;
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = absolute_process_cwd(process.pid) else {
            continue;
        };
        if shell_cwd != Some(&cwd) {
            return Some(cwd);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundShellAgentAction {
    ObserveProbe,
    ReportProcessExit,
    ReportReplacementProcess,
    ClearAgent,
}

fn foreground_shell_agent_action(
    previous_agent: Option<Agent>,
    new_agent: Option<Agent>,
    foreground_is_pane_shell: bool,
    process_exit_reported: bool,
) -> ForegroundShellAgentAction {
    let Some(previous_agent) = previous_agent else {
        return ForegroundShellAgentAction::ObserveProbe;
    };
    if process_exit_reported {
        return if new_agent == Some(previous_agent) {
            ForegroundShellAgentAction::ReportReplacementProcess
        } else if new_agent.is_none() {
            ForegroundShellAgentAction::ClearAgent
        } else {
            ForegroundShellAgentAction::ObserveProbe
        };
    }
    if new_agent.is_some() {
        return ForegroundShellAgentAction::ObserveProbe;
    }

    if foreground_is_pane_shell {
        // Do not clear identity immediately. First publish an idle process-exit
        // transition for the previous agent so notifications and wait-agent callers
        // observe completion before the pane becomes unknown.
        return ForegroundShellAgentAction::ReportProcessExit;
    }

    ForegroundShellAgentAction::ObserveProbe
}

/// Drops retained OSC evidence when changing away from an identified agent.
/// First acquisition keeps bytes that the newly identified process may have
/// emitted before the process probe recognized it.
fn clear_osc_evidence_for_agent_transition(terminal: &PaneTerminal, previous_agent: Option<Agent>) {
    if previous_agent.is_some() {
        terminal.clear_agent_osc_state();
    }
}

fn apply_foreground_shell_agent_action(
    agent_presence: &mut AgentDetectionPresence,
    action: ForegroundShellAgentAction,
    previous_agent: Option<Agent>,
    new_agent: Option<Agent>,
    pending_foreground_shell_clear: &mut bool,
    foreground_shell_exit_reported: &mut bool,
) -> bool {
    match action {
        ForegroundShellAgentAction::ReportReplacementProcess => {
            *pending_foreground_shell_clear = false;
            *foreground_shell_exit_reported = false;
            agent_presence.observe_process_probe(previous_agent);
            true
        }
        ForegroundShellAgentAction::ReportProcessExit => {
            *pending_foreground_shell_clear = true;
            false
        }
        ForegroundShellAgentAction::ClearAgent => {
            *pending_foreground_shell_clear = false;
            *foreground_shell_exit_reported = false;
            agent_presence.clear_current_agent()
        }
        ForegroundShellAgentAction::ObserveProbe => {
            *pending_foreground_shell_clear = false;
            *foreground_shell_exit_reported = false;
            agent_presence.observe_process_probe(new_agent)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessProbeInput {
    current_agent: Option<Agent>,
    suppressed_agent: Option<Agent>,
    foreground_pgid: Option<u32>,
    last_foreground_pgid: Option<u32>,
    has_process_probe: bool,
    acquisition_age: Option<std::time::Duration>,
    pending_foreground_shell_clear: bool,
    elapsed_since_process_check: std::time::Duration,
}

fn foreground_group_changed(
    foreground_pgid: Option<u32>,
    last_foreground_pgid: Option<u32>,
) -> bool {
    foreground_pgid != last_foreground_pgid
        && (foreground_pgid.is_some() || last_foreground_pgid.is_some())
}

// Only kernel-observed foreground groups drive change detection. Remembering an
// inferred group would look like a change on every tick while the kernel stays silent.
fn process_group_for_change_tracking(
    observed_foreground_pgid: Option<u32>,
    probed_process_group_id: Option<u32>,
) -> Option<u32> {
    observed_foreground_pgid?;
    probed_process_group_id.or(observed_foreground_pgid)
}

fn should_skip_process_probe_for_lifecycle_authority(
    full_lifecycle_authority_active: bool,
    input: ProcessProbeInput,
) -> bool {
    full_lifecycle_authority_active
        && input.foreground_pgid.is_some()
        && !input.pending_foreground_shell_clear
        && input.suppressed_agent.is_none()
        && input.has_process_probe
        && !foreground_group_changed(input.foreground_pgid, input.last_foreground_pgid)
}

fn should_probe_foreground_job(input: ProcessProbeInput) -> bool {
    if input.pending_foreground_shell_clear {
        return true;
    }

    let foreground_group_changed =
        foreground_group_changed(input.foreground_pgid, input.last_foreground_pgid);

    if input.suppressed_agent.is_some() {
        return !input.has_process_probe || foreground_group_changed;
    }

    if let Some(acquisition_age) = input.acquisition_age {
        let acquisition_interval = if acquisition_age <= PROCESS_ACQUISITION_FAST_WINDOW {
            PROCESS_ACQUISITION_FAST_RECHECK
        } else {
            PROCESS_ACQUISITION_SLOW_RECHECK
        };
        if acquisition_age <= PROCESS_ACQUISITION_WINDOW
            && input.elapsed_since_process_check >= acquisition_interval
        {
            return true;
        }
    }

    if input.current_agent.is_none() {
        return !input.has_process_probe
            || foreground_group_changed
            || (input.foreground_pgid.is_none()
                && input.elapsed_since_process_check >= PROCESS_RECHECK_MISSING_FOREGROUND_GROUP);
    }

    foreground_group_changed || input.elapsed_since_process_check >= PROCESS_RECHECK_IDENTIFIED
}

fn sync_content_change_acquisition(
    current_agent: Option<Agent>,
    suppressed_agent: Option<Agent>,
    process_group_changed: bool,
    content_changed: bool,
    now: std::time::Instant,
    acquisition_started_at: &mut Option<std::time::Instant>,
    last_content_change_at: &mut Option<std::time::Instant>,
) {
    if current_agent.is_some() || suppressed_agent.is_some() || process_group_changed {
        return;
    }

    if content_changed {
        let should_start = acquisition_started_at.is_none_or(|started| {
            now.duration_since(started) > PROCESS_ACQUISITION_WINDOW
                && last_content_change_at.is_none_or(|last_change| {
                    now.duration_since(last_change) >= PROCESS_ACQUISITION_IDLE_RESET
                })
        });
        if should_start {
            *acquisition_started_at = Some(now);
        }
        *last_content_change_at = Some(now);
        return;
    }

    let Some(acquisition_started) = *acquisition_started_at else {
        return;
    };
    let Some(last_content_change) = *last_content_change_at else {
        return;
    };

    if now.duration_since(acquisition_started) > PROCESS_ACQUISITION_WINDOW
        && now.duration_since(last_content_change) >= PROCESS_ACQUISITION_IDLE_RESET
    {
        *acquisition_started_at = None;
        *last_content_change_at = None;
    }
}

#[derive(Debug, Clone)]
struct ProcessProbeResult {
    process_group_id: Option<u32>,
    foreground_is_pane_shell: bool,
    agent: Option<Agent>,
    process_name: Option<String>,
}

fn agent_hint_for_foreground_job_members(
    job: &crate::platform::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    read_hint(job.process_group_id)
        .or_else(|| agent_hint_for_non_leader_foreground_job_members(job, read_hint))
}

fn agent_hint_for_non_leader_foreground_job_members(
    job: &crate::platform::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    job.processes
        .iter()
        .filter(|process| process.pid != job.process_group_id)
        .find_map(|process| read_hint(process.pid))
}

fn identify_process_group_leader_in_job(
    job: &crate::platform::ForegroundJob,
) -> Option<(Agent, String)> {
    let leader = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id)?;
    let leader_job = crate::platform::ForegroundJob {
        process_group_id: job.process_group_id,
        processes: vec![leader.clone()],
    };
    crate::detect::identify_agent_in_job(&leader_job)
}

fn process_probe_result(
    job: &crate::platform::ForegroundJob,
    pid: u32,
    agent: Agent,
    process_name: String,
) -> ProcessProbeResult {
    ProcessProbeResult {
        process_group_id: Some(job.process_group_id),
        foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
        agent: Some(agent),
        process_name: Some(process_name),
    }
}

fn hinted_process_probe_result(
    job: &crate::platform::ForegroundJob,
    pid: u32,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<ProcessProbeResult> {
    let agent = agent_hint_for_foreground_job_members(job, read_hint)?;
    Some(process_probe_result(
        job,
        pid,
        agent,
        crate::detect::agent_label(agent).to_string(),
    ))
}

fn probe_foreground_process_from_jobs(
    pid: u32,
    foreground_pgid: Option<u32>,
    leader_job: Option<&crate::platform::ForegroundJob>,
    foreground_job: impl FnOnce() -> Option<crate::platform::ForegroundJob>,
    read_hint: impl Fn(u32) -> Option<Agent> + Copy,
) -> ProcessProbeResult {
    if let Some(job) = leader_job {
        if let Some(hinted) = hinted_process_probe_result(job, pid, read_hint) {
            return hinted;
        }
        if let Some((agent, process_name)) = crate::detect::identify_agent_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
    }

    let foreground_job = foreground_job();
    if let Some(job) = foreground_job.as_ref() {
        if let Some(agent) = read_hint(job.process_group_id) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }
        if let Some((agent, process_name)) = identify_process_group_leader_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
        if let Some(agent) = agent_hint_for_non_leader_foreground_job_members(job, read_hint) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }

        let identified = crate::detect::identify_agent_in_job(job);
        return ProcessProbeResult {
            process_group_id: Some(job.process_group_id),
            foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
            agent: identified.as_ref().map(|(agent, _)| *agent),
            process_name: identified.map(|(_, process_name)| process_name),
        };
    }

    ProcessProbeResult {
        process_group_id: foreground_pgid,
        foreground_is_pane_shell: false,
        agent: None,
        process_name: None,
    }
}

fn probe_foreground_process(pid: u32, foreground_pgid: Option<u32>) -> ProcessProbeResult {
    probe_foreground_process_from_jobs(
        pid,
        foreground_pgid,
        foreground_pgid
            .and_then(crate::detect::foreground_group_leader_job)
            .as_ref(),
        || crate::detect::foreground_job(pid),
        crate::platform::process_agent_hint,
    )
}

impl AgentDetectionPresence {
    fn from_agent(current_agent: Option<Agent>) -> Self {
        Self {
            current_agent,
            consecutive_misses: 0,
        }
    }

    fn current_agent(&self) -> Option<Agent> {
        self.current_agent
    }

    fn clear_current_agent(&mut self) -> bool {
        if self.current_agent.is_none() {
            self.consecutive_misses = 0;
            return false;
        }
        self.current_agent = None;
        self.consecutive_misses = 0;
        true
    }

    fn observe_process_probe(&mut self, identified_agent: Option<Agent>) -> bool {
        match identified_agent {
            Some(agent) => {
                self.consecutive_misses = 0;
                if Some(agent) == self.current_agent {
                    return false;
                }
                self.current_agent = Some(agent);
                true
            }
            None => {
                if self.current_agent.is_none() {
                    self.consecutive_misses = 0;
                    return false;
                }
                self.consecutive_misses = self.consecutive_misses.saturating_add(1);
                if self.consecutive_misses < AGENT_MISS_CONFIRMATION_ATTEMPTS {
                    return false;
                }
                self.current_agent = None;
                self.consecutive_misses = 0;
                true
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PaneRuntime - PTY, parser, channels, background tasks
// ---------------------------------------------------------------------------

const MIN_PANE_ROWS: u16 = 2;
const MIN_PANE_COLS: u16 = 4;

/// The smallest geometry a pane's PTY and emulator ever get. Spawn and resize
/// both go through this so the child never sees a 0-row or 0-column PTY and
/// the PTY and the emulator always agree on the size.
fn clamp_pane_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(MIN_PANE_ROWS), cols.max(MIN_PANE_COLS))
}

/// The render a pane needs once a synchronized update (mode 2026) that never
/// ended is force-flushed by its timeout. Every PTY read inside the update
/// asks for it; one sleeping task per pane serves all of those requests
/// instead of one task per read.
#[derive(Debug, Default)]
struct SyncTimeoutRender {
    /// The latest wake-up asked for, while a task is armed; `None` when no
    /// task is sleeping.
    latest: Mutex<Option<std::time::Instant>>,
}

impl SyncTimeoutRender {
    /// Ask for a render at `at`. Returns the instant a new task must first
    /// wake at, or `None` when the armed task will cover it.
    fn arm(&self, at: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *latest {
            Some(armed) => {
                if at > armed {
                    *latest = Some(at);
                }
                None
            }
            None => {
                *latest = Some(at);
                Some(at)
            }
        }
    }

    /// Called by the armed task after waking for `woke_for`. Returns a later
    /// instant to sleep until when a later update asked for one meanwhile;
    /// otherwise disarms and returns `None`, and the task renders. Skipping
    /// the earlier wake is safe: a newer update only begins after the earlier
    /// one ended, and ending an update requests its own render.
    fn next_wake(&self, woke_for: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *latest {
            Some(later) if later > woke_for => Some(later),
            _ => {
                *latest = None;
                None
            }
        }
    }
}

/// PTY runtime for a pane. Owns the terminal, I/O channels, and background tasks.
/// Dropping this aborts async tasks and closes the PTY.
pub struct PaneRuntime {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: PaneRuntimeIo,
    current_size: Cell<(u16, u16, u32, u32)>,
    child_liveness: Arc<ChildLiveness>,
    reported_cwd: Arc<Mutex<Option<std::path::PathBuf>>>,
    persistence_cwd: Mutex<Option<std::path::PathBuf>>,
    content_seq: Arc<AtomicU64>,
    content_write_lock: Arc<Mutex<()>>,
    detection_content_seq: Arc<AtomicU64>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    detect_reset_notify: Arc<Notify>,
    pending_release: Arc<Mutex<Option<PendingAgentRelease>>>,
    preserve_processes_on_drop: bool,
    // Task handles for deterministic shutdown
    detect_handle: Option<tokio::task::AbortHandle>,
}

/// The pane's child identity and the observations used to decide whether it
/// has exited or has been reaped. Keeping the process handle with the pid and
/// wait result prevents each lifecycle path from choosing its own authority.
struct ChildLiveness {
    pid: AtomicU32,
    wait_completed: AtomicBool,
    /// A pidfd or start-time handle opened before the child watcher starts.
    /// Teardown signals through it so a reused pid is never hit.
    leader: Option<crate::platform::ProcessHandle>,
}

impl ChildLiveness {
    fn new(pid: u32, leader: Option<crate::platform::ProcessHandle>) -> Self {
        Self {
            pid: AtomicU32::new(pid),
            wait_completed: AtomicBool::new(false),
            leader,
        }
    }

    fn pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }

    fn mark_wait_completed(&self) {
        self.wait_completed.store(true, Ordering::Release);
    }

    fn wait_completed(&self) -> bool {
        self.wait_completed.load(Ordering::Acquire)
    }

    /// Whether the child has exited; a zombie counts as exited.
    fn has_exited(&self) -> bool {
        self.leader
            .as_ref()
            .map(crate::platform::ProcessHandle::has_exited)
            .unwrap_or_else(|| self.wait_completed())
    }

    /// Whether the child has been reaped and its pid can be reused.
    fn is_reaped(&self) -> bool {
        self.leader
            .as_ref()
            .map(|leader| !leader.is_unreaped())
            .unwrap_or_else(|| self.wait_completed())
    }

    fn leader(&self) -> Option<&crate::platform::ProcessHandle> {
        self.leader.as_ref()
    }
}

enum PaneRuntimeIo {
    Actor(PtyIoActorHandle),
    #[cfg(test)]
    TestChannel {
        sender: mpsc::Sender<Bytes>,
        resize_tx: watch::Sender<(u16, u16, u32, u32)>,
    },
}

impl PaneRuntimeIo {
    fn shutdown(&self) {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.shutdown(),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {}
        }
    }

    fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        match self {
            PaneRuntimeIo::Actor(actor) => {
                actor.resize(
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                    terminal_responses,
                );
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { resize_tx, .. } => {
                let _ = resize_tx.send((rows, cols, cell_width_px, cell_height_px));
            }
        }
    }

    fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.try_write_user_input(bytes),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => sender.try_send(bytes),
        }
    }

    fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.write_terminal_response(response),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => {
                if let Some(bytes) = response() {
                    let _ = sender.try_send(bytes);
                }
            }
        }
    }

    fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: std::time::Duration,
    ) -> std::io::Result<crate::pty::actor::QueuedSubmission> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.queue_user_input_submission(text, enter, delay),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => {
                let sender = sender.clone();
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = sender
                        .try_send(text)
                        .map_err(std::io::Error::other)
                        .and_then(|()| {
                            std::thread::sleep(delay);
                            sender.try_send(enter).map_err(std::io::Error::other)
                        });
                    let _ = reply_tx.send(result);
                });
                Ok(crate::pty::actor::QueuedSubmission {
                    completion: reply_rx,
                    cancel: crate::pty::actor::SubmissionCancel::untracked(),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

impl Drop for PaneRuntime {
    fn drop(&mut self) {
        // Abort detection task immediately and terminate the owned session.
        // The PTY actor shuts down before the process/session policy runs.
        if let Some(handle) = &self.detect_handle {
            handle.abort();
        }
        self.io.shutdown();
        if !self.preserve_processes_on_drop {
            shutdown_pane_processes(self.pane_id, Arc::clone(&self.child_liveness));
        }
    }
}

/// Pane session teardowns still running on their background threads, so a
/// process that is about to exit can let them finish first.
static PANE_TEARDOWNS_IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static PANE_TEARDOWNS_DONE: std::sync::Condvar = std::sync::Condvar::new();

/// Block until every pane session teardown started so far has finished, or
/// `timeout` passes. Returns whether they all finished. For exit paths only:
/// teardown runs off the caller's thread, and a process that exits right
/// after dropping its panes would otherwise cut the SIGTERM/SIGKILL
/// escalation short.
pub(crate) fn wait_for_pane_session_teardowns(timeout: std::time::Duration) -> bool {
    let guard = PANE_TEARDOWNS_IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match PANE_TEARDOWNS_DONE.wait_timeout_while(guard, timeout, |in_flight| *in_flight > 0) {
        Ok((guard, _)) => *guard == 0,
        Err(poisoned) => *poisoned.into_inner().0 == 0,
    }
}

struct PaneTeardownInFlight;

impl PaneTeardownInFlight {
    fn start() -> Self {
        *PANE_TEARDOWNS_IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Self
    }
}

impl Drop for PaneTeardownInFlight {
    fn drop(&mut self) {
        let mut in_flight = PANE_TEARDOWNS_IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight = in_flight.saturating_sub(1);
        if *in_flight == 0 {
            PANE_TEARDOWNS_DONE.notify_all();
        }
    }
}

/// Tear down the pane's session: its leader (the shell or command the pane
/// started, spawned with `setsid`, so the session id is its pid) and every
/// process still in that session, such as background jobs and servers an
/// agent started. Those usually outlive the leader: by the time a pane is
/// closed because its child exited, the leader is long reaped.
///
/// Returns at once. The leader, if still running, is sent SIGHUP here through
/// its process handle; finding the other members (a `/proc` scan) and the
/// SIGHUP/SIGTERM/SIGKILL escalation with its grace periods run on a
/// background thread, so closing a workspace never stalls the caller for
/// them. Every signal goes through a `platform::ProcessHandle` (a pidfd, or
/// on kernels without pidfds a pid checked against its start time right
/// before the kill), so a pid the kernel has handed to an unrelated process
/// is not signalled.
fn shutdown_pane_processes(pane_id: PaneId, child_liveness: Arc<ChildLiveness>) {
    let session_id = child_liveness.pid();
    if session_id == 0 {
        return;
    }
    if let Some(leader) = child_liveness.leader()
        && !child_liveness.has_exited()
    {
        leader.signal(crate::platform::Signal::Hangup);
    }
    // `thread::Builder::spawn` drops its closure on failure, so the work is
    // parked in a shared slot that the inline fallback can still take back.
    let work = Arc::new(Mutex::new(Some((
        PaneTeardownInFlight::start(),
        child_liveness,
    ))));
    let thread_work = Arc::clone(&work);
    let spawned = std::thread::Builder::new()
        .name(format!("shepr-pane-{}-teardown", pane_id.raw()))
        .spawn(move || run_pane_teardown(pane_id, &thread_work));
    if let Err(err) = spawned {
        warn!(
            pane = pane_id.raw(),
            %err,
            "could not start pane teardown thread; tearing down inline"
        );
        run_pane_teardown(pane_id, &work);
    }
}

type PaneTeardownWork = Mutex<Option<(PaneTeardownInFlight, Arc<ChildLiveness>)>>;

fn run_pane_teardown(pane_id: PaneId, work: &PaneTeardownWork) {
    let taken = work
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some((_in_flight, child_liveness)) = taken {
        terminate_pane_session(pane_id, &child_liveness);
    }
}

const PANE_TEARDOWN_STEPS: [(crate::platform::Signal, std::time::Duration); 3] = [
    (
        crate::platform::Signal::Hangup,
        std::time::Duration::from_millis(250),
    ),
    (
        crate::platform::Signal::Terminate,
        std::time::Duration::from_millis(250),
    ),
    (
        crate::platform::Signal::Kill,
        std::time::Duration::from_millis(250),
    ),
];

fn terminate_pane_session(pane_id: PaneId, child_liveness: &ChildLiveness) {
    let session_id = child_liveness.pid();
    let leader_reaped = || child_liveness.is_reaped();
    let mut members = Vec::new();
    for (signal, grace) in PANE_TEARDOWN_STEPS {
        // Rescan every round: a process that forked while being hung up is
        // still in the session and must not escape the next signal.
        members = crate::platform::session_member_handles(session_id, leader_reaped);
        let handles: Vec<&crate::platform::ProcessHandle> = child_liveness
            .leader()
            .into_iter()
            .chain(members.iter())
            .collect();
        for handle in &handles {
            if !handle.has_exited() {
                handle.signal(signal);
            }
        }
        if crate::platform::wait_for_process_exits(&handles, grace) {
            // A signalled member can fork before it exits. The newly forked
            // process was absent from `handles`, so confirm the whole session
            // is empty before ending the escalation.
            if crate::platform::session_member_handles(session_id, leader_reaped).is_empty() {
                info!(
                    pane = pane_id.raw(),
                    session = session_id,
                    ?signal,
                    "pane session terminated"
                );
                return;
            }
        }
    }

    let survivors: Vec<u32> = child_liveness
        .leader()
        .into_iter()
        .chain(members.iter())
        .filter(|handle| !handle.has_exited())
        .map(crate::platform::ProcessHandle::pid)
        .collect();
    warn!(
        pane = pane_id.raw(),
        session = session_id,
        ?survivors,
        "pane session still alive after forced shutdown"
    );
}

#[derive(Clone, Copy)]
pub(crate) struct PaneShellConfig<'a> {
    pub(crate) default_shell: &'a str,
    pub(crate) login_shell: bool,
}

impl<'a> PaneShellConfig<'a> {
    pub(crate) fn new(default_shell: &'a str, login_shell: bool) -> Self {
        Self {
            default_shell,
            login_shell,
        }
    }
}

/// `PtyCommand` selects and resolves the shell at spawn, and uses that resolved
/// path for both exec and the child-visible `SHELL`.
fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> PtyCommand {
    PtyCommand::interactive_shell(shell_config.default_shell, shell_config.login_shell)
}

fn usable_reported_cwd(cwd: std::path::PathBuf) -> Option<std::path::PathBuf> {
    UsableCwd::new(cwd).map(UsableCwd::into_path_buf)
}

fn publish_reported_cwd(
    pane_id: PaneId,
    cwd: std::path::PathBuf,
    reported_cwd: &Arc<Mutex<Option<std::path::PathBuf>>>,
    events: &mpsc::Sender<AppEvent>,
) {
    let Some(cwd) = usable_reported_cwd(cwd) else {
        return;
    };
    if reported_cwd
        .lock()
        .is_ok_and(|current| current.as_ref() == Some(&cwd))
    {
        return;
    }
    // The dedupe slot is updated only once the event is queued: if the shared
    // channel is full, the next identical OSC 7 must retry instead of being
    // swallowed as a duplicate of a report AppState never saw. Only the PTY
    // reader thread publishes for a pane, so check-then-store does not race.
    match events.try_send(AppEvent::TerminalCwdReported {
        pane_id,
        cwd: cwd.clone(),
    }) {
        Ok(()) => {
            if let Ok(mut current) = reported_cwd.lock() {
                *current = Some(cwd);
            }
        }
        Err(err) => {
            warn!(
                pane = pane_id.raw(),
                err = %err,
                "failed to send terminal cwd report"
            );
        }
    }
}

impl PaneRuntime {
    pub fn shutdown(mut self) {
        // Drop owns the ordered shutdown sequence for both explicit and
        // implicit closure; this only selects the process-session policy.
        self.preserve_processes_on_drop = false;
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.terminal.apply_host_terminal_theme(theme);
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<crate::terminal_theme::HostAppearance>,
    ) {
        self.io
            .write_terminal_response(|| self.terminal.apply_host_terminal_appearance(appearance));
    }

    // Runtime construction threads PTY geometry, host context, launch policy, and render hooks.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: &std::path::Path,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            None,
            events,
            render_notify,
            render_dirty,
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: &std::path::Path,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        let mut cmd = pane_shell_command_builder(shell_config);
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
            &cmd,
            "failed to spawn shell",
            initial_history_ansi,
            launch_env.agent_absence_startup_hold,
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: &std::path::Path,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        let Some((program, args)) = argv.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "argv must not be empty",
            ));
        };
        let mut cmd = PtyCommand::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
            &cmd,
            "failed to spawn argv command pane",
            None,
            launch_env.agent_absence_startup_hold,
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    fn spawn_command_builder(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
        cmd: &PtyCommand,
        spawn_error_message: &'static str,
        initial_history_ansi: Option<&str>,
        agent_absence_startup_hold: bool,
    ) -> std::io::Result<Self> {
        let (rows, cols) = clamp_pane_size(rows, cols);
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes);
        let pane_terminal = GhosttyPaneTerminal::new(terminal);
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
        if let Some(ansi) = initial_history_ansi {
            pane_terminal.seed_history_ansi(ansi);
        }
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        let content_write_lock = Arc::new(Mutex::new(()));

        let spawned = crate::pty::backend::spawn_pty(rows, cols, cmd)
            .inspect_err(|err| error!(pane = pane_id.raw(), err = %err, "{spawn_error_message}"))?;

        // --- Child watcher task ---
        let pid = spawned.child.id();
        // Opened before the watcher below exists, so nothing can have reaped
        // the child yet and the pid is certainly still this child's.
        let leader = crate::platform::ProcessHandle::open(pid);
        if leader.is_none() {
            warn!(
                pane = pane_id.raw(),
                "no process handle for the pane's child; closing the pane cannot signal it directly"
            );
        }
        let child_liveness = Arc::new(ChildLiveness::new(pid, leader));
        let reported_cwd = Arc::new(Mutex::new(None));
        let content_seq = Arc::new(AtomicU64::new(0));
        let detection_content_seq = Arc::new(AtomicU64::new(0));
        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        {
            let child_liveness = Arc::clone(&child_liveness);
            let events = events.clone();
            let rt = tokio::runtime::Handle::current();
            let mut child = spawned.child;
            let pid = child.id();
            crate::logging::pane_spawned(pane_id.raw(), pid);
            tokio::task::spawn_blocking(move || {
                // Blocking waitpid on this child only; no process-wide SIGCHLD
                // handling is involved.
                let exit_reason = match child.wait() {
                    Ok(status) => {
                        let exit_reason = crate::platform::classify_child_exit(&status);
                        let status_text = status.to_string();
                        crate::logging::pane_exited(pane_id.raw(), &status_text);
                        exit_reason
                    }
                    Err(e) => {
                        crate::logging::pane_exit_failed(pane_id.raw(), &e.to_string());
                        crate::platform::ChildExitReason::WaitFailed
                    }
                };
                child_liveness.mark_wait_completed();
                // Use blocking send - PaneDied is critical, must not be dropped
                if let Err(e) = rt.block_on(events.send(AppEvent::PaneDied {
                    pane_id,
                    exit_reason,
                })) {
                    error!(pane = pane_id.raw(), err = %e, "failed to send PaneDied event");
                }
            });
        }

        let io = {
            let timer_writer = Arc::new(std::sync::OnceLock::<PtyIoActorHandle>::new());
            let health_terminal = Arc::clone(&terminal);
            let terminal = Arc::clone(&terminal);
            let render_notify = Arc::clone(render_notify);
            let render_dirty = Arc::clone(render_dirty);
            let content_seq = Arc::clone(&content_seq);
            let content_write_lock = Arc::clone(&content_write_lock);
            let detection_content_seq = Arc::clone(&detection_content_seq);
            let child_liveness = Arc::clone(&child_liveness);
            let events = events.clone();
            let reader_exit_events = events.clone();
            let reported_cwd = Arc::clone(&reported_cwd);
            let rt = tokio::runtime::Handle::current();
            let sync_timeout_render = Arc::new(SyncTimeoutRender::default());
            let timer_writer_for_read = Arc::clone(&timer_writer);
            let on_read = Box::new(move |bytes: &[u8]| {
                let _content_write_guard = match content_write_lock.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                content_seq.fetch_add(1, Ordering::AcqRel);
                let shell_pid = child_liveness.pid();
                let result = terminal.process_pty_bytes(pane_id, shell_pid, bytes);
                content_seq.fetch_add(1, Ordering::Release);
                drop(_content_write_guard);
                if result.core_poisoned {
                    // The actor ends the loop and reports the pane dead.
                    return PtyReadResult {
                        terminal_responses: Vec::new(),
                        core_broken: true,
                    };
                }
                if result.default_color_owner_pending {
                    terminal.resolve_default_color_owner(
                        pane_id,
                        shell_pid,
                        result.default_color_generation,
                    );
                }
                observe_detection_content_change(bytes, &detection_content_seq);
                let title_requested =
                    result.terminal_title_changed && render_dirty.request_terminal_title(pane_id);
                let render_requested = result.request_render && render_dirty.request_pty(pane_id);
                if title_requested || render_requested {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay
                    && let Some(first_wake) =
                        sync_timeout_render.arm(std::time::Instant::now() + delay)
                {
                    let sync_timeout_render = Arc::clone(&sync_timeout_render);
                    let render_notify = Arc::clone(&render_notify);
                    let render_dirty = Arc::clone(&render_dirty);
                    let terminal = Arc::clone(&terminal);
                    let content_write_lock = Arc::clone(&content_write_lock);
                    let content_seq = Arc::clone(&content_seq);
                    let detection_content_seq = Arc::clone(&detection_content_seq);
                    let child_liveness = Arc::clone(&child_liveness);
                    let reported_cwd = Arc::clone(&reported_cwd);
                    let events = events.clone();
                    let timer_writer = Arc::clone(&timer_writer_for_read);
                    rt.spawn(async move {
                        let mut wake_at = first_wake;
                        loop {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at))
                                .await;
                            match sync_timeout_render.next_wake(wake_at) {
                                Some(later) => wake_at = later,
                                None => break,
                            }
                        }
                        // The terminal and content locks are synchronous. Keep
                        // their wait off a Tokio worker when a timer fires.
                        tokio::task::spawn_blocking(move || {
                            let _content_write_guard = match content_write_lock.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            content_seq.fetch_add(1, Ordering::AcqRel);
                            let result = terminal.flush_expired_synchronized_output(
                                pane_id,
                                child_liveness.pid(),
                            );
                            content_seq.fetch_add(1, Ordering::Release);
                            drop(_content_write_guard);
                            if result.default_color_owner_pending {
                                terminal.resolve_default_color_owner(
                                    pane_id,
                                    child_liveness.pid(),
                                    result.default_color_generation,
                                );
                            }
                            if result.request_render {
                                detection_content_seq.fetch_add(1, Ordering::AcqRel);
                            }
                            let title_requested = result.terminal_title_changed
                                && render_dirty.request_terminal_title(pane_id);
                            let render_requested = result.request_render
                                && render_dirty.request_pty(pane_id);
                            if title_requested || render_requested {
                                render_notify.notify_one();
                            }
                            if let Some(cwd) = result.reported_cwd {
                                publish_reported_cwd(pane_id, cwd, &reported_cwd, &events);
                            }
                            for content in result.clipboard_writes {
                                if let Err(err) = events.try_send(AppEvent::ClipboardWrite { content }) {
                                    warn!(pane = pane_id.raw(), err = %err, "failed to send OSC 52 clipboard write");
                                }
                            }
                            if let Some(writer) = timer_writer.get() {
                                for response in result.terminal_responses {
                                    writer.write_terminal_response(|| Some(response));
                                }
                            }
                        });
                    });
                }
                if let Some(cwd) = result.reported_cwd.clone() {
                    publish_reported_cwd(pane_id, cwd, &reported_cwd, &events);
                }
                for content in result.clipboard_writes {
                    if let Err(err) = events.try_send(AppEvent::ClipboardWrite { content }) {
                        warn!(
                            pane = pane_id.raw(),
                            err = %err,
                            "failed to send OSC 52 clipboard write"
                        );
                    }
                }
                PtyReadResult {
                    terminal_responses: result.terminal_responses,
                    core_broken: false,
                }
            });
            // A normal reader exit needs no report: the child watcher above
            // sends PaneDied once the child is reaped. A panic in the terminal
            // core is different, whether it hit this reader or another thread
            // holding the core lock (the actor's `core_broken` check, or the
            // next read, then finds the lock poisoned). The PTY actor closes the
            // master, but a child that ignores SIGHUP keeps running and is
            // never reaped, and the poisoned core leaves the pane frozen.
            // Report the pane dead so the app removes it and tears down its
            // session. The child watcher's own PaneDied that may follow is
            // dropped by the app for a pane that no longer exists.
            let on_reader_exit: Box<dyn FnOnce(ReaderExit) + Send> = {
                Box::new(move |exit: ReaderExit| {
                    if exit != ReaderExit::Panicked {
                        return;
                    }
                    // Not Interrupted: that checkpoints the session first,
                    // which would read history out of the broken core.
                    if let Err(err) = reader_exit_events.blocking_send(AppEvent::PaneDied {
                        pane_id,
                        exit_reason: crate::platform::ChildExitReason::Exited,
                    }) {
                        error!(
                            pane = pane_id.raw(),
                            err = %err,
                            "failed to report a pane whose PTY reader panicked"
                        );
                    }
                })
            };
            let actor = PtyIoActor::spawn(PtyIoActorConfig {
                pane_id: pane_id.raw(),
                master_fd: spawned.master_fd,
                on_read,
                on_reader_exit: Some(on_reader_exit),
                // A render, detection or API read that panicked while holding
                // the core lock breaks it for good; end the pane within the
                // actor's idle poll even if the child never prints again.
                core_broken: Some(Box::new(move || health_terminal.core_poisoned())),
            })?;
            let _ = timer_writer.set(actor.clone());
            PaneRuntimeIo::Actor(actor)
        };

        // --- Detection task ---
        let (detect_handle, detect_reset_notify, pending_release) = {
            use crate::detect;
            use std::time::{Duration, Instant};

            const TICK_UNIDENTIFIED: Duration = Duration::from_millis(500);
            const TICK_IDENTIFIED: Duration = Duration::from_millis(300);
            const TICK_PENDING_RELEASE: Duration = Duration::from_millis(50);

            let child_liveness = Arc::clone(&child_liveness);
            let terminal = Arc::clone(&terminal);
            let state_events = events.clone();
            let detection_content_seq = Arc::clone(&detection_content_seq);
            let full_lifecycle_authority_active_for_task =
                Arc::clone(&full_lifecycle_authority_active);
            let render_notify = Arc::clone(render_notify);
            let render_dirty = Arc::clone(render_dirty);
            let detect_reset_notify = Arc::new(Notify::new());
            let detect_reset = Arc::clone(&detect_reset_notify);
            let pending_release = Arc::new(Mutex::new(None));
            let pending_release_for_task = Arc::clone(&pending_release);

            let handle = tokio::spawn(async move {
                let mut agent_presence = AgentDetectionPresence::from_agent(None);
                let mut state = AgentState::Idle;
                let mut last_visible_idle = false;
                let mut last_process_check = Instant::now();
                let mut last_foreground_pgid = None;
                let mut has_process_probe = false;
                let mut acquisition_started_at = None;
                let mut last_content_change_at = None;
                let mut pending_foreground_shell_clear = false;
                let mut foreground_shell_exit_reported = false;
                let mut release_was_active = false;
                let mut last_visible_blocker = false;
                let mut last_visible_working = false;
                let mut last_visible_signal_refresh = None;
                let mut last_detection_text = String::new();
                let mut last_screen_scan_detection_content_seq = None;
                let mut agent_startup_grace_until = None;
                let mut pending_idle = PendingIdleConfirmation::default();
                let mut last_codex_prompt_ready = false;
                // See `withhold_agent_absence`: a restored pane's seeded agent
                // must not be withdrawn while its resumed process starts.
                let mut agent_absence_hold_until = agent_absence_startup_hold
                    .then(|| Instant::now().checked_add(AGENT_ABSENCE_STARTUP_HOLD))
                    .flatten();

                tokio::time::sleep(Duration::from_millis(50)).await;

                loop {
                    let now_for_tick = Instant::now();
                    let tick = if active_pending_release(&pending_release_for_task, now_for_tick)
                        .is_some()
                        || terminal.has_transient_default_color_override()
                    {
                        TICK_PENDING_RELEASE
                    } else if pending_idle.active() {
                        AGENT_PENDING_IDLE_RECHECK
                    } else if agent_presence.current_agent().is_none() {
                        TICK_UNIDENTIFIED
                    } else {
                        TICK_IDENTIFIED
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = detect_reset.notified() => {
                            publish_codex_prompt_observation(
                                &state_events, pane_id, Some(Agent::Codex), "", None, false,
                                &mut last_codex_prompt_ready,
                            ).await;
                            agent_presence = AgentDetectionPresence::from_agent(None);
                            state = AgentState::Unknown;
                            last_visible_idle = false;
                            last_foreground_pgid = None;
                            has_process_probe = false;
                            acquisition_started_at = None;
                            last_content_change_at = None;
                            pending_foreground_shell_clear = false;
                            foreground_shell_exit_reported = false;
                            release_was_active = false;
                            last_visible_blocker = false;
                            last_visible_working = false;
                            last_visible_signal_refresh = None;
                            last_detection_text.clear();
                            last_screen_scan_detection_content_seq = None;
                            agent_startup_grace_until = None;
                            pending_idle.clear();
                        }
                    }

                    let now = Instant::now();
                    let suppressed_agent = active_pending_release(&pending_release_for_task, now);
                    if suppressed_agent.is_none() && release_was_active {
                        has_process_probe = false;
                        acquisition_started_at = None;
                        last_content_change_at = None;
                    }
                    release_was_active = suppressed_agent.is_some();
                    let pid = child_liveness.pid();
                    let mut agent = agent_presence.current_agent();
                    let lifecycle_authority_active =
                        full_lifecycle_authority_active_for_task.load(Ordering::Acquire);
                    let process_probe_input = ProcessProbeInput {
                        current_agent: agent,
                        suppressed_agent,
                        foreground_pgid: last_foreground_pgid,
                        last_foreground_pgid,
                        has_process_probe,
                        acquisition_age: acquisition_started_at
                            .map(|started| now.duration_since(started)),
                        pending_foreground_shell_clear,
                        elapsed_since_process_check: now.duration_since(last_process_check),
                    };
                    let foreground_pgid = if pid > 0 {
                        match tokio::task::spawn_blocking(move || {
                            detect::foreground_process_group_id(pid)
                        })
                        .await
                        {
                            Ok(pgid) => pgid,
                            Err(error) => {
                                tracing::warn!(?error, "foreground process group probe failed");
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let process_group_changed =
                        foreground_group_changed(foreground_pgid, last_foreground_pgid);
                    let should_check_process = pid > 0 && {
                        let process_probe_input = ProcessProbeInput {
                            foreground_pgid,
                            ..process_probe_input
                        };
                        !should_skip_process_probe_for_lifecycle_authority(
                            lifecycle_authority_active,
                            process_probe_input,
                        ) && should_probe_foreground_job(process_probe_input)
                    };

                    let mut agent_changed = false;
                    if should_check_process {
                        last_process_check = now;
                        let had_process_probe = has_process_probe;
                        has_process_probe = true;
                        if pid > 0 {
                            let probe = match tokio::task::spawn_blocking(move || {
                                probe_foreground_process(pid, foreground_pgid)
                            })
                            .await
                            {
                                Ok(probe) => probe,
                                Err(error) => {
                                    tracing::warn!(?error, "foreground process probe failed");
                                    continue;
                                }
                            };
                            let process_name = probe.process_name;
                            let process_group_id = probe.process_group_id;
                            let tracked_process_group_id = process_group_for_change_tracking(
                                foreground_pgid,
                                process_group_id,
                            );
                            let foreground_is_pane_shell = probe.foreground_is_pane_shell;
                            let mut new_agent = probe.agent;

                            if let Some(suppressed_agent) = suppressed_agent {
                                if new_agent == Some(suppressed_agent) {
                                    new_agent = None;
                                } else if let Ok(mut pending_release) =
                                    pending_release_for_task.lock()
                                {
                                    *pending_release = None;
                                }
                            }

                            let previous_agent = agent_presence.current_agent();
                            let foreground_action = foreground_shell_agent_action(
                                previous_agent,
                                new_agent,
                                foreground_is_pane_shell,
                                foreground_shell_exit_reported,
                            );
                            let changed = apply_foreground_shell_agent_action(
                                &mut agent_presence,
                                foreground_action,
                                previous_agent,
                                new_agent,
                                &mut pending_foreground_shell_clear,
                                &mut foreground_shell_exit_reported,
                            );
                            last_foreground_pgid = tracked_process_group_id;
                            if new_agent.is_some() {
                                acquisition_started_at = None;
                                last_content_change_at = None;
                            } else if agent_presence.current_agent().is_none()
                                && had_process_probe
                                && process_group_changed
                            {
                                acquisition_started_at = Some(now);
                            }
                            if changed {
                                agent = agent_presence.current_agent();
                                if agent != previous_agent
                                    || foreground_action
                                        == ForegroundShellAgentAction::ReportReplacementProcess
                                {
                                    pending_idle.clear();
                                    last_codex_prompt_ready = false;
                                    last_screen_scan_detection_content_seq = None;
                                    // A replacement agent must not inherit OSC
                                    // evidence from the previous process; a first
                                    // acquisition keeps the evidence its own
                                    // process already emitted.
                                    clear_osc_evidence_for_agent_transition(
                                        &terminal,
                                        previous_agent,
                                    );
                                    if let Some(agent) = agent {
                                        agent_absence_hold_until = None;
                                        agent_startup_grace_until =
                                            Some(now + AGENT_STARTUP_GRACE_WINDOW);
                                        state = AgentState::Unknown;
                                        last_visible_idle = false;
                                        last_visible_blocker = false;
                                        last_visible_working = false;
                                        last_visible_signal_refresh = None;
                                        publish_agent_process_detected_event(
                                            state_events.clone(),
                                            pane_id,
                                            agent,
                                            now,
                                        )
                                        .await;
                                    } else {
                                        agent_startup_grace_until = None;
                                    }
                                }
                                if let Some(process_name) = process_name {
                                    info!(
                                        pane = pane_id.raw(),
                                        previous_agent = ?previous_agent,
                                        ?agent,
                                        process = %process_name,
                                        pgid = ?process_group_id,
                                        "agent changed"
                                    );
                                } else {
                                    info!(
                                        pane = pane_id.raw(),
                                        previous_agent = ?previous_agent,
                                        ?agent,
                                        pgid = ?process_group_id,
                                        "agent changed"
                                    );
                                }
                                agent_changed = true;
                            }
                        }
                    }

                    let pid = child_liveness.pid();
                    // The restore check reads /proc only while an override is
                    // active; keep that rare probe off the runtime worker too.
                    if pid > 0 && terminal.has_transient_default_color_override() {
                        let theme_terminal = Arc::clone(&terminal);
                        match tokio::task::spawn_blocking(move || {
                            theme_terminal.maybe_restore_host_terminal_theme(pane_id, pid)
                        })
                        .await
                        {
                            Ok(true) => {
                                if render_dirty.request_pty(pane_id) {
                                    render_notify.notify_one();
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(?error, "host terminal theme probe failed");
                            }
                        }
                    }

                    let process_exited = pending_foreground_shell_clear
                        && agent.is_some()
                        && !foreground_shell_exit_reported;

                    if lifecycle_authority_active && !process_exited {
                        pending_idle.clear();
                        continue;
                    }

                    if let Some(until) = agent_startup_grace_until {
                        if process_exited {
                            agent_startup_grace_until = None;
                            last_screen_scan_detection_content_seq = None;
                            pending_idle.clear();
                        } else {
                            if now < until {
                                pending_idle.clear();
                                continue;
                            }
                            agent_startup_grace_until = None;
                            pending_idle.clear();
                            continue;
                        }
                    }

                    let current_detection_content_seq =
                        Some(detection_content_seq.load(Ordering::Relaxed));
                    match decide_detection_screen_read(DetectionScreenReadInput {
                        state,
                        agent,
                        pending_idle_active: pending_idle.active(),
                        agent_changed,
                        process_exited,
                        current_detection_content_seq,
                        last_screen_scan_detection_content_seq,
                    }) {
                        DetectionScreenReadDecision::Read => {}
                        DetectionScreenReadDecision::Skip => continue,
                    }

                    // Without an identified agent, detection reports `Unknown`
                    // whatever the screen shows, and the screen would only feed
                    // the content-change signal for process acquisition. The PTY
                    // read counter gives that signal without copying the screen
                    // out of the terminal core, which plain shell panes would
                    // otherwise do on every tick.
                    let (content, content_changed, osc_title, osc_progress) = if agent.is_some() {
                        let content = terminal.detection_text();
                        let content_changed = content != last_detection_text;
                        last_detection_text.clone_from(&content);
                        (
                            content,
                            content_changed,
                            terminal.agent_osc_title(),
                            terminal.agent_osc_progress(),
                        )
                    } else {
                        let content_changed =
                            last_screen_scan_detection_content_seq != current_detection_content_seq;
                        last_detection_text.clear();
                        (String::new(), content_changed, String::new(), String::new())
                    };
                    last_screen_scan_detection_content_seq = current_detection_content_seq;
                    let screen_detection = detection_update_for_publish_with_osc(
                        agent,
                        &content,
                        &osc_title,
                        &osc_progress,
                        process_exited,
                    );
                    publish_codex_prompt_observation(
                        &state_events,
                        pane_id,
                        agent,
                        &content,
                        screen_detection.as_ref(),
                        process_exited,
                        &mut last_codex_prompt_ready,
                    )
                    .await;
                    let Some(screen_detection) = screen_detection else {
                        pending_idle.clear();
                        continue;
                    };
                    sync_content_change_acquisition(
                        agent_presence.current_agent(),
                        suppressed_agent,
                        process_group_changed,
                        content_changed,
                        now,
                        &mut acquisition_started_at,
                        &mut last_content_change_at,
                    );
                    if withhold_agent_absence(agent, &mut agent_absence_hold_until, now) {
                        pending_idle.clear();
                        continue;
                    }
                    match decide_screen_detection_publish(
                        ScreenDetectionPublishInput {
                            screen_detection,
                            current_state: state,
                            last_visible_idle,
                            last_visible_blocker,
                            last_visible_working,
                            last_visible_signal_refresh,
                            process_exited,
                            agent_changed,
                            now,
                        },
                        &mut pending_idle,
                    ) {
                        DetectionPublishDecision::NoPublish => {}
                        DetectionPublishDecision::Publish {
                            state: new_state,
                            visible_idle,
                            visible_blocker,
                            visible_working,
                            process_exited: publish_process_exited,
                        } => {
                            apply_agent_detection_publish_update(
                                state_events.clone(),
                                pane_id,
                                agent,
                                AgentDetectionPublishUpdate {
                                    state: new_state,
                                    visible_idle,
                                    visible_blocker,
                                    visible_working,
                                    process_exited: publish_process_exited,
                                },
                                now,
                                &mut state,
                                &mut last_visible_idle,
                                &mut last_visible_blocker,
                                &mut last_visible_working,
                                &mut last_visible_signal_refresh,
                                &mut foreground_shell_exit_reported,
                            )
                            .await;
                        }
                    }
                }
            });
            (
                Some(handle.abort_handle()),
                detect_reset_notify,
                pending_release,
            )
        };

        Ok(Self {
            pane_id,
            terminal,
            io,
            current_size: Cell::new((rows, cols, 0, 0)),
            child_liveness,
            reported_cwd,
            persistence_cwd: Mutex::new(None),
            content_seq,
            content_write_lock,
            detection_content_seq,
            full_lifecycle_authority_active,
            detect_reset_notify,
            pending_release,
            preserve_processes_on_drop: false,
            detect_handle,
        })
    }

    pub fn begin_graceful_release(&self, agent: Agent) {
        if let Ok(mut pending_release) = self.pending_release.lock() {
            *pending_release = Some(PendingAgentRelease {
                agent,
                until: std::time::Instant::now() + RELEASE_REACQUIRE_SUPPRESSION,
            });
        }
        self.detect_reset_notify.notify_one();
    }

    pub fn reset_agent_detection(&self) {
        self.detect_reset_notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_reset_notify_for_test(&self) -> Arc<Notify> {
        Arc::clone(&self.detect_reset_notify)
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        let previous = self
            .full_lifecycle_authority_active
            .swap(active, Ordering::AcqRel);
        if active && !previous {
            self.detect_reset_notify.notify_one();
        }
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        let (rows, cols, _, _) = self.current_size.get();
        (rows, cols)
    }

    pub(crate) fn content_seq(&self) -> u64 {
        self.content_seq.load(Ordering::Acquire)
    }

    /// Resize if the dimensions actually changed.
    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        let (rows, cols) = clamp_pane_size(rows, cols);
        let size = (rows, cols, cell_width_px, cell_height_px);
        if self.current_size.get() == size {
            return;
        }
        self.current_size.set(size);
        let _content_write_guard = match self.content_write_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.content_seq.fetch_add(1, Ordering::AcqRel);
        let terminal_responses = self
            .terminal
            .resize(rows, cols, cell_width_px, cell_height_px);
        self.content_seq.fetch_add(1, Ordering::Release);
        drop(_content_write_guard);
        mark_detection_content_changed(&self.detection_content_seq);
        self.io.resize(
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            terminal_responses,
        );
    }

    /// Scroll up by N lines (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        self.terminal.scroll_up(lines);
    }

    /// Scroll down by N lines (toward live output).
    pub fn scroll_down(&self, lines: usize) {
        self.terminal.scroll_down(lines);
    }

    pub fn clear_screen(&self) -> Result<(), String> {
        let guard = match self.content_write_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.content_seq.fetch_add(1, Ordering::AcqRel);
        let result = self.terminal.clear_screen();
        self.content_seq.fetch_add(1, Ordering::Release);
        drop(guard);
        mark_detection_content_changed(&self.detection_content_seq);
        result
    }

    /// Reset scroll to live view (offset = 0).
    pub fn scroll_reset(&self) {
        self.terminal.scroll_reset();
    }

    /// Set scrollback offset measured from the live bottom of the terminal.
    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.terminal.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.terminal.scroll_metrics()
    }

    pub(crate) fn search_text_window(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: crate::pane::TerminalSearchDirection,
        cursor: crate::pane::TerminalTextPoint,
        previous: Option<(
            crate::pane::TerminalTextPoint,
            crate::pane::TerminalTextPoint,
        )>,
        limit: usize,
    ) -> crate::pane::TerminalSearchWindow {
        self.terminal
            .search_text_window(query, case_sensitive, direction, cursor, previous, limit)
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.word_motion_target(row, col, motion)
    }

    pub(crate) fn terminal_dimensions(&self) -> Option<(u16, u16)> {
        self.terminal.dimensions()
    }

    pub(crate) fn paragraph_motion_target(
        &self,
        row: u32,
        direction: i8,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.paragraph_motion_target(row, direction)
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.terminal.bracketed_paste_enabled()
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.terminal.focus_reporting_enabled()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.terminal.mouse_reporting_enabled()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.terminal.sgr_pixel_mouse_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.terminal.plain_page_keys_use_host_scrollback()
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.terminal.alternate_screen_active()
    }

    pub fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState> {
        if !show_cursor {
            return None;
        }
        let cursor = self.terminal.cursor_state()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    pub(crate) fn synchronized_output_state(&self) -> (bool, u64) {
        self.terminal.synchronized_output_state()
    }

    pub fn visible_text(&self) -> String {
        self.terminal.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        self.terminal.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        self.terminal.detection_text()
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.terminal.terminal_title()
    }

    pub fn agent_osc_title(&self) -> String {
        self.terminal.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.terminal.agent_osc_progress()
    }

    pub(crate) fn recent_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.terminal.recent_text_snapshot(lines)
    }

    pub(crate) fn recent_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.terminal.recent_ansi_snapshot(lines)
    }

    pub(crate) fn recent_unwrapped_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.terminal.recent_unwrapped_text_snapshot(lines)
    }

    pub(crate) fn recent_unwrapped_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.terminal.recent_unwrapped_ansi_snapshot(lines)
    }

    pub fn snapshot_history(&self) -> Option<String> {
        self.terminal.primary_history_ansi()
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.terminal.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch_snapshot(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> Option<TerminalDirtyPatchSnapshot> {
        // PTY/resize writers announce changes before locking the terminal core.
        // Exclude them until rows and metadata have been paired with their revision.
        let _content_guard = self
            .content_write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revision = self.content_seq();
        if !revision.is_multiple_of(2) {
            return None;
        }
        let patch = self.terminal.collect_dirty_patch(area_width, area_height);
        if matches!(patch, TerminalDirtyPatchOutcome::Fallback) {
            return None;
        }
        let snapshot = TerminalDirtyPatchSnapshot {
            patch,
            content_revision: revision,
            scroll_metrics: self.scroll_metrics(),
            mouse_reporting: self.mouse_reporting_enabled(),
            sgr_pixel_mouse: self.sgr_pixel_mouse_enabled(),
            alternate_screen_active: self.alternate_screen_active(),
        };
        (self.content_seq() == revision).then_some(snapshot)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.terminal.visible_hyperlinks(area)
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        // Legacy only when the terminal core is unreadable (a poisoned lock).
        self.terminal
            .keyboard_protocol(crate::input::KeyboardProtocol::Legacy)
    }

    pub fn modify_other_keys_level(&self) -> u8 {
        self.terminal.modify_other_keys_level()
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        self.terminal
            .encode_terminal_key(key, self.keyboard_protocol())
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.io.try_send_bytes(bytes)
    }

    pub fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: std::time::Duration,
    ) -> std::io::Result<crate::pty::actor::QueuedSubmission> {
        self.io.queue_user_input_submission(text, enter, delay)
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.try_send_bytes(self.paste_payload(text))
    }

    fn paste_payload(&self, text: String) -> Bytes {
        let bracketed = self.bracketed_paste_enabled();
        let payload = if bracketed {
            let safe = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
            format!("\x1b[200~{safe}\x1b[201~")
        } else {
            text
        };
        Bytes::from(payload)
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        if !self.focus_reporting_enabled() {
            return false;
        }

        let bytes = crate::ghostty::encode_focus(event);
        if let Err(err) = self.try_send_bytes(Bytes::from_static(bytes)) {
            warn!(err = %err, ?event, "failed to forward pane focus event");
        }
        true
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        u16,
        Vec<crate::ghostty::ScreenTextRow>,
    )> {
        self.terminal.screen_text_snapshot()
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting_enabled() {
            return None;
        }
        self.terminal.encode_mouse_button(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal.encode_mouse_motion(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::MouseReport {
            return None;
        }
        self.terminal.encode_mouse_wheel(kind, position, modifiers)
    }

    pub(crate) fn pixel_size(&self) -> Option<(u32, u32)> {
        let (rows, cols, cell_width_px, cell_height_px) = self.current_size.get();
        let width = u32::from(cols).checked_mul(cell_width_px)?;
        let height = u32::from(rows).checked_mul(cell_height_px)?;
        (width > 0 && height > 0).then_some((width, height))
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(self.encode_terminal_key(crate::input::TerminalKey::new(
            key,
            crossterm::event::KeyModifiers::empty(),
        )))
    }

    /// Get the current working directory of the child shell process.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        if let Some(cwd) = self
            .reported_cwd
            .lock()
            .ok()
            .and_then(|reported_cwd| reported_cwd.clone())
        {
            return Some(cwd);
        }

        let pid = self.child_liveness.pid();
        crate::platform::process_cwd(pid)
    }

    pub fn cwd_for_persistence(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_liveness.pid();
        if let Some(cwd) = (!self.child_liveness.wait_completed())
            .then(|| crate::platform::process_cwd(pid))
            .flatten()
            .filter(|cwd| cwd.is_absolute())
        {
            // Persistence observations must not change OSC authority or follow-cwd behavior.
            if let Ok(mut known) = self.persistence_cwd.lock() {
                *known = Some(cwd.clone());
            }
            return Some(cwd);
        }
        self.persistence_cwd
            .lock()
            .ok()
            .and_then(|cwd| cwd.clone())
            .or_else(|| self.reported_cwd.lock().ok().and_then(|cwd| cwd.clone()))
    }

    pub fn child_pid(&self) -> Option<u32> {
        let pid = self.child_liveness.pid();
        (pid > 0).then_some(pid)
    }

    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        let leader_cwd = self
            .child_pid()
            .and_then(crate::platform::foreground_process_group_id)
            .and_then(usable_process_cwd);
        leader_cwd.or_else(|| self.cwd())
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_liveness.pid();
        let foreground_pgid = crate::platform::foreground_process_group_id(pid);
        let leader_cwd = foreground_pgid.and_then(absolute_process_cwd);

        // The group leader's cwd is authoritative (issue #3270): a helper
        // process that chdirs elsewhere inside the same foreground group
        // must not override it. Scan other members only when the leader's
        // cwd cannot be read at all.
        leader_cwd.or_else(|| {
            let shell_cwd = absolute_process_cwd(pid);
            foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref())
        })
    }
}

#[cfg(test)]
impl PaneRuntime {
    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], 4)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], capacity)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    pub(crate) fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
        let terminal = Arc::clone(&self.terminal);
        let sequence = Arc::clone(&self.content_seq);
        let write_lock = Arc::clone(&self.content_write_lock);
        let pane_id = self.pane_id;
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        self.terminal
            .ghostty
            .core
            .lock()
            .expect("test terminal core lock is not poisoned")
            .dirty_collection_hook = Some(Box::new(move || {
            start_tx.send(()).expect("test start channel is open");
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("test ready signal arrives within timeout");
        }));
        let writer = std::thread::spawn(move || {
            start_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("test start signal arrives within timeout");
            let guard = match write_lock.try_lock() {
                Ok(guard) => Some(guard),
                Err(std::sync::TryLockError::WouldBlock) => None,
                Err(error) => panic!("poisoned content lock: {error}"),
            };
            let announced = guard.is_some();
            if announced {
                sequence.fetch_add(1, Ordering::AcqRel);
                assert!(matches!(
                    terminal.ghostty.core.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
            }
            ready_tx.send(()).expect("test ready channel is open");
            let _ = release_rx.recv();
            let _guard = guard.unwrap_or_else(|| {
                let guard = write_lock
                    .lock()
                    .expect("test content write lock is not poisoned");
                sequence.fetch_add(1, Ordering::AcqRel);
                guard
            });
            let _ = terminal.process_pty_bytes(pane_id, 0, &bytes);
            sequence.fetch_add(1, Ordering::Release);
            announced
        });
        (release_tx, writer)
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        let _content_write_guard = match self.content_write_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.content_seq.fetch_add(1, Ordering::AcqRel);
        let _ = self.terminal.process_pty_bytes(self.pane_id, 0, bytes);
        self.content_seq.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let (resize_tx, _resize_rx) = watch::channel((rows, cols, 0, 0));
        let mut terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes);
        terminal.write(bytes);
        let pane_id = PaneId::from_raw(0);
        let terminal = Arc::new(PaneTerminal::new(GhosttyPaneTerminal::new(terminal)));

        (
            Self {
                pane_id,
                terminal,
                io: PaneRuntimeIo::TestChannel {
                    sender: tx,
                    resize_tx,
                },
                current_size: Cell::new((rows, cols, 0, 0)),
                child_liveness: Arc::new(ChildLiveness::new(0, None)),
                reported_cwd: Arc::new(Mutex::new(None)),
                persistence_cwd: Mutex::new(None),
                content_seq: Arc::new(AtomicU64::new(0)),
                content_write_lock: Arc::new(Mutex::new(())),
                detection_content_seq: Arc::new(AtomicU64::new(0)),
                full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
                detect_reset_notify: Arc::new(Notify::new()),
                pending_release: Arc::new(Mutex::new(None)),
                preserve_processes_on_drop: true,
                detect_handle: Some(tokio::spawn(async {}).abort_handle()),
            },
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[tokio::test]
    async fn clear_pane_preserves_wrapped_input_and_unfinished_vt_sequence() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel_and_scrollback_bytes(
            10,
            5,
            100_000,
            b"old\r\nold\r\nold\r\nold\r\nold\r\n\x1b[32m$ abcdefghijklmnop\x1b[1A\x1b[4G\x1b[",
            4,
        );
        let before = runtime.content_seq();
        runtime.scroll_up(1);
        runtime.clear_screen().expect("test precondition");
        let snapshot = runtime
            .collect_dirty_patch_snapshot(10, 5)
            .expect("test precondition");
        assert!(snapshot.content_revision > before);
        assert!(!matches!(snapshot.patch, TerminalDirtyPatchOutcome::Clean));
        let metrics = runtime.scroll_metrics().expect("test precondition");
        assert_eq!(metrics.max_offset_from_bottom, 0);
        assert_eq!(metrics.offset_from_bottom, 0);
        let text = runtime.recent_unwrapped_text_snapshot(100).text;
        assert!(text.contains("$ abcdefghijklmnop"), "{text:?}");
        assert!(!text.contains("old"), "{text:?}");
        runtime.test_process_pty_bytes(b"5 q");
        assert!(!runtime.visible_text().contains("5 q"));
        assert!(rx.try_recv().is_err(), "clear must not send child input");
    }

    #[tokio::test]
    async fn clear_pane_preserves_alternate_screen_and_primary_history() {
        let runtime = PaneRuntime::test_with_scrollback_bytes(
            20,
            4,
            100_000,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\x1b[?1049halt app",
        );
        let before = runtime.visible_text();
        runtime.clear_screen().expect("test precondition");
        assert_eq!(runtime.visible_text(), before);
        runtime.test_process_pty_bytes(b"\x1b[?1049l");
        assert!(
            runtime
                .recent_unwrapped_text_snapshot(100)
                .text
                .contains("one")
        );
        runtime.clear_screen().expect("test precondition");
        assert!(
            !runtime
                .recent_unwrapped_text_snapshot(100)
                .text
                .contains("one")
        );
        assert!(runtime.visible_text().contains("five"));
    }

    #[tokio::test]
    async fn dirty_patch_snapshot_keeps_clean_metadata_and_terminal_fallback() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(20, 4);
        runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("initial snapshot");
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1016h");
        let snapshot = runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("mode snapshot");
        assert!(matches!(snapshot.patch, TerminalDirtyPatchOutcome::Clean));
        assert_eq!(snapshot.content_revision, runtime.content_seq());
        assert!(snapshot.content_revision.is_multiple_of(2));
        assert!(snapshot.mouse_reporting);
        assert!(snapshot.sgr_pixel_mouse);
        assert!(!snapshot.alternate_screen_active);

        runtime.test_process_pty_bytes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        assert!(runtime.collect_dirty_patch_snapshot(20, 4).is_none());
        assert!(runtime.content_write_lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn dirty_patch_snapshot_tracks_serialized_scroll_and_resize() {
        let runtime = PaneRuntime::test_with_scrollback_bytes(
            20,
            4,
            100_000,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
        );
        runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("live snapshot");
        runtime.scroll_up(1);
        let scrolled = runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("scrolled snapshot");
        assert_eq!(
            scrolled.scroll_metrics.expect("metrics").offset_from_bottom,
            1
        );
        runtime.scroll_reset();
        runtime.resize(5, 24, 0, 0);
        let resized = runtime
            .collect_dirty_patch_snapshot(24, 5)
            .expect("resized snapshot");
        let metrics = resized.scroll_metrics.expect("resized metrics");
        assert_eq!(metrics.offset_from_bottom, 0);
        assert_eq!(metrics.viewport_rows, 5);
        assert!(resized.content_revision.is_multiple_of(2));
        let TerminalDirtyPatchOutcome::Patch(patch) = resized.patch else {
            panic!("resize must dirty the viewport");
        };
        assert_eq!(patch.rows.len(), 5);
        assert!(patch.rows.iter().all(|(_, cells)| cells.len() == 24));
    }

    #[test]
    fn pane_launch_env_removes_outer_agent_identity() {
        let keys = [
            "SHEPR_AGENT",
            "CODEX_THREAD_ID",
            "OMPCODE",
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_TOKEN",
        ];
        let mut cmd = PtyCommand::new("shell");
        for key in keys {
            cmd.env(key, "outer-session");
        }
        cmd.env("ANTHROPIC_API_KEY", "fake-api-key");
        cmd.env("DISPLAY", ":42");

        apply_pane_launch_env(&mut cmd, &PaneLaunchEnv::default());

        for key in keys {
            assert!(cmd.get_env(key).is_none(), "{key} must not leak into panes");
        }
        assert_eq!(
            cmd.get_env("ANTHROPIC_API_KEY"),
            Some(OsStr::new("fake-api-key"))
        );
        assert_eq!(cmd.get_env("DISPLAY"), Some(OsStr::new(":42")));
    }

    #[test]
    fn pane_terminal_identity_removes_outer_terminal_identity() {
        let keys = [
            "ITERM_SESSION_ID",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "WEZTERM_PANE",
            "KITTY_WINDOW_ID",
            "WT_SESSION",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "ZELLIJ_PANE_ID",
        ];
        let mut cmd = PtyCommand::new("shell");
        for key in keys {
            cmd.env(key, "outer-session");
        }
        cmd.env("TERM_PROGRAM", "iTerm.app");
        cmd.env("TERM_PROGRAM_VERSION", "outer-version");

        apply_pane_terminal_env(&mut cmd);

        for key in keys {
            assert!(cmd.get_env(key).is_none(), "{key} must not leak into panes");
        }
        assert_eq!(cmd.get_env("TERM_PROGRAM"), Some(OsStr::new("shepr")));
        assert_eq!(
            cmd.get_env("TERM_PROGRAM_VERSION"),
            Some(OsStr::new(&crate::build_info::version()))
        );
    }

    #[test]
    fn pane_launch_env_allows_explicit_session_identity() {
        let extra = vec![
            ("CLAUDE_CODE_CHILD_SESSION".into(), "1".into()),
            ("CLAUDE_CODE_SESSION_ID".into(), "intentional-child".into()),
            ("CLAUDE_CODE_MESSAGING_TOKEN".into(), "fake-token".into()),
            ("ITERM_SESSION_ID".into(), "intentional-host".into()),
        ];
        let mut cmd = PtyCommand::new("shell");
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, &PaneLaunchEnv::from_extra(extra.clone()));

        for (key, value) in extra {
            assert_eq!(cmd.get_env(key), Some(OsStr::new(&value)));
        }
    }

    #[tokio::test]
    async fn cwd_returns_accepted_report_without_rechecking_filesystem() {
        let cwd = crate::test_support::ScratchDir::new("reported-cwd").keep_until_exit();

        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let (events, _event_rx) = mpsc::channel(1);
        publish_reported_cwd(runtime.pane_id, cwd.clone(), &runtime.reported_cwd, &events);
        assert_eq!(
            runtime
                .reported_cwd
                .lock()
                .expect("test precondition")
                .as_ref(),
            Some(&cwd),
            "test setup must pass cache admission"
        );

        std::fs::remove_dir(&cwd).expect("remove reported cwd after admission");

        assert_eq!(runtime.cwd(), Some(cwd));
    }

    #[tokio::test]
    async fn dropped_cwd_report_is_resent_on_the_next_identical_report() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let (events, mut event_rx) = mpsc::channel(1);
        let scratch = crate::test_support::ScratchDir::new("cwd-report");
        let cwd = scratch.to_path_buf();
        let other = std::path::PathBuf::from("/");
        // Fill the channel so the first cwd report cannot be queued.
        events
            .try_send(AppEvent::TerminalCwdReported {
                pane_id: runtime.pane_id,
                cwd: other,
            })
            .expect("test precondition");

        publish_reported_cwd(runtime.pane_id, cwd.clone(), &runtime.reported_cwd, &events);
        assert!(
            runtime
                .reported_cwd
                .lock()
                .expect("test precondition")
                .is_none(),
            "an unsent report must not occupy the dedupe slot"
        );

        let _ = event_rx.recv().await.expect("drain filler event");
        publish_reported_cwd(runtime.pane_id, cwd.clone(), &runtime.reported_cwd, &events);
        let Ok(AppEvent::TerminalCwdReported { cwd: sent, .. }) = event_rx.try_recv() else {
            panic!("expected the retried cwd report");
        };
        assert_eq!(sent, cwd);
        assert_eq!(
            runtime
                .reported_cwd
                .lock()
                .expect("test precondition")
                .as_ref(),
            Some(&cwd)
        );
    }

    #[test]
    fn process_cwd_does_not_require_traversing_the_directory_path() {
        use std::os::unix::fs::PermissionsExt;

        let base = crate::test_support::ScratchDir::new("process-cwd").keep_until_exit();
        let private = base.join("private");
        let cwd = private.join("cwd");
        std::fs::create_dir_all(&cwd).expect("create process cwd");

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .current_dir(&cwd)
            .spawn()
            .expect("spawn process in cwd");
        let expected_cwd = crate::platform::process_cwd(child.id())
            .expect("resolve process cwd before restricting traversal");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000))
            .expect("make cwd path untraversable");

        let path_is_traversable = cwd.is_dir();
        let observed = (!path_is_traversable)
            .then(|| absolute_process_cwd(child.id()))
            .flatten();

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755))
            .expect("restore cwd path permissions");
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&base).expect("remove process cwd");

        if path_is_traversable {
            eprintln!("skipping untraversable cwd assertion for privileged test process");
            return;
        }
        assert_eq!(observed, Some(expected_cwd));
    }

    #[tokio::test]
    async fn follow_cwd_falls_back_to_reported_pane_cwd_without_foreground_group() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("follow-cwd");
        let cwd = scratch.to_path_buf();
        *runtime.reported_cwd.lock().expect("test precondition") = Some(cwd.clone());

        assert_eq!(runtime.follow_cwd(), Some(cwd));
    }

    #[tokio::test]
    async fn bracketed_paste_neutralizes_embedded_markers() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        let payload = runtime.paste_payload("before\x1b[201~middle\x1b[200~after".into());
        assert_eq!(payload.as_ref(), b"\x1b[200~beforemiddleafter\x1b[201~");
    }

    #[tokio::test]
    async fn alternate_screen_does_not_replace_primary_saved_history() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"primary history");
        assert!(
            runtime
                .snapshot_history()
                .is_some_and(|text| text.contains("primary history"))
        );
        runtime.test_process_pty_bytes(b"\x1b[?1049halt frame");
        assert_eq!(runtime.snapshot_history(), None);
    }

    #[test]
    fn pane_size_clamp_never_yields_an_empty_pty() {
        assert_eq!(clamp_pane_size(0, 0), (MIN_PANE_ROWS, MIN_PANE_COLS));
        assert_eq!(clamp_pane_size(1, 80), (MIN_PANE_ROWS, 80));
        assert_eq!(clamp_pane_size(24, 3), (24, MIN_PANE_COLS));
        assert_eq!(clamp_pane_size(24, 80), (24, 80));
    }

    #[test]
    fn pane_teardown_reaches_background_jobs_after_the_leader_is_reaped() {
        // The common close path: the pane's child has exited and been reaped,
        // but it left a job behind in its session that ignores SIGHUP and
        // SIGTERM, as a daemonised dev server might.
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "trap '' HUP TERM; sleep 30 & exit 0"]);
        let mut spawned = crate::pty::backend::spawn_pty(24, 80, &cmd).expect("spawn session");
        let leader_pid = spawned.child.id();
        let leader = crate::platform::ProcessHandle::open(leader_pid).expect("leader pidfd");
        let child_liveness = Arc::new(ChildLiveness::new(leader_pid, Some(leader)));
        spawned.child.wait().expect("reap the leader");
        assert!(child_liveness.has_exited());
        assert!(child_liveness.is_reaped());
        child_liveness.mark_wait_completed();

        let members = crate::platform::session_member_handles(leader_pid, || true);
        assert_eq!(members.len(), 1, "the background job survives its leader");

        let started = std::time::Instant::now();
        shutdown_pane_processes(PaneId::from_raw(0), child_liveness);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "teardown must not block its caller through the grace periods"
        );

        assert!(wait_for_pane_session_teardowns(
            std::time::Duration::from_secs(10)
        ));
        let handles: Vec<&crate::platform::ProcessHandle> = members.iter().collect();
        assert!(
            crate::platform::wait_for_process_exits(&handles, std::time::Duration::from_secs(1)),
            "the background job is killed once SIGHUP and SIGTERM are ignored"
        );
    }

    #[test]
    fn sync_timeout_render_arms_one_task_for_many_reads() {
        let timer = SyncTimeoutRender::default();
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(150);

        assert_eq!(
            timer.arm(deadline),
            Some(deadline),
            "first read arms a task"
        );
        for _ in 0..100 {
            assert_eq!(timer.arm(deadline), None, "later reads reuse the task");
        }
        // A later update's deadline extends the armed task instead.
        let later = deadline + std::time::Duration::from_millis(150);
        assert_eq!(timer.arm(later), None);
        assert_eq!(timer.next_wake(deadline), Some(later));
        assert_eq!(timer.next_wake(later), None, "the task disarms and renders");
        assert_eq!(timer.arm(later), Some(later), "a disarmed timer re-arms");
    }

    #[test]
    fn pane_teardown_without_a_session_does_nothing() {
        shutdown_pane_processes(PaneId::from_raw(0), Arc::new(ChildLiveness::new(0, None)));
    }

    fn capture_shell_output(command: &str, extra_env: &[(&str, &str)]) -> String {
        let scratch = crate::test_support::ScratchDir::new("pane-term");
        let output_path = scratch.join("output.txt");
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(format!("{command} > '{}'", output_path.display()));
        cmd.cwd(std::env::current_dir().expect("test precondition"));
        cmd.env("TERM", "xterm-ghostty");
        cmd.env("COLORTERM", "falsecolor");
        apply_pane_terminal_env(&mut cmd);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let mut spawned = crate::pty::backend::spawn_pty(24, 80, &cmd).expect("spawn in pty");
        let status = spawned.child.wait().expect("wait for shell");
        assert!(status.success(), "shell command failed: {status:?}");

        let output = std::fs::read_to_string(&output_path).expect("test precondition");
        let _ = std::fs::remove_file(output_path);
        output
    }

    #[test]
    fn login_shell_builder_uses_one_resolved_path_for_exec_and_shell_env() {
        let cmd = pane_shell_command_builder(PaneShellConfig::new("/bin/sh", true));
        assert!(cmd.is_login_shell());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(std_cmd.get_args().count(), 0);
        assert_eq!(
            std_cmd
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("SHELL"))
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("/bin/sh"))
        );
    }

    #[test]
    fn non_login_shell_builder_execs_configured_shell_without_login_argv0() {
        let cmd = pane_shell_command_builder(PaneShellConfig::new("/bin/sh", false));
        assert!(!cmd.is_login_shell());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(std_cmd.get_args().count(), 0);
    }

    #[test]
    fn pane_shell_spawn_rejects_a_missing_configured_shell() {
        let cmd =
            pane_shell_command_builder(PaneShellConfig::new("/__shepr_missing_shell__", true));
        let err = cmd.to_std_command().expect_err("test precondition");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn pane_shell_spawn_resolves_a_bare_name_on_the_child_path() {
        let bin = crate::test_support::ScratchDir::new("bin");
        let shell = bin.join("fake-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 0\n").expect("test precondition");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))
                .expect("test precondition");
        }

        let mut cmd = pane_shell_command_builder(PaneShellConfig::new("fake-shell", false));
        cmd.env("PATH", bin.as_os_str());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), shell.as_os_str());
        assert_eq!(
            std_cmd
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("SHELL"))
                .and_then(|(_, value)| value),
            Some(shell.as_os_str())
        );
    }

    #[test]
    fn pane_terminal_identity_overrides_outer_terminal_env() {
        let output = capture_shell_output("printf '%s\\n%s\\n' \"$TERM\" \"$COLORTERM\"", &[]);
        assert_eq!(output, "xterm-256color\ntruecolor\n");
    }

    #[test]
    fn pane_terminal_identity_allows_explicit_override() {
        let output = capture_shell_output(
            "printf '%s\\n%s\\n' \"$TERM\" \"$COLORTERM\"",
            &[("TERM", "vt100"), ("COLORTERM", "24bit")],
        );
        assert_eq!(output, "vt100\n24bit\n");
    }

    #[tokio::test]
    async fn exited_shell_keeps_persistence_cwd_when_pid_is_reused() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("exited-cwd");
        let saved = scratch.join("saved");
        *runtime.persistence_cwd.lock().expect("test precondition") = Some(saved.clone());
        // A different live process now owns the exited shell's numeric PID.
        runtime
            .child_liveness
            .pid
            .store(std::process::id(), Ordering::Release);
        runtime.child_liveness.mark_wait_completed();
        assert_eq!(runtime.cwd_for_persistence(), Some(saved));
        *runtime.persistence_cwd.lock().expect("test precondition") = None;
        assert_eq!(runtime.cwd_for_persistence(), None);
    }

    #[tokio::test]
    async fn scrollback_survives_shrink_and_grow_resize() {
        let suffix = "x".repeat(66);
        let history = (1..=2_000)
            .map(|line| format!("{line:05} {suffix}\r\n"))
            .collect::<String>();
        let runtime =
            PaneRuntime::test_with_scrollback_bytes(80, 45, 20_000_000, history.as_bytes());

        runtime.resize(21, 80, 0, 0);
        let snapshot = runtime.recent_unwrapped_text_snapshot(usize::MAX);
        assert!(snapshot.text.contains("00001 "));
        assert!(snapshot.text.contains("02000 "));

        runtime.resize(45, 80, 0, 0);

        assert_eq!(runtime.current_size(), (45, 80));
        assert_eq!(runtime.terminal_dimensions(), Some((80, 45)));
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("test precondition")
                .viewport_rows,
            45
        );
        let snapshot = runtime.recent_unwrapped_text_snapshot(usize::MAX);
        assert!(snapshot.text.contains("00001 "));
        assert!(snapshot.text.contains("02000 "));
    }

    #[tokio::test]
    async fn focus_events_are_forwarded_when_enabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let (resize_tx, _resize_rx) = watch::channel((80, 24, 0, 0));
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal
            .mode_set(crate::ghostty::MODE_FOCUS_EVENT, true)
            .expect("test precondition");
        let pane_id = PaneId::from_raw(0);
        let terminal = Arc::new(PaneTerminal::new(GhosttyPaneTerminal::new(terminal)));
        let runtime = PaneRuntime {
            persistence_cwd: Mutex::new(None),
            pane_id,
            terminal,
            io: PaneRuntimeIo::TestChannel {
                sender: tx,
                resize_tx,
            },
            current_size: Cell::new((80, 24, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            reported_cwd: Arc::new(Mutex::new(None)),
            content_seq: Arc::new(AtomicU64::new(0)),
            content_write_lock: Arc::new(Mutex::new(())),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            pending_release: Arc::new(Mutex::new(None)),
            preserve_processes_on_drop: true,
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        assert!(runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
        assert_eq!(
            rx.recv().await.expect("test precondition"),
            Bytes::from_static(b"\x1b[I")
        );
    }

    #[tokio::test]
    async fn focus_events_are_suppressed_when_disabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let (resize_tx, _resize_rx) = watch::channel((80, 24, 0, 0));
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane_id = PaneId::from_raw(0);
        let terminal = Arc::new(PaneTerminal::new(GhosttyPaneTerminal::new(terminal)));
        let runtime = PaneRuntime {
            persistence_cwd: Mutex::new(None),
            pane_id,
            terminal,
            io: PaneRuntimeIo::TestChannel {
                sender: tx,
                resize_tx,
            },
            current_size: Cell::new((80, 24, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            reported_cwd: Arc::new(Mutex::new(None)),
            content_seq: Arc::new(AtomicU64::new(0)),
            content_write_lock: Arc::new(Mutex::new(())),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            pending_release: Arc::new(Mutex::new(None)),
            preserve_processes_on_drop: true,
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        assert!(!runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn subscribed_idle_child_receives_color_scheme_transition() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.apply_host_terminal_appearance(Some(crate::terminal_theme::HostAppearance::Dark));
        runtime.test_process_pty_bytes(b"\x1b[?2031h");

        runtime.apply_host_terminal_appearance(Some(crate::terminal_theme::HostAppearance::Light));

        assert_eq!(rx.recv().await, Some(Bytes::from_static(b"\x1b[?997;2n")));
    }

    #[test]
    fn foreground_shell_reports_process_exit_before_clearing_agent() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Codex), None, true, false),
            ForegroundShellAgentAction::ReportProcessExit
        );
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Codex), None, true, true),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn same_agent_after_reported_exit_is_a_replacement_process() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Pi), Some(Agent::Pi), false, true),
            ForegroundShellAgentAction::ReportReplacementProcess
        );
    }

    #[test]
    fn unknown_non_shell_foreground_job_is_not_immediate_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), None, false, false),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    #[tokio::test]
    async fn first_agent_acquisition_keeps_osc_evidence_replacement_clears_it() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]2;startup title\x1b\\\x1b]9;4;1;\x1b\\");

        clear_osc_evidence_for_agent_transition(&runtime.terminal, None);
        assert_eq!(runtime.agent_osc_title(), "startup title");
        assert_eq!(runtime.agent_osc_progress(), "4;1;");

        clear_osc_evidence_for_agent_transition(&runtime.terminal, Some(Agent::Claude));
        assert_eq!(runtime.agent_osc_title(), "");
        assert_eq!(runtime.agent_osc_progress(), "");
    }

    #[test]
    fn reported_process_exit_clears_before_unknown_foreground_probe() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), None, false, true),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn foreground_agent_job_is_not_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), Some(Agent::OpenCode), true, false,),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    fn foreground_process(pid: u32, name: &str) -> crate::platform::ForegroundProcess {
        crate::platform::ForegroundProcess {
            pid,
            name: name.to_string(),
            argv0: None,
            argv: None,
            cmdline: None,
        }
    }

    #[test]
    fn foreground_agent_hint_accepts_pane_shell_environment() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 42,
            processes: vec![foreground_process(42, "bash")],
        };

        assert_eq!(
            agent_hint_for_foreground_job_members(&job, |pid| {
                (pid == 42).then_some(Agent::Claude)
            }),
            Some(Agent::Claude)
        );
    }

    #[test]
    fn foreground_agent_hint_accepts_non_leader_foreground_process_environment() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "fence"),
                foreground_process(100, "pi"),
            ],
        };

        assert_eq!(
            agent_hint_for_foreground_job_members(&job, |pid| {
                (pid == 100).then_some(Agent::Codex)
            }),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn foreground_agent_hint_wins_over_process_name_detection() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![foreground_process(99, "codex")],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            Some(&job),
            || None,
            |pid| (pid == 99).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    #[test]
    fn foreground_agent_hint_on_inherited_child_environment_is_authoritative() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![foreground_process(99, "vim")],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 99).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    #[test]
    fn non_leader_agent_hint_does_not_override_identifiable_leader() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "codex"),
                foreground_process(100, "vim"),
            ],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 100).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Codex));
        assert_eq!(result.process_name.as_deref(), Some("codex"));
    }

    #[test]
    fn non_leader_agent_hint_wins_when_leader_is_unidentified() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "some_vm"),
                foreground_process(100, "vim"),
            ],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 100).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    fn process_probe_input() -> ProcessProbeInput {
        ProcessProbeInput {
            current_agent: None,
            suppressed_agent: None,
            foreground_pgid: Some(42),
            last_foreground_pgid: Some(42),
            has_process_probe: true,
            acquisition_age: None,
            pending_foreground_shell_clear: false,
            elapsed_since_process_check: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn unchanged_unidentified_foreground_group_skips_full_process_probe() {
        assert!(!should_probe_foreground_job(process_probe_input()));
    }

    #[test]
    fn unidentified_foreground_group_change_runs_full_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: Some(43),
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_gets_initial_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn stable_unidentified_foreground_group_has_no_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            elapsed_since_process_check: PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_without_foreground_group_uses_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: None,
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_probes_when_foreground_group_disappears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: Some(42),
            ..process_probe_input()
        }));
    }

    #[test]
    fn inferred_group_does_not_trigger_a_probe_on_every_tick() {
        let tracked = process_group_for_change_tracking(None, Some(300));
        assert_eq!(tracked, None);
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Claude),
            foreground_pgid: None,
            last_foreground_pgid: tracked,
            elapsed_since_process_check: std::time::Duration::from_millis(300),
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_shell_clear_forces_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            pending_foreground_shell_clear: true,
            ..process_probe_input()
        }));
    }

    #[test]
    fn lifecycle_authority_skips_stable_routine_process_probe() {
        assert!(should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            false,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn lifecycle_authority_keeps_periodic_probes_without_an_observed_group() {
        let input = ProcessProbeInput {
            current_agent: Some(Agent::Pi),
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
            ..process_probe_input()
        };
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true, input
        ));
        assert!(should_probe_foreground_job(input));
    }

    #[test]
    fn lifecycle_authority_preserves_process_exit_and_release_probes() {
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                pending_foreground_shell_clear: true,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                suppressed_agent: Some(Agent::Pi),
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn lifecycle_authority_preserves_initial_and_foreground_group_change_probes() {
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: None,
                has_process_probe: false,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                foreground_pgid: Some(43),
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn pending_release_forces_initial_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            suppressed_agent: Some(Agent::Codex),
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_forces_process_probe_after_runtime_identity_clears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_skips_repeated_probe_when_foreground_group_is_stable() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_probes_when_foreground_group_changes() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            foreground_pgid: Some(43),
            ..process_probe_input()
        }));
    }

    #[test]
    fn acquisition_window_catches_delayed_same_group_wrapper_startup() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_millis(1250)),
            elapsed_since_process_check: PROCESS_ACQUISITION_FAST_RECHECK
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_millis(1250)),
            elapsed_since_process_check: PROCESS_ACQUISITION_FAST_RECHECK,
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_secs(5)),
            elapsed_since_process_check: PROCESS_ACQUISITION_SLOW_RECHECK,
            ..process_probe_input()
        }));
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(PROCESS_ACQUISITION_WINDOW + std::time::Duration::from_millis(1),),
            elapsed_since_process_check: PROCESS_ACQUISITION_SLOW_RECHECK,
            ..process_probe_input()
        }));
    }

    #[test]
    fn content_change_starts_bounded_unidentified_acquisition_window() {
        let now = std::time::Instant::now();
        let mut acquisition_started_at = None;
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, Some(now));
        assert_eq!(last_content_change_at, Some(now));

        let later = now + std::time::Duration::from_secs(1);
        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            later,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(
            acquisition_started_at,
            Some(now),
            "changed frames should not refresh the acquisition window"
        );
        assert_eq!(last_content_change_at, Some(later));

        let quiet_after_window =
            later + PROCESS_ACQUISITION_WINDOW + PROCESS_ACQUISITION_IDLE_RESET;
        sync_content_change_acquisition(
            None,
            None,
            false,
            false,
            quiet_after_window,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        let next_burst = quiet_after_window + std::time::Duration::from_secs(1);
        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            next_burst,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, Some(next_burst));
        assert_eq!(last_content_change_at, Some(next_burst));
    }

    #[test]
    fn content_change_does_not_start_acquisition_when_process_probe_has_other_signal() {
        let now = std::time::Instant::now();
        let mut acquisition_started_at = None;
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            Some(Agent::Codex),
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        sync_content_change_acquisition(
            None,
            Some(Agent::Codex),
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        sync_content_change_acquisition(
            None,
            None,
            true,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);
    }

    #[test]
    fn content_change_restarts_stale_process_group_acquisition_window() {
        let now = std::time::Instant::now();
        let stale_start = now - PROCESS_ACQUISITION_WINDOW - std::time::Duration::from_millis(1);
        let mut acquisition_started_at = Some(stale_start);
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );

        assert_eq!(acquisition_started_at, Some(now));
        assert_eq!(last_content_change_at, Some(now));
    }

    #[test]
    fn release_expiry_can_force_reacquire_probe_by_resetting_probe_state() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn identified_agent_uses_shorter_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
            ..process_probe_input()
        }));
    }

    #[test]
    fn identified_agent_probes_when_foreground_group_disappears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: Some(42),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
    }

    #[test]
    fn stable_missing_foreground_group_uses_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
            ..process_probe_input()
        }));
    }

    #[test]
    fn transient_process_miss_keeps_current_agent_detected() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        let changed = presence.observe_process_probe(None);

        assert!(!changed, "one miss should not clear the detected agent");
        assert_eq!(presence.current_agent(), Some(Agent::Pi));
    }

    #[test]
    fn agent_only_clears_after_confirmation_misses() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        for attempt in 1..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let changed = presence.observe_process_probe(None);
            assert!(
                !changed,
                "miss {attempt} should stay in the confirmation window"
            );
            assert_eq!(presence.current_agent(), Some(Agent::Pi));
        }

        let changed = presence.observe_process_probe(None);
        assert!(changed, "last confirmation miss should clear the agent");
        assert_eq!(presence.current_agent(), None);
    }

    #[tokio::test]
    async fn set_full_lifecycle_authority_active_notifies_only_on_activation_transitions() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        let reset_notify = runtime.agent_detection_reset_notify_for_test();

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("false-to-true transition should notify detection reset");

        runtime.set_full_lifecycle_authority_active(true);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "repeated true-to-true sync should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(false);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "true-to-false transition should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("re-entering active authority should notify detection reset");
    }

    #[tokio::test]
    async fn state_changed_event_waits_for_queue_space_instead_of_dropping() {
        let (tx, mut rx) = mpsc::channel(1);
        let pane_id = PaneId::from_raw(42);

        tx.try_send(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        })
        .expect("test precondition");

        let publish = publish_state_changed_event(
            tx.clone(),
            pane_id,
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            false,
            std::time::Instant::now(),
        );
        tokio::pin!(publish);

        let blocked = tokio::time::timeout(std::time::Duration::from_millis(20), async {
            (&mut publish).await;
        })
        .await;
        assert!(
            blocked.is_err(),
            "publisher should wait for queue space instead of dropping StateChanged"
        );

        let first = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield first event")
            .expect("sender still alive");
        assert!(matches!(first, AppEvent::GitStatusRefreshed { .. }));

        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            (&mut publish).await;
        })
        .await
        .expect("publisher should complete once queue space is available");

        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield second event")
            .expect("sender still alive");
        assert!(matches!(
            second,
            AppEvent::StateChanged {
                pane_id: delivered_pane,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited: false,
                observed_at: _,
            } if delivered_pane == pane_id
        ));
    }

    #[tokio::test]
    async fn codex_prompt_observation_revokes_on_working_or_skipped_screen() {
        let (tx, mut rx) = mpsc::channel(4);
        let pane_id = PaneId::from_raw(42);
        let detection = crate::detect::AgentDetection {
            state: AgentState::Unknown,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        };
        let mut last_ready = false;
        let prompt = "› Ask Codex to do anything";
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            Some(&detection),
            false,
            &mut last_ready,
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::CodexPromptObserved { ready: true, .. })
        ));
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            Some(&detection),
            false,
            &mut last_ready,
        )
        .await;
        assert!(rx.try_recv().is_err());
        let working = crate::detect::AgentDetection {
            state: AgentState::Working,
            ..detection
        };
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            Some(&working),
            false,
            &mut last_ready,
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::CodexPromptObserved { ready: false, .. })
        ));
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            Some(&detection),
            false,
            &mut last_ready,
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::CodexPromptObserved { ready: true, .. })
        ));
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            None,
            false,
            &mut last_ready,
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::CodexPromptObserved { ready: false, .. })
        ));
        publish_codex_prompt_observation(
            &tx,
            pane_id,
            Some(Agent::Codex),
            prompt,
            Some(&detection),
            false,
            &mut last_ready,
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::CodexPromptObserved { ready: true, .. })
        ));
    }
}
