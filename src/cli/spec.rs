//! The clap model of the whole command line. This is the only argv parser:
//! `main` parses argv with [`command`] once, then `cli` converts command
//! matches into typed arguments before dispatching to a handler. Value
//! validation (numbers, enums, `KEY=VALUE` pairs) lives here as value parsers,
//! so a bad value is a usage error (exit 2) instead of a transport error.

use std::ffi::OsStr;

use clap::builder::{
    NonEmptyStringValueParser, PossibleValue, PossibleValuesParser, TypedValueParser,
};
use clap::{Arg, ArgAction, ArgGroup, Command, ValueHint};

use shepr_api::schema::{
    PaneAgentState, PaneDirection, PaneRightClickTarget, ReadFormat, ReadSource, SplitDirection,
};
use shepr_remote::{
    COMMAND_CLIENT, COMMAND_REMOTE_API_BRIDGE, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER,
    COMMAND_STATUS, COMMAND_STOP, FLAG_CHECK, FLAG_JSON, FLAG_REMOTE, FLAG_REMOTE_KEYBINDINGS,
    FLAG_SESSION, KEYBINDINGS_LOCAL, KEYBINDINGS_SERVER, PROGRAM_NAME, option_name_from_flag,
};

mod machine;

pub(super) fn command() -> Command {
    // Launch options are root arguments, not `global` ones: clap accepts them
    // only before the subcommand, so a trailing `--session` or `--remote` is a
    // usage error instead of a silent retarget.
    let command = Command::new(PROGRAM_NAME)
        .bin_name(PROGRAM_NAME)
        .about("terminal workspace manager for AI coding agents")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(help_flag())
        .arg(
            option(option_name_from_flag(FLAG_SESSION), "NAME")
                .help("Use or create a named persistent session"),
        )
        .arg(
            option("machine", "LABEL-OR-ID")
                .value_parser(NonEmptyStringValueParser::new())
                .conflicts_with_all([
                    option_name_from_flag(FLAG_SESSION),
                    option_name_from_flag(FLAG_REMOTE),
                    option_name_from_flag(FLAG_REMOTE_KEYBINDINGS),
                    "default-config",
                    "version",
                    "help",
                ])
                .help("Run an API command on a saved SSH machine (uses that machine's session)"),
        )
        .arg(
            option(option_name_from_flag(FLAG_REMOTE), "TARGET")
                .help("Attach through SSH to a remote Shepr server"),
        )
        .arg(
            option(option_name_from_flag(FLAG_REMOTE_KEYBINDINGS), "MODE")
                .value_parser([KEYBINDINGS_LOCAL, KEYBINDINGS_SERVER])
                .requires(option_name_from_flag(FLAG_REMOTE))
                .help("Choose local or server keybindings for remote attach"),
        )
        .arg(flag("default-config").help("Print default configuration and exit"))
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::SetTrue)
                .help("Print version and exit"),
        )
        .subcommand(status_command())
        .subcommand(config_command())
        .subcommand(machine::command())
        .subcommand(server_command())
        .subcommand(workspace_command())
        .subcommand(tab_command())
        .subcommand(agent_command())
        .subcommand(pane_command())
        .subcommand(terminal_command())
        .subcommand(session_command())
        .subcommand(integration_command())
        .subcommand(
            Command::new(COMMAND_CLIENT)
                .hide(true)
                .about("Connect to a running server's client socket"),
        )
        .subcommand(
            Command::new(COMMAND_REMOTE_CLIENT_BRIDGE)
                .hide(true)
                .about("Relay a remote client connection over stdio"),
        )
        .subcommand(
            Command::new(COMMAND_REMOTE_API_BRIDGE)
                .hide(true)
                .about("Relay the API socket over stdio")
                .arg(flag(option_name_from_flag(FLAG_CHECK))),
        );
    configure_help(command, 0)
}

fn configure_help(command: Command, depth: usize) -> Command {
    // `disable_help_flag` is a *global* clap setting: once set on the root
    // (so the root can use its own plain `help` flag instead of clap's
    // immediate-exit one, see `help_flag`), it is unconditionally OR'd into
    // every descendant's settings when the tree is built, and there is no
    // public API to unset a setting a parent set globally. So subcommands
    // can't get clap's automatic `-h`/`--help` back by calling
    // `disable_help_flag(false)` on themselves; that call is silently
    // overridden. Instead, give every non-root command its own `-h`/`--help`
    // arg with the real help action, which works fine even though the
    // automatic one stays suppressed.
    let command = if depth == 0 {
        command
    } else {
        command.arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Print help"),
        )
    };
    command
        .disable_help_subcommand(true)
        .mut_subcommands(|subcommand| configure_help(subcommand, depth + 1))
}

/// A command that only groups subcommands. Invoked bare, it prints its help
/// to stderr and exits 2.
fn group(name: &'static str) -> Command {
    Command::new(name)
        .subcommand_required(true)
        .arg_required_else_help(true)
}

fn status_command() -> Command {
    Command::new(COMMAND_STATUS)
        .about("Show local client and running server status")
        .arg(flag(option_name_from_flag(FLAG_JSON)))
        .subcommand(
            Command::new(COMMAND_SERVER)
                .about("Show running server status")
                .arg(flag(option_name_from_flag(FLAG_JSON))),
        )
        .subcommand(
            Command::new(COMMAND_CLIENT)
                .about("Show local client status")
                .arg(flag(option_name_from_flag(FLAG_JSON))),
        )
}

fn config_command() -> Command {
    group("config")
        .about("Manage local configuration")
        .subcommand(Command::new("check").about("Validate config.toml and print diagnostics"))
}

fn server_command() -> Command {
    // Bare `shepr server` runs the headless server, so no subcommand is required.
    Command::new(COMMAND_SERVER)
        .about("Run or control the headless server")
        .subcommand(
            Command::new(COMMAND_STOP)
                .about("Stop the running server")
                .arg(force_stop_flag()),
        )
}

fn workspace_command() -> Command {
    group("workspace")
        .about("Manage workspaces over the socket API")
        .subcommand(Command::new("list").about("List workspaces"))
        .subcommand(
            Command::new("create")
                .about("Create a workspace")
                .arg(path_option("cwd", "PATH"))
                .arg(option("label", "TEXT"))
                .arg(env_option())
                .args(focus_flags()),
        )
        .subcommand(id_command("get", "workspace_id", "Show a workspace"))
        .subcommand(id_command("focus", "workspace_id", "Focus a workspace"))
        .subcommand(
            Command::new("rename")
                .about("Rename a workspace")
                .arg(required("workspace_id", "WORKSPACE_ID"))
                .arg(text_words("label", "LABEL")),
        )
        .subcommand(
            Command::new("report-metadata")
                .about("Report display-only workspace metadata")
                .arg(required("workspace_id", "WORKSPACE_ID"))
                .arg(option("source", "ID").required(true))
                .arg(token_option())
                .arg(repeatable_option("clear-token", "NAME"))
                .group(
                    ArgGroup::new("tokens")
                        .args(["token", "clear-token"])
                        .multiple(true)
                        .required(true),
                )
                .arg(u64_option("seq", "N"))
                .arg(u64_option("ttl-ms", "N")),
        )
        .subcommand(id_command("close", "workspace_id", "Close a workspace"))
}

fn tab_command() -> Command {
    group("tab")
        .about("Manage tabs over the socket API")
        .subcommand(
            Command::new("list")
                .about("List tabs")
                .arg(option("workspace", "WORKSPACE_ID")),
        )
        .subcommand(
            Command::new("create")
                .about("Create a tab")
                .arg(option("workspace", "WORKSPACE_ID"))
                .arg(path_option("cwd", "PATH"))
                .arg(option("label", "TEXT"))
                .arg(env_option())
                .args(focus_flags()),
        )
        .subcommand(id_command("get", "tab_id", "Show a tab"))
        .subcommand(id_command("focus", "tab_id", "Focus a tab"))
        .subcommand(
            Command::new("rename")
                .about("Rename a tab")
                .arg(required("tab_id", "TAB_ID"))
                .arg(text_words("label", "LABEL")),
        )
        .subcommand(id_command("close", "tab_id", "Close a tab"))
}

fn agent_command() -> Command {
    group("agent")
        .about("Inspect and navigate agent panes")
        .after_help("Targets accept unique agent names and pane ids that currently host agents.")
        .subcommand(Command::new("list").about("List agents"))
        .subcommand(id_command("get", "target", "Show an agent"))
        .subcommand(
            Command::new("read")
                .about("Read agent terminal output")
                .override_usage("shepr agent read <TARGET> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(read_source_option(READ_SOURCES))
                .arg(u32_option("lines", "N"))
                .arg(read_format_option())
                .arg(flag("ansi").conflicts_with("format")),
        )
        .subcommand(
            Command::new("rename")
                .about("Rename an agent")
                .override_usage("shepr agent rename <TARGET> <NAME>|--clear")
                .arg(required("target", "TARGET"))
                .arg(Arg::new("name").value_name("NAME"))
                .arg(flag("clear"))
                .group(
                    ArgGroup::new("rename")
                        .args(["name", "clear"])
                        .required(true),
                ),
        )
        .subcommand(id_command("focus", "target", "Focus an agent"))
        .subcommand(
            Command::new("attach")
                .about("Attach directly to an agent terminal")
                .override_usage("shepr agent attach <TARGET> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(flag("takeover")),
        )
        .subcommand(
            Command::new("explain")
                .about("Explain agent detection state")
                .override_usage(
                    "shepr agent explain <TARGET> [OPTIONS]\n       shepr agent explain --file <PATH> --agent <LABEL> [OPTIONS]",
                )
                .arg(
                    Arg::new("target")
                        .value_name("TARGET")
                        .required_unless_present("file")
                        .conflicts_with("file"),
                )
                .arg(
                    path_option("file", "PATH")
                        .requires("agent")
                        .help("Evaluate a saved screen capture locally"),
                )
                .arg(
                    option("agent", "LABEL")
                        .requires("file")
                        .help("Agent manifest to evaluate the --file capture against"),
                )
                .arg(json_flag().conflicts_with("format"))
                .arg(text_json_format_option())
                .arg(
                    Arg::new("verbose")
                        .short('v')
                        .long("verbose")
                        .action(ArgAction::SetTrue),
                ),
        )
}

fn pane_command() -> Command {
    group("pane")
        .about("Control terminal panes")
        .after_help(
            "Commands that take --pane/--current act on the calling pane when neither is given, and on the server's focused pane when run outside a pane of that server.",
        )
        .subcommand(
            Command::new("list")
                .about("List panes")
                .arg(option("workspace", "WORKSPACE_ID")),
        )
        .subcommand(
            Command::new("current")
                .about("Show the current pane")
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(id_command("get", "pane_id", "Show a pane"))
        .subcommand(
            Command::new("layout")
                .about("Show pane layout information")
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("process-info")
                .about("Show pane process information")
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("neighbor")
                .about("Find a pane neighbor")
                .arg(direction_option().required(true))
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("edges")
                .about("Show pane edge information")
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("focus")
                .about("Focus a neighboring pane")
                .arg(direction_option().required(true))
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("resize")
                .about("Resize a pane split")
                .arg(direction_option().required(true))
                .arg(
                    option("amount", "FLOAT")
                        .allow_negative_numbers(true)
                        .value_parser(finite_f32),
                )
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false)),
        )
        .subcommand(
            Command::new("zoom")
                .about("Toggle or set pane zoom")
                .arg(Arg::new("pane_id").value_name("PANE_ID"))
                .args(current_pane_args())
                .group(pane_selector(&["pane_id", "pane", "current"], false))
                .arg(flag("toggle"))
                .arg(flag("on"))
                .arg(flag("off"))
                .group(ArgGroup::new("mode").args(["toggle", "on", "off"])),
        )
        .subcommand(
            Command::new("read")
                .about("Read pane terminal output")
                .arg(required("pane_id", "PANE_ID"))
                .arg(read_source_option(READ_SOURCES))
                .arg(u32_option("lines", "N"))
                .arg(read_format_option())
                // No `--raw`: there is no raw PTY byte history, so it could
                // only repeat `--ansi`.
                .arg(
                    flag("ansi")
                        .conflicts_with("format")
                        .help("Same as --format ansi"),
                ),
        )
        .subcommand(
            Command::new("rename")
                .about("Rename a pane")
                .override_usage("shepr pane rename <PANE_ID> <LABEL>...|--clear")
                .arg(required("pane_id", "PANE_ID"))
                .arg(text_words("label", "LABEL").required(false))
                .arg(flag("clear"))
                .group(
                    ArgGroup::new("rename")
                        .args(["label", "clear"])
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("split")
                .about("Split a pane")
                .arg(Arg::new("pane_id").value_name("PANE_ID"))
                .args(current_pane_args())
                .group(pane_selector(&["pane_id", "pane", "current"], false))
                .arg(split_direction_option("direction").required(true))
                .arg(option("ratio", "FLOAT").value_parser(finite_f32))
                .arg(path_option("cwd", "PATH"))
                .arg(env_option())
                .arg(right_click_option())
                .args(focus_flags()),
        )
        .subcommand(
            Command::new("swap")
                .about("Swap panes")
                .override_usage(
                    "shepr pane swap --direction <DIRECTION> [--pane <ID>|--current]\n       shepr pane swap --source-pane <ID> --target-pane <ID>",
                )
                .arg(direction_option())
                .args(current_pane_args())
                .group(pane_selector(&["pane", "current"], false))
                .arg(option("source-pane", "ID"))
                .arg(option("target-pane", "ID")),
        )
        .subcommand(
            Command::new("move")
                .about("Move a pane")
                .override_usage(
                    "shepr pane move <PANE_ID> --tab <TAB_ID> --split <DIRECTION> [--target-pane <ID>] [--ratio <FLOAT>] [--focus|--no-focus]\n       shepr pane move <PANE_ID> --new-tab [--workspace <ID>] [--label <TEXT>] [--focus|--no-focus]\n       shepr pane move <PANE_ID> --new-workspace [--label <TEXT>] [--tab-label <TEXT>] [--focus|--no-focus]",
                )
                .arg(required("pane_id", "PANE_ID"))
                .arg(option("tab", "TAB_ID"))
                .arg(split_direction_option("split"))
                .arg(option("target-pane", "ID"))
                .arg(option("ratio", "FLOAT").value_parser(finite_f32))
                .arg(flag("new-tab"))
                .arg(option("workspace", "ID"))
                .arg(flag("new-workspace"))
                .arg(option("label", "TEXT"))
                .arg(option("tab-label", "TEXT"))
                .args(focus_flags())
                .group(
                    ArgGroup::new("destination")
                        .args(["tab", "new-tab", "new-workspace"])
                        .required(true),
                ),
        )
        .subcommand(id_command("close", "pane_id", "Close a pane"))
        .subcommand(report_agent_command())
        .subcommand(report_agent_session_command())
        .subcommand(release_agent_command())
        .subcommand(report_metadata_command())
}

fn report_agent_command() -> Command {
    Command::new("report-agent")
        .about("Report pane agent lifecycle state")
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(
            option("state", "STATUS")
                .required(true)
                .value_parser(Choice(PANE_AGENT_STATES)),
        )
        .arg(free_text_option("message", "TEXT"))
        .arg(u64_option("seq", "N"))
        .arg(option("agent-session-id", "ID"))
        .arg(path_option("agent-session-path", "PATH"))
}

fn report_agent_session_command() -> Command {
    Command::new("report-agent-session")
        .about("Report pane agent session identity")
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(u64_option("seq", "N"))
        .arg(option("agent-session-id", "ID"))
        .arg(path_option("agent-session-path", "PATH"))
        .arg(option("session-start-source", "SOURCE"))
}

fn release_agent_command() -> Command {
    Command::new("release-agent")
        .about("Release pane agent lifecycle authority")
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(u64_option("seq", "N"))
}

fn report_metadata_command() -> Command {
    Command::new("report-metadata")
        .about("Report display-only pane metadata")
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL"))
        .arg(option("applies-to-source", "ID"))
        .arg(free_text_option("title", "TEXT").conflicts_with("clear-title"))
        .arg(flag("clear-title"))
        .arg(free_text_option("display-agent", "TEXT").conflicts_with("clear-display-agent"))
        .arg(flag("clear-display-agent"))
        .arg(token_option())
        .arg(repeatable_option("clear-token", "NAME"))
        .arg(u64_option("seq", "N"))
        .arg(u64_option("ttl-ms", "N"))
        .group(
            ArgGroup::new("fields")
                .args([
                    "title",
                    "clear-title",
                    "display-agent",
                    "clear-display-agent",
                    "token",
                    "clear-token",
                ])
                .multiple(true)
                .required(true),
        )
}

fn terminal_command() -> Command {
    group("terminal")
        .about("Attach to or observe raw terminal streams")
        .subcommand(
            Command::new("attach")
                .about("Attach directly to a terminal stream")
                .arg(required("terminal_id", "TERMINAL_ID"))
                .arg(flag("takeover"))
                .after_help("Detach with ctrl+b q; send a literal ctrl+b with ctrl+b ctrl+b."),
        )
        .subcommand(
            group("title")
                .about("Manage the outer terminal title")
                .subcommand(
                    Command::new("set")
                        .about("Set the outer terminal title")
                        .arg(required("title", "TITLE").allow_hyphen_values(true)),
                )
                .subcommand(Command::new("clear").about("Clear the outer terminal title")),
        )
}

fn session_command() -> Command {
    group("session")
        .about("Manage named persistent sessions")
        .subcommand(Command::new("list").about("List sessions").arg(json_flag()))
        .subcommand(
            Command::new("attach")
                .about("Attach to a session")
                .arg(required("name", "NAME")),
        )
        .subcommand(
            Command::new(COMMAND_STOP)
                .about("Stop a session")
                .arg(required("name", "NAME"))
                .arg(json_flag())
                .arg(force_stop_flag())
                .after_help("Use 'default' as NAME to stop the default session."),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a stopped session")
                .arg(required("name", "NAME"))
                .arg(json_flag()),
        )
}

fn integration_command() -> Command {
    group("integration")
        .about("Manage built-in agent integrations")
        .subcommand(
            Command::new("install")
                .about("Install an integration")
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("uninstall")
                .about("Uninstall an integration")
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("status")
                .about("Show integration status")
                .arg(flag("outdated-only")),
        )
}

/// Resolved by `selected_pane` in `pane.rs`, the same way for every command.
fn current_pane_args() -> [Arg; 2] {
    [
        option("pane", "ID").help("Act on this pane"),
        flag("current").help(
            "Act on the calling pane (SHEPR_PANE_ID); an error outside a pane of the targeted server",
        ),
    ]
}

/// At most one way of naming the pane (`required` makes it exactly one).
fn pane_selector(args: &[&'static str], required: bool) -> ArgGroup {
    ArgGroup::new("pane_selector")
        .args(args.iter().copied())
        .required(required)
}

fn focus_flags() -> [Arg; 2] {
    // The later of the two wins, as with any repeated switch.
    [
        flag("focus").overrides_with("no-focus"),
        flag("no-focus").overrides_with("focus"),
    ]
}

fn integration_target_arg() -> Arg {
    Arg::new("target")
        .value_name("TARGET")
        .required(true)
        .value_parser(integration_target_values())
}

fn integration_target_values() -> Vec<&'static str> {
    let values: Vec<&'static str> = shepr_api::schema::IntegrationTarget::all()
        .map(shepr_agent::integration::integration_target_label)
        .collect();
    values
}

fn id_command(name: &'static str, id: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(required(id, id))
}

fn direction_option() -> Arg {
    option("direction", "DIRECTION").value_parser(Choice(PANE_DIRECTIONS))
}

fn split_direction_option(name: &'static str) -> Arg {
    option(name, "DIRECTION").value_parser(Choice(SPLIT_DIRECTIONS))
}

fn right_click_option() -> Arg {
    option("right-click", "TARGET").value_parser(Choice(RIGHT_CLICK_TARGETS))
}

fn read_source_option(values: &'static [(&'static str, ReadSource)]) -> Arg {
    option("source", "SOURCE")
        .value_parser(Choice(values))
        .help("Terminal snapshot source (default: recent)")
}

fn read_format_option() -> Arg {
    option("format", "FORMAT").value_parser(Choice(READ_FORMATS))
}

fn text_json_format_option() -> Arg {
    option("format", "FORMAT").value_parser(["text", "json"])
}

fn json_flag() -> Arg {
    flag(option_name_from_flag(FLAG_JSON))
}

/// `server stop` and `session stop` refuse a server of another build, since
/// stopping it exits its panes and it may be the installed server a dev build
/// reached by default; this flag states that stopping it is intended.
fn force_stop_flag() -> Arg {
    let long = option_name_from_flag(shepr_api::session::FORCE_STOP_FLAG);
    Arg::new(long)
        .long(long)
        .action(ArgAction::SetTrue)
        .help("Stop the server even when it runs a different shepr build")
}

fn help_flag() -> Arg {
    Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::SetTrue)
        .help("Show help")
}

fn env_option() -> Arg {
    repeatable_option("env", "KEY=VALUE")
        .value_parser(env_assignment)
        .help("Set an environment variable for the launched process")
}

fn token_option() -> Arg {
    repeatable_option("token", "NAME=VALUE").value_parser(token_assignment)
}

fn flag(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}

fn option(name: &'static str, value_name: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(value_name)
        .action(ArgAction::Set)
}

/// An option whose value is free text, so a value that starts with `-` is
/// taken as the value rather than as the next option.
fn free_text_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).allow_hyphen_values(true)
}

fn repeatable_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).action(ArgAction::Append)
}

fn path_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).value_hint(ValueHint::AnyPath)
}

fn u64_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).value_parser(clap::value_parser!(u64))
}

fn u32_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).value_parser(clap::value_parser!(u32))
}

fn required(name: &'static str, value_name: &'static str) -> Arg {
    Arg::new(name).value_name(value_name).required(true)
}

/// The trailing words of a command (a label, text, a command line, keys).
/// Once the first word is seen, every remaining argument belongs to it, even
/// ones that look like options.
fn text_words(name: &'static str, value_name: &'static str) -> Arg {
    required(name, value_name)
        .num_args(1..)
        .allow_hyphen_values(true)
        .trailing_var_arg(true)
}

fn finite_f32(value: &str) -> Result<f32, String> {
    match value.parse::<f32>() {
        Ok(parsed) if parsed.is_finite() => Ok(parsed),
        _ => Err("expected a finite number".to_string()),
    }
}

fn env_assignment(value: &str) -> Result<(String, String), String> {
    super::parse_env_assignment(value)
}

fn token_assignment(value: &str) -> Result<(String, Option<String>), String> {
    super::parse_token_assignment(value)
}

const PANE_DIRECTIONS: &[(&str, PaneDirection)] = &[
    ("left", PaneDirection::Left),
    ("right", PaneDirection::Right),
    ("up", PaneDirection::Up),
    ("down", PaneDirection::Down),
];

const SPLIT_DIRECTIONS: &[(&str, SplitDirection)] = &[
    ("right", SplitDirection::Right),
    ("down", SplitDirection::Down),
];

const RIGHT_CLICK_TARGETS: &[(&str, PaneRightClickTarget)] = &[
    ("shepr", PaneRightClickTarget::Shepr),
    ("pane", PaneRightClickTarget::Pane),
];

const READ_SOURCES: &[(&str, ReadSource)] = &[
    ("visible", ReadSource::Visible),
    ("recent", ReadSource::Recent),
    ("recent-unwrapped", ReadSource::RecentUnwrapped),
    ("detection", ReadSource::Detection),
];

const READ_FORMATS: &[(&str, ReadFormat)] =
    &[("text", ReadFormat::Text), ("ansi", ReadFormat::Ansi)];

const PANE_AGENT_STATES: &[(&str, PaneAgentState)] = &[
    ("idle", PaneAgentState::Idle),
    ("working", PaneAgentState::Working),
    ("blocked", PaneAgentState::Blocked),
    ("unknown", PaneAgentState::Unknown),
];

/// Parses one of a fixed set of names straight into its API value, and
/// reports the names as possible values for help and validation.
#[derive(Clone)]
struct Choice<T: 'static>(&'static [(&'static str, T)]);

impl<T: Clone + Send + Sync + 'static> TypedValueParser for Choice<T> {
    type Value = T;

    fn parse_ref(
        &self,
        cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let name = PossibleValuesParser::new(self.0.iter().map(|(name, _)| *name))
            .parse_ref(cmd, arg, value)?;
        self.0
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidValue).with_cmd(cmd))
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            self.0.iter().map(|(name, _)| PossibleValue::new(*name)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use clap::{Arg, ArgAction, Command};

    fn command_path<'a>(cmd: &'a Command, path: &[&str]) -> &'a Command {
        let mut current = cmd;
        for name in path {
            current = current
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *name)
                .unwrap_or_else(|| panic!("missing command path segment {name}"));
        }
        current
    }

    fn option_values(cmd: &Command, option: &str) -> Vec<String> {
        let arg = cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some(option))
            .unwrap_or_else(|| panic!("missing --{option}"));
        arg.get_value_parser()
            .possible_values()
            .into_iter()
            .flatten()
            .map(|value| value.get_name().to_string())
            .collect()
    }

    fn has_option(cmd: &Command, option: &str) -> bool {
        cmd.get_arguments()
            .any(|arg| arg.get_long() == Some(option))
    }

    fn option_arg<'a>(cmd: &'a Command, option: &str) -> &'a Arg {
        cmd.get_arguments()
            .find(|arg| arg.get_long() == Some(option))
            .unwrap_or_else(|| panic!("missing --{option}"))
    }

    fn argument<'a>(cmd: &'a Command, id: &str) -> &'a Arg {
        cmd.get_arguments()
            .find(|arg| arg.get_id() == id)
            .unwrap_or_else(|| panic!("missing argument {id}"))
    }

    fn collect_subcommand_paths(
        cmd: &Command,
        path: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
    ) {
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            paths.push(path.clone());
            collect_subcommand_paths(subcommand, path, paths);
            path.pop();
        }
    }

    fn collect_leaf_subcommand_paths(
        cmd: &Command,
        path: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
    ) {
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            if subcommand.get_subcommands().next().is_none() {
                paths.push(path.clone());
            } else {
                collect_leaf_subcommand_paths(subcommand, path, paths);
            }
            path.pop();
        }
    }

    fn sample_value(arg: &Arg) -> String {
        arg.get_value_parser()
            .possible_values()
            .into_iter()
            .flatten()
            .next()
            .map_or_else(
                || match arg.get_id().as_str() {
                    "token" => "KEY=VALUE".to_string(),
                    "clear-token" => "KEY".to_string(),
                    "ssh-target" => "user@example.test".to_string(),
                    _ => "value".to_string(),
                },
                |value| value.get_name().to_string(),
            )
    }

    fn append_argument(cmd: &Command, id: &str, args: &mut Vec<String>) {
        let arg = cmd
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == id)
            .unwrap_or_else(|| panic!("missing argument {id}"));
        if matches!(
            arg.get_action(),
            ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
        ) {
            if let Some(long) = arg.get_long() {
                args.push(format!("--{long}"));
            } else if let Some(short) = arg.get_short() {
                args.push(format!("-{short}"));
            }
            return;
        }

        let value = sample_value(arg);
        if let Some(long) = arg.get_long() {
            args.push(format!("--{long}"));
            args.push(value);
        } else if let Some(short) = arg.get_short() {
            args.push(format!("-{short}"));
            args.push(value);
        } else {
            args.push(value);
        }
    }

    fn append_required_arguments(cmd: &Command, args: &mut Vec<String>) {
        let mut selected = Vec::new();
        for arg in cmd.get_arguments().filter(|arg| arg.is_required_set()) {
            let id = arg.get_id().as_str().to_string();
            append_argument(cmd, &id, args);
            selected.push(id);
        }
        for group in cmd.get_groups().filter(|group| group.is_required_set()) {
            if group.get_args().any(|id| {
                selected
                    .iter()
                    .any(|selected| selected.as_str() == id.as_str())
            }) {
                continue;
            }
            if let Some(id) = group.get_args().next() {
                append_argument(cmd, id.as_str(), args);
                selected.push(id.as_str().to_string());
            }
        }
    }

    fn sample_leaf_invocation(spec: &Command, path: &[String]) -> Vec<String> {
        let mut args = vec!["shepr".to_string()];
        append_required_arguments(spec, &mut args);
        let mut current = spec;
        for name in path {
            let subcommand = current
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == name.as_str())
                .unwrap_or_else(|| panic!("missing command path segment {name}"));
            args.push(name.clone());
            append_required_arguments(subcommand, &mut args);
            current = subcommand;
        }
        // `agent explain` accepts either a target or a local file and requires
        // one through a conditional argument rule rather than an ArgGroup.
        if path.iter().map(String::as_str).eq(["agent", "explain"]) {
            args.push("target".to_string());
        }
        args
    }

    fn assert_command_descriptions(cmd: &Command, path: &mut Vec<String>) {
        if !path.is_empty() {
            assert!(
                cmd.get_about().is_some(),
                "missing completion description for {}",
                path.join(" ")
            );
        }
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            assert_command_descriptions(subcommand, path);
            path.pop();
        }
    }

    /// Renders what `shepr <path> <flag>` prints, via the real parser.
    fn rendered_help(path: &[String], flag: &str) -> String {
        let mut args = vec!["shepr".to_string()];
        args.extend(path.iter().cloned());
        args.push(flag.to_string());
        let error = super::command()
            .try_get_matches_from(&args)
            .expect_err("help flag should stop parsing");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "help was not handled for shepr {}",
            path.join(" ")
        );
        error.render().to_string()
    }

    fn long_help(path: &[&str]) -> String {
        let path: Vec<String> = path.iter().map(ToString::to_string).collect();
        rendered_help(&path, "--help")
    }

    #[test]
    fn spec_describes_all_completion_commands() {
        let cmd = super::command();
        assert_command_descriptions(&cmd, &mut Vec::new());
    }

    #[test]
    fn spec_passes_clap_invariants() {
        super::command().debug_assert();
    }

    #[test]
    fn generated_remote_cli_arguments_parse_with_the_cli_spec() {
        use shepr_remote::{RemoteCliCommand, RemoteKeybindings};

        use crate::cli::{CliCommand, Invocation, Launch, parse_invocation, server, status};

        // Parses what the producer emits and checks what the parser made of it,
        // so a spelling that parses into the wrong command or session fails too.
        fn parse(command: RemoteCliCommand<'_>) -> Invocation {
            let mut argv = vec![super::PROGRAM_NAME.to_owned()];
            argv.extend(command.args().into_iter().map(str::to_owned));
            parse_invocation(&argv)
                .unwrap_or_else(|code| panic!("{command:?} should parse, exit code {code}"))
        }
        let default = shepr_config::DEFAULT_SESSION_NAME;
        // Remote-host commands spell the default session by omitting the flag.
        let parsed_session = |session: &str| (session != default).then(|| session.to_owned());

        let invocation = parse(RemoteCliCommand::ClientStatus);
        assert!(matches!(
            &invocation.launch,
            Launch::Cli(command)
                if matches!(**command, CliCommand::Status(status::Command::Client { json: true }))
        ));
        assert_eq!(invocation.session, None);

        for session in [default, "agents"] {
            let invocation = parse(RemoteCliCommand::ServerStatus { session });
            assert!(matches!(
                &invocation.launch,
                Launch::Cli(command)
                    if matches!(**command, CliCommand::Status(status::Command::Server { json: true }))
            ));
            assert_eq!(invocation.session, parsed_session(session));

            let invocation = parse(RemoteCliCommand::ClientBridge { session });
            assert!(matches!(invocation.launch, Launch::ClientBridge));
            assert_eq!(invocation.session, parsed_session(session));

            for check in [false, true] {
                let invocation = parse(RemoteCliCommand::ApiBridge { session, check });
                assert!(matches!(
                    invocation.launch,
                    Launch::ApiBridge { check: parsed } if parsed == check
                ));
                assert_eq!(invocation.session, parsed_session(session));
            }

            for force in [false, true] {
                let invocation = parse(RemoteCliCommand::ServerStop { session, force });
                assert!(matches!(
                    &invocation.launch,
                    Launch::Cli(command)
                        if matches!(
                            **command,
                            CliCommand::Server(server::Command::Stop { force: parsed })
                                if parsed == force
                        )
                ));
                assert_eq!(invocation.session, parsed_session(session));
            }
        }

        for session in [None, Some(default), Some("agents")] {
            for keybindings in [
                None,
                Some(RemoteKeybindings::Local),
                Some(RemoteKeybindings::Server),
            ] {
                let invocation = parse(RemoteCliCommand::Attach {
                    target: "dev@host",
                    session,
                    keybindings,
                });
                assert!(matches!(
                    invocation.launch,
                    Launch::Tui {
                        attached_session: None
                    }
                ));
                assert_eq!(invocation.remote.as_deref(), Some("dev@host"));
                assert_eq!(invocation.session.as_deref(), session);
                assert_eq!(
                    invocation.remote_keybindings.as_deref(),
                    keybindings.map(RemoteKeybindings::to_env_value)
                );
            }
        }
    }

    #[test]
    fn every_cli_spec_leaf_parses_to_a_typed_command() {
        // Every leaf must reach a typed variant. The variant's `--machine`
        // policy is then an exhaustive `can_run_on_machine` match, so the
        // compiler, not this test, makes a new variant decide it.
        let spec = super::command();
        let mut paths = Vec::new();
        collect_leaf_subcommand_paths(&spec, &mut Vec::new(), &mut paths);
        let launch_only = ["client", "remote-api-bridge", "remote-client-bridge"];
        let mut classified = 0;

        for path in paths {
            if launch_only.contains(&path[0].as_str())
                || path.iter().map(String::as_str).eq(["session", "attach"])
            {
                continue;
            }
            let argv = sample_leaf_invocation(&spec, &path);
            let matches = spec
                .clone()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|error| panic!("{} should parse: {error}", path.join(" ")));
            let Some((name, command_matches)) = matches.subcommand() else {
                panic!("{} did not parse as a CLI command", path.join(" "));
            };
            let command = super::super::CliCommand::from_matches(name, command_matches)
                .unwrap_or_else(|| panic!("{} has no typed command", path.join(" ")));
            assert_eq!(command.name(), name, "{}", path.join(" "));
            assert!(
                command.subcommand_name().is_some(),
                "{} has no typed subcommand name",
                path.join(" ")
            );
            classified += 1;
        }

        assert!(classified > 0, "the spec has no classified CLI leaves");
    }

    #[test]
    fn every_spec_subcommand_renders_short_and_long_help() {
        let mut paths = Vec::new();
        collect_subcommand_paths(&super::command(), &mut Vec::new(), &mut paths);

        for path in paths {
            for flag in ["-h", "--help"] {
                let output = rendered_help(&path, flag);
                assert!(
                    output.contains(&format!("Usage: shepr {}", path.join(" "))),
                    "unexpected help for shepr {}: {output}",
                    path.join(" ")
                );
            }
        }
    }

    #[test]
    fn spec_matches_all_integration_targets() {
        let cmd = super::command();
        let install = command_path(&cmd, &["integration", "install"]);
        let expected: Vec<String> = shepr_api::schema::IntegrationTarget::all()
            .map(shepr_agent::integration::integration_target_label)
            .map(str::to_string)
            .collect();
        assert_eq!(
            argument(install, "target")
                .get_value_parser()
                .possible_values()
                .expect("test precondition")
                .map(|value| value.get_name().to_string())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn spec_marks_runtime_required_options_as_required() {
        for (path, options) in [
            (&["workspace", "report-metadata"][..], &["source"][..]),
            (&["pane", "neighbor"][..], &["direction"][..]),
            (&["pane", "focus"][..], &["direction"][..]),
            (&["pane", "resize"][..], &["direction"][..]),
            (&["pane", "split"][..], &["direction"][..]),
            (
                &["pane", "report-agent"][..],
                &["source", "agent", "state"][..],
            ),
            (
                &["pane", "report-agent-session"][..],
                &["source", "agent"][..],
            ),
            (&["pane", "release-agent"][..], &["source", "agent"][..]),
            (&["pane", "report-metadata"][..], &["source"][..]),
        ] {
            let cmd = command_path(&super::command(), path).clone();
            for option in options {
                assert!(
                    option_arg(&cmd, option).is_required_set(),
                    "shepr {} --{option} should be required",
                    path.join(" ")
                );
            }
        }
    }

    #[test]
    fn agent_rename_requires_exactly_one_name_or_clear() {
        for valid in [
            &["shepr", "agent", "rename", "reviewer", "worker"][..],
            &["shepr", "agent", "rename", "reviewer", "--clear"][..],
        ] {
            assert!(super::command().try_get_matches_from(valid).is_ok());
        }
        for invalid in [
            &["shepr", "agent", "rename", "reviewer"][..],
            &["shepr", "agent", "rename", "reviewer", "worker", "--clear"][..],
        ] {
            assert!(super::command().try_get_matches_from(invalid).is_err());
        }

        assert!(
            long_help(&["agent", "rename"])
                .contains("Usage: shepr agent rename <TARGET> <NAME>|--clear")
        );
    }

    #[test]
    fn spec_excludes_removed_agent_and_pane_commands() {
        let cmd = super::command();
        assert!(
            cmd.get_subcommands()
                .all(|command| command.get_name() != "wait")
        );

        let agent = command_path(&cmd, &["agent"]);
        for removed in ["prompt", "send-keys", "wait"] {
            assert!(
                agent
                    .get_subcommands()
                    .all(|command| command.get_name() != removed)
            );
        }

        let pane = command_path(&cmd, &["pane"]);
        for removed in ["send-text", "send-keys", "run", "wait-output", "input"] {
            assert!(
                pane.get_subcommands()
                    .all(|command| command.get_name() != removed)
            );
        }
    }

    #[test]
    fn spec_pane_read_has_ansi_and_no_raw_alias() {
        let cmd = super::command();
        let pane_read = command_path(&cmd, &["pane", "read"]);
        assert!(has_option(pane_read, "ansi"));
        assert!(!has_option(pane_read, "raw"));
        assert_eq!(
            option_values(pane_read, "source"),
            ["visible", "recent", "recent-unwrapped", "detection"]
        );
    }

    #[test]
    fn spec_matches_pane_split_direction_flag() {
        let cmd = super::command();
        let pane_split = command_path(&cmd, &["pane", "split"]);
        assert!(has_option(pane_split, "direction"));
        assert!(!has_option(pane_split, "split"));
        assert_eq!(option_values(pane_split, "direction"), ["right", "down"]);
    }

    #[test]
    fn spec_has_no_workspace_group_close() {
        let cmd = super::command();
        let close = command_path(&cmd, &["workspace", "close"]);
        assert!(!has_option(close, "group"));
        assert!(
            super::command()
                .try_get_matches_from(["shepr", "workspace", "close", "w1", "--group"])
                .is_err()
        );
    }

    #[test]
    fn bad_values_are_usage_errors() {
        for args in [
            &[
                "shepr",
                "workspace",
                "report-metadata",
                "w1",
                "--source",
                "s",
                "--token",
                "a=b",
                "--seq",
                "x",
            ][..],
            &[
                "shepr",
                "pane",
                "report-agent",
                "p1",
                "--source",
                "s",
                "--agent",
                "a",
                "--state",
                "sleepy",
            ],
            &[
                "shepr",
                "pane",
                "release-agent",
                "p1",
                "--source",
                "s",
                "--agent",
                "a",
                "--seq",
                "-1",
            ],
            &["shepr", "agent", "read", "t", "--lines", "many"],
            &["shepr", "agent", "read", "t", "--source", "everywhere"],
            &["shepr", "pane", "split", "--direction", "left"],
            &[
                "shepr",
                "pane",
                "resize",
                "--direction",
                "up",
                "--amount",
                "NaN",
            ],
            &["shepr", "workspace", "create", "--env", "NOVALUE"],
        ] {
            let error = super::command()
                .try_get_matches_from(args)
                .expect_err("bad value should be rejected");
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
    }
}
