//! The two executables of the root package are one installation: a pair built
//! together must report the same `<version>+<build id>` identity, because a
//! client accepts only a server of its own build.

use shepr_test_support::command_in_scratch;

/// The identity a binary prints for `--version`, without its program name.
fn reported_identity(program: &str, label: &str) -> String {
    let output = command_in_scratch(program, label)
        .arg("--version")
        .output()
        .expect("test precondition: the binary runs");
    assert!(
        output.status.success(),
        "{program} --version failed: {}",
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
    let client = reported_identity(env!("CARGO_BIN_EXE_shepr"), "identity-client");
    let server = reported_identity(env!("CARGO_BIN_EXE_shepr-server"), "identity-server");
    assert_eq!(client, server);
    assert_eq!(client, shepr_protocol::build_version());
}

#[test]
fn server_binary_rejects_unknown_arguments() {
    let output = command_in_scratch(env!("CARGO_BIN_EXE_shepr-server"), "identity-usage")
        .arg("server")
        .output()
        .expect("test precondition: the binary runs");
    assert_eq!(output.status.code(), Some(2));
}
