use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_VERSION_PROBE_OUTPUT: usize = 64 * 1024;

pub(crate) struct AgentVersionRequirement {
    pub label: &'static str,
    pub binary: &'static str,
    pub args: &'static [&'static str],
    pub min_version: &'static str,
}

pub(crate) fn agent_version_requirement(
    target: crate::agent::IntegrationTarget,
) -> Option<AgentVersionRequirement> {
    match target {
        crate::agent::IntegrationTarget::Kimi => Some(AgentVersionRequirement {
            label: "kimi code",
            binary: "kimi",
            args: &["--version"],
            min_version: super::KIMI_MIN_VERSION,
        }),
        _ => None,
    }
}

pub(crate) fn extract_version_triple(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches('v');
        let mut parts = token.splitn(3, '.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next()?.parse().ok()?;
        let patch: u64 = parts
            .next()
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0);
        Some((major, minor, patch))
    })
}

/// Returns `Ok(None)` when the installed agent satisfies the requirement,
/// `Ok(Some(warning))` when the version cannot be determined (install
/// proceeds), and `Err` when the installed agent is too old.
pub(crate) fn enforce_agent_version(
    requirement: &AgentVersionRequirement,
) -> io::Result<Option<String>> {
    let probe = format!("{} {}", requirement.binary, requirement.args.join(" "));
    let output = match run_version_probe(requirement, VERSION_PROBE_TIMEOUT) {
        Ok(Some(output)) if output.status.success() => output,
        Ok(None) => {
            return Ok(Some(format!(
                "{} `{probe}` timed out after {} seconds while verifying the installed version; hooks require {} {} or newer",
                super::INSTALL_WARNING_PREFIX,
                VERSION_PROBE_TIMEOUT.as_secs(),
                requirement.label,
                requirement.min_version
            )));
        }
        _ => {
            return Ok(Some(format!(
                "{} could not run `{probe}` to verify the installed version; hooks require {} {} or newer",
                super::INSTALL_WARNING_PREFIX,
                requirement.label,
                requirement.min_version
            )));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(found) = extract_version_triple(&stdout) else {
        return Ok(Some(format!(
            "{} could not parse the {} version from `{probe}` output; hooks require {} {} or newer",
            super::INSTALL_WARNING_PREFIX,
            requirement.label,
            requirement.label,
            requirement.min_version
        )));
    };
    // The minimum is a compile-time constant (a test pins it as parseable);
    // should it ever fail to parse, refuse the install rather than panic.
    let Some(required) = extract_version_triple(requirement.min_version) else {
        return Err(io::Error::other(format!(
            "shepr's minimum {} version {:?} is not a version number",
            requirement.label, requirement.min_version
        )));
    };

    if found < required {
        return Err(io::Error::other(format!(
            "{label} {}.{}.{} is too old: shepr hooks require {label} {min} or newer. upgrade {label}, then re-run install",
            found.0,
            found.1,
            found.2,
            label = requirement.label,
            min = requirement.min_version
        )));
    }
    Ok(None)
}

/// Polls the version command so a process that exceeds the deadline is killed.
/// Stdout is drained without blocking, so a grandchild that inherits the pipe
/// and outlives the command cannot hold the probe past the deadline. Stderr
/// goes to /dev/null. The probe runs in `/`: a `--version` call reads nothing
/// relative to its directory, and `/` neither pins the caller's directory nor
/// can vanish under it.
fn run_version_probe(
    requirement: &AgentVersionRequirement,
    timeout: Duration,
) -> io::Result<Option<Output>> {
    let mut child = shepr_platform::child_command(requirement.binary, Path::new("/"))
        .args(requirement.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = stop_version_probe(&mut child);
        return Err(io::Error::other("version probe stdout was not captured"));
    };
    // SAFETY: fcntl(2) on the pipe fd `stdout` keeps open; integers only.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        let error = io::Error::last_os_error();
        let _ = stop_version_probe(&mut child);
        return Err(error);
    }
    // SAFETY: as above; only the status flags of our own pipe end change.
    if unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        let error = io::Error::last_os_error();
        let _ = stop_version_probe(&mut child);
        return Err(error);
    }

    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut read_buffer = [0; 4096];
    let mut stdout_closed = false;
    let mut status = None;
    loop {
        if !stdout_closed {
            loop {
                match stdout.read(&mut read_buffer) {
                    Ok(0) => {
                        stdout_closed = true;
                        break;
                    }
                    Ok(read) => {
                        // Keep draining so the child never blocks on a full
                        // pipe, but retain only what a version line needs.
                        let keep = read.min(MAX_VERSION_PROBE_OUTPUT.saturating_sub(output.len()));
                        output.extend_from_slice(&read_buffer[..keep]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = stop_version_probe(&mut child);
                        return Err(error);
                    }
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exited)) => status = Some(exited),
                Ok(None) => {}
                Err(error) => {
                    let _ = stop_version_probe(&mut child);
                    return Err(error);
                }
            }
        }

        if stdout_closed && let Some(status) = status {
            return Ok(Some(Output {
                status,
                stdout: output,
                stderr: Vec::new(),
            }));
        }

        if Instant::now() >= deadline {
            if status.is_none() {
                stop_version_probe(&mut child)?;
            }
            // The direct child may have exited while a grandchild still owns
            // stdout. Drop our pipe end at the deadline instead of waiting for
            // that unrelated process to close it.
            return Ok(None);
        }
        thread::sleep(VERSION_PROBE_POLL_INTERVAL);
    }
}

fn stop_version_probe(child: &mut std::process::Child) -> io::Result<()> {
    if let Err(error) = child.kill()
        && child.try_wait()?.is_none()
    {
        return Err(error);
    }
    child.wait().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_minimum_agent_version_parses() {
        let target = crate::agent::IntegrationTarget::Kimi;
        let requirement = agent_version_requirement(target).expect("test precondition");
        assert!(
            extract_version_triple(requirement.min_version).is_some(),
            "{}",
            requirement.min_version
        );
    }

    #[test]
    fn version_probe_deadline_includes_inherited_stdout() {
        use shepr_test_support::fixture::{self, Held, Step};

        // The probed program exits at once, leaving a child that holds its
        // stdout for 300 ms.
        let args: Vec<&'static str> = fixture::args(&[
            Step::Spawn {
                argv0: "stdout-holder".into(),
                sleep: Duration::from_millis(300),
                held: Held::Stdout,
            },
            Step::Exit(0),
        ])
        .into_iter()
        .map(|token| -> &'static str {
            Box::leak(
                token
                    .into_string()
                    .expect("a UTF-8 fixture token")
                    .into_boxed_str(),
            )
        })
        .collect();
        let requirement = AgentVersionRequirement {
            label: "test command",
            binary: fixture::path_str(),
            args: Box::leak(args.into_boxed_slice()),
            min_version: "0.0.0",
        };
        let started = Instant::now();
        let output = run_version_probe(&requirement, Duration::from_millis(50))
            .expect("test probe should run");

        let elapsed = started.elapsed();
        // Let the grandchild finish so it does not outlive the test process.
        thread::sleep(
            Duration::from_millis(300).saturating_sub(elapsed) + Duration::from_millis(50),
        );

        assert!(output.is_none());
        assert!(elapsed < Duration::from_millis(250));
    }
}
