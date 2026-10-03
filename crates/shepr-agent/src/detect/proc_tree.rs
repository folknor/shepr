use shepr_platform::{Pgid, Pid, ProcStat, ProcState};
use std::{
    collections::{HashSet, VecDeque},
    io::Read,
    path::PathBuf,
};

use crate::limits::{
    FOREGROUND_CHILD_BYTE_LIMIT, FOREGROUND_CHILD_PID_LIMIT, FOREGROUND_TASK_ENTRY_LIMIT,
    FOREGROUND_TREE_SCAN_LIMIT, PROC_CHILDREN_READ_BUFFER_BYTES, PROCESS_CMDLINE_BYTE_LIMIT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: Pid,
    pub name: String,
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: Pgid,
    pub processes: Vec<ForegroundProcess>,
}

/// Every shell shepr recognises, as a process name. One list serves pane-shell
/// recognition, the generic-runtime ranking and `-c` unwrapping in detection:
/// each of these shells takes its command string as `-c <command>`.
const SHELL_NAMES: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "csh", "tcsh", "elvish", "xonsh", "nu",
];

/// Per-root work budget for foreground process discovery. The foreground-group
/// leader and the pane shell each get their own budget, so one side's expansion
/// cannot exhaust the other's allowance. Every task entry, child byte, and parsed
/// child pid is charged against the owning root's budget, keeping total `/proc` work
/// bounded independently of the process-tree size and of uptime. Discovery is best
/// effort once a budget is exhausted.
#[derive(Debug)]
struct ForegroundScanBudget {
    task_entries: usize,
    child_bytes: usize,
    child_pids: usize,
}

impl ForegroundScanBudget {
    fn for_probe() -> Self {
        Self {
            task_entries: FOREGROUND_TASK_ENTRY_LIMIT,
            child_bytes: FOREGROUND_CHILD_BYTE_LIMIT,
            child_pids: FOREGROUND_CHILD_PID_LIMIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcGroupMember {
    pid: Pid,
    comm: String,
    state: ProcState,
}

pub fn foreground_job(child_pid: Pid) -> Option<ForegroundJob> {
    let process_group_id = foreground_process_group_id(child_pid)?;
    let members = foreground_process_group_members(child_pid, process_group_id)?;
    foreground_job_from_members(process_group_id, members, process_argv)
}

/// Find job-control-stopped descendants of the pane shell. Once Ctrl-Z returns
/// the terminal to the shell, the stopped job is no longer in the foreground
/// process group, but its descendants remain in the shell's process tree.
/// A stop under a tracer reads `t` instead of `T` (a traced process that is
/// sent SIGTSTP enters a tracing stop), so both count as stopped.
pub(super) fn suspended_processes(child_pid: Pid) -> Vec<ForegroundProcess> {
    process_tree_pids([child_pid], process_task_ids, process_task_children)
        .into_iter()
        .filter_map(|pid| {
            let (_, name, state) = process_pgrp_comm_and_state(pid)?;
            if pid == child_pid || !state.is_stopped() {
                return None;
            }
            let argv = state
                .allows_remote_memory_read()
                .then(|| process_argv(pid))
                .flatten();
            Some(ForegroundProcess { pid, name, argv })
        })
        .collect()
}

fn foreground_job_from_members(
    process_group_id: Pgid,
    members: Vec<ProcGroupMember>,
    mut read_argv: impl FnMut(Pid) -> Option<Vec<String>>,
) -> Option<ForegroundJob> {
    let processes = members
        .into_iter()
        .map(|member| {
            // Reading procfs cmdline enters access_remote_vm, which can block on a
            // process that is exiting or in uninterruptible sleep.
            let argv = member
                .state
                .allows_remote_memory_read()
                .then(|| read_argv(member.pid))
                .flatten();
            ForegroundProcess {
                pid: member.pid,
                name: member.comm,
                argv,
            }
        })
        .collect::<Vec<_>>();

    if processes.is_empty() {
        return None;
    }

    Some(ForegroundJob {
        process_group_id,
        processes,
    })
}

fn foreground_process_group_members(
    child_pid: Pid,
    process_group_id: Pgid,
) -> Option<Vec<ProcGroupMember>> {
    foreground_process_group_members_from(
        child_pid,
        process_group_id,
        process_task_ids,
        process_task_children,
        live_process_group_member,
    )
}

fn foreground_process_group_members_from(
    child_pid: Pid,
    process_group_id: Pgid,
    task_ids: impl FnMut(Pid, &mut ForegroundScanBudget) -> Vec<Pid>,
    task_children: impl FnMut(Pid, Pid, &mut ForegroundScanBudget) -> Vec<Pid>,
    mut live_member: impl FnMut(Pgid, Pid) -> Option<ProcGroupMember>,
) -> Option<Vec<ProcGroupMember>> {
    // The leader is passed first; `process_tree_pids` advances both roots round-robin
    // so a truncated scan cannot let the pane shell's unrelated descendants starve
    // the foreground group, or vice versa.
    let mut members = process_tree_pids(
        [process_group_id.leader_pid(), child_pid],
        task_ids,
        task_children,
    )
    .into_iter()
    .filter_map(|pid| live_member(process_group_id, pid))
    .collect::<Vec<_>>();
    members.sort_unstable_by_key(|member| member.pid);
    (!members.is_empty()).then_some(members)
}

fn process_tree_pids(
    roots: impl IntoIterator<Item = Pid>,
    mut task_ids: impl FnMut(Pid, &mut ForegroundScanBudget) -> Vec<Pid>,
    mut task_children: impl FnMut(Pid, Pid, &mut ForegroundScanBudget) -> Vec<Pid>,
) -> Vec<Pid> {
    // Keep one breadth-first frontier with its own work budget per root, so a large
    // expansion on one side cannot consume the other side's allowance. Frontier turns
    // advance round-robin, sharing the candidate ceiling between the foreground-group
    // leader's subtree and the pane shell's descendants.
    struct Frontier {
        pending: VecDeque<Pid>,
        budget: ForegroundScanBudget,
    }

    let mut visited = HashSet::new();
    let mut pids = Vec::new();
    let mut frontiers: Vec<Frontier> = Vec::new();
    for root in roots {
        if visited.insert(root) {
            frontiers.push(Frontier {
                pending: VecDeque::from([root]),
                budget: ForegroundScanBudget::for_probe(),
            });
        }
    }

    loop {
        let mut progressed = false;
        for frontier in &mut frontiers {
            if pids.len() >= FOREGROUND_TREE_SCAN_LIMIT {
                return pids;
            }
            let Some(pid) = frontier.pending.pop_front() else {
                continue;
            };
            progressed = true;
            pids.push(pid);
            for tid in task_ids(pid, &mut frontier.budget) {
                for child_pid in task_children(pid, tid, &mut frontier.budget) {
                    if visited.insert(child_pid) {
                        frontier.pending.push_back(child_pid);
                    }
                }
            }
        }
        if !progressed {
            return pids;
        }
    }
}

fn process_task_ids(pid: Pid, budget: &mut ForegroundScanBudget) -> Vec<Pid> {
    let mut ids = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return ids;
    };
    for entry in entries.flatten() {
        if budget.task_entries == 0 {
            break;
        }
        budget.task_entries -= 1;
        if let Some(tid) = numeric_file_name(&entry) {
            ids.push(tid);
        }
    }
    ids
}

fn process_task_children(pid: Pid, tid: Pid, budget: &mut ForegroundScanBudget) -> Vec<Pid> {
    if budget.child_bytes == 0 || budget.child_pids == 0 {
        return Vec::new();
    }
    let Ok(file) = std::fs::File::open(format!("/proc/{pid}/task/{tid}/children")) else {
        return Vec::new();
    };
    read_bounded_pid_list(file, budget)
}

/// Read a whitespace-separated pid list, charging the shared budget for every byte
/// read and every parsed pid. A token cut off by the byte budget is discarded so a
/// partial value is never parsed as a different pid; the final token is only kept
/// when the reader reaches end-of-file.
fn read_bounded_pid_list(mut reader: impl Read, budget: &mut ForegroundScanBudget) -> Vec<Pid> {
    let mut pids = Vec::new();
    let mut token = Vec::new();
    let mut buffer = [0_u8; PROC_CHILDREN_READ_BUFFER_BYTES];

    while budget.child_bytes > 0 && budget.child_pids > 0 {
        let read_len = budget.child_bytes.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(0) => {
                push_pid_token(&mut pids, &mut token, budget);
                return pids;
            }
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return pids,
        };
        budget.child_bytes -= bytes_read;
        for &byte in &buffer[..bytes_read] {
            if byte.is_ascii_digit() {
                token.push(byte);
                continue;
            }
            push_pid_token(&mut pids, &mut token, budget);
            if budget.child_pids == 0 {
                return pids;
            }
        }
    }

    // The byte budget ran out mid-stream: drop the trailing token in case the read
    // truncated it.
    pids
}

fn push_pid_token(pids: &mut Vec<Pid>, token: &mut Vec<u8>, budget: &mut ForegroundScanBudget) {
    if token.is_empty() || budget.child_pids == 0 {
        token.clear();
        return;
    }
    if let Some(pid) = std::str::from_utf8(token)
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
        .and_then(Pid::new)
    {
        budget.child_pids -= 1;
        pids.push(pid);
    }
    token.clear();
}

fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<Pid> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().and_then(Pid::new)
}

fn live_process_group_member(process_group_id: Pgid, pid: Pid) -> Option<ProcGroupMember> {
    let (pgrp, comm, state) = process_pgrp_comm_and_state(pid)?;
    (pgrp == process_group_id).then_some(ProcGroupMember { pid, comm, state })
}

pub fn foreground_group_leader_job(process_group_id: Pgid) -> Option<ForegroundJob> {
    let leader_pid = process_group_id.leader_pid();
    let (pgrp, name, state) = process_pgrp_comm_and_state(leader_pid)?;
    if pgrp != process_group_id {
        return None;
    }

    let argv = state
        .allows_remote_memory_read()
        .then(|| process_argv(leader_pid))
        .flatten();
    Some(ForegroundJob {
        process_group_id,
        processes: vec![ForegroundProcess {
            pid: leader_pid,
            name,
            argv,
        }],
    })
}

pub fn foreground_process_group_id(child_pid: Pid) -> Option<Pgid> {
    ProcStat::read(child_pid).ok()?.foreground_group
}

fn process_pgrp_comm_and_state(pid: Pid) -> Option<(Pgid, String, ProcState)> {
    let stat = ProcStat::read(pid).ok()?;
    Some((stat.process_group, stat.comm, stat.state))
}

/// The argv of `pid`, or `None` when it is empty, unreadable or longer than
/// `PROCESS_CMDLINE_BYTE_LIMIT`. One byte past the limit is read so an
/// oversized argv is refused instead of identified from a truncated prefix.
/// The read grows a heap buffer only as far as the argv actually goes: this
/// runs per foreground process per detector probe, and a typical argv is far
/// below the limit.
fn process_argv(pid: Pid) -> Option<Vec<String>> {
    let file = std::fs::File::open(format!("/proc/{pid}/cmdline")).ok()?;
    let read_limit = u64::try_from(PROCESS_CMDLINE_BYTE_LIMIT)
        .ok()?
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.take(read_limit).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() > PROCESS_CMDLINE_BYTE_LIMIT {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Get the current working directory of a process.
/// Uses the `/proc/<pid>/cwd` symlink.
pub fn process_cwd(pid: Pid) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Whether a process name (a path, or a login shell's `-`-prefixed argv0) names
/// a shell from [`SHELL_NAMES`].
pub fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_start_matches('-');
    SHELL_NAMES
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(normalized))
}

#[cfg(test)]
fn process_pgrp_comm_and_state_from_stat(stat: &str) -> Option<(Pgid, String, ProcState)> {
    let stat = ProcStat::parse(stat)?;
    Some((stat.process_group, stat.comm, stat.state))
}

/// The production traversal driven by numeric test fixtures.
#[cfg(test)]
fn foreground_process_group_members_with(
    child_pid: u32,
    process_group_id: u32,
    mut task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut live_member: impl FnMut(u32, u32) -> Option<ProcGroupMember>,
) -> Option<Vec<ProcGroupMember>> {
    foreground_process_group_members_from(
        Pid::new(child_pid)?,
        Pgid::new(process_group_id)?,
        |pid, budget| {
            task_ids(pid.get(), budget)
                .into_iter()
                .filter_map(Pid::new)
                .collect()
        },
        |pid, tid, budget| {
            task_children(pid.get(), tid.get(), budget)
                .into_iter()
                .filter_map(Pid::new)
                .collect()
        },
        |group, pid| live_member(group.get(), pid.get()),
    )
}

#[cfg(test)]
#[path = "proc_tree_tests.rs"]
mod tests;
