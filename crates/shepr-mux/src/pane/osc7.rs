//! Working-directory reports (OSC 7, OSC 9;9, OSC 1337 CurrentDir): turning
//! the raw payload the terminal core framed into a path this machine can use.

use std::path::PathBuf;

use shepr_platform::HostNames;
use shepr_vt::WorkingDirectoryReport;

/// Parses an OSC 7 cwd report. `local_host` is the host names the server
/// resolved at startup (`None` when it could not), the ones the pane's
/// `file://` reports are matched against.
pub(super) fn parse_reported_cwd(
    report: &WorkingDirectoryReport,
    local_host: Option<&HostNames>,
) -> Option<PathBuf> {
    let payload = match report {
        WorkingDirectoryReport::Uri(payload) | WorkingDirectoryReport::Path(payload) => payload,
    };
    let value = std::str::from_utf8(payload).ok()?.trim();
    match report {
        // A hand-rolled prompt may send a bare absolute path in OSC 7; any
        // other scheme (kitty's `kitty-shell-cwd://`, say) is not a cwd.
        WorkingDirectoryReport::Uri(_) if !value.starts_with('/') => {
            parse_file_uri_cwd(value, local_host)
        }
        WorkingDirectoryReport::Uri(_) | WorkingDirectoryReport::Path(_) => {
            let path = value.trim_matches('"');
            (!path.is_empty()).then(|| PathBuf::from(path))
        }
    }
}

/// Parse a `file://` cwd report, accepting an empty host, `localhost`, or
/// `local_host`: standard shell integrations (vte.sh for bash/zsh, fish)
/// report `file://$HOSTNAME/path`, so the machine's own name must be
/// accepted. Any other host is a different machine (for example a shell
/// reached over SSH inside the pane), whose path means nothing here.
fn parse_file_uri_cwd(uri: &str, local_host: Option<&HostNames>) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else if let Some(slash) = rest.find('/') {
        let host = &rest[..slash];
        if !(host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || local_host.is_some_and(|local| is_same_host(host, local)))
        {
            return None;
        }
        &rest[slash..]
    } else {
        rest
    };
    // URI queries and fragments are metadata, not part of the reported cwd.
    // Strip their raw delimiters before percent decoding so encoded `%3F` and
    // `%23` remain valid path characters.
    let path_end = path.find(['?', '#']).unwrap_or(path.len());
    let path = &path[..path_end];
    let path = percent_decode_utf8(path)?;
    Some(PathBuf::from(path))
}

/// Host names compare case-insensitively. A reported name matches the local
/// full name, and a reported unqualified name also matches the local short
/// name. A reported qualified name is not matched by the short name alone:
/// its domain could name another machine.
fn is_same_host(reported: &str, local: &HostNames) -> bool {
    reported.eq_ignore_ascii_case(local.full())
        || (!reported.contains('.') && reported.eq_ignore_ascii_case(local.short()))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            output.push(hex_value(hi)? * 16 + hex_value(lo)?);
            idx += 3;
        } else {
            output.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_cwd_parses_file_uri_and_bare_paths() {
        assert_eq!(
            parse_reported_cwd(
                &WorkingDirectoryReport::Uri(b"file:///tmp/shepr%20repo".to_vec()),
                None
            ),
            Some(std::path::PathBuf::from("/tmp/shepr repo"))
        );
        assert_eq!(
            parse_reported_cwd(&WorkingDirectoryReport::Uri(b"/tmp/bare".to_vec()), None),
            Some(std::path::PathBuf::from("/tmp/bare"))
        );
        assert_eq!(
            parse_reported_cwd(&WorkingDirectoryReport::Path(b"/tmp/path".to_vec()), None),
            Some(std::path::PathBuf::from("/tmp/path"))
        );
    }

    #[test]
    fn reported_cwd_rejects_other_uri_schemes() {
        assert_eq!(
            parse_reported_cwd(
                &WorkingDirectoryReport::Uri(b"kitty-shell-cwd://host/tmp".to_vec()),
                names("host").as_ref()
            ),
            None
        );
    }

    fn names(node_name: &str) -> Option<HostNames> {
        HostNames::from_node_name(node_name)
    }

    #[test]
    fn reported_cwd_rejects_invalid_or_empty_values() {
        assert_eq!(
            parse_reported_cwd(&WorkingDirectoryReport::Path(Vec::new()), None),
            None
        );
        assert_eq!(
            parse_reported_cwd(&WorkingDirectoryReport::Path(vec![0xff]), None),
            None
        );
        assert_eq!(
            parse_file_uri_cwd("file://remote/tmp", names("workstation").as_ref()),
            None
        );
        assert_eq!(parse_file_uri_cwd("file://remote/tmp", None), None);
    }

    #[test]
    fn reported_cwd_accepts_the_machines_own_hostname() {
        let expected = Some(std::path::PathBuf::from("/home/me/src"));
        for (uri, local) in [
            ("file://workstation/home/me/src", "workstation"),
            ("file://WorkStation/home/me/src", "workstation"),
            ("file://workstation/home/me/src", "workstation.lan"),
            ("file://workstation.lan/home/me/src", "workstation.lan"),
            ("file://WorkStation.LAN/home/me/src", "workstation.lan"),
            ("file://localhost/home/me/src", "workstation"),
        ] {
            assert_eq!(
                parse_file_uri_cwd(uri, names(local).as_ref()),
                expected,
                "{uri} on {local}"
            );
        }
        assert_eq!(
            parse_file_uri_cwd(
                "file://workstation.other/home/me/src",
                names("workstation.lan").as_ref()
            ),
            None
        );
        assert_eq!(
            parse_file_uri_cwd(
                "file://workstation.other/home/me/src",
                names("workstation").as_ref()
            ),
            None
        );
        assert_eq!(
            parse_file_uri_cwd(
                "file://workstation.lan/home/me/src",
                names("workstation").as_ref()
            ),
            None
        );
    }

    /// A shell that reports the machine's fully qualified name is this
    /// machine, which is what `$HOSTNAME` holds on a host whose node name is
    /// qualified.
    #[test]
    fn reported_cwd_accepts_the_fully_qualified_node_name() {
        let report = WorkingDirectoryReport::Uri(b"file://box.lan/srv/app".to_vec());
        assert_eq!(
            parse_reported_cwd(&report, names("box.lan").as_ref()),
            Some(std::path::PathBuf::from("/srv/app"))
        );
        assert_eq!(parse_reported_cwd(&report, names("box").as_ref()), None);
        assert_eq!(
            parse_reported_cwd(&report, names("box.other").as_ref()),
            None
        );
    }

    /// The report is matched against the host it is given, never one the
    /// parser looks up itself: with no known local name, only an empty host
    /// or `localhost` is this machine.
    #[test]
    fn reported_cwd_matches_only_the_given_local_host() {
        let report = WorkingDirectoryReport::Uri(b"file://workstation/tmp/shepr%20repo".to_vec());
        assert_eq!(
            parse_reported_cwd(&report, names("workstation").as_ref()),
            Some(std::path::PathBuf::from("/tmp/shepr repo"))
        );
        assert_eq!(
            parse_reported_cwd(&report, names("buildbox").as_ref()),
            None
        );
        assert_eq!(parse_reported_cwd(&report, None), None);
        assert_eq!(
            parse_reported_cwd(
                &WorkingDirectoryReport::Uri(b"file://localhost/tmp".to_vec()),
                None
            ),
            Some(std::path::PathBuf::from("/tmp"))
        );
    }
}
