//! The restart offer for a running server of another build, local or remote:
//! observe it, ask for consent, stop exactly the instance observed, and offer
//! again a bounded number of times when the stop meets a new occupant. The
//! caller supplies the observation, the question and the stop, so the same
//! engine serves the local server and every configured machine.

use std::io;

use crate::stop::ServerStopError;

/// How a conditional stop ended when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The observed instance stopped answering and no replacement was found.
    Stopped,
    /// No server was present when the stop request ran.
    NoServer,
    /// A different boot answered while stopping the instance that had been observed.
    BootChanged,
}

/// A restart retains the stop failure until the operator notice is rendered.
#[derive(Debug)]
pub enum RestartFailure {
    Local(ServerStopError),
    Remote(io::Error),
}

impl std::fmt::Display for RestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(error) => error.fmt(f),
            Self::Remote(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RestartFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Local(error) => Some(error),
            Self::Remote(error) => Some(error),
        }
    }
}

/// How one offer to restart a server of another build ended.
#[derive(Debug)]
pub enum RestartResult {
    /// No server of another build was found.
    NotNeeded,
    /// There was no terminal to ask on, so the server was left running.
    NoTerminal,
    /// The operator kept the server running.
    Declined,
    /// The observed server was stopped; the caller can start a replacement.
    Stopped,
    /// No server was present when its conditional stop ran.
    NoServer,
    /// A different boot answered the stop or appeared while the observed
    /// instance was shutting down; it was not stopped as part of this offer.
    OccupantChanged,
    /// The stop failed; the server may still be running.
    Failed(RestartFailure),
}

impl RestartResult {
    /// Offers to restart the different-build server found by `observe`.
    /// After a changed boot identity, the server is observed again and another
    /// offer is made only while the new observation still needs a restart.
    ///
    /// `decide` is absent when there is no terminal on which to ask. `observe`
    /// returns `None` when no server answers; a present server that
    /// `restartable` rejects is a replacement that does not need this offer.
    pub fn offer<S>(
        max_offers: usize,
        decide: Option<&mut dyn FnMut(&S) -> RestartDecision>,
        mut observe: impl FnMut() -> Option<S>,
        mut restartable: impl FnMut(&S) -> bool,
        mut stop: impl FnMut(&S) -> Result<StopOutcome, RestartFailure>,
    ) -> Self {
        let Some(mut server) = observe().filter(|server| restartable(server)) else {
            return Self::NotNeeded;
        };
        let Some(decide) = decide else {
            return Self::NoTerminal;
        };

        for offer in 1..=max_offers {
            if decide(&server) == RestartDecision::Keep {
                return Self::Declined;
            }
            match stop(&server) {
                Ok(StopOutcome::Stopped) => return Self::Stopped,
                Ok(StopOutcome::NoServer) => return Self::NoServer,
                Err(error) => return Self::Failed(error),
                Ok(StopOutcome::BootChanged) => {
                    let Some(next) = observe() else {
                        return Self::NoServer;
                    };
                    if !restartable(&next) || offer == max_offers {
                        return Self::OccupantChanged;
                    }
                    server = next;
                }
            }
        }
        Self::OccupantChanged
    }
}

/// The answer to one restart offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    Restart,
    Keep,
}
