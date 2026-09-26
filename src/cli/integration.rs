use crate::api::schema::IntegrationTarget;

pub(super) fn run_integration_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_integration_help();
        return Ok(2);
    };

    match subcommand {
        "install" => integration_install(&args[1..]),
        "uninstall" => integration_uninstall(&args[1..]),
        "status" => integration_status(&args[1..]),
        "help" | "--help" | "-h" => {
            print_integration_help();
            Ok(0)
        }
        _ => {
            print_integration_help();
            Ok(2)
        }
    }
}

fn integration_status(args: &[String]) -> std::io::Result<i32> {
    let outdated_only = match args {
        [] => false,
        [flag] if flag == "--outdated-only" => true,
        _ => {
            eprintln!("usage: shepr integration status [--outdated-only]");
            return Ok(2);
        }
    };

    if outdated_only {
        crate::integration::print_outdated_update_notice();
        return Ok(0);
    }

    for status in crate::integration::installed_integration_statuses() {
        let target = crate::integration::integration_target_label(status.target);
        let state = describe_integration_state(
            status.state,
            status.installed_version,
            status.expected_version,
        );
        println!("{target}: {state} ({})", status.path.display());
    }

    if let Some(status) = crate::integration::experimental_letta_integration_status() {
        let state = describe_integration_state(
            status.state,
            status.installed_version,
            status.expected_version,
        );
        println!(
            "{} (experimental): {state} ({})",
            status.label,
            status.path.display()
        );
    }

    Ok(0)
}

fn describe_integration_state(
    state: crate::integration::IntegrationStatusKind,
    installed_version: Option<u32>,
    expected_version: u32,
) -> String {
    let version = match installed_version {
        Some(version) => format!("v{version}"),
        None => "legacy".to_string(),
    };
    match state {
        crate::integration::IntegrationStatusKind::NotInstalled => "not installed".to_string(),
        crate::integration::IntegrationStatusKind::Current => format!("current ({version})"),
        crate::integration::IntegrationStatusKind::Outdated
            if installed_version.is_some_and(|installed| installed >= expected_version) =>
        {
            format!("needs repair ({version})")
        }
        crate::integration::IntegrationStatusKind::Outdated => {
            format!("outdated ({version} < v{expected_version})")
        }
    }
}

fn integration_install(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "install")? else {
        return Ok(2);
    };

    let installed = match target {
        IntegrationCommandTarget::Builtin(target) => crate::integration::install_target(target),
        IntegrationCommandTarget::Letta => crate::integration::install_experimental_letta(),
    };
    match installed {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn integration_uninstall(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "uninstall")? else {
        return Ok(2);
    };

    let removed = match target {
        IntegrationCommandTarget::Builtin(target) => crate::integration::uninstall_target(target),
        IntegrationCommandTarget::Letta => crate::integration::uninstall_experimental_letta(),
    };
    match removed {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn print_integration_messages(messages: Vec<String>) {
    for message in messages {
        println!("{message}");
    }
}

/// Integration target accepted by the CLI. Letta is not an `IntegrationTarget`
/// variant only for historical reasons: nothing on the wire constrains that
/// enum (client and server are always the same build), so the separate
/// experimental path (this variant, the experimental install/uninstall/status
/// functions in `crate::integration`) is leftover structure that can be folded
/// into `IntegrationTarget` together with its registry and status handling.
/// Letta's install and uninstall already go through the same protected config
/// writer as the built-in targets.
enum IntegrationCommandTarget {
    Builtin(IntegrationTarget),
    Letta,
}

/// Every target label the CLI accepts, in the order usage and help list them.
/// Usage, the unknown-target hint and the help text all come from this list,
/// so a new target cannot be added to one and forgotten in another.
const INTEGRATION_TARGET_LABELS: &[&str] = &[
    "pi",
    "omp",
    "claude",
    "codex",
    "copilot",
    "devin",
    "droid",
    "kimi",
    "opencode",
    "kilo",
    "hermes",
    "qodercli",
    "qwen",
    "letta",
    "cursor",
    "mastracode",
    "antigravity-cli",
    "grok",
];

fn parse_integration_target(
    args: &[String],
    action: &str,
) -> std::io::Result<Option<IntegrationCommandTarget>> {
    let [target] = args else {
        eprintln!(
            "usage: shepr integration {action} <{}>",
            INTEGRATION_TARGET_LABELS.join("|")
        );
        return Ok(None);
    };
    let target = target.as_str();

    let parsed = match target {
        "pi" => IntegrationCommandTarget::Builtin(IntegrationTarget::Pi),
        "omp" => IntegrationCommandTarget::Builtin(IntegrationTarget::Omp),
        "claude" => IntegrationCommandTarget::Builtin(IntegrationTarget::Claude),
        "codex" => IntegrationCommandTarget::Builtin(IntegrationTarget::Codex),
        "copilot" => IntegrationCommandTarget::Builtin(IntegrationTarget::Copilot),
        "devin" => IntegrationCommandTarget::Builtin(IntegrationTarget::Devin),
        "droid" => IntegrationCommandTarget::Builtin(IntegrationTarget::Droid),
        "kimi" => IntegrationCommandTarget::Builtin(IntegrationTarget::Kimi),
        "opencode" => IntegrationCommandTarget::Builtin(IntegrationTarget::Opencode),
        "kilo" => IntegrationCommandTarget::Builtin(IntegrationTarget::Kilo),
        "hermes" => IntegrationCommandTarget::Builtin(IntegrationTarget::Hermes),
        "qodercli" => IntegrationCommandTarget::Builtin(IntegrationTarget::Qodercli),
        "qwen" => IntegrationCommandTarget::Builtin(IntegrationTarget::Qwen),
        "letta" => IntegrationCommandTarget::Letta,
        "cursor" => IntegrationCommandTarget::Builtin(IntegrationTarget::Cursor),
        "mastracode" => IntegrationCommandTarget::Builtin(IntegrationTarget::Mastracode),
        "antigravity-cli" | "antigravity_cli" => {
            IntegrationCommandTarget::Builtin(IntegrationTarget::AntigravityCli)
        }
        "grok" => IntegrationCommandTarget::Builtin(IntegrationTarget::Grok),
        _ => {
            eprintln!("unknown integration target: {target}");
            eprintln!(
                "currently supported: {}",
                INTEGRATION_TARGET_LABELS.join(", ")
            );
            return Ok(None);
        }
    };

    Ok(Some(parsed))
}

fn print_integration_help() {
    eprintln!("shepr integration commands:");
    for action in ["install", "uninstall"] {
        for target in INTEGRATION_TARGET_LABELS {
            eprintln!("  shepr integration {action} {target}");
        }
    }
    eprintln!("  shepr integration status [--outdated-only]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_label_parses_and_every_target_is_listed() {
        for label in INTEGRATION_TARGET_LABELS {
            let parsed = parse_integration_target(&[label.to_string()], "install")
                .expect("test precondition");
            assert!(parsed.is_some(), "listed label {label} does not parse");
        }
        for target in [
            IntegrationTarget::Pi,
            IntegrationTarget::Omp,
            IntegrationTarget::Claude,
            IntegrationTarget::Codex,
            IntegrationTarget::Copilot,
            IntegrationTarget::Devin,
            IntegrationTarget::Droid,
            IntegrationTarget::Kimi,
            IntegrationTarget::Opencode,
            IntegrationTarget::Kilo,
            IntegrationTarget::Hermes,
            IntegrationTarget::Qodercli,
            IntegrationTarget::Qwen,
            IntegrationTarget::Cursor,
            IntegrationTarget::Mastracode,
            IntegrationTarget::AntigravityCli,
            IntegrationTarget::Grok,
        ] {
            let label = crate::integration::integration_target_label(target);
            assert!(
                INTEGRATION_TARGET_LABELS.contains(&label),
                "{label} missing from the CLI target list"
            );
        }
    }
}
