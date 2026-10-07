//! Agent state detection: screen manifests matched against the pane's live
//! bottom-of-buffer text, process recognition over the platform's `/proc`
//! readers, and the per-pane ownership arbitration between screen detection
//! and integration reports (`ownership`, which never runs the manifest engine).

mod limits;
pub mod manifest;
pub mod ownership;
mod title_activity;

pub use limits::PARKED_START_LIFETIME;

use shepr_agent::{
    AGENT_EXECUTABLE_SUFFIXES, Agent, AgentState, normalized_agent_lookup_name, parse_agent_label,
};
use shepr_platform::{ForegroundJob, ForegroundProcess, Pid, is_shell_process_name};
pub use title_activity::is_title_activity_glyph;

/// A screen state with evidence that can only belong to that state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// No recognized live state or visibility evidence.
    Unknown,
    /// Visible live idle chrome bypasses the working-to-idle confirmation hold.
    Idle { visible: bool },
    /// Working never overrides hooks, and carries no visibility.
    Working,
    /// Visible input controls may override a non-blocked integration report.
    Blocked { visible: bool },
}

impl Detection {
    /// `visible` is ignored for Working and Unknown, which carry no visibility.
    pub const fn new(state: AgentState, visible: bool) -> Self {
        match state {
            AgentState::Unknown => Self::Unknown,
            AgentState::Idle => Self::Idle { visible },
            AgentState::Working => Self::Working,
            AgentState::Blocked => Self::Blocked { visible },
        }
    }

    pub const fn state(self) -> AgentState {
        match self {
            Self::Unknown => AgentState::Unknown,
            Self::Idle { .. } => AgentState::Idle,
            Self::Working => AgentState::Working,
            Self::Blocked { .. } => AgentState::Blocked,
        }
    }

    pub const fn visible_idle(self) -> bool {
        matches!(self, Self::Idle { visible: true })
    }

    pub const fn visible_blocker(self) -> bool {
        matches!(self, Self::Blocked { visible: true })
    }
}

/// An agent-owned history viewer preserves the previous live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDetection {
    /// Transcript or history chrome carries no live state update.
    Skip,
    State(Detection),
}

impl AgentDetection {
    pub const fn detection(self) -> Option<Detection> {
        match self {
            Self::Skip => None,
            Self::State(detection) => Some(detection),
        }
    }

    pub const fn state(self) -> AgentState {
        match self {
            Self::Skip => AgentState::Unknown,
            Self::State(detection) => detection.state(),
        }
    }

    pub const fn skip_state_update(self) -> bool {
        matches!(self, Self::Skip)
    }

    pub const fn visible_idle(self) -> bool {
        matches!(self, Self::State(Detection::Idle { visible: true }))
    }

    pub const fn visible_blocker(self) -> bool {
        matches!(self, Self::State(Detection::Blocked { visible: true }))
    }
}

/// Blocking: path-shaped argv tokens are resolved on the filesystem (through
/// `/proc/<pid>/cwd` for relative ones). Call from a blocking context.
/// The string is the selected process's display name, preserving a comm alias;
/// identification and ranking carry `Agent` and provenance before producing it.
pub fn identify_agent_in_job(job: &ForegroundJob) -> Option<(Agent, String)> {
    select_agent_process_in_job(job).map(|selected| (selected.agent, selected.display_name))
}

/// The agent a job runs, the process it was recognized from and that
/// process's display name. The process carries the start time read with its
/// name and state, so `process.instance()` names the incarnation recognized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAgentProcess<'a> {
    pub agent: Agent,
    pub process: &'a ForegroundProcess,
    pub display_name: String,
}

/// [`identify_agent_in_job`] keeping the process it selected. Blocking, as
/// that is.
pub fn select_agent_process_in_job(job: &ForegroundJob) -> Option<SelectedAgentProcess<'_>> {
    if let Some(process) = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id.leader_pid())
        && let Some(identified) = identify_process(process)
    {
        return Some(SelectedAgentProcess {
            agent: identified.agent,
            process,
            display_name: identified_display_name(process, identified),
        });
    }

    let mut best: Option<(ProcessPriority, Identified, &ForegroundProcess)> = None;

    for process in &job.processes {
        if process.pid == job.process_group_id.leader_pid() {
            continue;
        }
        let Some(identified) = identify_process(process) else {
            continue;
        };
        let score = process_priority(identified);

        match &best {
            Some((best_score, _, _)) if *best_score >= score => {}
            _ => best = Some((score, identified, process)),
        }
    }

    best.map(|(_, identified, process)| SelectedAgentProcess {
        agent: identified.agent,
        process,
        display_name: identified_display_name(process, identified),
    })
}

/// The agent one process is recognized as, by the same rules job selection
/// applies to each member. Blocking, as path-shaped argv tokens are resolved.
pub fn identify_agent_process(process: &ForegroundProcess) -> Option<Agent> {
    identify_process(process).map(|identified| identified.agent)
}

/// Blocking: scans descendants of the pane shell for job-control-stopped
/// processes that still identify as agents. Call from a blocking context.
pub fn suspended_agent_processes(child_pid: Pid) -> Vec<Agent> {
    let mut agents = Vec::new();
    for process in shepr_platform::suspended_processes(child_pid) {
        let Some(identified) = identify_process(&process) else {
            continue;
        };
        if !agents.contains(&identified.agent) {
            agents.push(identified.agent);
        }
    }
    agents
}

/// Detect state using screen content plus the OSC title and progress evidence,
/// each `None` when there is none.
pub fn detect_agent_with_osc(
    agent: Option<Agent>,
    screen_content: &str,
    osc_title: Option<&str>,
    osc_progress: Option<&str>,
) -> AgentDetection {
    let Some(agent) = agent else {
        return AgentDetection::State(Detection::Unknown);
    };
    manifest::detect_with_osc(
        agent,
        manifest::DetectionInput {
            screen: screen_content,
            osc_title,
            osc_progress,
        },
    )
}

// ---------------------------------------------------------------------------
// Process identification
// ---------------------------------------------------------------------------
//
// Everything in this section reads `/proc` synchronously. The per-pane
// detection task is async, so it has to reach these through a blocking
// section rather than calling them on a runtime worker.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Node,
    Bun,
    Python,
    Shell,
}

impl Runtime {
    fn classify(name: &str) -> Option<Self> {
        let name = path_basename(name).trim();
        // Runtime spelling uses the same executable suffixes as agent lookup,
        // but classification borrows the name instead of allocating per probe.
        let name = AGENT_EXECUTABLE_SUFFIXES
            .iter()
            .copied()
            .find_map(|suffix| {
                let end = name.len().checked_sub(suffix.len())?;
                name.get(end..)
                    .filter(|tail| tail.eq_ignore_ascii_case(suffix))
                    .map(|_| &name[..end])
            })
            .unwrap_or(name);
        if name.eq_ignore_ascii_case("node") {
            Some(Self::Node)
        } else if name.eq_ignore_ascii_case("bun") {
            Some(Self::Bun)
        } else if is_python_runtime(name) {
            Some(Self::Python)
        } else if is_shell_process_name(name) {
            Some(Self::Shell)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentifiedVia {
    Comm,
    Argv0,
    WrappedScript { runtime: Runtime },
    PackagePath,
    ResolvedSymlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identified {
    agent: Agent,
    via: IdentifiedVia,
}

fn process_identity(process: &ForegroundProcess) -> Option<Identified> {
    let cwd_pid = Some(process.pid);
    if let Some(runtime) = Runtime::classify(&process.name)
        && let Some(identified) =
            wrapped_agent_from_runtime_argv(runtime, process.argv.as_deref(), cwd_pid)
    {
        return Some(identified);
    }

    if let Some(agent) = parse_agent_label(&process.name) {
        return Some(Identified {
            agent,
            via: IdentifiedVia::Comm,
        });
    }

    if let Some(runtime) = process
        .argv
        .as_deref()
        .and_then(|argv| argv.first())
        .and_then(|name| Runtime::classify(name))
        && matches!(runtime, Runtime::Node | Runtime::Bun)
        && let Some(identified) =
            wrapped_agent_from_runtime_argv(runtime, process.argv.as_deref(), cwd_pid)
        && matches!(identified.agent, Agent::Qwen | Agent::Cline | Agent::Letta)
    {
        return Some(identified);
    }

    argv0_agent(process.argv.as_deref(), cwd_pid)
}

fn identify_process(process: &ForegroundProcess) -> Option<Identified> {
    let identified = process_identity(process)?;
    if identified.agent == Agent::Letta && !is_interactive_letta_process(process) {
        return None;
    }
    Some(identified)
}

fn identified_display_name(process: &ForegroundProcess, identified: Identified) -> String {
    // Comm preserves aliases such as opencode2 for the probe's displayed process
    // name. Inferred entrypoints display the canonical label; neither is parsed
    // back into an identity.
    match identified.via {
        IdentifiedVia::Comm => process.name.clone(),
        _ => identified.agent.label().to_owned(),
    }
}

/// Node and Bun options that run inline code instead of a script.
const NODE_EVAL_FLAGS: &[&str] = &["-e", "--eval", "-p", "--print"];
/// Node and Bun options that take their value as the next argument.
const NODE_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--require",
    "--loader",
    "--import",
    "--experimental-loader",
    "--inspect-port",
];
const PYTHON_VALUE_FLAGS: &[&str] = &["-W", "-X", "--check-hash-based-pycs"];

fn wrapped_agent_from_runtime_argv(
    runtime: Runtime,
    argv: Option<&[String]>,
    cwd_pid: Option<Pid>,
) -> Option<Identified> {
    let argv = argv?;
    let mut identified = match runtime {
        Runtime::Node | Runtime::Bun => {
            script_arg_agent(argv, NODE_EVAL_FLAGS, &[], NODE_VALUE_FLAGS, cwd_pid)
        }
        Runtime::Python => script_arg_agent(argv, &["-c"], &["-m"], PYTHON_VALUE_FLAGS, cwd_pid),
        Runtime::Shell => shell_agent_from_runtime_argv(argv, cwd_pid),
    }?;
    identified.via = IdentifiedVia::WrappedScript { runtime };
    Some(identified)
}

/// Inspect only a direct command word from shell `-c` input; do not parse shell grammar.
fn shell_agent_from_runtime_argv(argv: &[String], cwd_pid: Option<Pid>) -> Option<Identified> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args
                .next()
                .and_then(|token| agent_from_path_token(token, cwd_pid));
        }

        // `-c` alone or inside a short-flag cluster such as `-lc`.
        if arg
            .strip_prefix('-')
            .is_some_and(|flags| !flags.starts_with('-') && flags.contains('c'))
        {
            return args
                .next()
                .and_then(|command| shell_command_agent(command, cwd_pid));
        }

        if shell_option_takes_value(arg) {
            let _ = args.next();
            continue;
        }

        if arg.starts_with('-') {
            continue;
        }

        return agent_from_path_token(arg, cwd_pid);
    }

    None
}

fn shell_command_agent(command: &str, cwd_pid: Option<Pid>) -> Option<Identified> {
    let mut words = command.split_whitespace();
    let first = words.next()?;
    let executable = if first == "exec" {
        let next = words.next()?;
        if next == "--" { words.next()? } else { next }
    } else {
        first
    };
    agent_from_path_token(executable, cwd_pid)
}

fn script_arg_agent(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
    value_flags: &[&str],
    cwd_pid: Option<Pid>,
) -> Option<Identified> {
    let index = script_arg_index(argv, eval_flags, module_flags, value_flags)?;
    agent_from_path_token(argv.get(index)?, cwd_pid)
}

fn script_arg_index(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
    value_flags: &[&str],
) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            return argv.get(index + 1).map(|_| index + 1);
        }

        if flag_matches(arg, eval_flags)
            || flag_matches(arg, module_flags)
            || python_execution_flag_cluster(arg, eval_flags, module_flags)
        {
            return None;
        }

        if arg.starts_with('-') {
            index += if value_flags.contains(&arg.as_str()) {
                2
            } else {
                1
            };
            continue;
        }

        return Some(index);
    }

    None
}

fn python_execution_flag_cluster(arg: &str, eval_flags: &[&str], module_flags: &[&str]) -> bool {
    if !eval_flags.contains(&"-c") && !module_flags.contains(&"-m") {
        return false;
    }
    let Some(flags) = arg
        .strip_prefix('-')
        .filter(|flags| !flags.starts_with('-'))
    else {
        return false;
    };
    let Some(mode) = flags.chars().last() else {
        return false;
    };
    let mode_flag = format!("-{mode}");
    if !eval_flags.contains(&mode_flag.as_str()) && !module_flags.contains(&mode_flag.as_str()) {
        return false;
    }

    // Python permits no-argument flags such as -I to precede -c or -m in a
    // short-flag cluster. Do not interpret the value attached to -W or -X as
    // a cluster; those options take their value in the same argument.
    let prefix = &flags[..flags.len() - mode.len_utf8()];
    prefix.chars().all(|flag| {
        matches!(
            flag,
            'b' | 'B' | 'd' | 'E' | 'i' | 'I' | 'O' | 'P' | 'q' | 's' | 'S' | 'u' | 'v' | 'V' | 'x'
        )
    })
}

fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags
        .iter()
        .any(|flag| arg == *flag || short_flag_payload(arg, flag) || long_flag_value(arg, flag))
}

fn short_flag_payload(arg: &str, flag: &str) -> bool {
    flag.starts_with('-')
        && !flag.starts_with("--")
        && arg.starts_with(flag)
        && arg.len() > flag.len()
}

fn long_flag_value(arg: &str, flag: &str) -> bool {
    flag.starts_with("--")
        && arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

fn shell_option_takes_value(arg: &str) -> bool {
    matches!(arg, "-o" | "-O" | "+o" | "+O")
}

fn argv0_agent(argv: Option<&[String]>, cwd_pid: Option<Pid>) -> Option<Identified> {
    agent_from_path_token(argv?.first()?, cwd_pid)
}

/// `cwd_pid` is the process the token came from; relative paths resolve
/// against its working directory (see `resolved_agent_from_path_token`).
fn agent_from_path_token(token: &str, cwd_pid: Option<Pid>) -> Option<Identified> {
    let trimmed = token.trim_matches(|c| matches!(c, '"' | '\''));
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }

    parse_agent_label(path_basename(trimmed))
        .map(|agent| Identified {
            agent,
            via: IdentifiedVia::Argv0,
        })
        .or_else(|| {
            agent_from_known_package_path(trimmed).map(|agent| Identified {
                agent,
                via: IdentifiedVia::PackagePath,
            })
        })
        .or_else(|| resolved_agent_from_path_token(trimmed, cwd_pid))
}

// The package layouts matched here are upstream npm layouts, which can change
// between releases. The `identify_agent_in_job_detects_*` tests use constructed
// paths, so nothing here notices when an upstream layout moves.
fn agent_from_known_package_path(path: &str) -> Option<Agent> {
    let raw_components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let ends_with = |suffix: &[&str]| {
        raw_components.len() >= suffix.len()
            && raw_components[raw_components.len() - suffix.len()..]
                .iter()
                .zip(suffix)
                .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
    };
    if ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) || ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "bundle",
        "cli.js",
    ]) {
        return Some(Agent::Pi);
    }
    if ends_with(&[
        "node_modules",
        "@oh-my-pi",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) {
        return Some(Agent::Omp);
    }
    if ends_with(&[
        "node_modules",
        "@moonshot-ai",
        "kimi-code",
        "dist",
        "main.mjs",
    ]) {
        return Some(Agent::Kimi);
    }

    let components: Vec<String> = raw_components
        .into_iter()
        .map(normalized_agent_lookup_name)
        .collect();
    for window in components.windows(5) {
        if window == ["node_modules", "@qwen-code", "qwen-code", "dist", "index"] {
            return Some(Agent::Qwen);
        }
    }
    for window in components.windows(4) {
        if window == ["node_modules", "mastracode", "dist", "cli"] {
            return Some(Agent::Mastracode);
        }
        if window == ["node_modules", "@letta-ai", "letta-code", "letta"] {
            return Some(Agent::Letta);
        }
    }
    None
}

fn letta_entrypoint_index(argv: &[String], cwd_pid: Option<Pid>) -> Option<usize> {
    let is_letta = |arg: &str| {
        agent_from_path_token(arg, cwd_pid)
            .is_some_and(|identified| identified.agent == Agent::Letta)
    };
    if argv.first().is_some_and(|arg| is_letta(arg)) {
        return Some(0);
    }

    let runtime = Runtime::classify(argv.first()?)?;
    if !matches!(runtime, Runtime::Node | Runtime::Bun) {
        return None;
    }

    script_arg_index(argv, NODE_EVAL_FLAGS, &[], NODE_VALUE_FLAGS)
        .filter(|index| argv.get(*index).is_some_and(|arg| is_letta(arg)))
}

fn letta_first_arg_after_backend_selection(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--backend" {
            let _ = args.next();
            continue;
        }
        if arg.starts_with("--backend=") {
            continue;
        }
        return Some(arg);
    }
    None
}

fn is_interactive_letta_process(process: &ForegroundProcess) -> bool {
    let Some(argv) = process.argv.as_deref() else {
        return true;
    };

    let cli_args =
        letta_entrypoint_index(argv, Some(process.pid)).map_or(argv, |index| &argv[index + 1..]);

    if cli_args.iter().any(|arg| {
        let option = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        matches!(
            option,
            "-p" | "--print"
                | "--prompt"
                | "--json"
                | "--stream-json"
                | "--run"
                | "--disable-memory-guard"
                | "--output-format"
                | "--input-format"
                | "--include-partial-messages"
                | "--from-agent"
                | "--environment"
                | "--env"
                | "--pre-load-skills"
                | "--tags"
                | "--ephemeral"
                | "--stateless"
                | "--max-turns"
                | "--memfs-startup"
                | "-h"
                | "--help"
                | "-v"
                | "--version"
                | "--info"
                | "--update"
                | "--upgrade"
        )
    }) {
        return false;
    }

    letta_first_arg_after_backend_selection(cli_args).is_none_or(|arg| arg.starts_with('-'))
}

/// Resolve a path-shaped argv token (symlinks included) and identify the agent
/// from the target's basename.
///
/// A relative token such as `./agent` or `bin/x` names a file relative to the
/// *target* process's working directory, never the shepr server's, so it is
/// resolved through `/proc/<pid>/cwd` (a magic link `canonicalize` follows).
/// Without a pid a relative token is not resolved at all. Bare names (`agent`)
/// were found through `PATH`, not the cwd, and are left alone; their basename
/// has already been checked by the caller.
///
/// This touches the filesystem (`realpath`), like every other part of the
/// foreground-process probe (`identify_agent_in_job` and the `/proc` readers
/// behind `foreground_job`), so async callers must run the probe off the
/// runtime's worker threads, e.g. inside `tokio::task::spawn_blocking`.
fn resolved_agent_from_path_token(token: &str, cwd_pid: Option<Pid>) -> Option<Identified> {
    let path = std::path::Path::new(token);
    if path.components().count() < 2 {
        return None;
    }

    let resolved = if path.is_absolute() {
        std::fs::canonicalize(path).ok()?
    } else {
        let pid = cwd_pid?;
        std::fs::canonicalize(std::path::Path::new(&format!("/proc/{pid}/cwd")).join(path)).ok()?
    };
    let basename = resolved.file_name()?.to_str()?;
    parse_agent_label(basename).map(|agent| Identified {
        agent,
        via: IdentifiedVia::ResolvedSymlink,
    })
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(path)
}

/// Candidate preference from weakest to strongest; declaration order is rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProcessPriority {
    NormalizedAlias,
    AgentExecutable,
}

fn process_priority(identified: Identified) -> ProcessPriority {
    match identified.via {
        IdentifiedVia::Comm => ProcessPriority::AgentExecutable,
        IdentifiedVia::Argv0
        | IdentifiedVia::WrappedScript { .. }
        | IdentifiedVia::PackagePath
        | IdentifiedVia::ResolvedSymlink => ProcessPriority::NormalizedAlias,
    }
}

fn is_python_runtime(name: &str) -> bool {
    name.eq_ignore_ascii_case("python")
        || name
            .get(..6)
            .filter(|prefix| prefix.eq_ignore_ascii_case("python"))
            .and_then(|_| name.get(6..))
            .is_some_and(|version| {
                !version.is_empty()
                    && version
                        .split('.')
                        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
            })
}

/// Detect the state of an agent from the live terminal tail snapshot.
/// If `agent` is `None`, returns `Unknown`.
#[cfg(test)]
pub fn detect_state(agent: Option<Agent>, screen_content: &str) -> AgentState {
    detect_agent_with_osc(agent, screen_content, None, None).state()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_platform::Pgid;
    use shepr_platform::{ForegroundJob, foreground_job};
    use shepr_test_support::fixture::{self, Held, Step};
    use std::time::{Duration, Instant};

    fn foreground_process(pid: u32, name: &str, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            pid: Pid::new(pid).expect("test process id"),
            name: name.to_string(),
            argv: Some(argv.iter().map(|arg| (*arg).to_string()).collect()),
            start_ticks: 0,
        }
    }

    fn pgid(value: u32) -> Pgid {
        Pgid::new(value).expect("test process group")
    }

    fn wait_for_foreground_job(
        child_pid: Pid,
        expected: impl Fn(&ForegroundJob) -> bool,
    ) -> Option<ForegroundJob> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(job) = foreground_job(child_pid)
                && expected(&job)
            {
                return Some(job);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// A path that does not exist yet, in a fresh scratch directory.
    fn temp_detection_path(name: &str) -> std::path::PathBuf {
        shepr_test_support::ScratchDir::new(name).join("path")
    }

    // ---- Agent identification ----

    #[test]
    fn parse_known_agent_labels() {
        for agent in Agent::all() {
            assert_eq!(parse_agent_label(agent.executable()), Some(agent));
        }

        assert_eq!(parse_agent_label("pi"), Some(Agent::Pi));
        assert_eq!(parse_agent_label("claude"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("claude-code"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("codex"), Some(Agent::Codex));
        assert_eq!(parse_agent_label("gemini"), Some(Agent::Gemini));
        assert_eq!(parse_agent_label("cursor"), Some(Agent::Cursor));
        assert_eq!(parse_agent_label("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(parse_agent_label("devin"), Some(Agent::Devin));
        assert_eq!(parse_agent_label("devin-cli"), Some(Agent::Devin));
        assert_eq!(parse_agent_label("agy"), Some(Agent::Antigravity));
        assert_eq!(
            parse_agent_label("antigravity-cli"),
            Some(Agent::Antigravity)
        );
        assert_eq!(parse_agent_label("cline"), Some(Agent::Cline));
        assert_eq!(parse_agent_label("omp"), Some(Agent::Omp));
        assert_eq!(parse_agent_label("mastracode"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("mastra-code"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("opencode"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("opencode.exe"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("opencode2"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("opencode2.exe"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("kimi"), Some(Agent::Kimi));
        assert_eq!(parse_agent_label("Kimi Code"), Some(Agent::Kimi));
        assert_eq!(parse_agent_label("kiro"), Some(Agent::Kiro));
        assert_eq!(parse_agent_label("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(parse_agent_label("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(parse_agent_label("ghcs"), Some(Agent::GithubCopilot));
        assert_eq!(parse_agent_label("grok"), Some(Agent::Grok));
        assert_eq!(parse_agent_label("grok-build"), Some(Agent::Grok));
        assert_eq!(parse_agent_label("kilo"), Some(Agent::Kilo));
        assert_eq!(parse_agent_label("kilo-code"), Some(Agent::Kilo));
        assert_eq!(parse_agent_label("qwen"), Some(Agent::Qwen));
        assert_eq!(parse_agent_label("Qwen Code"), Some(Agent::Qwen));
        assert_eq!(parse_agent_label("letta"), Some(Agent::Letta));
        assert_eq!(parse_agent_label("Letta Code"), Some(Agent::Letta));
        assert_eq!(parse_agent_label("maki"), Some(Agent::Maki));
        assert_eq!(parse_agent_label("muse"), Some(Agent::Muse));
        assert_eq!(parse_agent_label("muse-code"), Some(Agent::Muse));
        assert_eq!(parse_agent_label("muse-cli"), Some(Agent::Muse));
        assert_eq!(
            parse_agent_label("muse-bin-0.1.0-R708.1"),
            Some(Agent::Muse)
        );
        assert_eq!(parse_agent_label("muse-bin-1.2.3"), Some(Agent::Muse));
        assert_eq!(
            parse_agent_label("/home/user/.local/bin/muse-bin-0.2.1-R1215.1"),
            Some(Agent::Muse)
        );
    }

    #[test]
    fn mastracode_is_hook_authority_without_screen_manifest() {
        assert!(
            shepr_agent::ReportOrigin::official(Agent::Mastracode)
                .expect("MastraCode integration")
                .is_full_lifecycle()
        );
        assert!(manifest::bundled_manifest_source(Agent::Mastracode).is_none());
    }

    #[test]
    fn session_identity_integrations_leave_state_to_screen_detection() {
        let origin = shepr_agent::ReportOrigin::official(Agent::Antigravity)
            .expect("Antigravity integration");
        assert_eq!(
            origin.authority_class(),
            shepr_agent::HookAuthorityClass::SessionOnly
        );
        assert!(!origin.is_full_lifecycle());
        assert!(manifest::bundled_manifest_source(Agent::Antigravity).is_some());
    }

    #[test]
    fn parse_unknown_process_labels() {
        assert_eq!(parse_agent_label("bash"), None);
        assert_eq!(parse_agent_label("zsh"), None);
        assert_eq!(parse_agent_label("vim"), None);
        assert_eq!(parse_agent_label("node"), None);
        assert_eq!(parse_agent_label("museum"), None);
        assert_eq!(parse_agent_label("muse-helper"), None);
        assert_eq!(parse_agent_label("muser"), None);
        assert_eq!(parse_agent_label("musescore"), None);
        assert_eq!(parse_agent_label("muse-bin"), None);
        assert_eq!(parse_agent_label("muse-bin-"), None);
        assert_eq!(parse_agent_label("muse-binary"), None);
    }

    #[test]
    fn parse_agent_labels_case_insensitively() {
        assert_eq!(parse_agent_label("Pi"), Some(Agent::Pi));
        assert_eq!(parse_agent_label("CLAUDE"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("Codex"), Some(Agent::Codex));
        assert_eq!(parse_agent_label("Devin"), Some(Agent::Devin));
    }

    #[test]
    fn identify_agent_in_job_prefers_wrapped_codex() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![
                foreground_process(1, "node", &["node", "/path/to/bin/codex"]),
                foreground_process(2, "bash", &["bash"]),
            ],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_node_wrapped_qwen() {
        for argv in [
            vec!["node", "/home/user/.fnm/bin/qwen"],
            vec![
                "node",
                "/usr/lib/node_modules/@qwen-code/qwen-code/dist/index.js",
            ],
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Qwen, "qwen".to_string()))
            );
        }
    }

    #[test]
    fn identify_agent_in_job_detects_cline_native_binaries() {
        for (name, executable) in [
            (
                ".cline",
                "/home/user/.npm/lib/node_modules/cline/bin/.cline",
            ),
            (
                "cline",
                "/usr/local/lib/node_modules/@cline/cli-linux-x64/bin/cline",
            ),
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, name, &[executable, "--tui"])],
            };

            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Cline, name.to_string()))
            );
        }
    }

    #[test]
    fn identify_agent_in_job_detects_cline_node_wrapper() {
        for (name, argv) in [
            (
                "MainThread",
                vec!["node", "/home/user/.fnm/bin/cline", "--tui"],
            ),
            (
                "node",
                vec!["node", "/usr/local/lib/node_modules/cline/bin/cline"],
            ),
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, name, &argv)],
            };

            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Cline, "cline".to_string()))
            );
        }
    }

    #[test]
    fn identify_agent_in_job_rejects_unrelated_cline_mentions() {
        for argv in [
            vec!["node"],
            vec!["node", "/path/to/other.js", "cline"],
            vec!["node", "-e", "cline"],
            vec!["node", "/path/to/cline-helper"],
            vec!["/path/to/.cline-helper"],
            vec!["/path/to/other", "/path/to/cline"],
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(identify_agent_in_job(&job), None);
        }
        assert_eq!(parse_agent_label("MainThread"), None);
    }

    #[test]
    fn identify_agent_in_job_detects_interactive_letta_entrypoints() {
        for argv in [
            vec!["letta", "--backend", "local"],
            vec![
                "node",
                "/home/user/project/node_modules/.bin/letta",
                "--conversation",
                "conversation-id",
            ],
            vec![
                "node",
                "/usr/lib/node_modules/@letta-ai/letta-code/letta.js",
                "--agent",
                "agent-id",
            ],
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Letta, "letta".to_string()))
            );
        }
    }

    #[test]
    fn identify_agent_in_job_ignores_noninteractive_letta_processes() {
        for args in [
            vec!["--prompt", "hello"],
            vec!["--output-format", "json"],
            vec!["--input-format=stream-json"],
            vec!["--ephemeral"],
            vec!["--max-turns=1"],
            vec!["server"],
            vec!["--backend", "local", "server"],
            vec!["fix this bug"],
            vec!["agents", "list"],
            vec!["version"],
        ] {
            let mut argv = vec!["node", "/home/user/project/node_modules/.bin/letta"];
            argv.extend(args);
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(identify_agent_in_job(&job), None, "argv: {argv:?}");
        }

        let unrelated = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &["node", "/tmp/server.js", "letta"],
            )],
        };
        assert_eq!(identify_agent_in_job(&unrelated), None);

        let source_checkout = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &["node", "/home/user/src/letta-code/letta/build.js"],
            )],
        };
        assert_eq!(identify_agent_in_job(&source_checkout), None);
    }

    #[test]
    fn identify_agent_in_job_prefers_recognized_process_group_leader() {
        let job = ForegroundJob {
            process_group_id: pgid(42),
            processes: vec![
                foreground_process(42, "claude", &["claude"]),
                foreground_process(43, "node", &["node", "/tmp/mcp/bin/codex"]),
            ],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Claude, "claude".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_falls_back_when_process_group_leader_is_unrecognized() {
        let job = ForegroundJob {
            process_group_id: pgid(42),
            processes: vec![
                foreground_process(42, "bash", &["bash"]),
                foreground_process(43, "node", &["node", "/tmp/mcp/bin/codex"]),
            ],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_prefers_an_agent_executable_over_a_wrapped_alias() {
        let job = ForegroundJob {
            process_group_id: pgid(42),
            processes: vec![
                foreground_process(42, "bash", &["bash"]),
                foreground_process(43, "node", &["node", "/opt/mcp/bin/codex"]),
                foreground_process(44, "claude", &["claude"]),
            ],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Claude, "claude".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_python_version_wrapped_script() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "python3.12",
                &[
                    "/nix/store/example/bin/python3.12",
                    "/nix/store/example/bin/codex",
                    "--model",
                    "gpt-5",
                ],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_skips_python_hash_based_pyc_option_value() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "python3",
                &["python3", "--check-hash-based-pycs", "always", "/tmp/codex"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_nix_wrapped_codex_from_argv0() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                ".codex-wrapped",
                &["/etc/profiles/per-user/user/bin/codex", "--model", "gpt-5"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_canonicalizes_nix_wrapped_aliases_from_argv0() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                ".claude-code-wrapped",
                &["/nix/store/example/bin/claude-code"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Claude, "claude".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_shell_wrapped_pi() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                "sh",
                // host-program-ok: a shell-wrapped agent's argv is the subject
                &["/bin/sh", "/tmp/test-bin/pi"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Pi, "pi".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_bun_wrapped_omp() {
        for (runtime, script) in [
            ("bun", "/home/can/.bun/bin/omp"),
            (
                "bun",
                "/usr/lib/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js",
            ),
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, runtime, &[runtime, script])],
            };
            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Omp, "omp".to_string())),
                "script: {script}"
            );
        }

        let other_script = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "bun",
                &[
                    "bun",
                    "/usr/lib/node_modules/@oh-my-pi/pi-coding-agent/dist/setup.js",
                ],
            )],
        };
        assert_eq!(identify_agent_in_job(&other_script), None);
    }

    #[test]
    fn identify_agent_in_job_detects_node_wrapped_pi_package_cli() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &[
                    "node",
                    "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
                ],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Pi, "pi".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_node_wrapped_pi_bundled_cli() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &[
                    "/home/user/.local/share/pi-node/current/node",
                    "/home/user/.local/share/pi-node/current/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js",
                ],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Pi, "pi".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_node_wrapped_mastracode_package_cli() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &["node", "/usr/lib/node_modules/mastracode/dist/cli.js"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Mastracode, "mastracode".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_node_wrapped_kimi_package_cli() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "node",
                &[
                    "/usr/bin/node",
                    "/opt/kimi-prefix/node_modules/@moonshot-ai/kimi-code/dist/main.mjs",
                ],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Kimi, "kimi".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_ignores_non_cli_pi_package_scripts() {
        for script in [
            "/usr/lib/node_modules/@earendil-works/pi-coding-agent/scripts/build.js",
            "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/update.js",
            "/workspace/dist/bundle/cli.js",
            "/workspace/node_modules/other-package/dist/bundle/cli.js",
            "/workspace/node_modules/@earendil-works/pi-coding-agent/dist/cli.exe",
            "/workspace/node_modules/@earendil-works/pi-coding-agent/dist/cli.js/other.js",
            "/workspace/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.exe",
            "/workspace/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js/other.js",
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(123, "node", &["node", script])],
            };

            assert_eq!(identify_agent_in_job(&job), None, "script: {script}");
        }
    }

    #[test]
    fn identify_agent_in_job_detects_opencode2_as_opencode() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "opencode2",
                &["opencode2", "--standalone"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::OpenCode, "opencode2".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_opencode_exe_from_pnpm_package() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "opencode.exe",
                &["/home/user/.local/share/pnpm/global/node_modules/opencode-ai/bin/opencode.exe"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::OpenCode, "opencode.exe".to_string()))
        );
    }

    #[test]
    fn identify_agent_in_job_detects_opencode_exe_from_argv0_path() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                123,
                "MainThread",
                &["/home/user/.local/share/pnpm/global/node_modules/opencode-ai/bin/opencode.exe"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::OpenCode, "opencode".to_string()))
        );
    }

    #[test]
    fn runtime_classification_covers_wrappers_without_accepting_similar_names() {
        for (name, expected) in [
            ("/usr/bin/NODE.exe", Runtime::Node),
            ("bun", Runtime::Bun),
            ("Python3.12", Runtime::Python),
            ("-bash", Runtime::Shell),
        ] {
            assert_eq!(Runtime::classify(name), Some(expected));
        }
        for name in [
            "node-helper",
            "python3.",
            "python3.x",
            "pythonista",
            "codex",
            "tmux",
        ] {
            assert_eq!(Runtime::classify(name), None);
        }
    }

    #[test]
    fn identification_ranks_provenance_and_keeps_comm_aliases() {
        let direct = foreground_process(42, "opencode2", &["opencode2"]);
        let wrapped = foreground_process(43, "node", &["node", "opencode.js"]);
        let direct_identity = identify_process(&direct).expect("comm identifies the agent");
        let wrapped_identity = identify_process(&wrapped).expect("script identifies the agent");
        assert_eq!(direct_identity.agent, wrapped_identity.agent);
        assert_eq!(direct_identity.via, IdentifiedVia::Comm);
        assert_eq!(
            wrapped_identity.via,
            IdentifiedVia::WrappedScript {
                runtime: Runtime::Node
            }
        );
        assert!(process_priority(direct_identity) > process_priority(wrapped_identity));
        assert_eq!(
            identified_display_name(&direct, direct_identity),
            "opencode2"
        );
        assert_eq!(
            identified_display_name(&wrapped, wrapped_identity),
            "opencode"
        );
    }

    #[test]
    fn wrapped_agent_from_runtime_argv_ignores_plain_shell_flags() {
        assert_eq!(
            wrapped_agent_from_runtime_argv(
                Runtime::Shell,
                Some(&["bash".into(), "-lc".into()]),
                None
            ),
            None
        );
    }

    #[test]
    fn identify_agent_in_job_ignores_python_c_argument_named_codex() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                "python3",
                &["python3", "-c", "import time; time.sleep(60)", "/tmp/codex"],
            )],
        };

        assert_eq!(identify_agent_in_job(&job), None);
    }

    #[test]
    fn identify_agent_in_job_ignores_node_eval_argument_named_codex() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                "node",
                &["node", "-e", "setTimeout(() => {}, 60000)", "/tmp/codex"],
            )],
        };

        assert_eq!(identify_agent_in_job(&job), None);
    }

    #[test]
    fn identify_agent_in_job_ignores_shell_c_argument_named_codex() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                "bash",
                &["bash", "-c", "sleep 60", "/tmp/codex"],
            )],
        };

        assert_eq!(identify_agent_in_job(&job), None);
    }

    #[test]
    fn identify_agent_in_job_unwraps_an_agent_run_through_shell_dash_c() {
        for argv in [
            &["dash", "-c", "codex --model gpt-5"][..],
            &["nu", "-c", "codex"][..],
            &["/usr/bin/ksh", "-lc", "exec -- /usr/local/bin/codex"][..],
            &["xonsh", "-c", "exec codex"][..],
        ] {
            let job = ForegroundJob {
                process_group_id: pgid(123),
                processes: vec![foreground_process(1, argv[0], argv)],
            };

            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Codex, "codex".to_string())),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn identify_agent_in_job_detects_python_script_named_codex() {
        let job = ForegroundJob {
            process_group_id: pgid(123),
            processes: vec![foreground_process(
                1,
                "python3",
                &["python3", "/tmp/codex", "--model", "gpt-5"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }

    #[test]
    fn relative_argv_path_resolves_against_target_process_cwd() {
        let dir = temp_detection_path("relative-argv-cwd");
        std::fs::create_dir_all(dir.join("bin")).expect("test directory should be created");
        let target = dir.join("bin").join("cursor-agent");
        // Never run: only the link's target name is read.
        std::fs::write(&target, b"cursor-agent").expect("target should be written");
        std::os::unix::fs::symlink(&target, dir.join("bin").join("agent"))
            .expect("symlink should be created");

        // A process whose cwd is the test directory stands in for the agent.
        let mut child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
            .current_dir(&dir)
            .spawn()
            .expect("the fixture should spawn");
        let child_pid = Pid::new(child.id()).expect("test child pid");
        let resolved_via_target =
            agent_from_path_token("bin/agent", Some(child_pid)).map(|identified| identified.agent);
        let resolved_via_dot = agent_from_path_token("./bin/agent", Some(child_pid))
            .map(|identified| identified.agent);
        child.kill().expect("kill the stand-in process");
        child.wait().expect("reap the stand-in process");

        assert_eq!(resolved_via_target, Some(Agent::Cursor));
        assert_eq!(resolved_via_dot, Some(Agent::Cursor));
    }

    #[test]
    fn relative_argv_path_is_not_resolved_without_a_target_pid() {
        // Without the target's pid the only cwd available is the server's own,
        // which says nothing about where the agent was launched.
        assert_eq!(agent_from_path_token("bin/agent", None), None);
        assert_eq!(agent_from_path_token("./agent", None), None);
        // Basename matches never needed the filesystem and still work.
        assert_eq!(
            agent_from_path_token("./bin/codex", None).map(|identified| identified.agent),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn identify_agent_in_job_resolves_cursor_agent_symlink_argv0() {
        let dir = temp_detection_path("cursor-agent-symlink");
        std::fs::create_dir_all(&dir).expect("test directory should be created");
        let target = dir.join("cursor-agent");
        let link = dir.join("agent");
        // Never run: only the link's target name is read.
        std::fs::write(&target, b"cursor-agent").expect("target should be written");
        std::os::unix::fs::symlink(&target, &link).expect("symlink should be created");

        let argv0 = link.to_string_lossy().into_owned();
        let job = ForegroundJob {
            process_group_id: pgid(42),
            processes: vec![foreground_process(
                42,
                "MainThread",
                &[&argv0, "--use-system-ca", "/tmp/index.js"],
            )],
        };

        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Cursor, "cursor".to_string()))
        );
    }

    // ---- Screen detection routing ----

    #[test]
    fn no_agent_returns_unknown() {
        assert_eq!(detect_state(None, "anything"), AgentState::Unknown);
    }

    // ---- Process identification (real PTY) ----

    #[test]
    fn foreground_job_detects_sleep() {
        use shepr_pty::{PtyCommand, backend::spawn_pty};

        // A known, deterministic process that is no agent.
        let scratch = shepr_test_support::ScratchDir::new("detect-pty-sleep");
        let process = fixture::stand_in(
            scratch.path(),
            "shepr-fixture",
            &[Step::Sleep(Duration::from_secs(999))],
        );
        let command = PtyCommand::interactive_shell(
            &shepr_test_support::fixture::resolved_shell(&process),
            false,
        );
        let mut spawned = spawn_pty(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            &command,
            Box::new(drop),
        )
        .expect("failed to spawn");
        let pid = spawned.child.process_id();

        let job = wait_for_foreground_job(pid, |job| {
            job.processes
                .iter()
                .any(|process| process.name == fixture::FIXTURE_NAME)
        });
        spawned.child.kill().expect("kill the fixture");
        spawned.child.wait().expect("reap the fixture");
        let job = job.expect("fixture should become the foreground job");
        assert!(
            job.processes
                .iter()
                .any(|p| p.name == fixture::FIXTURE_NAME),
            "expected the fixture in {job:?}"
        );
        assert_eq!(
            identify_agent_in_job(&job),
            None,
            "the fixture should not map to an agent"
        );
    }

    #[test]
    fn foreground_job_detects_shell_running_command() {
        use shepr_pty::{PtyCommand, backend::spawn_pty};
        use std::io::Write;

        // A stand-in shell named `sh` that, given a line, replaces itself
        // with a command, as `exec` typed into a shell does.
        let bin = shepr_test_support::ScratchDir::new("detect-shell");
        let shell = fixture::stand_in(
            &bin,
            "sh",
            &[
                Step::ReadLine,
                Step::Exec(
                    fixture::argv(&[Step::Sleep(Duration::from_secs(999))])
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                ),
            ],
        );
        let cmd = PtyCommand::interactive_shell(
            &shepr_test_support::fixture::resolved_shell(&shell),
            false,
        );
        let mut spawned = spawn_pty(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            &cmd,
            Box::new(drop),
        )
        .expect("failed to spawn");
        let pid = spawned.child.process_id();

        // Write a command to the shell
        let mut writer = std::fs::File::from(spawned.master_fd.try_clone().expect("clone master"));
        writer
            .write_all(b"exec the command\n")
            .expect("write the command to the stand-in shell");
        drop(writer);

        let job = wait_for_foreground_job(pid, |job| {
            job.processes
                .iter()
                .any(|process| process.name == fixture::FIXTURE_NAME)
        });
        spawned.child.kill().expect("kill the command");
        spawned.child.wait().expect("reap the command");
        let job = job.expect("command should become the foreground job");
        assert!(
            job.processes
                .iter()
                .any(|p| p.name == fixture::FIXTURE_NAME),
            "expected the command in {job:?}"
        );
        assert_eq!(
            identify_agent_in_job(&job),
            None,
            "the command should not map to an agent"
        );
    }

    #[test]
    fn foreground_job_detects_agent_behind_shell_wrapper() {
        use shepr_pty::{PtyCommand, backend::spawn_pty};

        // A stand-in wrapper named `bash` that starts a child whose argv[0]
        // is `codex` and waits for it.
        let bin = shepr_test_support::ScratchDir::new("detect-wrapper");
        let wrapper = fixture::stand_in(
            &bin,
            "bash",
            &[
                Step::Spawn {
                    argv0: "codex".into(),
                    sleep: Duration::from_secs(999),
                    held: Held::All,
                },
                Step::Wait,
            ],
        );
        let cmd = PtyCommand::interactive_shell(
            &shepr_test_support::fixture::resolved_shell(&wrapper),
            false,
        );
        let mut spawned = spawn_pty(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            &cmd,
            Box::new(drop),
        )
        .expect("failed to spawn");
        let child_pid = spawned.child.process_id();
        let job = wait_for_foreground_job(child_pid, |job| {
            identify_agent_in_job(job).is_some_and(|(agent, _)| agent == Agent::Codex)
        });
        let process_group_id = job
            .as_ref()
            .map_or(Pgid::led_by(child_pid), |job| job.process_group_id)
            .as_pid_t();
        // SAFETY: the process group belongs to this test's PTY child; a negative PID
        // targets that group and the value was checked before conversion.
        unsafe {
            libc::kill(-process_group_id, libc::SIGKILL);
        }
        spawned.child.wait().expect("reap the wrapper");

        let job = job.expect("expected foreground job");
        assert!(
            job.processes.iter().any(|process| process.name == "bash")
                && job.processes.iter().any(|process| {
                    process.name == fixture::FIXTURE_NAME
                        && process
                            .argv
                            .as_deref()
                            .and_then(|argv| argv.first())
                            .is_some_and(|argv0| argv0 == "codex")
                }),
            "expected wrapper and agent child in {job:?}"
        );
        assert_eq!(
            identify_agent_in_job(&job),
            Some((Agent::Codex, "codex".to_string()))
        );
    }
}
