use std::collections::HashMap;

use clap::ArgMatches;

use crate::api::schema::{
    Method, OutputMatch, PaneAgentState, PaneCurrentParams, PaneDirection, PaneEdgesParams,
    PaneFocusDirectionParams, PaneInputSetParams, PaneLayoutParams, PaneListParams,
    PaneMoveDestination, PaneMoveParams, PaneNeighborParams, PaneProcessInfoParams, PaneReadParams,
    PaneReleaseAgentParams, PaneRenameParams, PaneReportAgentParams, PaneReportAgentSessionParams,
    PaneReportMetadataParams, PaneResizeParams, PaneRightClickTarget, PaneSendInputParams,
    PaneSendKeysParams, PaneSendTextParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneWaitForOutputParams, PaneZoomMode, PaneZoomParams, ReadFormat, ReadSource, Request,
    SplitDirection,
};

use super::matches::{flag, report_source, required, string, value, values, words};
use super::target::CallerPane;

#[derive(Clone)]
pub(crate) enum Command {
    List {
        workspace: Option<String>,
    },
    Current {
        selector: PaneSelectorArgs,
    },
    Get {
        pane_id: String,
    },
    Layout {
        selector: PaneSelectorArgs,
    },
    ProcessInfo {
        selector: PaneSelectorArgs,
    },
    Neighbor {
        selector: PaneSelectorArgs,
        direction: PaneDirection,
    },
    Edges {
        selector: PaneSelectorArgs,
    },
    Focus {
        selector: PaneSelectorArgs,
        direction: PaneDirection,
    },
    Resize {
        selector: PaneSelectorArgs,
        direction: PaneDirection,
        amount: Option<f32>,
    },
    Zoom {
        selector: PaneSelectorArgs,
        on: bool,
        off: bool,
    },
    Read(PaneReadParams),
    Rename(PaneRenameParams),
    Input {
        selector: PaneSelectorArgs,
        right_click: PaneRightClickTarget,
    },
    Split(SplitArgs),
    Swap(SwapArgs),
    Move(Result<PaneMoveParams, String>),
    Close {
        pane_id: String,
    },
    SendText(PaneSendTextParams),
    SendKeys(PaneSendKeysParams),
    WaitOutput(PaneWaitForOutputParams),
    ReportAgent(Result<PaneReportAgentParams, String>),
    ReportAgentSession(Result<PaneReportAgentSessionParams, String>),
    ReleaseAgent(Result<PaneReleaseAgentParams, String>),
    ReportMetadata(Result<PaneReportMetadataParams, String>),
    Run {
        pane_id: String,
        command: String,
    },
    Invalid,
}

#[derive(Clone)]
pub(crate) struct PaneSelectorArgs {
    pane_id: Option<String>,
    pane: Option<String>,
    current: bool,
}

#[derive(Clone)]
pub(crate) struct SplitArgs {
    selector: PaneSelectorArgs,
    direction: SplitDirection,
    ratio: Option<f32>,
    cwd: Option<String>,
    focus: bool,
    right_click: PaneRightClickTarget,
    env: HashMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct SwapArgs {
    selector: PaneSelectorArgs,
    direction: Option<PaneDirection>,
    source_pane_id: Option<String>,
    target_pane_id: Option<String>,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Current { .. } => "current",
            Self::Get { .. } => "get",
            Self::Layout { .. } => "layout",
            Self::ProcessInfo { .. } => "process-info",
            Self::Neighbor { .. } => "neighbor",
            Self::Edges { .. } => "edges",
            Self::Focus { .. } => "focus",
            Self::Resize { .. } => "resize",
            Self::Zoom { .. } => "zoom",
            Self::Read(_) => "read",
            Self::Rename(_) => "rename",
            Self::Input { .. } => "input",
            Self::Split(_) => "split",
            Self::Swap(_) => "swap",
            Self::Move(_) => "move",
            Self::Close { .. } => "close",
            Self::SendText(_) => "send-text",
            Self::SendKeys(_) => "send-keys",
            Self::WaitOutput(_) => "wait-output",
            Self::ReportAgent(_) => "report-agent",
            Self::ReportAgentSession(_) => "report-agent-session",
            Self::ReleaseAgent(_) => "release-agent",
            Self::ReportMetadata(_) => "report-metadata",
            Self::Run { .. } => "run",
            Self::Invalid => "",
        }
    }

    pub(super) fn is_api_command(&self) -> bool {
        !matches!(self, Self::Invalid)
    }
}

pub(super) fn parse(matches: &ArgMatches) -> Command {
    match matches.subcommand() {
        Some(("list", command)) => Command::List {
            workspace: string(command, "workspace"),
        },
        Some(("current", command)) => Command::Current {
            selector: selector(command),
        },
        Some(("get", command)) => Command::Get {
            pane_id: required(command, "pane_id"),
        },
        Some(("layout", command)) => Command::Layout {
            selector: selector(command),
        },
        Some(("process-info", command)) => Command::ProcessInfo {
            selector: selector(command),
        },
        Some(("neighbor", command)) => Command::Neighbor {
            selector: selector(command),
            direction: direction(command),
        },
        Some(("edges", command)) => Command::Edges {
            selector: selector(command),
        },
        Some(("focus", command)) => Command::Focus {
            selector: selector(command),
            direction: direction(command),
        },
        Some(("resize", command)) => Command::Resize {
            selector: selector(command),
            direction: direction(command),
            amount: value::<f32>(command, "amount"),
        },
        Some(("zoom", command)) => {
            let (selector, on, off) = zoom_args(command);
            Command::Zoom { selector, on, off }
        }
        Some(("read", command)) => Command::Read(read_params(command)),
        Some(("rename", command)) => Command::Rename(PaneRenameParams {
            pane_id: required(command, "pane_id"),
            label: (!flag(command, "clear")).then(|| words(command, "label")),
        }),
        Some(("input", command)) => {
            let (selector, right_click) = input_args(command);
            Command::Input {
                selector,
                right_click,
            }
        }
        Some(("split", command)) => Command::Split(split_args(command)),
        Some(("swap", command)) => Command::Swap(swap_args(command)),
        Some(("move", command)) => Command::Move(move_params(command)),
        Some(("close", command)) => Command::Close {
            pane_id: required(command, "pane_id"),
        },
        Some(("send-text", command)) => Command::SendText(PaneSendTextParams {
            pane_id: required(command, "pane_id"),
            text: words(command, "text"),
        }),
        Some(("send-keys", command)) => Command::SendKeys(PaneSendKeysParams {
            pane_id: required(command, "pane_id"),
            keys: values::<String>(command, "key"),
        }),
        Some(("wait-output", command)) => Command::WaitOutput(wait_output_params(command)),
        Some(("report-agent", command)) => Command::ReportAgent(report_agent_params(command)),
        Some(("report-agent-session", command)) => {
            Command::ReportAgentSession(report_agent_session_params(command))
        }
        Some(("release-agent", command)) => Command::ReleaseAgent(release_agent_params(command)),
        Some(("report-metadata", command)) => {
            Command::ReportMetadata(report_metadata_params(command))
        }
        Some(("run", command)) => Command::Run {
            pane_id: required(command, "pane_id"),
            command: words(command, "command"),
        },
        _ => Command::Invalid,
    }
}

pub(super) fn run_pane_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    let caller = super::target::caller_pane(paths);
    match command {
        Command::List { workspace } => print_request(
            paths,
            "cli:pane:list",
            Method::PaneList(PaneListParams {
                workspace_id: workspace,
            }),
        ),
        Command::Current { selector } => match selected_pane(&selector, &caller) {
            Ok(caller_pane_id) => print_request(
                paths,
                "cli:pane:current",
                Method::PaneCurrent(PaneCurrentParams { caller_pane_id }),
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Get { pane_id } => print_request(
            paths,
            "cli:pane:get",
            Method::PaneGet(PaneTarget { pane_id }),
        ),
        Command::Layout { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => print_request(
                paths,
                "cli:pane:layout",
                Method::PaneLayout(PaneLayoutParams { pane_id }),
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ProcessInfo { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => print_request(
                paths,
                "cli:pane:process_info",
                Method::PaneProcessInfo(PaneProcessInfoParams { pane_id }),
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Neighbor {
            selector,
            direction,
        } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => print_request(
                paths,
                "cli:pane:neighbor",
                Method::PaneNeighbor(PaneNeighborParams { pane_id, direction }),
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Edges { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => print_request(
                paths,
                "cli:pane:edges",
                Method::PaneEdges(PaneEdgesParams { pane_id }),
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Focus {
            selector,
            direction,
        } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => {
                super::runtime::pane_focus(paths, PaneFocusDirectionParams { pane_id, direction })
            }
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Resize {
            selector,
            direction,
            amount,
        } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => super::runtime::pane_resize(
                paths,
                PaneResizeParams {
                    pane_id,
                    direction,
                    amount,
                },
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Zoom { selector, on, off } => match zoom_params(&selector, on, off, &caller) {
            Ok(params) => super::runtime::pane_zoom(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Read(params) => {
            let response = super::send_request(
                paths,
                &Request {
                    id: "cli:pane:read".into(),
                    method: Method::PaneRead(params),
                },
            )?;
            super::print_read_response(&response)
        }
        Command::Rename(params) => super::runtime::pane_rename(paths, params),
        Command::Input {
            selector,
            right_click,
        } => match input_params(&selector, right_click, &caller) {
            Ok(params) => super::runtime::pane_input_set(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Split(args) => match split_params(args, &caller, paths) {
            Ok(params) => super::runtime::pane_split(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Swap(args) => match swap_params(args, &caller) {
            Ok(params) => super::runtime::pane_swap(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Move(result) => match result {
            Ok(params) => super::runtime::pane_move(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Close { pane_id } => super::runtime::pane_close(paths, pane_id),
        Command::SendText(params) => super::send_ok_request(paths, Method::PaneSendText(params)),
        Command::SendKeys(params) => super::send_ok_request(paths, Method::PaneSendKeys(params)),
        Command::WaitOutput(params) => print_request(
            paths,
            "cli:pane:wait-output",
            Method::PaneWaitForOutput(params),
        ),
        Command::ReportAgent(result) => match result {
            Ok(params) => super::send_ok_request(paths, Method::PaneReportAgent(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ReportAgentSession(result) => match result {
            Ok(params) => super::send_ok_request(paths, Method::PaneReportAgentSession(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ReleaseAgent(result) => match result {
            Ok(params) => super::send_ok_request(paths, Method::PaneReleaseAgent(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ReportMetadata(result) => match result {
            Ok(params) => super::send_ok_request(paths, Method::PaneReportMetadata(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Run { pane_id, command } => super::send_ok_request(
            paths,
            Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text: command,
                keys: vec!["Enter".into()],
            }),
        ),
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

fn print_request(
    paths: &super::target::CliContext,
    id: &'static str,
    method: Method,
) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: id.into(),
            method,
        },
    )?)
}

/// The pane a pane command acts on. One rule for every command that takes
/// `--pane`/`--current` (the spec makes the selectors mutually exclusive):
///
/// - an explicit id, positional or `--pane`, is that pane;
/// - `--current` is the calling pane (`SHEPR_PANE_ID`), and an error when the
///   caller is not in a pane of the targeted server;
/// - no selector is the calling pane when there is one, and otherwise unset,
///   which the server resolves to its focused pane.
///
/// So an agent in a background pane that runs `pane resize` resizes its own
/// pane, not whichever pane the user happens to have focused.
fn selector(matches: &ArgMatches) -> PaneSelectorArgs {
    PaneSelectorArgs {
        pane_id: string(matches, "pane_id"),
        pane: string(matches, "pane"),
        current: flag(matches, "current"),
    }
}

fn split_args(matches: &ArgMatches) -> SplitArgs {
    SplitArgs {
        selector: selector(matches),
        direction: value::<SplitDirection>(matches, "direction").unwrap_or(SplitDirection::Right),
        ratio: value::<f32>(matches, "ratio"),
        cwd: string(matches, "cwd"),
        focus: flag(matches, "focus"),
        right_click: value::<PaneRightClickTarget>(matches, "right-click")
            .unwrap_or(PaneRightClickTarget::Shepr),
        env: values::<(String, String)>(matches, "env")
            .into_iter()
            .collect(),
    }
}

fn swap_args(matches: &ArgMatches) -> SwapArgs {
    SwapArgs {
        selector: selector(matches),
        direction: value::<PaneDirection>(matches, "direction"),
        source_pane_id: string(matches, "source-pane"),
        target_pane_id: string(matches, "target-pane"),
    }
}

fn zoom_args(matches: &ArgMatches) -> (PaneSelectorArgs, bool, bool) {
    (selector(matches), flag(matches, "on"), flag(matches, "off"))
}

fn input_args(matches: &ArgMatches) -> (PaneSelectorArgs, PaneRightClickTarget) {
    (
        selector(matches),
        value::<PaneRightClickTarget>(matches, "right-click")
            .unwrap_or(PaneRightClickTarget::Shepr),
    )
}

fn selected_pane(
    selector: &PaneSelectorArgs,
    caller: &CallerPane,
) -> Result<Option<String>, String> {
    if let Some(pane_id) = explicit_pane(selector) {
        return Ok(Some(pane_id));
    }
    if selector.current {
        return caller.require().map(Some);
    }
    Ok(caller.id())
}

fn explicit_pane(selector: &PaneSelectorArgs) -> Option<String> {
    selector.pane_id.clone().or_else(|| selector.pane.clone())
}

fn direction(matches: &ArgMatches) -> PaneDirection {
    // Required by the spec for every command that calls this.
    value::<PaneDirection>(matches, "direction").unwrap_or(PaneDirection::Right)
}

fn zoom_params(
    selector: &PaneSelectorArgs,
    on: bool,
    off: bool,
    caller: &CallerPane,
) -> Result<PaneZoomParams, String> {
    let mode = if on {
        PaneZoomMode::On
    } else if off {
        PaneZoomMode::Off
    } else {
        PaneZoomMode::Toggle
    };
    Ok(PaneZoomParams {
        pane_id: selected_pane(selector, caller)?,
        mode,
    })
}

fn read_params(matches: &ArgMatches) -> PaneReadParams {
    // `--ansi` and `--format ansi` keep escapes through the ANSI renderer.
    // There is no `--raw`: no raw PTY byte history exists to return, so it
    // could only ever repeat `--ansi`.
    let format = if flag(matches, "ansi") {
        ReadFormat::Ansi
    } else {
        value::<ReadFormat>(matches, "format").unwrap_or(ReadFormat::Text)
    };
    PaneReadParams {
        pane_id: required(matches, "pane_id"),
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        format,
        // Same params as `agent read`: an ANSI read keeps its escapes.
        strip_ansi: format != ReadFormat::Ansi,
        intent: crate::api::schema::ReadIntent::Interactive,
    }
}

fn input_params(
    selector: &PaneSelectorArgs,
    right_click: PaneRightClickTarget,
    caller: &CallerPane,
) -> Result<PaneInputSetParams, String> {
    // The spec requires exactly one of the positional, `--pane` or `--current`,
    // so the no-selector fallback never applies here.
    Ok(PaneInputSetParams {
        pane_id: selected_pane(selector, caller)?.unwrap_or_default(),
        right_click,
    })
}

fn split_params(
    args: SplitArgs,
    caller: &CallerPane,
    paths: &super::target::CliContext,
) -> Result<PaneSplitParams, String> {
    let cwd = args
        .cwd
        .map(|raw| {
            super::matches::resolve_cwd(
                &raw,
                paths.is_remote(),
                paths.home_dir(),
                paths.current_dir(),
            )
        })
        .transpose()?;
    Ok(PaneSplitParams {
        workspace_id: None,
        target_pane_id: selected_pane(&args.selector, caller)?,
        direction: args.direction,
        ratio: args.ratio,
        cwd,
        focus: args.focus,
        right_click: args.right_click,
        env: args.env,
    })
}

fn swap_params(args: SwapArgs, caller: &CallerPane) -> Result<PaneSwapParams, String> {
    const USAGE: &str = "usage: shepr pane swap --direction left|right|up|down [--pane ID|--current]\n       shepr pane swap --source-pane ID --target-pane ID";

    let direction = args.direction;
    let source_pane_id = args.source_pane_id;
    let target_pane_id = args.target_pane_id;
    // `--pane`/`--current` only belong to the directional form.
    let selector_given = explicit_pane(&args.selector).is_some() || args.selector.current;
    match (direction, source_pane_id, target_pane_id) {
        (Some(direction), None, None) => Ok(PaneSwapParams {
            pane_id: selected_pane(&args.selector, caller)?,
            direction: Some(direction),
            ..PaneSwapParams::default()
        }),
        (None, Some(source_pane_id), Some(target_pane_id)) if !selector_given => {
            Ok(PaneSwapParams {
                source_pane_id: Some(source_pane_id),
                target_pane_id: Some(target_pane_id),
                ..PaneSwapParams::default()
            })
        }
        _ => Err(USAGE.into()),
    }
}

fn move_params(matches: &ArgMatches) -> Result<PaneMoveParams, String> {
    const USAGE: &str = "usage: shepr pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       shepr pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       shepr pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]";

    let split = value::<SplitDirection>(matches, "split");
    let target_pane_id = string(matches, "target-pane");
    let ratio = value::<f32>(matches, "ratio");
    let workspace_id = string(matches, "workspace");
    let label = string(matches, "label");
    let tab_label = string(matches, "tab-label");

    // The spec requires exactly one destination; each destination accepts
    // only its own options.
    let destination = if let Some(tab_id) = string(matches, "tab") {
        let Some(split) = split else {
            return Err(USAGE.into());
        };
        if workspace_id.is_some() || label.is_some() || tab_label.is_some() {
            return Err(USAGE.into());
        }
        PaneMoveDestination::Tab {
            tab_id,
            target_pane_id,
            split,
            ratio,
        }
    } else if flag(matches, "new-tab") {
        if split.is_some() || target_pane_id.is_some() || tab_label.is_some() {
            return Err(USAGE.into());
        }
        PaneMoveDestination::NewTab {
            workspace_id,
            label,
        }
    } else {
        if split.is_some() || target_pane_id.is_some() || workspace_id.is_some() {
            return Err(USAGE.into());
        }
        PaneMoveDestination::NewWorkspace { label, tab_label }
    };

    Ok(PaneMoveParams {
        pane_id: required(matches, "pane_id"),
        destination,
        focus: !flag(matches, "no-focus"),
    })
}

fn wait_output_params(matches: &ArgMatches) -> PaneWaitForOutputParams {
    // The spec requires exactly one of `--match` and `--regex`.
    let matcher = match string(matches, "regex") {
        Some(value) => OutputMatch::Regex { value },
        None => OutputMatch::Substring {
            value: required(matches, "match"),
        },
    };
    PaneWaitForOutputParams {
        pane_id: required(matches, "pane_id"),
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        r#match: matcher,
        timeout_ms: value::<u64>(matches, "timeout"),
        strip_ansi: !flag(matches, "raw"),
    }
}

fn report_agent_params(matches: &ArgMatches) -> Result<PaneReportAgentParams, String> {
    Ok(PaneReportAgentParams {
        pane_id: required(matches, "pane_id"),
        source: report_source(matches).ok_or("missing required --source")?,
        agent: required(matches, "agent"),
        state: value::<PaneAgentState>(matches, "state").ok_or("missing required --state")?,
        message: string(matches, "message"),
        seq: value::<u64>(matches, "seq"),
        agent_session_id: string(matches, "agent-session-id"),
        agent_session_path: string(matches, "agent-session-path"),
    })
}

fn report_agent_session_params(
    matches: &ArgMatches,
) -> Result<PaneReportAgentSessionParams, String> {
    Ok(PaneReportAgentSessionParams {
        pane_id: required(matches, "pane_id"),
        source: report_source(matches).ok_or("missing required --source")?,
        agent: required(matches, "agent"),
        seq: value::<u64>(matches, "seq"),
        agent_session_id: string(matches, "agent-session-id"),
        agent_session_path: string(matches, "agent-session-path"),
        session_start_source: string(matches, "session-start-source"),
    })
}

fn release_agent_params(matches: &ArgMatches) -> Result<PaneReleaseAgentParams, String> {
    Ok(PaneReleaseAgentParams {
        pane_id: required(matches, "pane_id"),
        source: report_source(matches).ok_or("missing required --source")?,
        agent: required(matches, "agent"),
        seq: value::<u64>(matches, "seq"),
    })
}

fn report_metadata_params(matches: &ArgMatches) -> Result<PaneReportMetadataParams, String> {
    // Setting and clearing the same field conflict in the spec, and at least
    // one field is required there.
    let source = report_source(matches).ok_or("missing required --source")?;
    let applies_to_source = string(matches, "applies-to-source");
    if applies_to_source
        .as_deref()
        .is_some_and(|source| source.trim().is_empty())
    {
        return Err("missing value for --applies-to-source".into());
    }
    Ok(PaneReportMetadataParams {
        pane_id: required(matches, "pane_id"),
        source,
        agent: string(matches, "agent"),
        applies_to_source,
        title: string(matches, "title"),
        display_agent: string(matches, "display-agent"),
        state_labels: values::<(String, String)>(matches, "state-label")
            .into_iter()
            .collect::<HashMap<_, _>>(),
        tokens: super::matches::metadata_tokens(matches),
        clear_title: flag(matches, "clear-title"),
        clear_display_agent: flag(matches, "clear-display-agent"),
        clear_state_labels: flag(matches, "clear-state-labels"),
        seq: value::<u64>(matches, "seq"),
        ttl_ms: value::<u64>(matches, "ttl-ms"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::command_matches;
    use super::*;

    fn pane(args: &[&str]) -> ArgMatches {
        let mut argv = vec!["pane"];
        argv.extend_from_slice(args);
        command_matches(&argv)
    }

    fn rejected(args: &[&str]) -> bool {
        let mut argv = vec!["shepr".to_string(), "pane".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        super::super::spec::command()
            .try_get_matches_from(&argv)
            .is_err()
    }

    fn known(pane_id: &str) -> CallerPane {
        CallerPane::Known(pane_id.into())
    }

    fn selected_pane(matches: &ArgMatches, caller: &CallerPane) -> Result<Option<String>, String> {
        super::selected_pane(&super::selector(matches), caller)
    }

    fn split_params(
        matches: &ArgMatches,
        caller: &CallerPane,
        paths: &super::super::target::CliContext,
    ) -> Result<PaneSplitParams, String> {
        super::split_params(super::split_args(matches), caller, paths)
    }

    fn swap_params(matches: &ArgMatches, caller: &CallerPane) -> Result<PaneSwapParams, String> {
        super::swap_params(super::swap_args(matches), caller)
    }

    fn input_params(
        matches: &ArgMatches,
        caller: &CallerPane,
    ) -> Result<PaneInputSetParams, String> {
        let (selector, right_click) = super::input_args(matches);
        super::input_params(&selector, right_click, caller)
    }

    fn zoom_params(matches: &ArgMatches, caller: &CallerPane) -> Result<PaneZoomParams, String> {
        let (selector, on, off) = super::zoom_args(matches);
        super::zoom_params(&selector, on, off, caller)
    }

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(crate::config::AppPaths::test_with_context(
            std::path::Path::new("/tmp/shepr-cli-paths"),
            Some(std::path::Path::new("/home/me")),
            Some(std::path::Path::new("/home/me/proj")),
        ))
    }

    const OUTSIDE: CallerPane = CallerPane::Unset;

    #[test]
    fn split_accepts_ratio() {
        let params = split_params(
            &pane(&[
                "split",
                "issue-1",
                "--direction",
                "right",
                "--ratio",
                "0.333",
            ]),
            &OUTSIDE,
            &test_paths(),
        )
        .expect("test precondition");

        assert_eq!(params.target_pane_id, Some("issue-1".into()));
        assert_eq!(params.direction, SplitDirection::Right);
        assert_eq!(params.ratio, Some(0.333));
        assert_eq!(params.right_click, PaneRightClickTarget::Shepr);
    }

    #[test]
    fn split_accepts_right_click_target_in_both_forms() {
        for form in [&["--right-click", "pane"][..], &["--right-click=pane"]] {
            let mut args = vec!["split", "--direction", "right"];
            args.extend_from_slice(form);
            let params =
                split_params(&pane(&args), &OUTSIDE, &test_paths()).expect("test precondition");
            assert_eq!(params.right_click, PaneRightClickTarget::Pane);
        }
    }

    #[test]
    fn split_accepts_equals_forms() {
        let params = split_params(
            &pane(&[
                "split",
                "--direction=down",
                "--cwd=/srv",
                "--ratio=0.25",
                "--env=A=b",
            ]),
            &OUTSIDE,
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.direction, SplitDirection::Down);
        assert_eq!(params.cwd.as_deref(), Some("/srv"));
        assert_eq!(params.ratio, Some(0.25));
        assert_eq!(params.env.get("A").map(String::as_str), Some("b"));
    }

    #[test]
    fn split_resolves_relative_cwd_against_the_caller() {
        let params = split_params(
            &pane(&["split", "--direction", "down", "--cwd", "."]),
            &OUTSIDE,
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/home/me/proj"));
    }

    #[test]
    fn split_current_uses_calling_pane_and_requires_it() {
        let params = split_params(
            &pane(&["split", "--direction", "down", "--current"]),
            &known("issue-1"),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.target_pane_id, Some("issue-1".into()));
        assert_eq!(params.direction, SplitDirection::Down);

        assert!(
            split_params(
                &pane(&["split", "--direction", "down", "--current"]),
                &OUTSIDE,
                &test_paths(),
            )
            .is_err()
        );
    }

    #[test]
    fn split_omitted_target_uses_caller() {
        let params = split_params(
            &pane(&[
                "split",
                "--no-focus",
                "--direction",
                "right",
                "--cwd",
                "/var/tmp",
            ]),
            &known("w1:p2"),
            &test_paths(),
        )
        .expect("test precondition");

        assert_eq!(params.target_pane_id, Some("w1:p2".into()));
        assert!(!params.focus);
    }

    #[test]
    fn split_without_caller_keeps_focused_fallback() {
        for caller in [
            CallerPane::Unset,
            CallerPane::OtherServer,
            CallerPane::Remote,
        ] {
            let params = split_params(
                &pane(&["split", "--direction", "down"]),
                &caller,
                &test_paths(),
            )
            .expect("test precondition");
            assert_eq!(params.target_pane_id, None);
        }
    }

    #[test]
    fn split_explicit_target_overrides_caller() {
        for target in [&["w2:p3"][..], &["--pane", "w2:p3"]] {
            let mut args = vec!["split"];
            args.extend_from_slice(target);
            args.extend_from_slice(&["--direction", "right"]);
            let params = split_params(&pane(&args), &known("w1:p2"), &test_paths())
                .expect("test precondition");

            assert_eq!(params.target_pane_id, Some("w2:p3".into()));
        }
    }

    #[test]
    fn split_requires_direction_and_rejects_bad_values() {
        assert!(rejected(&["split"]));
        assert!(rejected(&["split", "--direction", "left"]));
        assert!(rejected(&[
            "split",
            "--direction",
            "down",
            "--ratio",
            "inf"
        ]));
        assert!(rejected(&[
            "split",
            "p1",
            "--pane",
            "p2",
            "--direction",
            "down"
        ]));
    }

    #[test]
    fn input_requires_and_uses_calling_pane() {
        let params = input_params(
            &pane(&["input", "--current", "--right-click", "pane"]),
            &known("issue-1:p1"),
        )
        .expect("test precondition");

        assert_eq!(params.pane_id, "issue-1:p1");
        assert_eq!(params.right_click, PaneRightClickTarget::Pane);
        assert!(
            input_params(
                &pane(&["input", "--current", "--right-click", "pane"]),
                &OUTSIDE
            )
            .is_err()
        );
    }

    #[test]
    fn input_rejects_conflicting_or_missing_selectors() {
        assert!(rejected(&[
            "input",
            "pane-a",
            "--pane",
            "pane-b",
            "--right-click",
            "pane"
        ]));
        assert!(rejected(&[
            "input",
            "--pane",
            "pane-a",
            "--current",
            "--right-click",
            "pane"
        ]));
        assert!(rejected(&["input", "--right-click", "pane"]));
        assert!(rejected(&["input", "pane-a"]));
    }

    /// Every command with a pane selector, with the arguments it needs besides it.
    const SELECTOR_COMMANDS: &[&[&str]] = &[
        &["current"],
        &["layout"],
        &["process-info"],
        &["edges"],
        &["neighbor", "--direction", "down"],
        &["focus", "--direction", "left"],
        &["resize", "--direction", "up"],
        &["zoom"],
        &["split", "--direction", "right"],
        &["swap", "--direction", "right"],
    ];

    #[test]
    fn every_pane_command_resolves_its_pane_the_same_way() {
        for command in SELECTOR_COMMANDS {
            let with = |extra: &[&str]| {
                let mut args = command.to_vec();
                args.extend_from_slice(extra);
                pane(&args)
            };

            // `--current` is the caller's pane, and an error without one.
            assert_eq!(
                selected_pane(&with(&["--current"]), &known("w1:p2")),
                Ok(Some("w1:p2".into())),
                "{command:?}"
            );
            for caller in [
                CallerPane::Unset,
                CallerPane::OtherServer,
                CallerPane::Remote,
            ] {
                assert!(
                    selected_pane(&with(&["--current"]), &caller).is_err(),
                    "{command:?} {caller:?}"
                );
            }

            // No selector: the caller's pane when known, else the server's
            // focused pane.
            assert_eq!(
                selected_pane(&with(&[]), &known("w1:p2")),
                Ok(Some("w1:p2".into())),
                "{command:?}"
            );
            assert_eq!(
                selected_pane(&with(&[]), &CallerPane::OtherServer),
                Ok(None),
                "{command:?}"
            );

            // An explicit pane always wins.
            assert_eq!(
                selected_pane(&with(&["--pane", "w9:p9"]), &known("w1:p2")),
                Ok(Some("w9:p9".into())),
                "{command:?}"
            );
            let mut both = command.to_vec();
            both.extend_from_slice(&["--pane", "a", "--current"]);
            assert!(rejected(&both), "{command:?}");
        }
    }

    #[test]
    fn swap_accepts_directional_current() {
        let params = swap_params(&pane(&["swap", "--direction", "right"]), &OUTSIDE)
            .expect("test precondition");

        assert_eq!(params.pane_id, None);
        assert_eq!(params.direction, Some(PaneDirection::Right));
        assert_eq!(params.source_pane_id, None);
        assert_eq!(params.target_pane_id, None);

        let params = swap_params(&pane(&["swap", "--direction", "right"]), &known("w1:p2"))
            .expect("test precondition");
        assert_eq!(params.pane_id, Some("w1:p2".into()));
    }

    #[test]
    fn swap_accepts_explicit_source_and_target() {
        let params = swap_params(
            &pane(&[
                "swap",
                "--source-pane",
                "issue-1",
                "--target-pane",
                "issue-2",
            ]),
            &known("w1:p2"),
        )
        .expect("test precondition");

        assert_eq!(params.pane_id, None);
        assert_eq!(params.direction, None);
        assert_eq!(params.source_pane_id, Some("issue-1".into()));
        assert_eq!(params.target_pane_id, Some("issue-2".into()));
    }

    #[test]
    fn swap_rejects_mixed_forms() {
        let err = swap_params(
            &pane(&[
                "swap",
                "--direction",
                "left",
                "--source-pane",
                "issue-1",
                "--target-pane",
                "issue-2",
            ]),
            &OUTSIDE,
        )
        .expect_err("test precondition");

        assert!(err.contains("usage: shepr pane swap"));

        // A pane selector belongs to the directional form only, even when the
        // caller's pane is known.
        for selector in [&["--current"][..], &["--pane", "issue-3"]] {
            let mut args = vec![
                "swap",
                "--source-pane",
                "issue-1",
                "--target-pane",
                "issue-2",
            ];
            args.extend_from_slice(selector);
            assert!(
                swap_params(&pane(&args), &known("w1:p2")).is_err(),
                "{selector:?}"
            );
        }
    }

    #[test]
    fn move_accepts_existing_tab_destination() {
        let params = move_params(&pane(&[
            "move",
            "issue-1",
            "--tab",
            "issue:2",
            "--split",
            "right",
            "--target-pane",
            "issue-3",
            "--ratio",
            "0.25",
            "--no-focus",
        ]))
        .expect("test precondition");

        assert_eq!(params.pane_id, "issue-1");
        assert!(!params.focus);
        assert_eq!(
            params.destination,
            PaneMoveDestination::Tab {
                tab_id: "issue:2".into(),
                target_pane_id: Some("issue-3".into()),
                split: SplitDirection::Right,
                ratio: Some(0.25),
            }
        );
    }

    #[test]
    fn move_rejects_target_pane_without_tab() {
        assert!(rejected(&["move", "issue-1", "--target-pane", "issue-2"]));
        let err = move_params(&pane(&[
            "move",
            "issue-1",
            "--new-tab",
            "--target-pane",
            "issue-2",
        ]))
        .expect_err("test precondition");

        assert!(err.contains("usage: shepr pane move"));
    }

    #[test]
    fn move_rejects_non_finite_ratio_and_two_destinations() {
        assert!(rejected(&[
            "move", "issue-1", "--tab", "issue:2", "--split", "right", "--ratio", "NaN",
        ]));
        assert!(rejected(&[
            "move",
            "issue-1",
            "--new-tab",
            "--new-workspace"
        ]));
    }

    #[test]
    fn zoom_defaults_to_toggle_outside_a_pane() {
        let params = zoom_params(&pane(&["zoom"]), &OUTSIDE).expect("test precondition");

        assert_eq!(params.pane_id, None);
        assert_eq!(params.mode, PaneZoomMode::Toggle);
    }

    #[test]
    fn zoom_accepts_positional_or_option_pane_and_one_mode() {
        let params = zoom_params(&pane(&["zoom", "issue-1", "--on"]), &known("w1:p2"))
            .expect("test precondition");
        assert_eq!(params.pane_id, Some("issue-1".into()));
        assert_eq!(params.mode, PaneZoomMode::On);

        let params = zoom_params(&pane(&["zoom", "--pane", "issue-2", "--off"]), &OUTSIDE)
            .expect("test precondition");
        assert_eq!(params.pane_id, Some("issue-2".into()));
        assert_eq!(params.mode, PaneZoomMode::Off);

        assert!(rejected(&["zoom", "--on", "--off"]));
        assert!(rejected(&["zoom", "issue-1", "--current"]));
    }

    #[test]
    fn directional_commands_read_direction_and_amount() {
        let neighbor = pane(&["neighbor", "--direction", "down", "--current"]);
        assert_eq!(direction(&neighbor), PaneDirection::Down);

        let resize = pane(&[
            "resize",
            "--pane",
            "issue-2",
            "--direction",
            "left",
            "--amount",
            "0.125",
        ]);
        assert_eq!(
            selected_pane(&resize, &known("w1:p2")),
            Ok(Some("issue-2".into()))
        );
        assert_eq!(direction(&resize), PaneDirection::Left);
        assert_eq!(value::<f32>(&resize, "amount"), Some(0.125));

        assert!(rejected(&["focus"]));
        assert!(rejected(&["neighbor", "--direction", "sideways"]));
    }

    #[test]
    fn read_defaults_with_bare_pane_id() {
        let params = read_params(&pane(&["read", "issue-1"]));

        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(params.source, ReadSource::Recent);
        assert_eq!(params.lines, None);
        assert_eq!(params.format, ReadFormat::Text);
        assert!(params.strip_ansi);
    }

    #[test]
    fn read_accepts_space_separated_and_reordered_equals_options() {
        let params = read_params(&pane(&[
            "read", "issue-1", "--source", "visible", "--lines", "5", "--ansi",
        ]));
        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(params.source, ReadSource::Visible);
        assert_eq!(params.lines, Some(5));
        assert_eq!(params.format, ReadFormat::Ansi);
        assert!(!params.strip_ansi);

        let params = read_params(&pane(&["read", "--source=visible", "--lines=5", "issue-1"]));
        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(params.source, ReadSource::Visible);
        assert_eq!(params.lines, Some(5));

        // `--raw` only ever repeated `--ansi`; it is gone.
        assert!(rejected(&["read", "issue-1", "--raw"]));
        assert!(rejected(&["read", "issue-1", "--lines", "-3"]));
        assert!(rejected(&["read", "issue-1", "--source", "nowhere"]));
    }

    #[test]
    fn wait_output_accepts_space_separated_options() {
        let params = wait_output_params(&pane(&[
            "wait-output",
            "issue-1",
            "--match",
            "ready",
            "--timeout",
            "5000",
        ]));

        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(
            params.r#match,
            OutputMatch::Substring {
                value: "ready".into()
            }
        );
        assert_eq!(params.timeout_ms, Some(5000));
        assert_eq!(params.source, ReadSource::Recent);
        assert!(params.strip_ansi);
    }

    #[test]
    fn wait_output_accepts_reordered_equals_and_hyphenated_patterns() {
        let params = wait_output_params(&pane(&[
            "wait-output",
            "--match=a=b",
            "--timeout=100",
            "issue-1",
        ]));
        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(
            params.r#match,
            OutputMatch::Substring {
                value: "a=b".into()
            }
        );
        assert_eq!(params.timeout_ms, Some(100));

        let params = wait_output_params(&pane(&["wait-output", "issue-1", "--regex", "-{3} done"]));
        assert_eq!(
            params.r#match,
            OutputMatch::Regex {
                value: "-{3} done".into()
            }
        );
    }

    #[test]
    fn wait_output_requires_exactly_one_matcher() {
        assert!(rejected(&["wait-output", "issue-1"]));
        assert!(rejected(&[
            "wait-output",
            "issue-1",
            "--match",
            "a",
            "--regex",
            "b"
        ]));
        assert!(rejected(&[
            "wait-output",
            "issue-1",
            "--match",
            "a",
            "--source",
            "detection"
        ]));
    }

    #[test]
    fn report_agent_reads_every_option_and_trims_source() {
        let params = report_agent_params(&pane(&[
            "report-agent",
            "p1",
            "--source= hook ",
            "--agent",
            "claude",
            "--state",
            "working",
            "--message",
            "-- compiling",
            "--seq=9",
            "--agent-session-id",
            "abc",
            "--agent-session-path=/tmp/s.jsonl",
        ]))
        .expect("test precondition");
        assert_eq!(params.source, "hook");
        assert_eq!(params.state, PaneAgentState::Working);
        assert_eq!(params.message.as_deref(), Some("-- compiling"));
        assert_eq!(params.seq, Some(9));
        assert_eq!(params.agent_session_path.as_deref(), Some("/tmp/s.jsonl"));

        assert!(
            report_agent_params(&pane(&[
                "report-agent",
                "p1",
                "--source",
                " ",
                "--agent",
                "a",
                "--state",
                "idle",
            ]))
            .is_err()
        );
        assert!(rejected(&[
            "report-agent",
            "p1",
            "--source",
            "s",
            "--agent",
            "a"
        ]));
    }

    #[test]
    fn report_agent_session_and_release_read_their_options() {
        let params = report_agent_session_params(&pane(&[
            "report-agent-session",
            "p1",
            "--source",
            "hook",
            "--agent",
            "codex",
            "--session-start-source=resume",
        ]))
        .expect("test precondition");
        assert_eq!(params.session_start_source.as_deref(), Some("resume"));

        let params = release_agent_params(&pane(&[
            "release-agent",
            "p1",
            "--source",
            "hook",
            "--agent",
            "codex",
            "--seq",
            "3",
        ]))
        .expect("test precondition");
        assert_eq!(params.seq, Some(3));
        assert!(rejected(&[
            "release-agent",
            "p1",
            "--source",
            "hook",
            "--agent",
            "a",
            "--seq",
            "x"
        ]));
    }

    #[test]
    fn report_metadata_collects_fields_and_rejects_conflicts() {
        let params = report_metadata_params(&pane(&[
            "report-metadata",
            "p1",
            "--source",
            "hook",
            "--title",
            "build",
            "--state-label",
            "Working=compiling",
            "--state-label=idle=ready",
            "--token",
            "a=1",
            "--clear-token",
            "b",
            "--ttl-ms",
            "50",
        ]))
        .expect("test precondition");
        assert_eq!(params.title.as_deref(), Some("build"));
        assert_eq!(
            params.state_labels.get("working").map(String::as_str),
            Some("compiling")
        );
        assert_eq!(
            params.state_labels.get("idle").map(String::as_str),
            Some("ready")
        );
        assert_eq!(params.tokens.get("a"), Some(&Some("1".to_string())));
        assert_eq!(params.tokens.get("b"), Some(&None));
        assert_eq!(params.ttl_ms, Some(50));

        assert!(rejected(&["report-metadata", "p1", "--source", "s"]));
        assert!(rejected(&[
            "report-metadata",
            "p1",
            "--source",
            "s",
            "--title",
            "t",
            "--clear-title",
        ]));
        assert!(rejected(&[
            "report-metadata",
            "p1",
            "--source",
            "s",
            "--state-label",
            "nope",
        ]));
        assert!(
            report_metadata_params(&pane(&[
                "report-metadata",
                "p1",
                "--source",
                "s",
                "--applies-to-source",
                " ",
                "--clear-title",
            ]))
            .is_err()
        );
    }

    #[test]
    fn rename_joins_words_or_clears() {
        let rename = pane(&["rename", "p1", "build", "--clear", "logs"]);
        assert!(!flag(&rename, "clear"));
        assert_eq!(words(&rename, "label"), "build --clear logs");

        let rename = pane(&["rename", "p1", "--clear"]);
        assert!(flag(&rename, "clear"));
        assert!(rejected(&["rename", "p1"]));
    }

    #[test]
    fn run_and_send_text_keep_their_words() {
        let run = pane(&["run", "p1", "ls", "-la", "--session", "work"]);
        assert_eq!(words(&run, "command"), "ls -la --session work");

        let run = pane(&["run", "p1", "--", "-x"]);
        assert_eq!(words(&run, "command"), "-x");

        let text = pane(&["send-text", "p1", "echo", "--remote", "x"]);
        assert_eq!(words(&text, "text"), "echo --remote x");
    }
}
