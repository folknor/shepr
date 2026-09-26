use std::io;

pub(crate) const SHEPR_ENV_VAR: &str = "SHEPR_ENV";
pub(crate) const SHEPR_ENV_VALUE: &str = "1";
const NESTED_SHEPR_MESSAGES: [&str; 6] = [
    "inception detected. we need to go deeper... said no one ever.",
    "recursion is a pathway to many abilities some consider to be... unnatural.",
    "you were so preoccupied with whether you could, you didn't stop to think if you should. \u{2014} dr. malcolm",
    "recursive shepring is disabled. somewhere, a call stack breathes a sigh of relief.",
    "recursive descent denied. there is, in fact, such a thing as too much shepr.",
    "recursion detected. base case not found. aborting.",
];

mod agent_resume;
mod api;
mod app;
mod build_info;
mod cli;
mod client;
mod config;
mod copy_mode;
mod detect;
mod events;
mod ghostty;
mod input;
mod integration;
mod ipc;
mod layout;
mod logging;
mod metadata_tokens;
mod noninteractive_process;
mod pane;
mod pathutil;
mod persist;
mod platform;
mod protocol;
mod pty;
mod raw_input;
mod remote;
mod render_signal;
mod selection;
mod server;
mod session;
mod terminal;
mod terminal_cell_size;
mod terminal_effects;
mod terminal_modes;
mod terminal_theme;
mod ui;
mod workspace;

const DEFAULT_CONFIG: &str = r##"# shepr configuration
# Place this file at ~/.config/shepr/config.toml

[theme]
# Built-in themes: catppuccin, terminal, tokyo-night, dracula, nord,
#                  gruvbox, one-dark, solarized, kanagawa, rose-pine,
#                  vesper
# name = "catppuccin"

# Override individual color tokens on top of the base theme.
# Accepts: hex (#rrggbb), named colors, rgb(r,g,b), or panel_bg = "reset"
# [theme.custom]
# sidebar_bg = "#181825"
# active_row_bg = "#1e1e2e"
# selection_bg = "#313244"
# panel_bg = "reset"
# accent = "#f5c2e7"
# red = "#ff6188"
# green = "#a6e3a1"

[terminal]
# Executable used for new interactive panes.
# Empty means $SHELL, then /bin/sh.
# default_shell = ""

# Startup mode for new interactive pane shells: "auto", "login", or "non_login".
# "auto" is the same as "non_login".
# shell_mode = "auto"

# CWD policy for new panes, tabs, and workspaces when no explicit --cwd is provided.
# Use "follow" to inherit the source pane/workspace, "home" for $HOME,
# "current" for Shepr's process directory, or a fixed path such as "~/Projects".
# new_cwd = "follow"


[keys]
# Prefix key to enter prefix mode (default: "ctrl+b")
# Examples: "ctrl+b", "f12", "esc", "-"
# Action bindings use explicit syntax: "prefix+n" requires the prefix;
# "ctrl+alt+n" is a direct terminal-mode shortcut.
# Accepted key syntax: plain keys, ctrl/shift/alt/cmd/super modifiers, and special keys like enter/tab/esc/left/right/up/down.
# Named punctuation such as minus, comma, ampersand, plus, and backtick is also accepted.
# Most reliable direct bindings are ctrl+letter, function keys, and explicit modified chords.
# alt+..., cmd/super, and punctuation-with-modifiers may depend on your terminal/tmux setup.
# prefix = "ctrl+b"

# Prefix-mode actions
# help = "prefix+?"
# detach = "prefix+q"
# workspace_picker = "prefix+w"
# goto = "prefix+g"
# new_workspace = "prefix+shift+n"
# rename_workspace = "prefix+shift+w"
# close_workspace = "prefix+shift+d"
# previous_workspace = "" # optional, unset by default
# next_workspace = ""     # optional, unset by default
# previous_agent = ""     # optional, unset by default
# next_agent = ""         # optional, unset by default
# focus_agent = ""        # optional indexed binding, e.g. "prefix+alt+1..9"
# new_tab = "prefix+c"
# rename_tab = "prefix+shift+t"
# previous_tab = "prefix+p"
# next_tab = "prefix+n"
# move_tab_previous = ""   # optional, e.g. "alt+shift+left" moves the tab toward the front
# move_tab_next = ""       # optional, e.g. "alt+shift+right" moves the tab toward the back
# switch_tab = "prefix+1..9"
# switch_workspace = ""   # optional indexed binding, e.g. "prefix+shift+1..9"
# close_tab = "prefix+shift+x"
# rename_pane = "prefix+shift+p"
# clear_pane = ""                  # unbound; e.g. "prefix+ctrl+k"
# focus_pane_left = "prefix+h"
# focus_pane_down = "prefix+j"
# focus_pane_up = "prefix+k"
# focus_pane_right = "prefix+l"
# cycle_pane_next = "prefix+tab"
# cycle_pane_previous = "prefix+shift+tab"
# last_pane = ""          # optional, unset by default; bind e.g. "prefix+tab" for global back-and-forth
# split_vertical = "prefix+v"
# split_horizontal = "prefix+minus"
# close_pane = "prefix+x"
# zoom = "prefix+z"
# resize_mode = "prefix+r"
# resize_pane_left = ""   # optional, e.g. "ctrl+shift+alt+left" resizes without entering resize mode
# resize_pane_down = ""   # optional, e.g. "ctrl+shift+alt+down"
# resize_pane_up = ""     # optional, e.g. "ctrl+shift+alt+up"
# resize_pane_right = ""  # optional, e.g. "ctrl+shift+alt+right"
# toggle_sidebar = "prefix+b"

# Navigate-mode movement. These local shortcuts win while navigate mode is open.
# They are independent from focus_pane_*. Do not include prefix+, esc, enter, tab, or 1..9 here.
# navigate_workspace_up = "up"
# navigate_workspace_down = "down"
# navigate_pane_left = "h"      # left arrow always focuses the pane to the left
# navigate_pane_down = "j"
# navigate_pane_up = "k"
# navigate_pane_right = "l"     # right arrow always focuses the pane to the right

# Size of the virtual terminal used when no client is attached.
# Attached clients always use their own terminal size.
[server]
# headless_cols = 120
# headless_rows = 40

[ui]
# Sidebar width (auto-scaled based on workspace names, this sets the default)
# sidebar_width = 26

# Minimum sidebar width when expanded (columns)
# sidebar_min_width = 18

# Maximum sidebar width when expanded (columns)
# sidebar_max_width = 36

# Start with the sidebar collapsed. Changes take effect on the next launch.
# sidebar_start_collapsed = false

# Collapsed sidebar presentation: "compact" keeps the narrow status rail, "hidden" uses zero width.
# sidebar_collapsed_mode = "compact"

# Capture mouse input for Shepr's mouse UI.
# Set false to let the terminal handle normal clicks, such as clicking URLs.
# Pane apps like lazygit and btop can still receive mouse when they request it.
# mouse_capture = true

# Automatically copy text selected with the mouse.
# Set false to retain drag or double-click word selection until Ctrl+C
# copies and clears it.
# copy_on_select = true

# Host cursor policy: "auto", "native", or "drawn".
# "auto" draws Shepr's own cursor under WSL to avoid cursor flicker, and uses the native terminal cursor elsewhere.
# "native" always uses the outer terminal cursor. "drawn" always draws Shepr's cursor as terminal cell content.
# host_cursor = "auto"

# Optional modifier that forwards right-click hold/drag gestures to pane apps instead of opening Shepr's pane menu.
# Empty/off disables this. Shift is intentionally unsupported because terminals commonly reserve Shift+mouse.
# right_click_passthrough_modifier = ""

# Force a full redraw when the outer terminal regains focus.
# Set false to reduce visible flashing when switching back to Shepr.
# Trade-off: rare host terminal surface corruption may persist until the next full redraw.
# redraw_on_focus_gained = true

# Pane scrollback lines to scroll per mouse wheel notch.
# mouse_scroll_lines = 3

# Ask for confirmation before closing a workspace
# confirm_close = true

# Ask for a tab name before creating a new tab.
# Set false to create tabs immediately with generated names.
# prompt_new_tab_name = true

# Ask for a workspace name before interactive creation.
# prompt_new_workspace_name = false

# Draw borders around split panes.
# "auto" draws them only for split panes, "always" also frames a lone pane
# (only while pane_outer_borders is enabled), "off" disables them.
# pane_borders = "auto"

# Draw borders along the outside edge of the pane area.
# Disable for tmux-style internal splitters without an outside frame.
# pane_outer_borders = true

# Draw interactive scrollbars beside terminal panes.
# Set false to reclaim the scrollbar column and keep it out of terminal-native selections.
# pane_scrollbars = true

# Keep split panes visually separated instead of sharing divider borders.
# pane_gaps = true

# Show detected/reported agent labels in split pane borders when no manual pane name is set.
# show_agent_labels_on_pane_borders = false

# Hide the tab row when a workspace has exactly one tab.
# New tabs can still be created with the configured keybinding.
# hide_tab_bar_when_single_tab = false

# Desktop tab row placement: "top" or "bottom".
# tab_bar_position = "top"

# Ordered status entries at the right edge of the desktop tab bar.
# Supported types: zoom, hostname, datetime, text, and command.
# Hostname, datetime, and command entries resolve on the Shepr server.
# tab_bar_right = []
# tab_bar_right_separator = " "

# Title Shepr writes to the terminal it runs in, which is what window managers
# show in title, tab, and group bars. Tokens are {hostname}, {workspace}, {tab},
# {pane}, and {terminal_title}; {{ and }} are literal braces.
# The title renders on the Shepr server, so {hostname} names the host the panes
# run on even when attaching from a remote client.
# Set to "" to leave the outer terminal title alone.
# window_title = "{hostname}: {workspace}"

# Agent panel ordering: "spaces" (grouped by space) or "priority" (attention queue).
# agent_panel_sort = "spaces"

# Agent status indicators: "dots" preserves the compact color marks; "symbols" uses
# distinct static glyphs for blocked, working, done, idle, and unknown states.
# status_indicators = "dots"

# Accent color for highlights, borders, and navigation UI.
# Accepts: hex (#89b4fa), named colors (cyan, blue, magenta), or rgb(r,g,b)
# accent = "cyan"

# Expanded agent rows. Built-ins are state_icon, state_text, machine, workspace, tab,
# pane, agent, terminal_title, and terminal_title_stripped.
# Custom values reported through pane metadata use a $name token.
# A token occurrence may be styled with { token = "workspace", fg = "#89b4fa", bold = true, dim = false }.
# Omitted style fields preserve the contextual default.
# [ui.sidebar.agents]
# Blank rows between agent entries. Set to 1 to restore the previous spacing.
# row_gap = 0
# rows = [["state_icon", "machine", "workspace", "tab"], ["agent"]]
# Optional canonical agent IDs replace the default rows for matching agents.
# [ui.sidebar.agents.rows_by_agent]
# claude = [["state_icon", "machine", "workspace", "tab"], ["terminal_title_stripped"], ["agent"]]

# Expanded space rows. Built-ins are state_icon, state_text, workspace, branch, and git_status.
# Custom values reported through workspace metadata use a $name token, for example $jj_status.
# Inline token styles accept strict #RGB/#RRGGBB foregrounds plus bold and dim booleans.
# [ui.sidebar.spaces]
# Blank rows between space entries. Set to 1 to restore the previous spacing.
# row_gap = 0
# rows = [["state_icon", "workspace"], ["branch", "git_status"]]

[session]
# Resume supported AI-agent panes into their native conversation sessions after
# a Shepr server restart. Requires official integrations that report session refs.
# resume_agents_on_restore = true
# Milliseconds between automatic agent restores; 0 starts them without spacing.
# startup_per_agent_delay_ms = 100

[remote]
# Whether shepr manages the ssh config used for `shepr --remote`.
# When true (default), shepr runs remote ssh through a generated config that
# includes your ~/.ssh/config first and adds ServerAliveInterval/
# ServerAliveCountMax as fallbacks (so any keepalive values you set yourself
# still win) to survive idle network/NAT timeouts. Shepr also uses a private
# per-attach OpenSSH control socket to reuse the first authenticated connection.
# Set false to run plain ssh against your ssh config unchanged; this does not
# force keepalive or multiplexing off, it only stops shepr from adding its own.
# manage_ssh_config = true

[experimental]
# Allow launching shepr from inside a shepr-managed pane.
# allow_nested = false
# Save recent pane screen history across full server restarts.
pane_history = false
# Expose the focused pane's cursor to the outer terminal so input
# methods keep tracking the candidate window when TUIs paint their own
# cursor (Claude Code, pi, codex). Trade-off: extra cursor visible for
# apps that hide it without painting a replacement (vim normal mode, etc.).
# reveal_hidden_cursor_for_cjk_ime = false
# Optional allow-list: only reveal for focused panes whose detected agent
# matches one of these names. Empty means apply to any focused pane.
# If the list contains no valid names, the reveal does not apply.
# Accepted: pi, claude, codex, gemini, cursor, devin, cline, opencode,
# copilot, kimi, kiro, droid, amp, grok, hermes, kilo, qodercli, qoder, qwen,
# qwen-code, letta, letta-code, maki.
# cjk_ime_agents = []
# Cursor shape rendered when reveal_hidden_cursor_for_cjk_ime is true.
# Values: block, steady_block (default), underline, steady_underline, bar, steady_bar.
# cjk_ime_cursor_shape = "steady_block"

[advanced]
# Maximum scrollback buffer size in bytes retained per pane terminal.
# Matches Ghostty's default scrollback-limit behavior.
# scrollback_limit_bytes = 10000000
"##;

fn should_block_nested(config: &config::Config) -> bool {
    should_block_nested_for_env(config, std::env::var(SHEPR_ENV_VAR).ok().as_deref())
}

fn should_block_nested_for_env(config: &config::Config, shepr_env: Option<&str>) -> bool {
    !config.experimental.allow_nested && shepr_env == Some(SHEPR_ENV_VALUE)
}

fn random_nested_message() -> &'static str {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as usize)
        .unwrap_or(0);
    let index = (nanos ^ (std::process::id() as usize)) % NESTED_SHEPR_MESSAGES.len();
    NESTED_SHEPR_MESSAGES[index]
}

fn exit_if_nested_disabled(config: &config::Config) {
    if should_block_nested(config) {
        eprintln!("\x1b[1merror:\x1b[0m nested shepr is disabled by default.");
        eprintln!("see configuration if you want to enable it.");
        eprintln!();
        eprintln!("\x1b[2m\"{}\"\x1b[0m", random_nested_message());
        std::process::exit(1);
    }
}

fn args_as_utf8<I>(args: I) -> Result<Vec<String>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .enumerate()
        .map(|(index, arg)| {
            arg.into_string()
                .map_err(|_| format!("argument {index} is not valid UTF-8"))
        })
        .collect()
}

fn finish_cli(outcome: io::Result<cli::CommandOutcome>) -> io::Result<()> {
    match outcome {
        Ok(cli::CommandOutcome::Handled(code)) => std::process::exit(code),
        Ok(cli::CommandOutcome::NotCli) => Ok(()),
        Err(err) if cli::protocol_mismatch_was_reported(&err) => std::process::exit(1),
        Err(err) if cli::server_not_running_was_reported(&err) => {
            if let Some(response) = cli::server_not_running_reported_response(&err)
                && let Ok(json) = serde_json::to_string(response)
            {
                eprintln!("{json}");
            }
            std::process::exit(1);
        }
        Err(err) => {
            // Transport and I/O failures of a CLI command: report them like
            // every other CLI error instead of letting `main` print the
            // error's Debug form.
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

fn usage_exit(message: &str) -> ! {
    eprintln!("error: {message}");
    eprintln!("run 'shepr --help' for usage");
    std::process::exit(2);
}

fn main() -> io::Result<()> {
    let raw_args: Vec<String> = match args_as_utf8(std::env::args_os()) {
        Ok(args) => args,
        Err(err) => usage_exit(&err),
    };
    // The one command-line parser: the clap spec in `cli/spec.rs`. It prints
    // its own usage errors and subcommand help.
    let invocation = match cli::parse_invocation(&raw_args) {
        Ok(invocation) => invocation,
        Err(exit_code) => std::process::exit(exit_code),
    };

    if let Some(machine) = invocation.machine() {
        return finish_cli(cli::run_on_machine(&invocation, &machine));
    }
    let requested_session = match invocation.requested_session() {
        Ok(session) => session,
        Err(err) => usage_exit(&err),
    };
    if let Err(err) = session::configure(requested_session.as_deref()) {
        usage_exit(&err);
    }
    let remote_launch = match remote::remote_launch(
        invocation.remote().as_deref(),
        invocation.remote_keybindings().as_deref(),
    ) {
        Ok(remote_launch) => remote_launch,
        Err(err) => usage_exit(&err),
    };

    if remote_launch.is_some()
        && invocation.command_name().is_some()
        && !(invocation.help_requested()
            || invocation.version_requested()
            || invocation.default_config_requested())
    {
        usage_exit("--remote can only be used with the default launch command");
    }

    // Root-level `--help`, `--version` and `--default-config` win over any
    // subcommand given with them.
    if invocation.help_requested() {
        print_help();
        return Ok(());
    }

    if invocation.version_requested() {
        platform::begin_cli_output();
        println!("shepr {}", crate::build_info::version());
        return Ok(());
    }

    if invocation.default_config_requested() {
        platform::begin_cli_output();
        print!("{DEFAULT_CONFIG}");
        return Ok(());
    }

    finish_cli(cli::run(&invocation))?;

    // Whatever `cli::run` did not handle launches something: a hidden mode,
    // the headless server, or (below) the TUI.
    match invocation.command_name() {
        Some("remote-api-bridge") => {
            return remote::run_remote_api_bridge(&invocation.bridge_args());
        }
        Some("remote-client-bridge") => {
            return remote::run_remote_client_bridge(&invocation.bridge_args());
        }
        // `server` with a subcommand was handled by `cli::run`.
        Some("server") => return server::headless::run_server(),
        // Hidden client mode: connect to an existing server's client socket.
        Some("client") => {
            let loaded_config = config::Config::load();
            exit_if_nested_disabled(&loaded_config.config);
            return client::run_client();
        }
        _ => {}
    }

    if let Some(remote_launch) = remote_launch {
        let remote_target = remote_launch.target.clone();
        if let Err(err) = remote::run_remote(remote_launch) {
            eprintln!("error: {err}");
            remote::print_remote_error_hint(&err, &remote_target);
            std::process::exit(1);
        }
        return Ok(());
    }

    let loaded_config = config::Config::load();
    exit_if_nested_disabled(&loaded_config.config);

    let saved_federation =
        client::endpoint::EndpointCatalog::load().is_ok_and(|catalog| catalog.has_enabled_ssh());
    if let Err(err) = server::autodetect::auto_detect_launch(saved_federation) {
        eprintln!("shepr: {err}");
        std::process::exit(1);
    }
    Ok(())
}

fn print_help() {
    platform::begin_cli_output();
    println!("shepr \u{2014} terminal workspace manager for AI coding agents");
    println!();
    println!("Usage: shepr [options]");
    println!("       shepr --session <name> [options]");
    println!("       shepr --machine <label-or-id> <command>");
    println!("       shepr --remote <ssh-target> [--session <name>]");
    println!("       shepr session attach <name>");
    println!("       shepr machine <subcommand> ...");
    println!("       shepr server stop");
    println!("       shepr config <subcommand> ...");
    println!("       shepr workspace <subcommand> ...");
    println!("       shepr tab <subcommand> ...");
    println!("       shepr agent <subcommand> ...");
    println!("       shepr pane <subcommand> ...");
    println!("       shepr session <subcommand> ...");
    println!("       shepr integration <subcommand> ...");
    println!();
    println!("Common commands:");
    for (command, description) in [
        ("shepr", "Launch or attach to the persistent session"),
        (
            "shepr status [server|client]",
            "Show local client and running server status",
        ),
        (
            "shepr server stop",
            "Stop the running server via the API socket",
        ),
        (
            "shepr config reset-keys",
            "Back up config.toml and remove custom keybindings",
        ),
        ("shepr machine <subcommand>", "Manage saved SSH machines"),
        (
            "shepr workspace <subcommand>",
            "Workspace helpers over the socket API",
        ),
        ("shepr tab <subcommand>", "Tab helpers over the socket API"),
        (
            "shepr agent <subcommand>",
            "Agent/terminal helpers over the socket API",
        ),
        (
            "shepr pane <subcommand>",
            "Pane control helpers over the socket API",
        ),
        (
            "shepr session <subcommand>",
            "Manage named persistent sessions",
        ),
        (
            "shepr integration <subcommand>",
            "Manage built-in agent integrations",
        ),
    ] {
        println!("  {command:<32} {description}");
    }
    println!();
    println!("Advanced commands:");
    println!("  {:<32} Run as headless server", "shepr server");
    println!();
    println!("Options:");
    println!("  --session <name>    Use or create a named persistent session");
    println!("  --machine <label-or-id>  Run an API command on a saved SSH machine");
    println!("  --remote <target>   Attach through SSH to a remote Shepr server");
    println!("  --remote-keybindings <local|server>");
    println!("                      Keybindings for --remote app attach (default: local)");
    println!("  --default-config    Print default configuration and exit");
    println!("  --version, -V       Print version and exit");
    println!("  --help, -h          Show this help");
    println!();
    println!("Config: {}", config::config_path().display());
    println!("Logs:   {}", logging::help_log_paths_summary());
    println!("Env:    SHEPR_CONFIG_PATH overrides config file path");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_lists_ui_accent_before_nested_tables() {
        let accent_marker = "# accent = \"cyan\"";
        assert_eq!(DEFAULT_CONFIG.matches(accent_marker).count(), 1);

        let accent = DEFAULT_CONFIG
            .find(accent_marker)
            .expect("test precondition");
        let sidebar = DEFAULT_CONFIG
            .find("# [ui.sidebar.agents]")
            .expect("test precondition");

        assert!(accent < sidebar);
    }

    #[test]
    fn nested_shepr_blocks_when_env_is_set() {
        let config = config::Config::default();
        assert!(should_block_nested_for_env(&config, Some(SHEPR_ENV_VALUE)));
    }

    #[test]
    fn nested_shepr_does_not_block_when_allowed() {
        let config: config::Config =
            toml::from_str("[experimental]\nallow_nested = true\n").expect("test precondition");
        assert!(!should_block_nested_for_env(&config, Some(SHEPR_ENV_VALUE)));
    }

    #[test]
    fn nested_shepr_does_not_block_without_env() {
        let config = config::Config::default();
        assert!(!should_block_nested_for_env(&config, None));
    }

    #[test]
    fn random_nested_message_comes_from_known_set() {
        let message = random_nested_message();
        assert!(NESTED_SHEPR_MESSAGES.contains(&message));
    }

    #[test]
    fn nested_message_strings_no_longer_repeat_shepr_prefix() {
        assert!(
            NESTED_SHEPR_MESSAGES
                .iter()
                .all(|message| !message.starts_with("shepr:"))
        );
    }

    fn invalid_utf8_arg() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    }

    #[test]
    fn args_as_utf8_passes_through_valid_arguments() {
        let args = ["shepr", "pane", "get", "pane-1"].map(std::ffi::OsString::from);
        assert_eq!(
            args_as_utf8(args).expect("test precondition"),
            ["shepr", "pane", "get", "pane-1"]
        );
    }

    #[test]
    fn args_as_utf8_reports_the_offending_argument_instead_of_panicking() {
        let args = vec![
            std::ffi::OsString::from("shepr"),
            std::ffi::OsString::from("pane"),
            invalid_utf8_arg(),
        ];
        assert_eq!(
            args_as_utf8(args).expect_err("test precondition"),
            "argument 2 is not valid UTF-8"
        );
    }
}
