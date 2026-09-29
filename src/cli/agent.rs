use clap::ArgMatches;

use shepr_api::schema::{
    AgentReadParams, AgentRenameParams, AgentTarget, EmptyParams, Method, ReadFormat, ReadSource,
    Request,
};

use super::matches::{flag, required, string, value};

#[derive(Clone)]
pub(crate) enum Command {
    List,
    Get { target: String },
    Read(AgentReadParams),
    Rename(AgentRenameParams),
    Focus { target: String },
    Attach { target: String, takeover: bool },
    Explain(super::detect::ExplainArgs),
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Get { .. } => "get",
            Self::Read(_) => "read",
            Self::Rename(_) => "rename",
            Self::Focus { .. } => "focus",
            Self::Attach { .. } => "attach",
            Self::Explain(_) => "explain",
        }
    }

    pub(super) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::List
            | Self::Get { .. }
            | Self::Read(_)
            | Self::Rename(_)
            | Self::Focus { .. } => true,
            Self::Explain(args) => args.file.is_none(),
            Self::Attach { .. } => false,
        }
    }
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("list", _)) => Some(Command::List),
        Some(("get", command)) => Some(Command::Get {
            target: required(command, "target")?,
        }),
        Some(("read", command)) => Some(Command::Read(read_params(command)?)),
        Some(("rename", command)) => Some(Command::Rename(AgentRenameParams {
            target: required(command, "target")?,
            name: string(command, "name"),
        })),
        Some(("focus", command)) => Some(Command::Focus {
            target: required(command, "target")?,
        }),
        Some(("attach", command)) => Some(Command::Attach {
            target: required(command, "target")?,
            takeover: flag(command, "takeover"),
        }),
        Some(("explain", command)) => Some(Command::Explain(super::detect::ExplainArgs {
            pane: string(command, "target"),
            file: string(command, "file"),
            agent: string(command, "agent"),
            json: flag(command, "json") || string(command, "format").as_deref() == Some("json"),
            verbose: flag(command, "verbose"),
        })),
        _ => None,
    }
}

pub(super) fn run_agent_command(
    command: Command,
    config: Option<shepr_config::ValidatedConfig>,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::List => agent_list(paths),
        Command::Get { target } => agent_get(paths, target),
        Command::Read(params) => agent_read(paths, params),
        Command::Rename(params) => agent_rename(paths, params),
        Command::Focus { target } => agent_focus(paths, target),
        Command::Attach { target, takeover } => agent_attach(&target, takeover, config, paths),
        Command::Explain(args) => agent_explain(paths, args),
    }
}

/// Temporary alias of `detect explain` until the old command groups go;
/// unlike it, a live target may still be an agent name.
fn agent_explain(
    paths: &super::target::CliContext,
    args: super::detect::ExplainArgs,
) -> super::CliResult<i32> {
    super::detect::explain(paths, args)
}

fn agent_list(paths: &super::target::CliContext) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:list".into(),
            method: Method::AgentList(EmptyParams::default()),
        },
    )?)
}

fn agent_get(paths: &super::target::CliContext, target: String) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:get".into(),
            method: Method::AgentGet(AgentTarget { target }),
        },
    )?)
}

fn agent_focus(paths: &super::target::CliContext, target: String) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:focus".into(),
            method: Method::AgentFocus(AgentTarget { target }),
        },
    )?)
}

fn agent_attach(
    target: &str,
    takeover: bool,
    config: Option<shepr_config::ValidatedConfig>,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    let config = match config {
        Some(config) => config,
        None => super::load_validated_config(paths)?,
    };
    let response = resolve_agent_target(paths, target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!(
            "{}",
            serde_json::to_string(&response).map_err(std::io::Error::other)?
        );
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        return Err(super::CliError::Failed {
            message: "agent attach failed: response did not include terminal_id".into(),
            hints: Vec::new(),
        });
    };
    let Ok(terminal_id) = terminal_id.parse::<shepr_protocol::TerminalId>() else {
        return Err(super::CliError::Failed {
            message: format!(
                "agent attach failed: server reported invalid terminal_id {terminal_id:?}"
            ),
            hints: Vec::new(),
        });
    };
    crate::init_client_logging(paths)?;
    super::finish_client(shepr_client::run_terminal_attach(
        &config,
        paths,
        terminal_id,
        takeover,
    ))
}

fn resolve_agent_target(
    paths: &super::target::CliContext,
    target: &str,
    request_id: &str,
) -> super::CliResult<serde_json::Value> {
    super::send_request(paths, &agent_get_request(target, request_id))
}

fn agent_get_request(target: &str, request_id: &str) -> Request {
    Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    }
}

fn agent_rename(
    paths: &super::target::CliContext,
    params: AgentRenameParams,
) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:rename".into(),
            method: Method::AgentRename(params),
        },
    )?)
}

fn read_params(matches: &ArgMatches) -> Option<AgentReadParams> {
    // `--ansi` conflicts with `--format` in the spec; either selects ANSI, and
    // an ANSI read of an agent keeps its escapes.
    let format = if flag(matches, "ansi") {
        ReadFormat::Ansi
    } else {
        value::<ReadFormat>(matches, "format").unwrap_or(ReadFormat::Text)
    };
    Some(AgentReadParams {
        target: required(matches, "target")?,
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        format,
        strip_ansi: format != ReadFormat::Ansi,
    })
}

fn agent_read(paths: &super::target::CliContext, params: AgentReadParams) -> super::CliResult<i32> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:agent:read".into(),
            method: Method::AgentRead(params),
        },
    )?;
    super::print_read_response(&response)
}

#[cfg(test)]
mod parse_tests {
    use super::super::tests::group_matches;
    use shepr_api::schema::{ReadFormat, ReadSource};

    fn command(args: &[&str]) -> super::Command {
        super::parse(&group_matches(args)).expect("test precondition")
    }

    #[test]
    fn read_defaults_and_ansi_forms() {
        let super::Command::Read(params) = command(&["agent", "read", "worker"]) else {
            panic!("expected agent read");
        };
        assert_eq!(params.target, "worker");
        assert_eq!(params.source, ReadSource::Recent);
        assert_eq!(params.format, ReadFormat::Text);
        assert!(params.strip_ansi);

        for form in [&["--ansi"][..], &["--format", "ansi"], &["--format=ansi"]] {
            let mut args = vec![
                "agent",
                "read",
                "worker",
                "--source=detection",
                "--lines",
                "7",
            ];
            args.extend_from_slice(form);
            let super::Command::Read(params) = command(&args) else {
                panic!("expected agent read");
            };
            assert_eq!(params.source, ReadSource::Detection);
            assert_eq!(params.lines, Some(7));
            assert_eq!(params.format, ReadFormat::Ansi);
            assert!(!params.strip_ansi);
        }
    }
}
