//! What the pane's process tree says: the shell's working directory, which
//! group owns the foreground and which agent runs in it. The detector that
//! interprets these observations is `detect`.

use crate::UsableCwd;
use shepr_agent::Agent;
use shepr_detect::BackgroundAgent;
use shepr_platform::{Pgid, Pid, ProcessInstance};

/// Event-loop reads never stat: a hung mount must not stall other panes.
pub(super) fn readlink_process_cwd(pid: Pid) -> Option<std::path::PathBuf> {
    shepr_platform::process_cwd(pid).filter(|path| path.is_absolute())
}

/// Only background save work checks directory usability.
pub(super) fn usable_process_cwd(pid: Pid) -> Option<UsableCwd> {
    readlink_process_cwd(pid).and_then(UsableCwd::new)
}

/// Classify an existing process snapshot without reading /proc again.
/// Unknown is kept distinct from a job: lack of evidence is not shell absence.
#[derive(Clone, Copy)]
pub(super) enum Foreground<'a> {
    Shell,
    Job(&'a shepr_platform::ForegroundJob),
    Unknown,
}

impl<'a> Foreground<'a> {
    pub(super) fn from_job(job: Option<&'a shepr_platform::ForegroundJob>, shell_pid: Pid) -> Self {
        match job {
            Some(job) if job.processes.iter().any(|process| process.pid == shell_pid) => {
                Self::Shell
            }
            Some(job) => Self::Job(job),
            None => Self::Unknown,
        }
    }

    pub(super) fn is_shell(self) -> bool {
        matches!(self, Self::Shell)
    }
}

pub(super) fn foreground_member_cwd_different_from_shell(
    shell_pid: Pid,
    shell_cwd: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let job = shepr_platform::foreground_job(shell_pid)?;
    // This is a fallback when the group leader cwd is unreadable. Even a
    // shell group can contain a nested shell with a different cwd; skipping
    // the pane shell here is candidate selection, not job classification.
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = readlink_process_cwd(process.pid) else {
            continue;
        };
        if shell_cwd != Some(&cwd) {
            return Some(cwd);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub(super) struct ProcessProbeResult {
    pub(super) process_group_id: Option<Pgid>,
    pub(super) foreground_is_pane_shell: bool,
    /// The agent process the detector held an identity for, found outside the
    /// terminal's foreground (stopped or running in the background). Only
    /// looked for when an agent was identified before and none is now.
    pub(super) background_agents: Vec<BackgroundAgent>,
    pub(super) identity: ProcessProbeIdentity,
}

#[derive(Debug, Clone)]
pub(super) enum ProcessProbeIdentity {
    /// `process` is the incarnation the agent was recognized from.
    Agent {
        agent: Agent,
        process_name: String,
        process: ProcessInstance,
    },
    Unidentified,
}

impl ProcessProbeResult {
    pub(super) fn process_group_id(&self) -> Option<Pgid> {
        self.process_group_id
    }

    pub(super) fn foreground_is_pane_shell(&self) -> bool {
        self.foreground_is_pane_shell
    }

    pub(super) fn agent(&self) -> Option<Agent> {
        match &self.identity {
            ProcessProbeIdentity::Agent { agent, .. } => Some(*agent),
            ProcessProbeIdentity::Unidentified => None,
        }
    }

    pub(super) fn process_name(&self) -> Option<&str> {
        match &self.identity {
            ProcessProbeIdentity::Agent { process_name, .. } => Some(process_name),
            ProcessProbeIdentity::Unidentified => None,
        }
    }

    /// The incarnation the foreground agent was recognized from.
    pub(super) fn agent_process(&self) -> Option<ProcessInstance> {
        match &self.identity {
            ProcessProbeIdentity::Agent { process, .. } => Some(*process),
            ProcessProbeIdentity::Unidentified => None,
        }
    }
}

fn agent_identity(job: &shepr_platform::ForegroundJob) -> ProcessProbeIdentity {
    shepr_detect::select_agent_process_in_job(job).map_or(
        ProcessProbeIdentity::Unidentified,
        |selected| ProcessProbeIdentity::Agent {
            agent: selected.agent,
            process_name: selected.display_name,
            process: selected.process.instance(),
        },
    )
}

fn process_probe_result(
    job: &shepr_platform::ForegroundJob,
    pid: Pid,
    identity: ProcessProbeIdentity,
) -> ProcessProbeResult {
    ProcessProbeResult {
        process_group_id: Some(job.process_group_id),
        foreground_is_pane_shell: Foreground::from_job(Some(job), pid).is_shell(),
        background_agents: Vec::new(),
        identity,
    }
}

pub(super) fn probe_foreground_process_from_jobs(
    pid: Pid,
    foreground_pgid: Option<Pgid>,
    leader_job: Option<&shepr_platform::ForegroundJob>,
    foreground_job: impl FnOnce() -> Option<shepr_platform::ForegroundJob>,
) -> ProcessProbeResult {
    if let Some(job) = leader_job {
        let identity = agent_identity(job);
        if !matches!(identity, ProcessProbeIdentity::Unidentified) {
            return process_probe_result(job, pid, identity);
        }
    }

    let foreground_job = foreground_job();
    if let Some(job) = foreground_job.as_ref() {
        return process_probe_result(job, pid, agent_identity(job));
    }

    ProcessProbeResult {
        process_group_id: foreground_pgid,
        foreground_is_pane_shell: false,
        background_agents: Vec::new(),
        identity: ProcessProbeIdentity::Unidentified,
    }
}

/// Probes the pane's foreground for an agent. `held` is the incarnation the
/// detector last identified an agent from. Only when it is set and the
/// foreground now holds no agent does the probe look for that process outside
/// the foreground: a pane with no agent, or one whose agent is in front, never
/// pays for the scan.
pub(super) fn probe_foreground_process(
    pid: Pid,
    foreground_pgid: Option<Pgid>,
    held: Option<ProcessInstance>,
) -> ProcessProbeResult {
    let mut probe = probe_foreground_process_from_jobs(
        pid,
        foreground_pgid,
        foreground_pgid
            .and_then(shepr_platform::foreground_group_leader_job)
            .as_ref(),
        || shepr_platform::foreground_job(pid),
    );
    if let Some(held) = held
        && probe.agent().is_none()
    {
        probe.background_agents = shepr_detect::background_agent_processes(pid, held);
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foreground_process(pid: u32, name: &str) -> shepr_platform::ForegroundProcess {
        shepr_platform::ForegroundProcess {
            pid: test_pid(pid),
            name: name.to_string(),
            argv: None,
            start_ticks: 0,
        }
    }

    fn test_pid(value: u32) -> Pid {
        Pid::new(value).expect("test process id")
    }

    fn test_pgid(value: u32) -> Pgid {
        Pgid::new(value).expect("test process group")
    }

    #[test]
    fn identifiable_foreground_leader_wins_over_other_job_members() {
        let job = shepr_platform::ForegroundJob {
            process_group_id: test_pgid(99),
            processes: vec![
                foreground_process(99, "codex"),
                foreground_process(100, "claude"),
            ],
        };

        let result =
            probe_foreground_process_from_jobs(test_pid(42), Some(test_pgid(99)), None, || {
                Some(job)
            });

        assert_eq!(result.agent(), Some(Agent::Codex));
        assert_eq!(result.process_name(), Some("codex"));
        assert_eq!(
            result.agent_process(),
            Some(ProcessInstance {
                pid: test_pid(99),
                start_ticks: 0
            })
        );
    }

    #[test]
    fn unidentified_leader_job_falls_through_to_foreground_job() {
        let leader_job = shepr_platform::ForegroundJob {
            process_group_id: test_pgid(99),
            processes: vec![foreground_process(99, "some_vm")],
        };
        let foreground_job = shepr_platform::ForegroundJob {
            process_group_id: test_pgid(99),
            processes: vec![
                foreground_process(99, "some_vm"),
                foreground_process(100, "codex"),
            ],
        };

        let result = probe_foreground_process_from_jobs(
            test_pid(42),
            Some(test_pgid(99)),
            Some(&leader_job),
            || Some(foreground_job),
        );

        assert_eq!(result.agent(), Some(Agent::Codex));
        assert_eq!(result.process_name(), Some("codex"));
    }

    #[test]
    fn unidentified_probe_cannot_carry_a_process_name() {
        let result = ProcessProbeResult {
            process_group_id: Pgid::new(17),
            foreground_is_pane_shell: false,
            background_agents: Vec::new(),
            identity: ProcessProbeIdentity::Unidentified,
        };
        assert_eq!(result.agent(), None);
        assert_eq!(result.process_name(), None);
    }
}
