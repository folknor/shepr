//! The workspace-built stand-in for the host programs tests used to borrow.
//! Its grammar and interpreter live in `shepr_test_support::fixture`.

fn main() -> std::process::ExitCode {
    shepr_test_support::fixture::main()
}
