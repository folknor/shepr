//! Requests over https through the system `curl`, supervised as a child process. shepr
//! compiles in no TLS. Every option, the URL and the headers reach curl
//! through its config on stdin, so nothing secret is in its argv or
//! environment; curl reads no curlrc (`-q`) and gets an empty environment.
//! Response headers come back on their own pipe, the body on stdout, so body
//! bytes can never be read as framing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shepr_platform::supervised::{
    ChildBudget, ChildEnd, ChildStream, Overflow, PreparedChild, StreamSpec, SupervisedRun,
    run_supervised,
};

use crate::limits::{
    CURL_PROBE_BUDGET, MAX_CURL_STDERR_BYTES, MAX_CURL_VERSION_BYTES, MAX_HEADER_LINE_BYTES,
    MAX_RESPONSE_BLOCKS, MAX_RESPONSE_BODY_BYTES, MAX_RESPONSE_HEADER_BYTES, REQUEST_BUDGET,
    RETRY_AFTER_MAX,
};
use crate::model::FailureClass;
use crate::secret::{Secret, SecretBuffer};

/// One request slot per host: a curl still unreaped holds it, so a stuck
/// request blocks only its own host and never piles up processes.
static CLAUDE_HOST_CHILDREN: ChildBudget = ChildBudget::new("usage-anthropic", 1);
static CODEX_HOST_CHILDREN: ChildBudget = ChildBudget::new("usage-chatgpt", 1);
static PROBE_CHILDREN: ChildBudget = ChildBudget::new("usage-probe", 1);

/// Where curl is looked for. Only these fixed system locations are used,
/// never `PATH`.
const CURL_CANDIDATES: [&str; 3] = ["/usr/bin/curl", "/bin/curl", "/usr/local/bin/curl"];

/// One request: a fixed URL and its headers. Header values come from the
/// caller's endpoint table and credentials and are validated before use.
pub(crate) struct Request {
    pub(crate) host: Host,
    pub(crate) url: &'static str,
    pub(crate) bearer: Secret,
    pub(crate) headers: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Host {
    Anthropic,
    ChatGpt,
}

impl Host {
    fn budget(self) -> &'static ChildBudget {
        match self {
            Self::Anthropic => &CLAUDE_HOST_CHILDREN,
            Self::ChatGpt => &CODEX_HOST_CHILDREN,
        }
    }
}

/// What a request produced. HTTP evidence survives a failed transfer: a
/// complete 429 header block is kept even when its body timed out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// A 200 with a complete, bounded body and a completed transfer.
    Success(serde_json::Value),
    /// A final response other than a usable 200.
    Status {
        status: u16,
        retry_after: RetryAfter,
    },
    /// No final response could be used.
    Failed(FailureClass),
    /// The host's request slot is held by an unreaped earlier request.
    HostBlocked,
    /// The request was refused locally, before anything was sent.
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A header value holds a control character or is not ASCII.
    InvalidHeaderValue,
}

/// A Retry-After hint, already validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryAfter {
    None,
    /// A positive delay, clamped to [`RETRY_AFTER_MAX`].
    Delay(Duration),
}

/// The usable curl: its absolute path, checked once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Curl {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurlProblem {
    /// No acceptable executable in the fixed locations.
    Missing,
    /// Found, but its probe failed or it lacks the https protocol.
    Unusable,
}

/// Finds and probes curl. Blocking; run off the scheduler thread.
pub(crate) fn find_curl() -> Result<Curl, CurlProblem> {
    let path = CURL_CANDIDATES
        .iter()
        .find_map(|candidate| trusted_executable(Path::new(candidate)))
        .ok_or(CurlProblem::Missing)?;
    probe(&path)?;
    Ok(Curl { path })
}

/// The fully resolved location of `path` when it is a regular file that no
/// other user can replace: the file and every directory from it up to the
/// root are owned by root or this user and are neither group- nor
/// world-writable. The resolved path is what runs, so no symlink along the
/// candidate can redirect it afterwards.
fn trusted_executable(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let resolved = std::fs::canonicalize(path).ok()?;
    let trusted = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            let owner_ok = metadata.uid() == 0 || metadata.uid() == shepr_platform::effective_uid();
            owner_ok && metadata.mode() & 0o022 == 0 && !metadata.file_type().is_symlink()
        })
    };
    let is_file = std::fs::metadata(&resolved).is_ok_and(|metadata| metadata.is_file());
    (is_file && resolved.ancestors().all(trusted)).then_some(resolved)
}

fn probe(path: &Path) -> Result<(), CurlProblem> {
    let program = path.to_path_buf();
    let report = run_supervised(
        SupervisedRun {
            budget: &PROBE_CHILDREN,
            extra_pipes: 0,
            streams: vec![StreamSpec {
                stream: ChildStream::Stdout,
                cap: MAX_CURL_VERSION_BYTES,
                overflow: Overflow::Terminate,
            }],
            // clock-io-ok: the probe's deadline bounds a real child.
            deadline: Instant::now() + CURL_PROBE_BUDGET,
        },
        move |_| PreparedChild {
            command: curl_command(&program, &["-q", "--version"]),
            stdin: None,
        },
    );
    let ChildEnd::Exited(status) = report.end else {
        return Err(CurlProblem::Unusable);
    };
    let output = report
        .streams
        .first()
        .map_or_else(Default::default, |stream| {
            String::from_utf8_lossy(&stream.bytes)
        });
    let has_https = output
        .lines()
        .find_map(|line| line.strip_prefix("Protocols:"))
        .is_some_and(|protocols| protocols.split_whitespace().any(|name| name == "https"));
    if status.success() && output.starts_with("curl ") && has_https {
        Ok(())
    } else {
        Err(CurlProblem::Unusable)
    }
}

fn curl_command(program: &Path, args: &[&str]) -> std::process::Command {
    // host-program-ok: production fetches usage through the system curl
    let mut command = shepr_platform::child_command(program, Path::new("/"));
    command.env_clear().args(args);
    command
}

impl Curl {
    /// Runs one request. Blocking for at most the request budget plus the
    /// supervisor's grace; run off the scheduler thread.
    pub(crate) fn fetch(&self, request: Request, user_agent: &str) -> Outcome {
        let mut headers = vec![("Accept", "application/json".to_owned())];
        headers.push(("Accept-Encoding", "identity".to_owned()));
        headers.extend(request.headers);
        let mut authorization = b"Bearer ".to_vec();
        authorization.extend_from_slice(request.bearer.expose());
        let authorization = SecretBuffer(authorization);
        if !headers
            .iter()
            .all(|(_, value)| valid_header_value(value.as_bytes()))
            || !valid_header_value(&authorization.0)
            || !valid_header_value(user_agent.as_bytes())
        {
            return Outcome::Refused(Refusal::InvalidHeaderValue);
        }
        let program = self.path.clone();
        let url = request.url;
        let user_agent = user_agent.to_owned();
        // clock-io-ok: the request's deadline bounds a real child.
        let deadline = Instant::now() + REQUEST_BUDGET;
        let report = run_supervised(
            SupervisedRun {
                budget: request.host.budget(),
                extra_pipes: 1,
                streams: vec![
                    StreamSpec {
                        stream: ChildStream::Extra(0),
                        cap: MAX_RESPONSE_HEADER_BYTES,
                        overflow: Overflow::Terminate,
                    },
                    StreamSpec {
                        stream: ChildStream::Stdout,
                        cap: MAX_RESPONSE_BODY_BYTES,
                        overflow: Overflow::Terminate,
                    },
                    StreamSpec {
                        stream: ChildStream::Stderr,
                        cap: MAX_CURL_STDERR_BYTES,
                        overflow: Overflow::Discard,
                    },
                ],
                deadline,
            },
            move |fds| {
                let header_fd = fds.first().copied().unwrap_or(-1);
                // The config holds the bearer; it lives only in this pipe write.
                let config = curl_config(url, &authorization.0, &headers, &user_agent, header_fd);
                PreparedChild {
                    command: curl_command(&program, &["-q", "--config", "-"]),
                    stdin: Some(config),
                }
            },
        );
        classify(&report)
    }
}

/// A header value safe to put on the wire: visible ASCII and spaces only, so
/// nothing can end the line early or inject another header.
fn valid_header_value(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|&byte| byte == b' ' || byte.is_ascii_graphic())
}

/// Quotes a value for curl's config grammar: backslash and double quote are
/// escaped, everything else is literal. Values are validated first, so no
/// control character reaches here.
fn quoted(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for &byte in value {
        if byte == b'\\' || byte == b'"' {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
    out
}

fn curl_config(
    url: &str,
    authorization: &[u8],
    headers: &[(&str, String)],
    user_agent: &str,
    header_fd: std::os::fd::RawFd,
) -> Vec<u8> {
    let mut config = Vec::new();
    let mut option = |name: &str, value: Option<&[u8]>| {
        config.extend_from_slice(name.as_bytes());
        if let Some(value) = value {
            config.extend_from_slice(b" = ");
            config.extend_from_slice(&quoted(value));
        }
        config.push(b'\n');
    };
    option("url", Some(url.as_bytes()));
    let mut auth_line = b"Authorization: ".to_vec();
    auth_line.extend_from_slice(authorization);
    let auth_line = SecretBuffer(auth_line);
    option("header", Some(&auth_line.0));
    for (name, value) in headers {
        option("header", Some(format!("{name}: {value}").as_bytes()));
    }
    option("user-agent", Some(user_agent.as_bytes()));
    option("proto", Some(b"=https"));
    option("tlsv1.2", None);
    option("noproxy", Some(b"*"));
    option(
        "max-time",
        Some(REQUEST_BUDGET.as_secs().to_string().as_bytes()),
    );
    option(
        "max-filesize",
        Some(MAX_RESPONSE_BODY_BYTES.to_string().as_bytes()),
    );
    option(
        "dump-header",
        Some(format!("/proc/self/fd/{header_fd}").as_bytes()),
    );
    option("silent", None);
    option("show-error", None);
    config
}

/// The final response of a header stream: its status and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
}

impl FinalResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Parses curl's header dump: response blocks, each a status line and header
/// lines ending in a blank line. Interim 1xx blocks are skipped; the last
/// complete non-1xx block is the final response. Lines after it that do not
/// start a block are trailers and are ignored. `None` when no complete final
/// block is present or a bound is exceeded.
pub(crate) fn parse_final_response(stream: &[u8]) -> Option<FinalResponse> {
    let text = std::str::from_utf8(stream).ok()?;
    // A stream cut mid-line is incomplete; nothing in it is trusted.
    if !text.is_empty() && !text.ends_with('\n') {
        return None;
    }
    let mut blocks = 0;
    let mut final_response: Option<FinalResponse> = None;
    let mut current: Option<FinalResponse> = None;
    for line in text.split_terminator('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() > MAX_HEADER_LINE_BYTES {
            return None;
        }
        if let Some(block) = current.as_mut() {
            if line.is_empty() {
                let block = current.take()?;
                if !(100..200).contains(&block.status) {
                    final_response = Some(block);
                }
            } else {
                let (name, value) = line.split_once(':')?;
                block
                    .headers
                    .push((name.trim().to_owned(), value.trim().to_owned()));
            }
            continue;
        }
        if let Some(status) = status_line(line) {
            blocks += 1;
            if blocks > MAX_RESPONSE_BLOCKS {
                return None;
            }
            current = Some(FinalResponse {
                status,
                headers: Vec::new(),
            });
            continue;
        }
        // Outside a block only trailers of a final response may appear: a
        // header field, or the blank line ending them. Anything else, or
        // anything before the first status line, is broken framing.
        let trailer_ok = final_response.is_some() && (line.is_empty() || line.contains(':'));
        if !trailer_ok {
            return None;
        }
    }
    // A block still open at the end was never finished.
    if current.is_some() {
        return None;
    }
    final_response
}

fn status_line(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("HTTP/")?;
    let (_version, rest) = rest.split_once(' ')?;
    let code = rest.split(' ').next()?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    code.parse().ok()
}

fn retry_after(response: &FinalResponse, now: std::time::SystemTime) -> RetryAfter {
    let Some(value) = response.header("Retry-After") else {
        return RetryAfter::None;
    };
    let delay = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse::<u64>().ok().map(Duration::from_secs)
    } else {
        crate::time::parse_http_date(value).and_then(|at| at.duration_since(now).ok())
    };
    match delay {
        Some(delay) if !delay.is_zero() => RetryAfter::Delay(delay.min(RETRY_AFTER_MAX)),
        // Zero, malformed or past: no hint, and never an immediate retry.
        _ => RetryAfter::None,
    }
}

/// curl exit codes that name a failure class (the exit codes section of
/// `man curl`).
fn curl_exit_class(code: i32) -> FailureClass {
    match code {
        5 | 6 => FailureClass::Dns,
        7 => FailureClass::Connect,
        28 => FailureClass::Timeout,
        35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91 => {
            FailureClass::Tls
        }
        63 => FailureClass::Response,
        _ => FailureClass::Transport,
    }
}

fn classify(report: &shepr_platform::supervised::ChildReport) -> Outcome {
    // clock-io-ok: Retry-After dates are relative to the real wall clock.
    classify_at(report, std::time::SystemTime::now())
}

fn classify_at(
    report: &shepr_platform::supervised::ChildReport,
    now: std::time::SystemTime,
) -> Outcome {
    let headers = report.streams.first();
    let body = report.streams.get(1);
    let final_response = headers
        .filter(|stream| !stream.overflowed)
        .and_then(|stream| parse_final_response(&stream.bytes));
    let transfer_complete = match &report.end {
        ChildEnd::Exited(status) => status.code() == Some(0),
        _ => false,
    };
    // A complete non-200 response is evidence on its own, whatever happened
    // to the body afterwards.
    if let Some(response) = &final_response
        && response.status != 200
    {
        return Outcome::Status {
            status: response.status,
            retry_after: retry_after(response, now),
        };
    }
    match &report.end {
        ChildEnd::Exhausted => return Outcome::HostBlocked,
        ChildEnd::TimedOut => return Outcome::Failed(FailureClass::Timeout),
        ChildEnd::Overflowed => return Outcome::Failed(FailureClass::Response),
        ChildEnd::Spawn(_) | ChildEnd::Supervision(_) => {
            return Outcome::Failed(FailureClass::Transport);
        }
        // A signalled curl has no exit code and so is a transport failure.
        ChildEnd::Exited(status) if !transfer_complete => {
            return Outcome::Failed(
                status
                    .code()
                    .map_or(FailureClass::Transport, curl_exit_class),
            );
        }
        ChildEnd::Exited(_) => {}
    }
    let Some(response) = final_response else {
        return Outcome::Failed(FailureClass::Response);
    };
    // Every Content-Encoding field, each value of each, must be identity.
    let encoding_ok = response
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("Content-Encoding"))
        .flat_map(|(_, value)| value.split(','))
        .all(|encoding| encoding.trim().eq_ignore_ascii_case("identity"));
    // A 200 is accepted only from a header stream read cleanly to its end.
    let headers_clean =
        headers.is_some_and(|stream| stream.eof && !stream.overflowed && stream.error.is_none());
    let Some(body) = body.filter(|body| body.eof && !body.overflowed && body.error.is_none())
    else {
        return Outcome::Failed(FailureClass::Response);
    };
    if !headers_clean {
        return Outcome::Failed(FailureClass::Response);
    }
    if !encoding_ok {
        return Outcome::Failed(FailureClass::Response);
    }
    match serde_json::from_slice(&body.bytes) {
        Ok(value) => Outcome::Success(value),
        Err(_) => Outcome::Failed(FailureClass::Response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_platform::supervised::{ChildReport, StreamReport};
    use std::os::unix::process::ExitStatusExt;
    use std::time::UNIX_EPOCH;

    fn stream(bytes: &[u8]) -> StreamReport {
        StreamReport {
            bytes: bytes.to_vec(),
            overflowed: false,
            eof: true,
            error: None,
        }
    }

    fn report(end: ChildEnd, headers: &[u8], body: &[u8]) -> ChildReport {
        ChildReport {
            end,
            streams: vec![stream(headers), stream(body), stream(b"")],
            stdin_error: None,
        }
    }

    fn exited(code: i32) -> ChildEnd {
        ChildEnd::Exited(std::process::ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn config_values_are_quoted_and_escaped() {
        let config = curl_config(
            "https://example.test/u",
            b"Bearer a\"b\\c",
            &[("X-Test", "v".to_owned())],
            "shepr/1",
            7,
        );
        let text = String::from_utf8(config).expect("ascii config");
        assert!(text.contains("url = \"https://example.test/u\"\n"));
        assert!(text.contains("header = \"Authorization: Bearer a\\\"b\\\\c\"\n"));
        assert!(text.contains("header = \"X-Test: v\"\n"));
        assert!(text.contains("dump-header = \"/proc/self/fd/7\"\n"));
        assert!(text.contains("noproxy = \"*\"\n"));
        assert!(!text.contains("location"));
    }

    #[test]
    fn header_values_with_line_breaks_or_controls_are_refused() {
        assert!(valid_header_value(b"Bearer abc.def-ghi_jkl"));
        for bad in [
            &b"a\nb"[..],
            b"a\rb",
            b"a\0b",
            b"a\tb",
            b"",
            "\u{e9}".as_bytes(),
        ] {
            assert!(!valid_header_value(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_final_response_skips_interim_blocks_and_trailers() {
        let stream = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 429\r\nretry-after: 120\r\ncontent-type: application/json\r\n\r\ntrailer: x\r\n";
        let response = parse_final_response(stream).expect("final");
        assert_eq!(response.status, 429);
        assert_eq!(response.header("Retry-After"), Some("120"));
        assert_eq!(response.header("trailer"), None);
    }

    #[test]
    fn an_incomplete_block_is_no_final_response() {
        assert_eq!(
            parse_final_response(b"HTTP/2 200\r\ncontent-type: x\r\n"),
            None
        );
        assert_eq!(parse_final_response(b""), None);
        assert_eq!(parse_final_response(b"HTTP/2 20x\r\n\r\n"), None);
        assert_eq!(parse_final_response(b"HTTP/2 200\r\n\r\nHTTP/2 429"), None);
    }

    #[test]
    fn broken_framing_rejects_the_whole_stream() {
        // A later block left unfinished does not leave the earlier one standing.
        assert_eq!(
            parse_final_response(b"HTTP/2 200\r\n\r\nHTTP/2 429\r\nretry-after: 5\r\n"),
            None
        );
        // Noise before the first status line, or a non-field after a final one.
        assert_eq!(parse_final_response(b"garbage\r\nHTTP/2 200\r\n\r\n"), None);
        assert_eq!(
            parse_final_response(b"HTTP/2 200\r\n\r\nnot a field\r\n"),
            None
        );
        // A trailer field after the final response is allowed.
        assert!(parse_final_response(b"HTTP/2 200\r\n\r\nx-trailer: 1\r\n").is_some());
    }

    #[test]
    fn every_content_encoding_field_must_be_identity() {
        let doubled = classify_at(
            &report(
                exited(0),
                b"HTTP/2 200\r\ncontent-encoding: identity\r\ncontent-encoding: gzip\r\n\r\n",
                b"{}",
            ),
            UNIX_EPOCH,
        );
        assert_eq!(doubled, Outcome::Failed(FailureClass::Response));
        let listed = classify_at(
            &report(
                exited(0),
                b"HTTP/2 200\r\ncontent-encoding: identity, br\r\n\r\n",
                b"{}",
            ),
            UNIX_EPOCH,
        );
        assert_eq!(listed, Outcome::Failed(FailureClass::Response));
    }

    #[test]
    fn a_429_survives_a_body_timeout_and_zero_retry_after_is_no_hint() {
        let classified = classify_at(
            &report(
                ChildEnd::TimedOut,
                b"HTTP/2 429\r\nretry-after: 0\r\n\r\n",
                b"",
            ),
            UNIX_EPOCH,
        );
        assert_eq!(
            classified,
            Outcome::Status {
                status: 429,
                retry_after: RetryAfter::None
            }
        );
        let classified = classify_at(
            &report(exited(0), b"HTTP/2 429\r\nretry-after: 90\r\n\r\n", b"{}"),
            UNIX_EPOCH,
        );
        assert_eq!(
            classified,
            Outcome::Status {
                status: 429,
                retry_after: RetryAfter::Delay(Duration::from_secs(90))
            }
        );
    }

    #[test]
    fn a_200_needs_a_completed_transfer_identity_encoding_and_json() {
        let ok = classify_at(
            &report(exited(0), b"HTTP/2 200\r\n\r\n", b"{\"a\":1}"),
            UNIX_EPOCH,
        );
        assert_eq!(ok, Outcome::Success(serde_json::json!({"a": 1})));
        let cut = classify_at(
            &report(exited(18), b"HTTP/2 200\r\n\r\n", b"{\"a\":1}"),
            UNIX_EPOCH,
        );
        assert_eq!(cut, Outcome::Failed(FailureClass::Transport));
        let gzip = classify_at(
            &report(
                exited(0),
                b"HTTP/2 200\r\ncontent-encoding: gzip\r\n\r\n",
                b"{}",
            ),
            UNIX_EPOCH,
        );
        assert_eq!(gzip, Outcome::Failed(FailureClass::Response));
        let not_json = classify_at(
            &report(exited(0), b"HTTP/2 200\r\n\r\n", b"<html>"),
            UNIX_EPOCH,
        );
        assert_eq!(not_json, Outcome::Failed(FailureClass::Response));
    }

    #[test]
    fn transport_failures_are_classified_by_curl_exit_code() {
        let dns = classify_at(&report(exited(6), b"", b""), UNIX_EPOCH);
        assert_eq!(dns, Outcome::Failed(FailureClass::Dns));
        let tls = classify_at(&report(exited(60), b"", b""), UNIX_EPOCH);
        assert_eq!(tls, Outcome::Failed(FailureClass::Tls));
        let blocked = classify_at(&report(ChildEnd::Exhausted, b"", b""), UNIX_EPOCH);
        assert_eq!(blocked, Outcome::HostBlocked);
    }

    #[test]
    fn an_http_date_retry_after_is_relative_to_now() {
        let response = FinalResponse {
            status: 429,
            headers: vec![(
                "Retry-After".to_owned(),
                "Sun, 06 Nov 1994 08:49:37 GMT".to_owned(),
            )],
        };
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777 - 30);
        assert_eq!(
            retry_after(&response, now),
            RetryAfter::Delay(Duration::from_secs(30))
        );
        let later = UNIX_EPOCH + Duration::from_secs(784_111_777 + 30);
        assert_eq!(retry_after(&response, later), RetryAfter::None);
    }
}
