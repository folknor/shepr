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

mod cli;
#[cfg(test)]
mod netside_tests;
#[cfg(test)]
mod test_support;

fn should_block_nested(config: &shepr_config::ValidatedConfig) -> bool {
    should_block_nested_for_env(config, std::env::var(SHEPR_ENV_VAR).ok().as_deref())
}

fn should_block_nested_for_env(
    config: &shepr_config::ValidatedConfig,
    shepr_env: Option<&str>,
) -> bool {
    !config.experimental().allow_nested && shepr_env == Some(SHEPR_ENV_VALUE)
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

fn exit_if_nested_disabled(config: &shepr_config::ValidatedConfig) {
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

fn finish_cli(outcome: cli::CliResult<i32>) -> io::Result<()> {
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            error.print();
            std::process::exit(error.exit_code());
        }
    }
}

fn usage_exit(message: &str) -> ! {
    let error = cli::CliError::Usage(message.into());
    error.print();
    std::process::exit(error.exit_code());
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

    let command = match &invocation.launch {
        cli::Launch::Cli(command) => Some(command.as_ref()),
        _ => None,
    };
    if let Some(machine) = invocation.machine() {
        return finish_cli(cli::run_on_machine(command, &machine));
    }
    let requested_session = match invocation.requested_session() {
        Ok(session) => session,
        Err(err) => usage_exit(&err),
    };
    let requested_session = match requested_session
        .as_deref()
        .map(shepr_config::SessionId::parse)
        .transpose()
    {
        Ok(session) => session,
        Err(err) => usage_exit(&err.to_string()),
    };
    let remote_launch = match shepr_remote::remote_launch(
        invocation.remote().as_deref(),
        invocation.remote_keybindings().as_deref(),
    ) {
        Ok(remote_launch) => remote_launch,
        Err(err) => usage_exit(&err),
    };

    if remote_launch.is_some()
        && invocation.has_subcommand()
        && !(invocation.help_requested()
            || invocation.version_requested()
            || invocation.default_config_requested())
    {
        usage_exit("--remote can only be used with the default launch command");
    }

    // Root-level `--help`, `--version` and `--default-config` win over any
    // subcommand given with them.
    if invocation.help_requested() {
        cli::print_help(requested_session.clone());
        return Ok(());
    }

    if invocation.version_requested() {
        shepr_platform::begin_cli_output();
        println!("shepr {}", shepr_protocol::build_version());
        return Ok(());
    }

    if invocation.default_config_requested() {
        shepr_platform::begin_cli_output();
        print!("{}", shepr_config::DEFAULT_CONFIG);
        return Ok(());
    }

    if let Some(command) = command {
        return finish_cli(cli::run(command, requested_session.clone()));
    }

    match &invocation.launch {
        cli::Launch::ApiBridge { check } => {
            let paths = shepr_config::AppPaths::resolve_with_session(requested_session.clone())
                .map_err(|errors| {
                    io::Error::other(format!(
                        "application paths could not be resolved: {}",
                        errors.join("; ")
                    ))
                })?;
            return shepr_remote::run_remote_api_bridge(*check, &paths);
        }
        cli::Launch::ClientBridge { idle_timeout_v1 } => {
            let paths = shepr_config::AppPaths::resolve_with_session(requested_session.clone())
                .map_err(|errors| {
                    io::Error::other(format!(
                        "application paths could not be resolved: {}",
                        errors.join("; ")
                    ))
                })?;
            return shepr_remote::run_remote_client_bridge(*idle_timeout_v1, &paths);
        }
        _ => {}
    }

    let loaded_config = load_validated_config_or_exit(requested_session);
    let paths = loaded_config.paths();

    match invocation.launch {
        cli::Launch::HeadlessServer => {
            return shepr_server::server::headless::run_server(&loaded_config, paths);
        }
        cli::Launch::Client => {
            exit_if_nested_disabled(&loaded_config);
            return shepr_client::run_client(&loaded_config, paths);
        }
        cli::Launch::Tui { .. } => {}
        cli::Launch::ApiBridge { .. } | cli::Launch::ClientBridge { .. } | cli::Launch::Cli(_) => {
            return Err(io::Error::other("launch was already handled"));
        }
    }

    if let Some(remote_launch) = remote_launch {
        let remote_target = remote_launch.target.clone();
        let ssh_settings = shepr_remote::SavedSshSettings {
            manage_ssh_config: loaded_config.remote().manage_ssh_config,
        };
        if let Err(err) = shepr_remote::run_remote(remote_launch, ssh_settings, paths) {
            eprintln!("error: {err}");
            shepr_remote::print_remote_error_hint(&err, &remote_target);
            std::process::exit(1);
        }
        return Ok(());
    }

    exit_if_nested_disabled(&loaded_config);

    let saved_federation =
        shepr_remote::machine::EndpointCatalog::load(paths).is_ok_and(|catalog| catalog.has_ssh());
    if let Err(err) = shepr_remote::autodetect::auto_detect_launch(
        saved_federation,
        &loaded_config,
        paths,
        shepr_client::run_client,
    ) {
        eprintln!("shepr: {err}");
        std::process::exit(1);
    }
    Ok(())
}

fn load_validated_config_or_exit(
    requested_session: Option<shepr_config::SessionId>,
) -> shepr_config::ValidatedConfig {
    let paths = match shepr_config::AppPaths::resolve_with_session(requested_session) {
        Ok(paths) => paths,
        Err(diagnostics) => {
            eprintln!("shepr: configuration error:");
            for diagnostic in diagnostics {
                eprintln!("  {diagnostic}");
            }
            std::process::exit(1);
        }
    };
    match shepr_config::load_validated(&paths) {
        Ok(config) => config,
        Err(diagnostics) => {
            eprintln!("shepr: configuration error:");
            for diagnostic in diagnostics {
                eprintln!("  {diagnostic}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_lists_ui_accent_before_nested_tables() {
        let accent_marker = "# accent = \"#89b4fa\"";
        assert_eq!(
            shepr_config::DEFAULT_CONFIG.matches(accent_marker).count(),
            1
        );

        let accent = shepr_config::DEFAULT_CONFIG
            .find(accent_marker)
            .expect("test precondition");
        let sidebar = shepr_config::DEFAULT_CONFIG
            .find("# [ui.sidebar.agents]")
            .expect("test precondition");

        assert!(accent < sidebar);
    }

    #[test]
    fn default_config_documents_every_keybinding_with_its_default() {
        let keys =
            toml::Value::try_from(shepr_config::Config::default().keys).expect("test precondition");
        let keys = keys.as_table().expect("test precondition");
        assert!(!keys.is_empty());
        for (field, value) in keys {
            let Some(default) = value.as_str() else {
                continue;
            };
            let marker = format!("# {field} = ");
            let quoted = format!("{default:?}");
            let documented = shepr_config::DEFAULT_CONFIG.lines().any(|line| {
                line.strip_prefix(marker.as_str())
                    .and_then(|rest| rest.split_whitespace().next())
                    == Some(quoted.as_str())
            });
            assert!(
                documented,
                "keys.{field} = {default:?} missing from the printed default config"
            );
        }
    }

    #[test]
    fn nested_shepr_blocks_when_env_is_set() {
        let config = shepr_config::ValidatedConfig::test_default();
        assert!(should_block_nested_for_env(&config, Some(SHEPR_ENV_VALUE)));
    }

    #[test]
    fn nested_shepr_does_not_block_when_allowed() {
        let config = shepr_config::ValidatedConfig::test_from_config(
            toml::from_str("[experimental]\nallow_nested = true\n").expect("test precondition"),
            Some("[experimental]\nallow_nested = true\n"),
        );
        assert!(!should_block_nested_for_env(&config, Some(SHEPR_ENV_VALUE)));
    }

    #[test]
    fn nested_shepr_does_not_block_without_env() {
        let config = shepr_config::ValidatedConfig::test_default();
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
