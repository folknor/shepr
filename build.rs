//! Build identity.
//!
//! Client and server must be the same build: the wire codec is positional and
//! not self-describing, so two builds that disagree on any message layout
//! decode each other's frames into garbage instead of failing. A hand-bumped
//! protocol number cannot guard that, because nothing forces anyone to bump
//! it. This script fingerprints every input that shapes the binary (the
//! manifest, the lockfile, this script and every file under `src/`) and hands
//! the result to the crate as `BUILD_ID` and `PROTOCOL_VERSION`, which the
//! handshake, `ping` and `status` compare. Any source change yields a new
//! identity, so a stale server or a hand-copied remote binary is reported as
//! a mismatch instead of passing as compatible.
//!
//! The hash is FNV-1a over relative paths and file contents in sorted order,
//! so the same tree gives the same identity on every host and toolchain.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Top-level files hashed alongside the `src/` tree.
const ROOT_INPUTS: [&str; 3] = ["Cargo.toml", "Cargo.lock", "build.rs"];

struct Fnv(u64);

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    /// Hashes one file as a self-delimiting record, so moving bytes between
    /// a path and its contents, or between files, changes the result.
    fn write_file(&mut self, relative: &str, contents: &[u8]) {
        self.write(relative.as_bytes());
        self.write(&[0]);
        let len = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        self.write(&len.to_le_bytes());
        self.write(contents);
    }
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
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

/// Path relative to the manifest directory with `/` separators, so the hash
/// does not depend on where the tree is checked out.
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

/// Folds the 64-bit fingerprint into `1..u32::MAX`. Keeping clear of both
/// ends means `PROTOCOL_VERSION + 1` and `- 1`, which tests use to build a
/// mismatching peer, can never overflow.
fn protocol_version(hash: u64) -> u32 {
    let folded = (hash ^ (hash >> 32)) & u64::from(u32::MAX);
    let span = u64::from(u32::MAX - 1);
    u32::try_from(folded % span).map_or(1, |value| value + 1)
}

pub fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?,
    );
    let root = if manifest_dir
        .file_name()
        .is_some_and(|name| name == "shepr-protocol")
    {
        manifest_dir
            .parent()
            .and_then(Path::parent)
            .ok_or("missing workspace root")?
            .to_path_buf()
    } else {
        manifest_dir
    };
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);

    let mut files = ROOT_INPUTS
        .iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    collect_files(&root.join("src"), &mut files)?;
    collect_files(&root.join("crates"), &mut files)?;

    let mut named = files
        .iter()
        .map(|path| relative_name(&root, path).map(|name| (name, path)))
        .collect::<Result<Vec<_>, _>>()?;
    named.sort();

    let mut hash = Fnv(FNV_OFFSET);
    for (name, path) in &named {
        hash.write_file(name, &fs::read(path)?);
    }
    let build_id = format!("{:016x}", hash.0);
    let protocol = protocol_version(hash.0);

    fs::write(
        out_dir.join("build_id.rs"),
        format!(
            "/// Fingerprint of the source tree this binary was built from.\n\
             pub(crate) const BUILD_ID: &str = \"{build_id}\";\n"
        ),
    )?;
    fs::write(
        out_dir.join("protocol_identity.rs"),
        format!(
            "/// Fingerprint of the source tree this binary was built from.\n\
             pub const BUILD_ID: &str = \"{build_id}\";\n\
             /// Wire protocol identity, derived from `BUILD_ID`.\n\
             pub const PROTOCOL_VERSION: u32 = {protocol};\n"
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
