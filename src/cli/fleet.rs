//! What `status --all` and `stop --all` share: the configured machines, read
//! from `client.toml` (the one CLI path that reads it), a plain reason for a
//! machine that could not be reached, and the per-host lines both print.

use shepr_launch::{EndpointFailure, FailureDisposition};

/// This host's own name and every other configured machine.
pub(super) struct Fleet {
    /// The local server's label, as the TUI names it.
    pub(super) local_label: shepr_config::MachineLabel,
    /// The configured machines in config order, this host's own entry dropped.
    pub(super) machines: Vec<shepr_config::MachineConfig>,
}

/// Reads `client.toml` as the TUI does, so a file the TUI refuses is refused
/// here too, and this host's own entry is skipped the same way.
pub(super) fn load(paths: &shepr_paths::AppPaths) -> super::CliResult<Fleet> {
    let config = shepr_config::load_client_validated(paths).map_err(super::CliError::Config)?;
    Ok(Fleet {
        local_label: config.local_label().clone(),
        machines: config.machines().to_vec(),
    })
}

/// Why a machine gave no answer, in a few words and then what SSH said.
pub(super) fn failure_label(error: &std::io::Error) -> String {
    let failure = EndpointFailure::from_error(error);
    let message = failure.message();
    match failure.disposition() {
        FailureDisposition::Authentication | FailureDisposition::PossibleAuthentication => {
            shepr_launch::guidance::fleet_login_hint(message)
        }
        FailureDisposition::HostKey => format!("host key not accepted: {message}"),
        FailureDisposition::Offline => format!("unreachable: {message}"),
        FailureDisposition::Incompatible
        | FailureDisposition::Repair
        | FailureDisposition::Retry => format!("unavailable: {message}"),
    }
}

/// `rows` as lines of a label column padded to the widest label, then the
/// text.
pub(super) fn render_rows(rows: &[(String, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (label, text) in rows {
        let padding = width.saturating_sub(label.chars().count());
        out.push_str(&format!("  {label}{}  {text}\n", " ".repeat(padding)));
    }
    out
}

/// The local server's row label.
pub(super) fn local_row_label(fleet: &Fleet) -> String {
    format!("{} (local)", fleet.local_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_align_on_the_widest_label() {
        let rows = vec![
            ("bygg (local)".to_owned(), "stopped".to_owned()),
            ("dm6".to_owned(), "not running".to_owned()),
        ];
        assert_eq!(
            render_rows(&rows),
            "  bygg (local)  stopped\n  dm6           not running\n"
        );
    }

    #[test]
    fn a_failure_reads_as_its_disposition_and_what_ssh_said() {
        let offline = std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect to host speilegg port 22: timed out",
        );
        assert_eq!(
            failure_label(&offline),
            "unreachable: connect to host speilegg port 22: timed out"
        );
        let incompatible = std::io::Error::other(EndpointFailure::incompatible("no shepr"));
        assert_eq!(failure_label(&incompatible), "unavailable: no shepr");
    }
}
