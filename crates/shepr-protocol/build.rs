#[path = "../../build.rs"]
mod workspace_build;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    workspace_build::main()
}
