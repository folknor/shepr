//! The `shepr` and `shepr-server` executables are one installation: a pair
//! built together must report the same `<version>+<build id>` identity, because
//! a client accepts only a server of its own build. `shepr-server` belongs to
//! the `shepr-daemon` package, so cargo does not hand its path to this test;
//! it sits next to the client binary, where the client's own launcher looks
//! for it, and `brokkr.toml` builds it before the tests run.

use std::path::{Path, PathBuf};

use shepr_test_support::command_in_scratch;

/// The `shepr-server` executable beside the client binary under test.
fn server_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_shepr")).with_file_name("shepr-server");
    let metadata = std::fs::metadata(&path).unwrap_or_else(|error| {
        panic!(
            "test precondition: {} is built (package shepr-daemon): {error}",
            path.display()
        )
    });
    assert!(metadata.is_file(), "{} is not a file", path.display());
    path
}

/// The identity a binary prints for `--version`, without its program name.
fn reported_identity(program: &Path, label: &str) -> String {
    let output = command_in_scratch(program, label)
        .arg("--version")
        .output()
        .expect("test precondition: the binary runs");
    assert!(
        output.status.success(),
        "{} --version failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("test precondition: utf-8 output");
    let (_program_name, identity) = text
        .trim_end()
        .split_once(' ')
        .expect("test precondition: the output is `<name> <identity>`");
    identity.to_owned()
}

#[test]
fn client_and_server_binaries_report_one_build_identity() {
    let client = reported_identity(Path::new(env!("CARGO_BIN_EXE_shepr")), "identity-client");
    let server = reported_identity(&server_binary(), "identity-server");
    assert_eq!(client, server);
    assert_eq!(client, shepr_protocol::build_version());
}

#[test]
fn server_binary_rejects_unknown_arguments() {
    let output = command_in_scratch(server_binary(), "identity-usage")
        .arg("server")
        .output()
        .expect("test precondition: the binary runs");
    assert_eq!(output.status.code(), Some(2));
}
