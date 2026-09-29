use std::{
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use shepr_mux::events::{TabBarCommandError, TabBarCommandFailure};
use tokio::io::AsyncReadExt;

use super::{App, state::TabBarStatusSegment};
#[cfg(test)]
use shepr_config::TabBarRightEntryConfig;
use shepr_config::ValidatedTabBarRightEntry;

impl App {
    /// Environment and working directory for tab bar status commands.
    ///
    /// The working directory is the active pane's, the same one exported as
    /// `SHEPR_ACTIVE_PANE_CWD`: a status segment describes what the user is
    /// looking at, so `git branch --show-current` or `ls` must see the focused
    /// pane's directory. With no focused pane, or when its directory is gone,
    /// the command runs in the home directory, and in `/` when that is unknown
    /// or missing too. It never inherits the server's own working directory, which is
    /// wherever the daemon happened to be started.
    fn status_command_env(&self) -> (Vec<(String, String)>, std::path::PathBuf) {
        use shepr_core::env::{ChildEnv, EnvVar};

        let mut env = vec![(
            EnvVar::SheprSocketPath.name().to_string(),
            shepr_api::socket_path(&self.paths).display().to_string(),
        )];
        // Not raw `current_exe()`: after an install replaces the binary, Linux
        // reports the running one as "/…/shepr (deleted)", which a status
        // command cannot run.
        if let Ok(current_exe) = shepr_platform::launch_executable() {
            env.push((
                ChildEnv::SheprBinPath.name().to_string(),
                current_exe.display().to_string(),
            ));
        }

        let mut cwd = None;
        if let Some(ws_idx) = self.state.active_index() {
            if let Some(workspace_id) = self.public_workspace_id(ws_idx) {
                env.push((
                    ChildEnv::SheprActiveWorkspaceId.name().to_string(),
                    workspace_id,
                ));
            }
            if let Some(workspace) = self.state.workspaces.get(ws_idx) {
                let tab_idx = workspace.active_tab_index();
                if let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) {
                    env.push((ChildEnv::SheprActiveTabId.name().to_string(), tab_id));
                }
                let pane_id = workspace.focused_pane_id();
                if let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) {
                    env.push((
                        ChildEnv::SheprActivePaneId.name().to_string(),
                        public_pane_id,
                    ));
                }
                if let Some(pane_cwd) = workspace.active_tab().cwd_for_pane(
                    pane_id,
                    &self.state.terminals,
                    &self.terminal_runtimes,
                ) {
                    env.push((
                        ChildEnv::SheprActivePaneCwd.name().to_string(),
                        pane_cwd.display().to_string(),
                    ));
                    if is_directory(&pane_cwd) {
                        cwd = Some(pane_cwd);
                    }
                }
            }
        }
        let cwd = cwd.unwrap_or_else(|| {
            self.paths
                .home_dir()
                .filter(|home| is_directory(home))
                .map_or_else(|| std::path::PathBuf::from("/"), Path::to_path_buf)
        });
        (env, cwd)
    }
}

/// Whether `path` names a directory a status command can start in. A stat
/// failure other than absence (a permission problem, a dead mount) is logged,
/// and the caller falls back as it would for a missing directory: a status
/// command has to run somewhere.
fn is_directory(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::debug!(
                path = %path.display(),
                %error,
                "status command cannot start in this directory"
            );
            false
        }
    }
}

use crate::limits::{
    DATETIME_REFRESH_INTERVAL, MAX_COMMAND_LINE_BYTES, MAX_TAB_BAR_TEXT_CHARS,
    TAB_BAR_COMMAND_SHELL, TAB_BAR_COMMAND_SHELL_ARGS, TAB_BAR_STATUS_READ_BUFFER_BYTES,
};

#[derive(Default)]
pub(super) struct TabBarStatus {
    datetimes: Vec<TabBarDatetimeRuntime>,
    commands: Vec<TabBarCommandRuntime>,
    next_datetime_refresh: Option<std::time::Instant>,
}

impl TabBarStatus {
    fn deadline(&self) -> Option<std::time::Instant> {
        self.commands
            .iter()
            .filter(|runtime| runtime.task.is_none())
            .map(|runtime| runtime.next_run_at)
            .chain(self.next_datetime_refresh)
            .min()
    }
}

pub(super) struct TabBarDatetimeRuntime {
    segment_index: usize,
    format: time::format_description::OwnedFormatItem,
}

pub(super) struct TabBarCommandRuntime {
    segment_index: usize,
    command: String,
    interval: Duration,
    timeout: Duration,
    next_run_at: std::time::Instant,
    task: Option<StatusCommandTask>,
    failure_logged: bool,
}

impl Drop for TabBarCommandRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort_handle.abort();
            // Kill the whole process group now, when the app is torn down,
            // instead of waiting for Tokio to cancel the task: aborting it only
            // drops the direct `sh` child, and its descendants would outlive
            // the server.
            task.control.terminate();
        }
    }
}

impl App {
    /// Sets up the tab bar's right-hand segments. Config is read once at
    /// launch and never reloaded, so this runs once from `App::new`; the
    /// segment indices it hands out therefore stay valid for every command
    /// result that comes back.
    pub(super) fn configure_tab_bar_status(
        &mut self,
        entries: &[ValidatedTabBarRightEntry],
        separator: &str,
    ) {
        self.tab_bar_status.datetimes.clear();
        self.tab_bar_status.commands.clear();
        self.state.tab_bar_right.clear();
        self.state.tab_bar_right_separator = TabBarText::new(separator).into_string();

        let now = self.clock.now;
        for entry in entries {
            match entry {
                ValidatedTabBarRightEntry::Zoom => {
                    self.state.tab_bar_right.push(TabBarStatusSegment::Zoom);
                }
                ValidatedTabBarRightEntry::Hostname => {
                    self.state.tab_bar_right.push(TabBarStatusSegment::Text(
                        TabBarText::trimmed(&self.hostname).into_option(),
                    ));
                }
                ValidatedTabBarRightEntry::Datetime { format } => {
                    let value = format_local_datetime(format);
                    let segment_index = self.state.tab_bar_right.len();
                    self.state
                        .tab_bar_right
                        .push(TabBarStatusSegment::Text(value));
                    self.tab_bar_status.datetimes.push(TabBarDatetimeRuntime {
                        segment_index,
                        format: format.clone(),
                    });
                }
                ValidatedTabBarRightEntry::Text { text } => {
                    self.state.tab_bar_right.push(TabBarStatusSegment::Text(
                        TabBarText::new(text).into_option(),
                    ));
                }
                ValidatedTabBarRightEntry::Command {
                    command,
                    interval_seconds,
                    timeout_seconds,
                } => {
                    let segment_index = self.state.tab_bar_right.len();
                    self.state
                        .tab_bar_right
                        .push(TabBarStatusSegment::Text(None));
                    self.tab_bar_status.commands.push(TabBarCommandRuntime {
                        segment_index,
                        command: command.clone(),
                        interval: Duration::from_secs(interval_seconds.get()),
                        timeout: Duration::from_secs(timeout_seconds.get()),
                        next_run_at: now,
                        task: None,
                        failure_logged: false,
                    });
                }
            }
        }

        self.tab_bar_status.next_datetime_refresh =
            (!self.tab_bar_status.datetimes.is_empty()).then_some(now + DATETIME_REFRESH_INTERVAL);
    }

    /// Test helper: parse raw entries like config validation does. Invalid
    /// entries are a broken test, so they panic instead of being skipped.
    #[cfg(test)]
    pub(super) fn configure_tab_bar_status_config(
        &mut self,
        entries: &[TabBarRightEntryConfig],
        separator: &str,
    ) {
        use crate::test_support::ValidatedConfigFixture as _;
        let mut config = shepr_config::Config::default();
        config.ui.tab_bar_right = entries.to_vec();
        let config = shepr_config::ValidatedConfig::test_from_config(config, None);
        self.configure_tab_bar_status(&config.ui().tab_bar_right, separator);
    }

    pub(crate) fn handle_tab_bar_status_tasks(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;

        if self
            .tab_bar_status
            .next_datetime_refresh
            .is_some_and(|deadline| now >= deadline)
        {
            for runtime in &self.tab_bar_status.datetimes {
                let value = format_local_datetime(&runtime.format);
                if let Some(TabBarStatusSegment::Text(current)) =
                    self.state.tab_bar_right.get_mut(runtime.segment_index)
                {
                    changed |= *current != value;
                    *current = value;
                }
            }
            self.tab_bar_status.next_datetime_refresh = Some(now + DATETIME_REFRESH_INTERVAL);
        }

        let command_due = self
            .tab_bar_status
            .commands
            .iter()
            .any(|runtime| runtime.task.is_none() && now >= runtime.next_run_at);
        if !command_due {
            return changed;
        }

        let (environment, cwd) = self.status_command_env();
        for runtime in &mut self.tab_bar_status.commands {
            if runtime.task.is_some() || now < runtime.next_run_at {
                continue;
            }
            runtime.next_run_at = now.checked_add(runtime.interval).unwrap_or(now);
            runtime.task = Some(spawn_status_command(
                self.event_tx.clone(),
                runtime.segment_index,
                runtime.command.clone(),
                runtime.timeout,
                tokio::time::Instant::from_std(now),
                environment.clone(),
                cwd.clone(),
            ));
        }

        changed
    }

    pub(crate) fn next_tab_bar_status_deadline(&self) -> Option<std::time::Instant> {
        self.tab_bar_status.deadline()
    }

    pub(super) fn handle_tab_bar_command_finished(
        &mut self,
        segment_index: usize,
        result: Result<Option<String>, TabBarCommandError>,
    ) -> bool {
        let Some(runtime) = self
            .tab_bar_status
            .commands
            .iter_mut()
            .find(|runtime| runtime.segment_index == segment_index)
        else {
            return false;
        };
        runtime.task = None;

        let output = match result {
            Ok(output) => {
                runtime.failure_logged = false;
                output
            }
            Err(error) => {
                if !runtime.failure_logged {
                    tracing::warn!(segment_index, command = %error.command,
                        cause = %tab_bar_command_failure_message(&error.cause),
                        "tab bar status command failed");
                    runtime.failure_logged = true;
                }
                None
            }
        };
        let Some(TabBarStatusSegment::Text(current)) =
            self.state.tab_bar_right.get_mut(segment_index)
        else {
            return false;
        };
        let changed = *current != output;
        *current = output;
        changed
    }
}

fn format_local_datetime(format: &time::format_description::OwnedFormatItem) -> Option<String> {
    let datetime = shepr_platform::local_datetime()?;
    datetime
        .format(format)
        .ok()
        .and_then(|value| TabBarText::trimmed(&value).into_option())
}

/// The only constructors for text that can reach the tab bar. All sources
/// share the same control filtering and length bound. Separators and literal
/// entries keep their spacing, which the user wrote on purpose; generated
/// values (command output, hostname, datetime) are trimmed, so padding or a
/// whitespace-only line does not render as a blank segment.
struct TabBarText(String);

impl TabBarText {
    fn new(value: &str) -> Self {
        Self(
            Self::printable(value)
                .take(MAX_TAB_BAR_TEXT_CHARS)
                .collect(),
        )
    }

    fn trimmed(value: &str) -> Self {
        let printable: String = Self::printable(value).collect();
        Self(
            printable
                .trim()
                .chars()
                .take(MAX_TAB_BAR_TEXT_CHARS)
                .collect(),
        )
    }

    fn printable(value: &str) -> impl Iterator<Item = char> + '_ {
        value
            .chars()
            .filter(|character| !character.is_control() && !is_unicode_format_control(*character))
    }

    fn into_string(self) -> String {
        self.0
    }

    fn into_option(self) -> Option<String> {
        (!self.0.is_empty()).then_some(self.0)
    }
}

fn tab_bar_command_failure_message(cause: &TabBarCommandFailure) -> String {
    match cause {
        TabBarCommandFailure::TimedOut(timeout) => format!("timed out after {timeout:?}"),
        TabBarCommandFailure::Cancelled => "status command was cancelled".into(),
        TabBarCommandFailure::Spawn(error)
        | TabBarCommandFailure::ProcessGroup(error)
        | TabBarCommandFailure::Wait(error)
        | TabBarCommandFailure::Output(error) => error.to_string(),
        TabBarCommandFailure::Exited(status) => format!("exited with {status}"),
    }
}

fn is_unicode_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

fn command_output_text(output: &[u8]) -> Option<String> {
    let output = String::from_utf8_lossy(output);
    let output = strip_terminal_control_sequences(output.as_bytes());
    let output = String::from_utf8_lossy(&output);
    output
        .lines()
        .next_back()
        .and_then(|line| TabBarText::trimmed(line).into_option())
}

#[derive(Clone, Copy)]
enum ControlSequenceState {
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    StString,
}

#[expect(
    clippy::match_same_arms,
    reason = "a state transition table reads by source state; merging rows by target state scatters each state's transitions"
)]
fn strip_terminal_control_sequences(value: &[u8]) -> Vec<u8> {
    use ControlSequenceState::*;

    let mut output = Vec::with_capacity(value.len());
    let mut state = Text;
    for &byte in value {
        state = match (state, byte) {
            (Text, b'\x1b') => Escape,
            (Text, _) => {
                output.push(byte);
                Text
            }
            (Escape, b'[') => Csi,
            (Escape, b']') => Osc,
            (Escape, b'P' | b'X' | b'^' | b'_') => StString,
            (Escape, 0x20..=0x2f) => EscapeIntermediate,
            (Escape, 0x30..=0x7e) => Text,
            (Escape, b'\x1b') => Escape,
            (Escape, b'\x18' | b'\x1a') => Text,
            (Escape, byte) if byte.is_ascii_control() => Escape,
            (Escape, _) => {
                output.push(byte);
                Text
            }
            (EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (EscapeIntermediate, 0x30..=0x7e) => Text,
            (EscapeIntermediate, b'\x1b') => Escape,
            (EscapeIntermediate, b'\x18' | b'\x1a') => Text,
            (EscapeIntermediate, byte) if byte.is_ascii_control() => EscapeIntermediate,
            (EscapeIntermediate, _) => {
                output.push(byte);
                Text
            }
            (Csi, 0x20..=0x3f) => Csi,
            (Csi, 0x40..=0x7e) => Text,
            (Csi, b'\x1b') => Escape,
            (Csi, b'\x18' | b'\x1a') => Text,
            (Csi, byte) if byte.is_ascii_control() => Csi,
            (Csi, _) => {
                output.push(byte);
                Text
            }
            (Osc, b'\x07') => Text,
            (Osc, b'\x1b') => Escape,
            (Osc, b'\x18' | b'\x1a') => Text,
            (Osc, _) => Osc,
            (StString, b'\x1b') => Escape,
            (StString, b'\x18' | b'\x1a') => Text,
            (StString, _) => StString,
        };
    }
    output
}

async fn read_last_output_line(
    mut stdout: tokio::process::ChildStdout,
) -> std::io::Result<Vec<u8>> {
    let mut current_line = Vec::new();
    let mut last_line = Vec::new();
    let mut ended_with_newline = false;
    let mut buffer = [0_u8; TAB_BAR_STATUS_READ_BUFFER_BYTES];

    loop {
        let count = stdout.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                last_line = std::mem::take(&mut current_line);
                ended_with_newline = true;
            } else {
                if current_line.len() < MAX_COMMAND_LINE_BYTES {
                    current_line.push(byte);
                }
                ended_with_newline = false;
            }
        }
    }

    Ok(if ended_with_newline {
        last_line
    } else {
        current_line
    })
}

struct StatusCommandTask {
    abort_handle: tokio::task::AbortHandle,
    control: Arc<StatusCommandControl>,
}

struct StatusCommandControl {
    terminated: AtomicBool,
    process_group: Mutex<Option<StatusCommandGuard>>,
}

impl StatusCommandControl {
    fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
        if let Some(mut process_group) = self
            .process_group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            process_group.terminate();
        }
    }

    fn register(&self, mut process_group: StatusCommandGuard) {
        let mut registered = self
            .process_group
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_terminated() {
            process_group.terminate();
        } else {
            *registered = Some(process_group);
        }
    }
}

fn spawn_status_command(
    event_tx: tokio::sync::mpsc::Sender<shepr_mux::events::AppEvent>,
    segment_index: usize,
    command: String,
    timeout: Duration,
    started_at: tokio::time::Instant,
    environment: Vec<(String, String)>,
    cwd: std::path::PathBuf,
) -> StatusCommandTask {
    let control = Arc::new(StatusCommandControl {
        terminated: AtomicBool::new(false),
        process_group: Mutex::new(None),
    });
    let task_control = Arc::clone(&control);
    let deadline = started_at + timeout;
    let task = tokio::spawn(async move {
        let result = run_status_command(
            task_control.as_ref(),
            command.clone(),
            timeout,
            deadline,
            environment,
            cwd,
        )
        .await
        .map_err(|cause| TabBarCommandError { command, cause });
        task_control.terminate();
        // Fails only once the app dropped its event receiver, when no tab
        // bar is left to show the result.
        event_tx
            .send(shepr_mux::events::AppEvent::TabBarCommandFinished {
                segment_index,
                result,
            })
            .await
            .ok();
    });
    StatusCommandTask {
        abort_handle: task.abort_handle(),
        control,
    }
}

async fn run_status_command(
    control: &StatusCommandControl,
    command: String,
    timeout: Duration,
    deadline: tokio::time::Instant,
    environment: Vec<(String, String)>,
    cwd: std::path::PathBuf,
) -> Result<Option<String>, TabBarCommandFailure> {
    // clock-io-ok: a task may first be polled after its subprocess deadline.
    if control.is_terminated() || tokio::time::Instant::now() >= deadline {
        return Err(TabBarCommandFailure::TimedOut(timeout));
    }

    // host-program-ok: a status command is the user's shell command line
    // The working directory is chosen by `App::status_command_env`.
    let mut process = shepr_platform::child_command(TAB_BAR_COMMAND_SHELL, &cwd);
    process
        .args([TAB_BAR_COMMAND_SHELL_ARGS, &command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Arbitrary command stderr is unbounded and may contain secrets;
        // failures are reported through redacted status errors instead.
        .stderr(Stdio::null())
        .envs(environment);
    configure_status_command(&mut process);

    let mut process = tokio::process::Command::from(process);
    process.kill_on_drop(true);
    let mut child = process.spawn().map_err(TabBarCommandFailure::Spawn)?;
    let process_group =
        StatusCommandGuard::new(&child).map_err(TabBarCommandFailure::ProcessGroup)?;
    control.register(process_group);
    if control.is_terminated() {
        return Err(TabBarCommandFailure::Cancelled);
    }

    let operation = async {
        let stdout = child.stdout.take();
        let read_output = async {
            let Some(stdout) = stdout else {
                return std::io::Result::Ok(Vec::new());
            };
            read_last_output_line(stdout).await
        };
        let (status, output) = tokio::join!(child.wait(), read_output);
        let status = status.map_err(TabBarCommandFailure::Wait)?;
        let output = output.map_err(TabBarCommandFailure::Output)?;
        if status.success() {
            Ok(command_output_text(&output))
        } else {
            Err(TabBarCommandFailure::Exited(status))
        }
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(result) => result,
        Err(_) => Err(TabBarCommandFailure::TimedOut(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_config::Config;
    use shepr_mux::events::AppEvent;
    use shepr_test_support::fixture::{self, Step};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        )
    }

    // Status commands are shell command lines, since production runs them
    // under the shell; these run the fixture program through it.

    fn multiline_command() -> String {
        fixture::shell_line(&[Step::Print("old\nfinal\n".into())])
    }

    /// Output far past the capture cap, then a last line.
    fn over_cap_command() -> String {
        fixture::shell_line(&[
            Step::Fill {
                byte: b'x',
                count: 5000,
            },
            Step::Print("\nREADY\n".into()),
        ])
    }

    /// A marker path a status command writes to, in a fresh scratch directory
    /// that outlives the test body (a descendant may still write after it
    /// ends).
    fn marker_exists(path: &std::path::Path) -> bool {
        path.try_exists().expect("stat status command marker")
    }

    /// A stated working directory for a status command a test spawns directly.
    fn command_cwd() -> std::path::PathBuf {
        crate::test_support::ScratchDir::new("tab-status-cwd")
            .path()
            .to_path_buf()
    }

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        crate::test_support::ScratchDir::new("tab-status").join(name)
    }

    #[tokio::test]
    async fn status_command_reports_its_sanitized_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            3,
            multiline_command(),
            Duration::from_secs(2),
            tokio::time::Instant::now(),
            Vec::new(),
            command_cwd(),
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                segment_index: 3,
                result: Ok(Some(ref output)),
            } if output == "final"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_command_timeout_starts_before_task_is_polled() {
        let ran = unique_temp_path("ran-after-timeout");
        let command = fixture::shell_line(&[Step::To(ran.clone()), Step::Print("ran".into())]);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            3,
            command.clone(),
            Duration::from_secs(1),
            tokio::time::Instant::now() - Duration::from_secs(2),
            Vec::new(),
            command_cwd(),
        );

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        let command_ran = marker_exists(&ran);
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Err(ref error),
                ..
            } if error.command == command && matches!(&error.cause, TabBarCommandFailure::TimedOut(duration) if *duration == Duration::from_secs(1))
        ));
        assert!(!command_ran, "status command ran after its deadline");
    }

    #[tokio::test]
    async fn status_command_drains_large_output_and_keeps_the_last_line() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_status_command(
            event_tx,
            3,
            over_cap_command(),
            Duration::from_secs(2),
            tokio::time::Instant::now(),
            Vec::new(),
            command_cwd(),
        );

        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("status command timed out")
            .expect("status command event channel closed");
        assert!(matches!(
            event,
            AppEvent::TabBarCommandFinished {
                result: Ok(Some(ref output)),
                ..
            } if output == "READY"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_the_app_kills_an_in_flight_command_and_its_descendants() {
        let descendant_started = unique_temp_path("descendant-started");
        let survived = unique_temp_path("survived");
        // A background descendant of the command's shell marks that it
        // started, and would mark that it survived 300 ms later.
        let command = format!(
            "{} & wait",
            fixture::shell_line(&[
                Step::To(descendant_started.clone()),
                Step::Print("descendant-started".into()),
                Step::Sleep(Duration::from_millis(300)),
                Step::To(survived.clone()),
                Step::Print("survived".into()),
            ])
        );
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[TabBarRightEntryConfig::Command {
                command,
                interval_seconds: 5,
                timeout_seconds: 20,
            }],
            " ",
        );
        app.handle_tab_bar_status_tasks(std::time::Instant::now());
        for _ in 0..50 {
            if marker_exists(&descendant_started) {
                break;
            }
            // The child reports through a file marker with no async notifier.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            marker_exists(&descendant_started),
            "status command descendant did not start"
        );

        drop(app);

        // Task cancellation is delivered when Tokio next polls the task. Block
        // this current-thread test runtime long enough for the descendant to
        // run, proving teardown kills its process group synchronously.
        std::thread::sleep(Duration::from_millis(400));
        let descendant_survived = marker_exists(&survived);
        assert!(!descendant_survived, "status command descendant survived");
    }

    #[tokio::test]
    async fn in_flight_command_has_no_second_deadline() {
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[TabBarRightEntryConfig::Command {
                command: multiline_command(),
                interval_seconds: 5,
                timeout_seconds: 2,
            }],
            " ",
        );

        let now = std::time::Instant::now();
        assert!(app.next_tab_bar_status_deadline().is_some());
        app.handle_tab_bar_status_tasks(now);

        assert!(app.tab_bar_status.commands[0].task.is_some());
        assert_eq!(app.next_tab_bar_status_deadline(), None);
    }

    #[test]
    fn datetime_refresh_updates_its_segment_once_per_deadline() {
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[TabBarRightEntryConfig::Datetime {
                format: "%Y-%m-%d %H:%M:%S".into(),
            }],
            " ",
        );
        app.state.tab_bar_right[0] = TabBarStatusSegment::Text(None);
        let deadline = app
            .tab_bar_status
            .next_datetime_refresh
            .expect("datetime refresh deadline");

        assert!(app.handle_tab_bar_status_tasks(deadline));
        assert!(matches!(
            &app.state.tab_bar_right[0],
            TabBarStatusSegment::Text(Some(value)) if !value.is_empty()
        ));
        assert!(!app.handle_tab_bar_status_tasks(deadline));
    }

    #[test]
    fn command_output_uses_sanitized_last_line() {
        assert_eq!(
            command_output_text(b"old\n win\x1b[31mter\r\n"),
            Some("winter".into())
        );
        assert_eq!(command_output_text(b"\r\n"), None);
        // Generated text is trimmed; padding alone is not a segment.
        assert_eq!(command_output_text(b"  42%\t \n"), Some("42%".into()));
        assert_eq!(command_output_text(b"   \n"), None);
    }

    #[test]
    fn command_output_strips_ansi_style_sequences() {
        assert_eq!(
            command_output_text(b"\x1b[32mHELLO\x1b[0m"),
            Some("HELLO".into())
        );
    }

    #[test]
    fn command_output_strips_terminal_control_sequence_families() {
        assert_eq!(
            command_output_text(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
            Some("link".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x1b\\visible\x1b7"),
            Some("visible".into())
        );
        assert_eq!(command_output_text(b"\x1b[31m\x1b[0m"), None);
        assert_eq!(
            command_output_text(b"\x1b\x07[32mHELLO\x1b[0m"),
            Some("HELLO".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x18VISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(
            command_output_text(b"\x1bPignored\x1b7VISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(
            command_output_text(b"\x1b]ignored\x1aVISIBLE"),
            Some("VISIBLE".into())
        );
        assert_eq!(command_output_text(b"\xc2\x1b[31m\xa2"), Some("��".into()));

        let styled = format!("\x1b[38;2;1;2;3m{}\x1b[0m", "x".repeat(80));
        assert_eq!(command_output_text(styled.as_bytes()), Some("x".repeat(80)));
    }

    #[test]
    fn tab_bar_sources_share_control_filter_and_length_cap() {
        let source = format!("safe\u{202e}{}", "x".repeat(90));
        let expected = format!("safe{}", "x".repeat(76));
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[TabBarRightEntryConfig::Text {
                text: source.clone(),
            }],
            &source,
        );

        assert_eq!(
            app.state.tab_bar_right,
            vec![TabBarStatusSegment::Text(Some(expected.clone()))]
        );
        assert_eq!(app.state.tab_bar_right_separator, expected);
        assert_eq!(command_output_text(source.as_bytes()), Some(expected));
    }

    #[test]
    fn tab_bar_text_preserves_spacing_and_drops_controls() {
        assert_eq!(TabBarText::new(" \x1b|\n ").into_string(), " | ");
    }

    #[test]
    fn timeout_error_keeps_subsecond_precision() {
        assert_eq!(
            tab_bar_command_failure_message(&TabBarCommandFailure::TimedOut(
                Duration::from_millis(500)
            )),
            "timed out after 500ms"
        );
    }

    #[test]
    fn command_failure_warning_state_resets_only_after_success() {
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[TabBarRightEntryConfig::Command {
                command: "false".into(),
                interval_seconds: 1,
                timeout_seconds: 1,
            }],
            " ",
        );

        let failure = || TabBarCommandError {
            command: "false".into(),
            cause: TabBarCommandFailure::Cancelled,
        };
        app.handle_tab_bar_command_finished(0, Err(failure()));
        assert!(app.tab_bar_status.commands[0].failure_logged);
        app.handle_tab_bar_command_finished(0, Err(failure()));
        assert!(app.tab_bar_status.commands[0].failure_logged);
        app.handle_tab_bar_command_finished(0, Ok(Some("healthy".into())));
        assert!(!app.tab_bar_status.commands[0].failure_logged);
    }
}

// Status commands run in their own process group so completion, timeout, and
// cancellation can stop any background descendants safely.
fn configure_status_command(process: &mut Command) {
    use std::os::unix::process::CommandExt;

    process.process_group(0);
}

struct StatusCommandGuard {
    process_group_id: Option<i32>,
    /// A handle on the group leader, opened while the child was certainly
    /// unreaped. `None` only if it could not be opened at all.
    leader: Option<shepr_platform::ProcessHandle>,
}

impl StatusCommandGuard {
    pub(crate) fn new(child: &tokio::process::Child) -> std::io::Result<Self> {
        // `id()` is `None` once tokio has reaped the child, and reaping needs
        // `&mut Child`, so the pid cannot be reused before the handle is open.
        let process_id = child
            .id()
            .ok_or_else(|| std::io::Error::other("status command has no process id"))?;
        let process_group_id = i32::try_from(process_id)
            .map_err(|_| std::io::Error::other("status command process id exceeds i32"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
            leader: shepr_platform::ProcessHandle::open(process_id),
        })
    }

    pub(crate) fn terminate(&mut self) {
        let Some(process_group_id) = self.process_group_id.take() else {
            return;
        };
        let leader = self.leader.take();
        // The command was spawned as this process group's leader. Killing the
        // group also cleans up background descendants on completion or
        // cancellation, but only while the id still names that group: tokio
        // may have reaped the leader already, and a reused number would send
        // SIGKILL to an unrelated group. The remaining gap (the number is
        // reused, the new owner leads a group and exits, all between the reap
        // and this call) needs a full pid wraparound in that window.
        let ours = status_group_is_ours(
            leader
                .as_ref()
                .map(shepr_platform::ProcessHandle::is_unreaped),
            // A stat error other than absence cannot prove the number free,
            // so it counts as held: skipping the kill is the safe side.
            || {
                Path::new(&format!("/proc/{process_group_id}"))
                    .try_exists()
                    .unwrap_or(true)
            },
        );
        if !ours {
            return;
        }
        // SAFETY: kill(2) touches no memory of this process.
        unsafe {
            libc::kill(-process_group_id, libc::SIGKILL);
        }
    }
}

/// Whether process group `process_group_id` can still only be the one the
/// status command led. The kernel reuses a number only once nothing holds it
/// as a pid, process-group id or session id. An unreaped leader holds it.
/// After the leader is reaped, any task that holds that pid again is proof
/// the number was reused, and the original group had no members left when
/// that happened. Without a leader handle the second test is all there is.
fn status_group_is_ours(
    leader_unreaped: Option<bool>,
    pid_held_by_a_task: impl FnOnce() -> bool,
) -> bool {
    leader_unreaped == Some(true) || !pid_held_by_a_task()
}

impl Drop for StatusCommandGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod status_command_tests {
    use super::*;

    #[test]
    fn status_group_is_signalled_only_while_its_id_cannot_have_been_reused() {
        assert!(status_group_is_ours(Some(true), || true));
        assert!(status_group_is_ours(Some(false), || false));
        assert!(status_group_is_ours(None, || false));
        assert!(!status_group_is_ours(Some(false), || true));
        assert!(!status_group_is_ours(None, || true));
    }

    #[tokio::test]
    async fn status_guard_kills_the_group_while_the_leader_is_unreaped() {
        use shepr_test_support::fixture::{self, Held, Step};

        let mut command = fixture::command(&[
            Step::Spawn {
                argv0: "background-job".into(),
                sleep: Duration::from_secs(30),
                held: Held::All,
            },
            Step::Sleep(Duration::from_secs(30)),
        ]);
        configure_status_command(&mut command);
        let mut child = tokio::process::Command::from(command)
            .spawn()
            .expect("spawn status command");
        let mut guard = StatusCommandGuard::new(&child).expect("guard");
        guard.terminate();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("leader dies after the group kill")
            .expect("wait");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}
