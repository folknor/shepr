//! Stamps `BUILD_PROFILE`, the cargo profile, for path selection. The script
//! is the workspace one; it skips the build identity for this crate.

#[path = "../../build.rs"]
mod workspace_build;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    workspace_build::main()
}
