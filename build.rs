//! Build identity.
//!
//! Client and server must be the same build: the wire codec is positional and
//! not self-describing, so two builds that disagree on any message layout
//! decode each other's frames into garbage instead of failing. A hand-bumped
//! protocol number cannot guard that, because nothing forces anyone to bump
//! it. This script fingerprints what decides the wire layout, then hands the
//! result to the crate as `BUILD_ID`, which the handshake preamble, `ping` and
//! `status` compare exactly. Any change to an input yields a new identity, so
//! a stale server, a hand-copied remote binary, or a dev build meeting the
//! installed release server is reported as a mismatch instead of passing as
//! compatible.
//!
//! The inputs are:
//!
//! - The source tree: the root manifest, the lockfile, this script and every
//!   file under `src/` and `crates/`. The wire layout is defined here and
//!   nowhere else. Each named root input is required. A missing or unreadable
//!   one fails the build rather than silently dropping out of the identity,
//!   which would let two trees that differ in it hash alike.
//! - The cargo profile (`PROFILE`, release or debug). It does not change the
//!   wire layout; it is an input so a dev build and the installed release
//!   build of one tree refuse each other, and a dev run never attaches to the
//!   installed server.
//!
//! Nothing else is: the compiler and its version, rustflags, wrappers, linker,
//! profile overrides, the target and its cfg values change how the code is
//! built, not the frames it exchanges. The codec writes explicit
//! little-endian frames and varints, and no wire type is conditional on the
//! target, so one tree built on hosts with different toolchains, settings or
//! architectures speaks one protocol and gets one identity.
//!
//! When cargo does not hand the script `PROFILE`, the identity is
//! [`UNIDENTIFIABLE_BUILD_ID`]. It is not hex, so the comparison in
//! `shepr-protocol` refuses it against every peer, itself included: a build
//! whose inputs are unknown cannot prove it is the same build as anything.
//!
//! The hash is FNV-1a over labelled, length-prefixed records in a fixed
//! order, so the same source and profile give the same identity on every host.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Top-level files hashed alongside the `src/` and `crates/` trees. Every one
/// is required.
const ROOT_INPUTS: [&str; 3] = ["Cargo.toml", "Cargo.lock", "build.rs"];

/// The identity of a build whose profile inputs could not be established.
/// Sixteen ASCII bytes, so it fits the preamble's fixed identity field, and not
/// hex, so it never compares equal to anything.
pub(crate) const UNIDENTIFIABLE_BUILD_ID: &str = "unidentifiable--";

/// The profile variables in the identity, each one cargo sets for every build
/// script. A build missing one cannot say how it was built.
const REQUIRED_PROFILE_VARS: [&str; 1] = ["PROFILE"];

/// Whether `ProfileInputs::from_env` records the variable `name` at all.
#[cfg(test)]
pub(crate) fn is_profile_input(name: &str) -> bool {
    REQUIRED_PROFILE_VARS.contains(&name)
}

struct Fnv(u64);

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    /// Hashes one labelled record, self-delimiting, so moving bytes between a
    /// label and its contents, or between records, changes the result.
    fn write_record(&mut self, label: &str, contents: &[u8]) {
        self.write(label.as_bytes());
        self.write(&[0]);
        let len = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        self.write(&len.to_le_bytes());
        self.write(contents);
    }
}

/// The build-profile half of the identity.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfileInputs {
    /// Every profile variable read, by name, with its raw value or `None` when
    /// unset. Order does not matter; the hash sorts them.
    pub vars: Vec<(String, Option<OsString>)>,
}

impl ProfileInputs {
    /// Reads the profile inputs cargo hands this build script.
    #[expect(
        clippy::disallowed_methods,
        reason = "a build script reads cargo's own variables; shepr_core::env governs the variables shepr processes interpret"
    )]
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "unit tests include this script as a module and state their own inputs"
        )
    )]
    fn from_env() -> Self {
        let vars = REQUIRED_PROFILE_VARS
            .iter()
            .map(|name| ((*name).to_owned(), std::env::var_os(name)))
            .collect();
        Self { vars }
    }

    /// Whether every input that is always present in a real build is present.
    pub(crate) fn is_established(&self) -> bool {
        REQUIRED_PROFILE_VARS.iter().all(|required| {
            self.vars
                .iter()
                .any(|(name, value)| name == required && value.is_some())
        })
    }
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("cannot read build input {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// Path relative to the workspace root with `/` separators, so the hash does
/// not depend on where the tree is checked out.
fn relative_name(root: &Path, path: &Path) -> Result<String, Box<dyn Error>> {
    let relative = path.strip_prefix(root)?;
    let parts = relative
        .components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 path in source tree: {}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("/"))
}

/// The build identity of the tree at `root` built with `profile`.
///
/// # Errors
///
/// A required source input that is missing or unreadable. The build fails on
/// it rather than stamping an identity that silently leaves the input out.
pub(crate) fn build_id(root: &Path, profile: &ProfileInputs) -> Result<String, Box<dyn Error>> {
    let mut files = Vec::new();
    for name in ROOT_INPUTS {
        let path = root.join(name);
        // A stat failure other than absence (EACCES, ELOOP) is reported as
        // itself, not folded into "missing", and a present non-file refuses too.
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(format!(
                    "required build-identity input {} is not a file; the build identity cannot be established without it",
                    path.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "required build-identity input {} is missing; the build identity cannot be established without it",
                    path.display()
                )
                .into());
            }
            Err(error) => {
                return Err(format!(
                    "cannot read build-identity input {}: {error}",
                    path.display()
                )
                .into());
            }
        }
        files.push(path);
    }
    collect_files(&root.join("src"), &mut files)?;
    collect_files(&root.join("crates"), &mut files)?;

    let mut named = files
        .iter()
        .map(|path| relative_name(root, path).map(|name| (name, path)))
        .collect::<Result<Vec<_>, _>>()?;
    named.sort();

    let mut hash = Fnv(FNV_OFFSET);
    for (name, path) in &named {
        let contents = fs::read(path)
            .map_err(|error| format!("cannot read build input {}: {error}", path.display()))?;
        hash.write_record(&format!("source/{name}"), &contents);
    }

    if !profile.is_established() {
        return Ok(UNIDENTIFIABLE_BUILD_ID.to_owned());
    }
    let mut vars = profile.vars.clone();
    vars.sort();
    vars.dedup();
    for (name, value) in &vars {
        // A presence tag plus the raw bytes, so unset, empty and any literal
        // value are distinct inputs.
        let mut bytes = Vec::new();
        match value {
            None => bytes.extend_from_slice(b"unset"),
            Some(value) => {
                bytes.extend_from_slice(b"set:");
                bytes.extend_from_slice(value.as_encoded_bytes());
            }
        }
        hash.write_record(&format!("profile/{name}"), &bytes);
    }
    Ok(format!("{:016x}", hash.0))
}

/// The cargo profile name as a build script sees it (`PROFILE`): `release`
/// for the release profile and every profile inheriting from it, `debug` for
/// the dev profile and its descendants.
///
/// Emitted as its own constant, independent of the build identity, because the
/// identity only tells two builds apart while nothing in it says which paths a
/// build should use. `shepr-paths` reads it to give dev builds a runtime and
/// saved-layout namespace of their own, so a dev server and the installed
/// release server neither share sockets nor overwrite each other's layout.
pub(crate) fn profile_constant_source(profile: &str) -> String {
    format!(
        "/// The cargo profile this crate was built with: `release` or `debug`.\n\
         pub(crate) const BUILD_PROFILE: &str = \"{profile}\";\n"
    )
}

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads cargo's own variables; shepr_core::env governs the variables shepr processes interpret"
)]
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "unit tests include this script as a module for build_id and never run main"
    )
)]
pub(crate) fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?,
    );
    // Only the build scripts of shepr-protocol and shepr-paths include this
    // one (the root package has none), so the workspace root is two levels up.
    let crate_dir_name = manifest_dir.file_name().and_then(|name| name.to_str());
    // shepr-paths includes this script only for the profile constant, so it
    // skips the identity, which would hash the whole tree a second time.
    let stamps_identity = crate_dir_name != Some("shepr-paths");
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("missing workspace root")?
        .to_path_buf();
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);

    let profile = std::env::var("PROFILE")
        .map_err(|error| format!("cargo did not hand the build script PROFILE: {error}"))?;
    fs::write(
        out_dir.join("build_profile.rs"),
        profile_constant_source(&profile),
    )?;
    println!("cargo:rerun-if-changed={}", root.join("build.rs").display());
    if !stamps_identity {
        return Ok(());
    }

    let profile_inputs = ProfileInputs::from_env();
    let build_id = build_id(&root, &profile_inputs)?;

    fs::write(
        out_dir.join("build_identity.rs"),
        format!(
            "/// Fingerprint of the source tree and build profile this binary was built from.\n\
             pub const BUILD_ID: &str = \"{build_id}\";\n"
        ),
    )?;

    // A directory entry makes Cargo rescan it, so added, removed and edited
    // files under src/ all rerun this script.
    println!("cargo:rerun-if-changed={}", root.join("src").display());
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    for name in ROOT_INPUTS {
        println!("cargo:rerun-if-changed={}", root.join(name).display());
    }
    Ok(())
}
