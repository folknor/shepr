//! The agent detector of one pane: a state machine over timed observations.
//! It decides when to probe the process tree, which agent runs, and what the
//! screen says about it, and performs none of the I/O itself; `detection_task`
//! is the async shell that supplies the observations.
//!
//! - `state`: `DetectorState` and the tick protocol (`begin`, `resume`)
//! - `schedule`: when probes run
//! - `probe`: what a probe result means for the identified agent
//! - `publish`: reading the screen, its cache, and what reaches the server

mod probe;
mod publish;
mod schedule;
mod state;

pub(super) use publish::{publish_agent_process_detected_event, publish_state_changed_event};
pub(super) use state::{
    DetectorState, PROCESS_RECHECK_NO_AGENT, Step, Tick, TickContext, TickOutput,
};
