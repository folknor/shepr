//! The build identity recipe in the workspace build script, driven with
//! stated inputs over a scratch source tree.

use std::ffi::OsString;
use std::path::Path;

use shepr_test_support::ScratchDir;

use super::build_script::{
    ProfileInputs, UNIDENTIFIABLE_BUILD_ID, build_id, profile_constant_source,
};
use super::{builds_match, is_identifiable_build_id};

/// A minimal tree with every required source input.
fn source_tree(label: &str) -> ScratchDir {
    let root = ScratchDir::new(label);
    for (path, contents) in [
        ("Cargo.toml", "[package]\nname = \"fixture\"\n"),
        ("Cargo.lock", "version = 4\n"),
        ("build.rs", "fn main() {}\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("crates/lib/src/lib.rs", "pub fn f() {}\n"),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
        std::fs::write(path, contents).expect("test precondition");
    }
    root
}

/// The inputs cargo hands a build script for `profile`, with the resolved
/// settings that profile has by default.
fn profile(profile: &str) -> ProfileInputs {
    let (opt_level, debug) = match profile {
        "release" => ("3", "false"),
        _ => ("0", "true"),
    };
    let set = |name: &str, value: &str| (name.to_owned(), Some(OsString::from(value)));
    ProfileInputs {
        vars: vec![
            set("PROFILE", profile),
            set("OPT_LEVEL", opt_level),
            set("DEBUG", debug),
            set("TARGET", "x86_64-unknown-linux-gnu"),
            ("RUSTFLAGS".to_owned(), None),
            set("CARGO_CFG_TARGET_OS", "linux"),
        ],
        rustc_version: Some("rustc 1.99.0 (fixture 2026-01-01)".to_owned()),
    }
}

fn with_var(mut inputs: ProfileInputs, name: &str, value: Option<&str>) -> ProfileInputs {
    inputs.vars.retain(|(existing, _)| existing != name);
    inputs
        .vars
        .push((name.to_owned(), value.map(OsString::from)));
    inputs
}

fn id(root: &Path, inputs: &ProfileInputs) -> String {
    build_id(root, inputs).expect("the fixture tree has every required input")
}

#[test]
fn one_tree_and_profile_give_one_identifiable_identity() {
    let tree = source_tree("identity-stable");
    let first = id(&tree, &profile("debug"));
    assert!(is_identifiable_build_id(&first), "{first}");
    assert_eq!(first, id(&tree, &profile("debug")));
    assert!(builds_match(&first, &id(&tree, &profile("debug"))));
}

/// The reason the profile is an input: a dev build of the installed tree must
/// not pass as the installed release build.
#[test]
fn a_dev_and_a_release_build_of_one_tree_differ() {
    let tree = source_tree("identity-profile");
    let dev = id(&tree, &profile("debug"));
    let release = id(&tree, &profile("release"));
    assert_ne!(dev, release);
    assert!(!builds_match(&dev, &release));
}

#[test]
fn every_profile_input_moves_the_identity() {
    let tree = source_tree("identity-profile-inputs");
    let base = id(&tree, &profile("debug"));
    let mut changed = vec![
        with_var(profile("debug"), "OPT_LEVEL", Some("1")),
        with_var(profile("debug"), "DEBUG", Some("false")),
        with_var(
            profile("debug"),
            "TARGET",
            Some("aarch64-unknown-linux-gnu"),
        ),
        with_var(profile("debug"), "RUSTFLAGS", Some("-Ctarget-cpu=native")),
        // Unset and empty are distinct inputs.
        with_var(profile("debug"), "RUSTFLAGS", Some("")),
        with_var(profile("debug"), "CARGO_CFG_TARGET_FEATURE", Some("sse2")),
        with_var(profile("debug"), "CARGO_PROFILE_DEV_LTO", Some("true")),
    ];
    let mut compiler = profile("debug");
    compiler.rustc_version = Some("rustc 1.99.1 (fixture 2026-02-01)".to_owned());
    changed.push(compiler);
    for inputs in &changed {
        let moved = id(&tree, inputs);
        assert!(is_identifiable_build_id(&moved), "{moved}");
        assert_ne!(moved, base, "{inputs:?}");
    }
}

#[test]
fn a_source_change_moves_the_identity() {
    let tree = source_tree("identity-source");
    let before = id(&tree, &profile("debug"));
    std::fs::write(tree.join("crates/lib/src/lib.rs"), "pub fn g() {}\n")
        .expect("test precondition");
    assert_ne!(before, id(&tree, &profile("debug")));
}

/// A required input that is missing refuses the build, instead of dropping out
/// of the identity so that two trees differing only in it hash alike.
#[test]
fn a_missing_required_input_refuses_the_build() {
    for missing in ["Cargo.lock", "Cargo.toml", "build.rs"] {
        let tree = source_tree("identity-missing");
        std::fs::remove_file(tree.join(missing)).expect("test precondition");
        let error = build_id(&tree, &profile("debug"))
            .expect_err("a missing required input refuses")
            .to_string();
        assert!(error.contains(missing), "{missing}: {error}");
    }
    let tree = source_tree("identity-missing-tree");
    std::fs::remove_dir_all(tree.join("crates")).expect("test precondition");
    assert!(build_id(&tree, &profile("debug")).is_err());
}

/// A build that cannot say how it was built states the unidentifiable marker,
/// and that marker matches nothing, not even another unidentifiable build.
#[test]
fn unestablished_profile_inputs_are_unidentifiable_and_match_nothing() {
    let tree = source_tree("identity-unidentifiable");
    let mut no_compiler = profile("debug");
    no_compiler.rustc_version = None;
    for inputs in [
        no_compiler,
        with_var(profile("debug"), "PROFILE", None),
        with_var(profile("debug"), "OPT_LEVEL", None),
        with_var(profile("debug"), "DEBUG", None),
        with_var(profile("debug"), "TARGET", None),
    ] {
        let unidentifiable = id(&tree, &inputs);
        assert_eq!(unidentifiable, UNIDENTIFIABLE_BUILD_ID, "{inputs:?}");
        assert!(!is_identifiable_build_id(&unidentifiable));
        assert!(!builds_match(&unidentifiable, &unidentifiable));
        assert!(!builds_match(
            &unidentifiable,
            &id(&tree, &profile("debug"))
        ));
    }
}

/// The marker travels in the preamble's fixed sixteen-byte identity field, so
/// it must fill that field with printable ASCII to be reported intact.
#[test]
fn the_unidentifiable_marker_fits_the_preamble_field() {
    assert_eq!(UNIDENTIFIABLE_BUILD_ID.len(), 16);
    assert!(
        UNIDENTIFIABLE_BUILD_ID
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
    );
}

#[test]
fn only_sixteen_lowercase_hex_digits_are_an_identity() {
    assert!(is_identifiable_build_id("0123456789abcdef"));
    for not_an_identity in [
        "",
        "0123456789abcde",
        "0123456789abcdef0",
        "0123456789ABCDEF",
        "0123456789abcdeg",
        UNIDENTIFIABLE_BUILD_ID,
    ] {
        assert!(
            !is_identifiable_build_id(not_an_identity),
            "{not_an_identity:?}"
        );
        assert!(!builds_match(not_an_identity, not_an_identity));
    }
}

#[test]
fn profile_constant_states_the_cargo_profile_verbatim() {
    assert!(
        profile_constant_source("release")
            .contains("pub(crate) const BUILD_PROFILE: &str = \"release\";")
    );
    assert!(
        profile_constant_source("debug")
            .contains("pub(crate) const BUILD_PROFILE: &str = \"debug\";")
    );
}
