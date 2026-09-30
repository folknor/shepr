//! Agent state detection via terminal tail pattern matching.
//!
//! Each pane's live bottom-of-buffer text is read periodically and matched
//! against known agent output patterns to determine state.

pub mod manifest;

pub use crate::agent::Agent;

mod proc_tree;
pub use proc_tree::is_pane_shell_process_name;
pub use proc_tree::{
    ForegroundJob, ForegroundProcess, foreground_group_leader_job, foreground_job,
    foreground_process_group_id, process_cwd,
};

/// The detected state of a terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working/processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Plain shell or unrecognized program.
    Unknown,
}

/// An agent state after applying the user-facing presentation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentedAgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    /// Collapse an unknown state to idle for user-facing presentation.
    pub const fn presentation_state(self) -> PresentedAgentState {
        match self {
            Self::Idle | Self::Unknown => PresentedAgentState::Idle,
            Self::Working => PresentedAgentState::Working,
            Self::Blocked => PresentedAgentState::Blocked,
        }
    }

    /// Rank agent states for attention, from least to most urgent.
    pub const fn attention_rank(self) -> u8 {
        match self.presentation_state() {
            PresentedAgentState::Idle => 0,
            PresentedAgentState::Working => 1,
            PresentedAgentState::Blocked => 2,
        }
    }
}

/// Screen-derived agent state plus confidence metadata used for source arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDetection {
    pub state: AgentState,
    /// True when the current screen is an agent-owned viewer that shows
    /// transcript/history instead of the live prompt state.
    pub skip_state_update: bool,
    /// True when the current screen visibly shows live idle chrome. The pane's
    /// detection loop uses it to publish a Working -> Idle change at once
    /// instead of waiting for the idle to be confirmed over several ticks.
    pub visible_idle: bool,
    /// True when the current screen visibly shows live UI chrome that needs
    /// human input. This is stronger than arbitrary prompt-like text in the
    /// scrollback and may override a non-blocked integration state.
    pub visible_blocker: bool,
    /// True when the current screen visibly shows live working chrome. The
    /// pane detector uses this internally to track screen evidence and refresh
    /// its local state, but does not forward the flag in `StateChanged`. Screen
    /// detection can supply a `Working` state when source arbitration accepts
    /// it, but screen working evidence never overrides a hook's report.
    pub visible_working: bool,
}

pub fn agent_label(agent: Agent) -> &'static str {
    agent.label()
}

pub fn parse_agent_label(agent: &str) -> Option<Agent> {
    let name = normalized_agent_lookup_name(agent);
    Agent::parse_label(&name)
}

pub fn parse_canonical_agent_label(label: &str) -> Option<Agent> {
    Agent::parse_canonical_label(label)
}

/// Identify which agent is running from the process name.
/// Returns `None` for plain shells or unrecognized programs.
pub fn identify_agent(process_name: &str) -> Option<Agent> {
    parse_agent_label(process_name)
}

/// Blocking: path-shaped argv tokens are resolved on the filesystem (through
/// `/proc/<pid>/cwd` for relative ones). Call from a blocking context.
pub fn identify_agent_in_job(job: &ForegroundJob) -> Option<(Agent, String)> {
    if let Some(process) = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id)
    {
        let candidate = normalized_process_name(process);
        if let Some(agent) = identify_agent(&candidate)
            && (agent != Agent::Letta || is_interactive_letta_process(process))
        {
            return Some((agent, candidate));
        }
    }

    let mut best: Option<(ProcessPriority, Agent, String)> = None;

    for process in &job.processes {
        let candidate = normalized_process_name(process);
        let Some(agent) = identify_agent(&candidate) else {
            continue;
        };
        if agent == Agent::Letta && !is_interactive_letta_process(process) {
            continue;
        }
        let score = process_priority(process, &candidate);

        match &best {
            Some((best_score, _, _)) if *best_score >= score => {}
            _ => best = Some((score, agent, candidate)),
        }
    }

    best.map(|(_, agent, name)| (agent, name))
}

/// Blocking: scans descendants of the pane shell for job-control-stopped
/// processes that still identify as agents. Call from a blocking context.
pub fn suspended_agent_processes(child_pid: u32) -> Vec<Agent> {
    let mut agents = Vec::new();
    for process in proc_tree::suspended_processes(child_pid) {
        let candidate = normalized_process_name(&process);
        let Some(agent) = identify_agent(&candidate) else {
            continue;
        };
        if agent == Agent::Letta && !is_interactive_letta_process(&process) {
            continue;
        }
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    agents
}

/// Detect state using screen content plus OSC title/progress strings.
pub fn detect_agent_with_osc(
    agent: Option<Agent>,
    screen_content: &str,
    osc_title: &str,
    osc_progress: &str,
) -> AgentDetection {
    let Some(agent) = agent else {
        return AgentDetection {
            state: AgentState::Unknown,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        };
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

pub fn full_lifecycle_hook_authority(source: &str, agent_label: &str) -> bool {
    crate::agent::AgentSource::from_pair(source, agent_label)
        .and_then(|source| source.agent())
        .is_some_and(|agent| agent.descriptor().full_lifecycle_hook_authority)
}

pub fn session_identity_only_integration(source: &str, agent_label: &str) -> bool {
    crate::agent::AgentSource::from_pair(source, agent_label)
        .and_then(|source| source.agent())
        .is_some_and(|agent| agent.descriptor().session_identity_only_integration)
}

// ---------------------------------------------------------------------------
// Process identification
// ---------------------------------------------------------------------------
//
// Everything in this section reads `/proc` synchronously. The per-pane
// detection task is async, so it has to reach these through a blocking
// section rather than calling them on a runtime worker.

fn normalized_process_name(process: &ForegroundProcess) -> String {
    let effective = process.name.as_str();
    let lower_effective = effective.to_lowercase();
    let cwd_pid = Some(process.pid);

    if is_generic_runtime_or_shell(&lower_effective)
        && let Some(wrapped_agent) =
            wrapped_agent_name_from_runtime_argv(&lower_effective, process.argv.as_deref(), cwd_pid)
    {
        return wrapped_agent;
    }

    if identify_agent(effective).is_some() {
        return effective.to_string();
    }

    if let Some(runtime) = process.argv.as_deref().and_then(|argv| argv.first()) {
        let runtime_name = normalized_agent_lookup_name(path_basename(runtime));
        if matches!(runtime_name.as_str(), "node" | "bun")
            && let Some(wrapped_agent) =
                wrapped_agent_name_from_runtime_argv(runtime, process.argv.as_deref(), cwd_pid)
            && matches!(
                identify_agent(&wrapped_agent),
                Some(Agent::Qwen | Agent::Cline | Agent::Letta)
            )
        {
            return wrapped_agent;
        }
    }

    if let Some(wrapped_agent) = argv0_agent_name(process.argv.as_deref(), cwd_pid).or_else(|| {
        cmdline_argv0_agent_name(process.cmdline.as_deref().unwrap_or_default(), cwd_pid)
    }) {
        return wrapped_agent;
    }

    effective.to_string()
}

fn wrapped_agent_name_from_runtime_argv(
    runtime: &str,
    argv: Option<&[String]>,
    cwd_pid: Option<u32>,
) -> Option<String> {
    let argv = argv?;
    let runtime_name = normalized_agent_lookup_name(path_basename(runtime));

    match runtime_name.as_str() {
        "node" | "bun" => {
            script_arg_agent_name(argv, &["-e", "--eval", "-p", "--print"], &[], cwd_pid)
        }
        name if is_python_runtime(name) => script_arg_agent_name(argv, &["-c"], &["-m"], cwd_pid),
        name if is_pane_shell_process_name(name) => {
            shell_agent_name_from_runtime_argv(argv, cwd_pid)
        }
        _ => None,
    }
}

/// Inspect only a direct command word from shell `-c` input; do not parse shell grammar.
fn shell_agent_name_from_runtime_argv(argv: &[String], cwd_pid: Option<u32>) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args
                .next()
                .and_then(|token| agent_name_from_path_token(token, cwd_pid));
        }

        // `-c` alone or inside a short-flag cluster such as `-lc`.
        if arg
            .strip_prefix('-')
            .is_some_and(|flags| !flags.starts_with('-') && flags.contains('c'))
        {
            return args
                .next()
                .and_then(|command| shell_command_agent_name(command, cwd_pid));
        }

        if arg.starts_with('-') {
            if option_takes_value(arg) {
                let _ = args.next();
            }
            continue;
        }

        return agent_name_from_path_token(arg, cwd_pid);
    }

    None
}

fn shell_command_agent_name(command: &str, cwd_pid: Option<u32>) -> Option<String> {
    let mut words = command.split_whitespace();
    let first = words.next()?;
    let executable = if first == "exec" {
        let next = words.next()?;
        if next == "--" { words.next()? } else { next }
    } else {
        first
    };
    agent_name_from_path_token(executable, cwd_pid)
}

fn script_arg_agent_name(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
    cwd_pid: Option<u32>,
) -> Option<String> {
    let index = script_arg_index(argv, eval_flags, module_flags)?;
    agent_name_from_path_token(argv.get(index)?, cwd_pid)
}

fn script_arg_index(argv: &[String], eval_flags: &[&str], module_flags: &[&str]) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            return argv.get(index + 1).map(|_| index + 1);
        }

        if flag_matches(arg, eval_flags) || flag_matches(arg, module_flags) {
            return None;
        }

        if arg.starts_with('-') {
            index += if option_takes_value(arg) { 2 } else { 1 };
            continue;
        }

        return Some(index);
    }

    None
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

fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-r" | "--require"
            | "--loader"
            | "--import"
            | "--experimental-loader"
            | "--inspect-port"
            | "-W"
            | "-X"
            | "-S"
            | "-L"
            | "-o"
    )
}

fn argv0_agent_name(argv: Option<&[String]>, cwd_pid: Option<u32>) -> Option<String> {
    agent_name_from_path_token(argv?.first()?, cwd_pid)
}

fn cmdline_argv0_agent_name(cmdline: &str, cwd_pid: Option<u32>) -> Option<String> {
    agent_name_from_path_token(cmdline.split_whitespace().next()?, cwd_pid)
}

/// `cwd_pid` is the process the token came from; relative paths resolve
/// against its working directory (see `resolved_agent_name_from_path_token`).
fn agent_name_from_path_token(token: &str, cwd_pid: Option<u32>) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '"' | '\''));
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }

    agent_name_from_basename(path_basename(trimmed))
        .or_else(|| agent_name_from_known_package_path(trimmed))
        .or_else(|| resolved_agent_name_from_path_token(trimmed, cwd_pid))
}

// The package layouts matched here are upstream npm layouts, which can change
// between releases. The `identify_agent_in_job_detects_*` tests use constructed
// paths, so nothing here notices when an upstream layout moves.
fn agent_name_from_known_package_path(path: &str) -> Option<String> {
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
        return Some(agent_label(Agent::Pi).to_string());
    }
    if ends_with(&[
        "node_modules",
        "@oh-my-pi",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) {
        return Some(agent_label(Agent::Omp).to_string());
    }
    if ends_with(&[
        "node_modules",
        "@moonshot-ai",
        "kimi-code",
        "dist",
        "main.mjs",
    ]) {
        return Some(agent_label(Agent::Kimi).to_string());
    }

    let components: Vec<String> = raw_components
        .into_iter()
        .map(normalized_agent_lookup_name)
        .collect();
    for window in components.windows(5) {
        if window == ["node_modules", "@qwen-code", "qwen-code", "dist", "index"] {
            return Some(agent_label(Agent::Qwen).to_string());
        }
    }
    for window in components.windows(4) {
        if window == ["node_modules", "mastracode", "dist", "cli"] {
            return Some(agent_label(Agent::Mastracode).to_string());
        }
        if window == ["node_modules", "@letta-ai", "letta-code", "letta"] {
            return Some(agent_label(Agent::Letta).to_string());
        }
    }
    None
}

fn letta_entrypoint_index(argv: &[String], cwd_pid: Option<u32>) -> Option<usize> {
    let is_letta = |arg: &str| {
        agent_name_from_path_token(arg, cwd_pid).as_deref() == Some(agent_label(Agent::Letta))
    };
    if argv.first().is_some_and(|arg| is_letta(arg)) {
        return Some(0);
    }

    let runtime = argv
        .first()
        .map(|arg| normalized_agent_lookup_name(path_basename(arg)))?;
    if !matches!(runtime.as_str(), "node" | "bun") {
        return None;
    }

    script_arg_index(argv, &["-e", "--eval", "-p", "--print"], &[])
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
    let parsed_cmdline;
    let argv = if let Some(argv) = process.argv.as_deref() {
        argv
    } else {
        parsed_cmdline = process
            .cmdline
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(|arg| arg.trim_matches(|ch| matches!(ch, '\'' | '"')).to_string())
            .collect::<Vec<_>>();
        if parsed_cmdline.is_empty() {
            return true;
        }
        &parsed_cmdline
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
fn resolved_agent_name_from_path_token(token: &str, cwd_pid: Option<u32>) -> Option<String> {
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
    agent_name_from_basename(basename)
}

fn agent_name_from_basename(basename: &str) -> Option<String> {
    let agent = parse_agent_label(basename)?;
    Some(agent_label(agent).to_string())
}

fn normalized_agent_lookup_name(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    // opencode's npm package names its native binary `opencode.exe` on every platform.
    for suffix in [".exe", ".js"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(path)
}

/// Candidate preference from weakest to strongest; declaration order is rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProcessPriority {
    GenericRuntime,
    AgentExecutable,
    NormalizedAlias,
}

fn process_priority(process: &ForegroundProcess, normalized_name: &str) -> ProcessPriority {
    let lower_name = normalized_name.to_lowercase();
    if lower_name != process.name.to_lowercase() {
        return ProcessPriority::NormalizedAlias;
    }
    if !is_generic_runtime_or_shell(&lower_name) {
        return ProcessPriority::AgentExecutable;
    }
    ProcessPriority::GenericRuntime
}

fn is_generic_runtime_or_shell(name: &str) -> bool {
    let name = normalized_agent_lookup_name(path_basename(name));
    is_pane_shell_process_name(&name)
        || is_python_runtime(&name)
        || matches!(name.as_str(), "tmux" | "node" | "bun")
}

fn is_python_runtime(name: &str) -> bool {
    name == "python"
        || name.strip_prefix("python").is_some_and(|version| {
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
    detect_agent_with_osc(agent, screen_content, "", "").state
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Held, Step};
    use std::time::Duration;

    #[test]
    fn presentation_state_collapses_unknown_and_attention_rank_orders_states() {
        assert_eq!(
            AgentState::Unknown.presentation_state(),
            PresentedAgentState::Idle
        );
        assert!(AgentState::Blocked.attention_rank() > AgentState::Working.attention_rank());
        assert!(AgentState::Working.attention_rank() > AgentState::Idle.attention_rank());
        assert_eq!(
            AgentState::Unknown.attention_rank(),
            AgentState::Idle.attention_rank()
        );
    }

    fn foreground_process(pid: u32, name: &str, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            pid,
            name: name.to_string(),
            argv: Some(argv.iter().map(|arg| (*arg).to_string()).collect()),
            cmdline: Some(argv.join(" ")),
        }
    }

    /// A path that does not exist yet, in a fresh scratch directory.
    fn temp_detection_path(name: &str) -> std::path::PathBuf {
        shepr_test_support::ScratchDir::new(name).join("path")
    }

    // ---- Agent identification ----

    #[test]
    fn identify_known_agents() {
        for agent in Agent::all() {
            assert_eq!(identify_agent(agent.executable()), Some(agent));
        }

        assert_eq!(identify_agent("pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("claude"), Some(Agent::Claude));
        assert_eq!(identify_agent("claude-code"), Some(Agent::Claude));
        assert_eq!(identify_agent("codex"), Some(Agent::Codex));
        assert_eq!(identify_agent("gemini"), Some(Agent::Gemini));
        assert_eq!(identify_agent("cursor"), Some(Agent::Cursor));
        assert_eq!(identify_agent("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(identify_agent("devin"), Some(Agent::Devin));
        assert_eq!(identify_agent("devin-cli"), Some(Agent::Devin));
        assert_eq!(identify_agent("agy"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("antigravity-cli"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("cline"), Some(Agent::Cline));
        assert_eq!(identify_agent("omp"), Some(Agent::Omp));
        assert_eq!(identify_agent("mastracode"), Some(Agent::Mastracode));
        assert_eq!(identify_agent("mastra-code"), Some(Agent::Mastracode));
        assert_eq!(identify_agent("opencode"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode.exe"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode2"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode2.exe"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("kimi"), Some(Agent::Kimi));
        assert_eq!(identify_agent("Kimi Code"), Some(Agent::Kimi));
        assert_eq!(identify_agent("kiro"), Some(Agent::Kiro));
        assert_eq!(identify_agent("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(identify_agent("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("ghcs"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("grok"), Some(Agent::Grok));
        assert_eq!(identify_agent("grok-build"), Some(Agent::Grok));
        assert_eq!(identify_agent("kilo"), Some(Agent::Kilo));
        assert_eq!(identify_agent("kilo-code"), Some(Agent::Kilo));
        assert_eq!(identify_agent("qwen"), Some(Agent::Qwen));
        assert_eq!(identify_agent("Qwen Code"), Some(Agent::Qwen));
        assert_eq!(identify_agent("letta"), Some(Agent::Letta));
        assert_eq!(identify_agent("Letta Code"), Some(Agent::Letta));
        assert_eq!(identify_agent("maki"), Some(Agent::Maki));
        assert_eq!(identify_agent("muse"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-code"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-cli"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-bin-0.1.0-R708.1"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-bin-1.2.3"), Some(Agent::Muse));
        assert_eq!(
            identify_agent("/home/user/.local/bin/muse-bin-0.2.1-R1215.1"),
            Some(Agent::Muse)
        );
    }

    #[test]
    fn parse_known_agent_labels() {
        // Canonical labels are covered for every agent by the round-trip test below.
        assert_eq!(parse_agent_label("pi"), Some(Agent::Pi));
        assert_eq!(parse_agent_label("claude"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(parse_agent_label("devin-cli"), Some(Agent::Devin));
        assert_eq!(parse_agent_label("agy"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("antigravity"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("omp"), Some(Agent::Omp));
        assert_eq!(parse_agent_label("mastracode"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("mastra code"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("opencode.exe"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(parse_agent_label("kimi-code"), Some(Agent::Kimi));
        assert_eq!(
            parse_agent_label("github-copilot"),
            Some(Agent::GithubCopilot)
        );
        assert_eq!(parse_agent_label("amp-local"), Some(Agent::Amp));
        assert_eq!(parse_agent_label("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(parse_agent_label("grok-build"), Some(Agent::Grok));
        assert_eq!(parse_agent_label("qwen-code"), Some(Agent::Qwen));
        assert_eq!(parse_agent_label("letta-code"), Some(Agent::Letta));
        assert_eq!(parse_agent_label("maki"), Some(Agent::Maki));
        assert_eq!(parse_agent_label("kilo-code"), Some(Agent::Kilo));
    }

    #[test]
    fn every_agent_label_round_trips_through_canonical_and_alias_parsers() {
        for agent in Agent::all() {
            let label = agent_label(agent);
            assert_eq!(parse_canonical_agent_label(label), Some(agent));
            assert_eq!(parse_agent_label(label), Some(agent));
        }
    }

    #[test]
    fn every_agent_has_a_canonical_interactive_executable() {
        let expected = [
            (Agent::Pi, "pi"),
            (Agent::Claude, "claude"),
            (Agent::Codex, "codex"),
            (Agent::Gemini, "gemini"),
            (Agent::Cursor, "cursor-agent"),
            (Agent::Devin, "devin"),
            (Agent::Antigravity, "agy"),
            (Agent::Cline, "cline"),
            (Agent::Omp, "omp"),
            (Agent::Mastracode, "mastracode"),
            (Agent::OpenCode, "opencode"),
            (Agent::GithubCopilot, "copilot"),
            (Agent::Kimi, "kimi"),
            (Agent::Kiro, "kiro-cli"),
            (Agent::Droid, "droid"),
            (Agent::Amp, "amp"),
            (Agent::Grok, "grok"),
            (Agent::Kilo, "kilo"),
            (Agent::Qodercli, "qodercli"),
            (Agent::Qwen, "qwen"),
            (Agent::Letta, "letta"),
            (Agent::Maki, "maki"),
            (Agent::Muse, "muse"),
        ];
        assert_eq!(expected.len(), Agent::all().len());
        for (agent, executable) in expected {
            assert_eq!(agent.executable(), executable);
        }
    }

    #[test]
    fn canonical_agent_labels_are_strict() {
        assert_eq!(parse_canonical_agent_label("claude-code"), None);
        assert_eq!(parse_canonical_agent_label("Pi"), None);
        assert_eq!(parse_canonical_agent_label(" pi "), None);
        assert_eq!(parse_canonical_agent_label("opencode.exe"), None);
    }

    #[test]
    fn mastracode_is_hook_authority_without_screen_manifest() {
        assert!(full_lifecycle_hook_authority(
            "shepr:mastracode",
            "mastracode"
        ));
        assert!(!Agent::Mastracode.screen_manifest());
    }

    #[test]
    fn session_identity_integrations_leave_state_to_screen_detection() {
        assert!(!full_lifecycle_hook_authority("shepr:agy", "agy"));
        assert!(session_identity_only_integration("shepr:agy", "agy"));
        assert!(Agent::Antigravity.screen_manifest());
    }

    #[test]
    fn identify_unknown_processes() {
        assert_eq!(identify_agent("bash"), None);
        assert_eq!(identify_agent("zsh"), None);
        assert_eq!(identify_agent("vim"), None);
        assert_eq!(identify_agent("node"), None);
        assert_eq!(identify_agent("museum"), None);
        assert_eq!(identify_agent("muse-helper"), None);
        assert_eq!(identify_agent("muser"), None);
        assert_eq!(identify_agent("musescore"), None);
        assert_eq!(identify_agent("muse-bin"), None);
        assert_eq!(identify_agent("muse-bin-"), None);
        assert_eq!(identify_agent("muse-binary"), None);
    }

    #[test]
    fn identify_case_insensitive() {
        assert_eq!(identify_agent("Pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("CLAUDE"), Some(Agent::Claude));
        assert_eq!(identify_agent("Codex"), Some(Agent::Codex));
        assert_eq!(identify_agent("Devin"), Some(Agent::Devin));
    }

    #[test]
    fn identify_agent_in_job_prefers_wrapped_codex() {
        let job = ForegroundJob {
            process_group_id: 123,
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
                process_group_id: 123,
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
                process_group_id: 123,
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
                process_group_id: 123,
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
                process_group_id: 123,
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(identify_agent_in_job(&job), None);
        }
        assert_eq!(identify_agent("MainThread"), None);
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
                process_group_id: 123,
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
                process_group_id: 123,
                processes: vec![foreground_process(123, "MainThread", &argv)],
            };

            assert_eq!(identify_agent_in_job(&job), None, "argv: {argv:?}");
        }

        let unrelated = ForegroundJob {
            process_group_id: 123,
            processes: vec![foreground_process(
                123,
                "node",
                &["node", "/tmp/server.js", "letta"],
            )],
        };
        assert_eq!(identify_agent_in_job(&unrelated), None);

        let source_checkout = ForegroundJob {
            process_group_id: 123,
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
            process_group_id: 42,
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
            process_group_id: 42,
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
    fn identify_agent_in_job_detects_python_version_wrapped_script() {
        let job = ForegroundJob {
            process_group_id: 123,
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
    fn identify_agent_in_job_detects_nix_wrapped_codex_from_cmdline_argv0() {
        let job = ForegroundJob {
            process_group_id: 123,
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
    fn identify_agent_in_job_canonicalizes_nix_wrapped_aliases_from_cmdline_argv0() {
        let job = ForegroundJob {
            process_group_id: 123,
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
            process_group_id: 123,
            processes: vec![foreground_process(
                1,
                "sh",
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
                process_group_id: 123,
                processes: vec![foreground_process(123, runtime, &[runtime, script])],
            };
            assert_eq!(
                identify_agent_in_job(&job),
                Some((Agent::Omp, "omp".to_string())),
                "script: {script}"
            );
        }

        let other_script = ForegroundJob {
            process_group_id: 123,
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
            process_group_id: 123,
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
            process_group_id: 123,
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
            process_group_id: 123,
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
            process_group_id: 123,
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
                process_group_id: 123,
                processes: vec![foreground_process(123, "node", &["node", script])],
            };

            assert_eq!(identify_agent_in_job(&job), None, "script: {script}");
        }
    }

    #[test]
    fn identify_agent_in_job_detects_opencode2_as_opencode() {
        let job = ForegroundJob {
            process_group_id: 123,
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
            process_group_id: 123,
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
            process_group_id: 123,
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
    fn wrapped_agent_name_from_runtime_argv_ignores_plain_shell_flags() {
        assert_eq!(
            wrapped_agent_name_from_runtime_argv(
                "bash",
                Some(&["bash".into(), "-lc".into()]),
                None
            ),
            None
        );
    }

    #[test]
    fn identify_agent_in_job_ignores_python_c_argument_named_codex() {
        let job = ForegroundJob {
            process_group_id: 123,
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
            process_group_id: 123,
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
            process_group_id: 123,
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
                process_group_id: 123,
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
            process_group_id: 123,
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
    fn cmdline_argv0_agent_name_canonicalizes_known_aliases() {
        assert_eq!(
            cmdline_argv0_agent_name("/nix/store/example/bin/ghcs", None),
            Some("copilot".to_string())
        );
    }

    #[test]
    fn cmdline_argv0_agent_name_requires_exact_agent_basename() {
        assert_eq!(cmdline_argv0_agent_name("/tmp/my-codex-helper", None), None);
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
        let resolved_via_target = agent_name_from_path_token("bin/agent", Some(child.id()));
        let resolved_via_dot = agent_name_from_path_token("./bin/agent", Some(child.id()));
        child.kill().expect("kill the stand-in process");
        child.wait().expect("reap the stand-in process");

        assert_eq!(resolved_via_target, Some("cursor".to_string()));
        assert_eq!(resolved_via_dot, Some("cursor".to_string()));
    }

    #[test]
    fn relative_argv_path_is_not_resolved_without_a_target_pid() {
        // Without the target's pid the only cwd available is the server's own,
        // which says nothing about where the agent was launched.
        assert_eq!(agent_name_from_path_token("bin/agent", None), None);
        assert_eq!(agent_name_from_path_token("./agent", None), None);
        // Basename matches never needed the filesystem and still work.
        assert_eq!(
            agent_name_from_path_token("./bin/codex", None),
            Some("codex".to_string())
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
            process_group_id: 42,
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

    fn open_test_pty() -> shepr_pty::backend::OpenedPty {
        shepr_pty::backend::open_pty(24, 80).expect("failed to open pty")
    }

    #[test]
    fn foreground_job_detects_sleep() {
        use shepr_pty::{PtyCommand, backend::spawn_in_pty};

        let pty = open_test_pty();

        // A known, deterministic process that is no agent.
        let mut cmd = PtyCommand::new(fixture::path());
        cmd.args(fixture::args(&[Step::Sleep(Duration::from_secs(999))]));
        let mut child = spawn_in_pty(&pty.slave, &cmd).expect("failed to spawn");
        let pid = child.id();

        // Give the process a moment to become the foreground group
        std::thread::sleep(std::time::Duration::from_millis(50));

        let job = foreground_job(pid).expect("expected foreground job");
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

        // Clean up
        child.kill().expect("kill the fixture");
        child.wait().expect("reap the fixture");
    }

    #[test]
    fn foreground_job_detects_shell_running_command() {
        use shepr_pty::{PtyCommand, backend::spawn_in_pty};
        use std::io::Write;

        let pty = open_test_pty();

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
        let cmd = PtyCommand::new(&shell);
        let mut child = spawn_in_pty(&pty.slave, &cmd).expect("failed to spawn");
        let pid = child.id();

        // Write a command to the shell
        let mut writer = std::fs::File::from(pty.master.try_clone().expect("clone master"));
        writer
            .write_all(b"exec the command\n")
            .expect("write the command to the stand-in shell");
        drop(writer);

        std::thread::sleep(std::time::Duration::from_millis(100));

        let job = foreground_job(pid).expect("expected foreground job");
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

        child.kill().expect("kill the command");
        child.wait().expect("reap the command");
    }

    #[test]
    fn foreground_job_detects_agent_behind_shell_wrapper() {
        use shepr_pty::{PtyCommand, backend::spawn_in_pty};

        let pty = open_test_pty();

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
        let cmd = PtyCommand::new(&wrapper);
        let mut child = spawn_in_pty(&pty.slave, &cmd).expect("failed to spawn");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let job = foreground_job(pid);
        let process_group_id = job.as_ref().map_or(pid, |job| job.process_group_id);
        let process_group_id =
            i32::try_from(process_group_id).expect("test process group fits pid_t");
        // SAFETY: the process group belongs to this test's PTY child; a negative PID
        // targets that group and the value was checked before conversion.
        unsafe {
            libc::kill(-process_group_id, libc::SIGKILL);
        }
        child.wait().expect("reap the wrapper");

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

    #[test]
    fn proc_stat_parsing_handles_spaces_in_comm() {
        // Verify our /proc/pid/stat parser correctly extracts fields
        // even when (comm) could contain spaces.
        let pid = std::process::id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("test precondition");

        // Our parsing: find last ')' then split the rest
        let close_paren = stat.rfind(')').expect("should have closing paren");
        let rest = &stat[close_paren + 2..];
        let fields: Vec<&str> = rest.split_whitespace().collect();

        // We should have enough fields (at least 6 for tpgid)
        assert!(
            fields.len() >= 6,
            "not enough fields in stat: {}",
            fields.len()
        );

        // Field 0 should be a valid state char (S, R, D, etc.)
        let state = fields[0];
        assert!(
            ["S", "R", "D", "Z", "T", "t", "W", "X", "I"].contains(&state),
            "unexpected state: {state}"
        );

        // Field 5 (tpgid) should parse as i32 (can be -1 if no controlling terminal)
        let tpgid: i32 = fields[5].parse().expect("tpgid should be a number");
        // In CI/test environments without a terminal, tpgid is typically -1
        let _ = tpgid;
    }
}
