use std::collections::HashMap;

use clap::ArgMatches;

use shepr_api::schema::{
    Method, PaneAgentState, PaneCurrentParams, PaneDirection, PaneEdgesParams,
    PaneFocusDirectionParams, PaneLayoutParams, PaneListParams, PaneMoveDestination,
    PaneMoveParams, PaneNeighborParams, PaneProcessInfoParams, PaneReadParams, PaneRenameParams,
    PaneReportAgentParams, PaneReportAgentSessionParams, PaneResizeParams, PaneRightClickTarget,
    PaneSplitParams, PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, ReadFormat,
    ReadSource, Request, SplitDirection,
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
    Split(SplitArgs),
    Swap(SwapArgs),
    Move(Result<PaneMoveParams, String>),
    Close {
        pane_id: String,
    },
    ReportAgent(Result<PaneReportAgentParams, String>),
    ReportAgentSession(Result<PaneReportAgentSessionParams, String>),
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
            Self::Split(_) => "split",
            Self::Swap(_) => "swap",
            Self::Move(_) => "move",
            Self::Close { .. } => "close",
            Self::ReportAgent(_) => "report-agent",
            Self::ReportAgentSession(_) => "report-agent-session",
        }
    }

    pub(super) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::List { .. }
            | Self::Current { .. }
            | Self::Get { .. }
            | Self::Layout { .. }
            | Self::ProcessInfo { .. }
            | Self::Neighbor { .. }
            | Self::Edges { .. }
            | Self::Focus { .. }
            | Self::Resize { .. }
            | Self::Zoom { .. }
            | Self::Read(_)
            | Self::Rename(_)
            | Self::Split(_)
            | Self::Swap(_)
            | Self::Move(_)
            | Self::Close { .. }
            | Self::ReportAgent(_)
            | Self::ReportAgentSession(_) => true,
        }
    }
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("list", command)) => Some(Command::List {
            workspace: string(command, "workspace"),
        }),
        Some(("current", command)) => Some(Command::Current {
            selector: selector(command),
        }),
        Some(("get", command)) => Some(Command::Get {
            pane_id: required(command, "pane_id")?,
        }),
        Some(("layout", command)) => Some(Command::Layout {
            selector: selector(command),
        }),
        Some(("process-info", command)) => Some(Command::ProcessInfo {
            selector: selector(command),
        }),
        Some(("neighbor", command)) => Some(Command::Neighbor {
            selector: selector(command),
            direction: direction(command)?,
        }),
        Some(("edges", command)) => Some(Command::Edges {
            selector: selector(command),
        }),
        Some(("focus", command)) => Some(Command::Focus {
            selector: selector(command),
            direction: direction(command)?,
        }),
        Some(("resize", command)) => Some(Command::Resize {
            selector: selector(command),
            direction: direction(command)?,
            amount: value::<f32>(command, "amount"),
        }),
        Some(("zoom", command)) => {
            let (selector, on, off) = zoom_args(command);
            Some(Command::Zoom { selector, on, off })
        }
        Some(("read", command)) => Some(Command::Read(read_params(command)?)),
        Some(("rename", command)) => Some(Command::Rename(PaneRenameParams {
            pane_id: required(command, "pane_id")?,
            label: (!flag(command, "clear")).then(|| words(command, "label")),
        })),
        Some(("split", command)) => Some(Command::Split(split_args(command)?)),
        Some(("swap", command)) => Some(Command::Swap(swap_args(command))),
        Some(("move", command)) => Some(Command::Move(move_params(command))),
        Some(("close", command)) => Some(Command::Close {
            pane_id: required(command, "pane_id")?,
        }),
        Some(("report-agent", command)) => Some(Command::ReportAgent(report_agent_params(command))),
        Some(("report-agent-session", command)) => Some(Command::ReportAgentSession(
            report_agent_session_params(command),
        )),
        _ => None,
    }
}

pub(super) fn run_pane_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    let caller = super::target::caller_pane(paths);
    match command {
        Command::List { workspace } => super::send_method_response(
            paths,
            "cli:pane:list",
            Method::PaneList(PaneListParams {
                workspace_id: workspace,
            }),
            super::MethodResponseMode::Print,
        ),
        Command::Current { selector } => match selected_pane(&selector, &caller) {
            Ok(caller_pane_id) => super::send_method_response(
                paths,
                "cli:pane:current",
                Method::PaneCurrent(PaneCurrentParams { caller_pane_id }),
                super::MethodResponseMode::Print,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Get { pane_id } => super::send_method_response(
            paths,
            "cli:pane:get",
            Method::PaneGet(PaneTarget { pane_id }),
            super::MethodResponseMode::Print,
        ),
        Command::Layout { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => super::send_method_response(
                paths,
                "cli:pane:layout",
                Method::PaneLayout(PaneLayoutParams { pane_id }),
                super::MethodResponseMode::Print,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ProcessInfo { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => super::send_method_response(
                paths,
                "cli:pane:process_info",
                Method::PaneProcessInfo(PaneProcessInfoParams { pane_id }),
                super::MethodResponseMode::Print,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Neighbor {
            selector,
            direction,
        } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => super::send_method_response(
                paths,
                "cli:pane:neighbor",
                Method::PaneNeighbor(PaneNeighborParams { pane_id, direction }),
                super::MethodResponseMode::Print,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Edges { selector } => match selected_pane(&selector, &caller) {
            Ok(pane_id) => super::send_method_response(
                paths,
                "cli:pane:edges",
                Method::PaneEdges(PaneEdgesParams { pane_id }),
                super::MethodResponseMode::Print,
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
        Command::ReportAgent(result) => match result {
            Ok(params) => super::send_method_response(
                paths,
                "cli:request",
                Method::PaneReportAgent(params),
                super::MethodResponseMode::ErrorsOnly,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::ReportAgentSession(result) => match result {
            Ok(params) => super::send_method_response(
                paths,
                "cli:request",
                Method::PaneReportAgentSession(params),
                super::MethodResponseMode::ErrorsOnly,
            ),
            Err(message) => Ok(super::usage_error(&message)),
        },
    }
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

fn split_args(matches: &ArgMatches) -> Option<SplitArgs> {
    Some(SplitArgs {
        selector: selector(matches),
        direction: value::<SplitDirection>(matches, "direction")?,
        ratio: value::<f32>(matches, "ratio"),
        cwd: string(matches, "cwd"),
        focus: flag(matches, "focus"),
        right_click: value::<PaneRightClickTarget>(matches, "right-click")
            .unwrap_or(PaneRightClickTarget::Shepr),
        env: values::<(String, String)>(matches, "env")
            .into_iter()
            .collect(),
    })
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

fn direction(matches: &ArgMatches) -> Option<PaneDirection> {
    value::<PaneDirection>(matches, "direction")
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

fn read_params(matches: &ArgMatches) -> Option<PaneReadParams> {
    // `--ansi` and `--format ansi` keep escapes through the ANSI renderer.
    // There is no `--raw`: no raw PTY byte history exists to return, so it
    // could only ever repeat `--ansi`.
    let format = if flag(matches, "ansi") {
        ReadFormat::Ansi
    } else {
        value::<ReadFormat>(matches, "format").unwrap_or(ReadFormat::Text)
    };
    Some(PaneReadParams {
        pane_id: required(matches, "pane_id")?,
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        format,
        // Same params as `agent read`: an ANSI read keeps its escapes.
        strip_ansi: format != ReadFormat::Ansi,
        intent: shepr_api::schema::ReadIntent::Interactive,
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
        pane_id: required(matches, "pane_id").ok_or("missing required pane_id")?,
        destination,
        focus: !flag(matches, "no-focus"),
    })
}

fn report_agent_params(matches: &ArgMatches) -> Result<PaneReportAgentParams, String> {
    Ok(PaneReportAgentParams {
        pane_id: required(matches, "pane_id").ok_or("missing required pane_id")?,
        source: report_source(matches).ok_or("missing required --source")?,
        agent: required(matches, "agent").ok_or("missing required agent")?,
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
        pane_id: required(matches, "pane_id").ok_or("missing required pane_id")?,
        source: report_source(matches).ok_or("missing required --source")?,
        agent: required(matches, "agent").ok_or("missing required agent")?,
        seq: value::<u64>(matches, "seq"),
        agent_session_id: string(matches, "agent-session-id"),
        agent_session_path: string(matches, "agent-session-path"),
        session_start_source: string(matches, "session-start-source"),
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
        super::split_params(
            super::split_args(matches).expect("test precondition"),
            caller,
            paths,
        )
    }

    fn swap_params(matches: &ArgMatches, caller: &CallerPane) -> Result<PaneSwapParams, String> {
        super::swap_params(super::swap_args(matches), caller)
    }

    fn zoom_params(matches: &ArgMatches, caller: &CallerPane) -> Result<PaneZoomParams, String> {
        let (selector, on, off) = super::zoom_args(matches);
        super::zoom_params(&selector, on, off, caller)
    }

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(shepr_config::AppPaths::rooted_at(
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
        assert_eq!(direction(&neighbor), Some(PaneDirection::Down));

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
        assert_eq!(direction(&resize), Some(PaneDirection::Left));
        assert_eq!(value::<f32>(&resize, "amount"), Some(0.125));

        assert!(rejected(&["focus"]));
        assert!(rejected(&["neighbor", "--direction", "sideways"]));
    }

    #[test]
    fn read_defaults_with_bare_pane_id() {
        let params = read_params(&pane(&["read", "issue-1"])).expect("test precondition");

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
        ]))
        .expect("test precondition");
        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(params.source, ReadSource::Visible);
        assert_eq!(params.lines, Some(5));
        assert_eq!(params.format, ReadFormat::Ansi);
        assert!(!params.strip_ansi);

        let params = read_params(&pane(&["read", "--source=visible", "--lines=5", "issue-1"]))
            .expect("test precondition");
        assert_eq!(params.pane_id, "issue-1");
        assert_eq!(params.source, ReadSource::Visible);
        assert_eq!(params.lines, Some(5));

        // `--raw` only ever repeated `--ansi`; it is gone.
        assert!(rejected(&["read", "issue-1", "--raw"]));
        assert!(rejected(&["read", "issue-1", "--lines", "-3"]));
        assert!(rejected(&["read", "issue-1", "--source", "nowhere"]));
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
    fn report_agent_session_reads_its_options() {
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
}
