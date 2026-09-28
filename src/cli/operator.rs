//! The operator's terminal for interactive remote setup. `shepr-remote` asks
//! its questions and reports its progress through [`shepr_remote::Operator`];
//! this is the binary's side, the only place those reach stderr and stdin.

use std::io::{self, IsTerminal as _, Write as _};

/// Notices and questions on stderr, answers from stdin. With no terminal on
/// stdin, questions go unanswered rather than blocking.
pub(crate) struct TerminalOperator;

impl shepr_remote::Operator for TerminalOperator {
    fn notice(&mut self, line: &str) {
        super::print_notice(&line);
    }

    fn confirm(&mut self, confirmation: &shepr_remote::Confirmation) -> io::Result<Option<bool>> {
        if !io::stdin().is_terminal() {
            return Ok(None);
        }
        let mut stderr = io::stderr().lock();
        for line in &confirmation.context {
            writeln!(stderr, "{line}")?;
        }
        write!(stderr, "{}", confirmation.prompt())?;
        stderr.flush()?;
        drop(stderr);
        confirmation.read_answer(&mut io::stdin().lock()).map(Some)
    }
}
