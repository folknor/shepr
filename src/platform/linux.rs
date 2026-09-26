use std::{
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    os::fd::RawFd,
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
};

pub(super) const REMOTE_BRIDGE_CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;

use super::{
    ClipboardCommand, ForegroundJob, ForegroundProcess, LimitedRead, Signal, read_limited_reader,
};

pub(crate) use super::unix_common::{
    ClientStreamReader, StatusCommandGuard, configure_status_command, create_remote_ssh_config_dir,
    create_remote_ssh_config_file, hostname, local_datetime, remote_bridge_endpoint_path,
    remote_ssh_config_paths, wait_client_stream_readable, write_client_stream,
};

#[cfg(test)]
mod config_file_tests;

mod shutdown;
pub(crate) use shutdown::monitor_host_shutdown;

const WSL_MARKER_ENV_VARS: &[&str] = &["WSL_DISTRO_NAME", "WSL_INTEROP"];
const PROCESS_DETECTION_ENV_VAR: &str = "SHEPR_PROCESS_DETECTION";
const CHILD_GROUPS_SCAN_LIMIT: usize = 64;
/// Upper bound on the number of processes visited while resolving a pane's
/// foreground process-group tree. Foreground-job detection reads /proc/<pid>/stat
/// and task/children files for every visited process on a repeated (per-tick/5s)
/// cadence, so an unbounded walk lets accumulated descendants or unreaped zombies
/// under the pane shell grow the server's read-syscall rate and CPU without limit
/// at a constant pane count (see AGENTS.md multiplicative performance paths). The
/// foreground-group leader's subtree and the pane shell's descendants advance
/// round-robin under a shared candidate ceiling, with independent per-root work
/// budgets, so a pathologically large accumulation on either side cannot starve the
/// other. Discovery is best effort once a budget is exhausted.
const FOREGROUND_TREE_SCAN_LIMIT: usize = 512;
/// Number of `/proc/<pid>/task` entries a root's subtree may consume, bounding how
/// far one process's thread count can multiply the walk's work.
const FOREGROUND_TASK_ENTRY_LIMIT: usize = 2_048;
/// Number of `/proc/<pid>/task/<tid>/children` bytes a root's subtree may read,
/// stopping a parent that accumulates unreaped children from growing read work
/// without limit.
const FOREGROUND_CHILD_BYTE_LIMIT: usize = 128 * 1024;
/// Aggregate number of child pids a root's subtree may parse and enqueue, bounding
/// the walk's pending queues and allocations.
const FOREGROUND_CHILD_PID_LIMIT: usize = 2_048;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessDetectionMode {
    Native,
    ChildGroups,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcGroupMember {
    pid: u32,
    comm: String,
    state: char,
}

pub(crate) fn launch_executable() -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let executable = std::env::current_exe()?;
    if !executable.is_file() {
        // Linux marks the old inode as deleted after an update replaces the binary.
        if let Some(path) = executable
            .as_os_str()
            .as_bytes()
            .strip_suffix(b" (deleted)")
        {
            let replacement = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if replacement.is_file() {
                return Ok(replacement);
            }
        }
    }
    Ok(executable)
}

pub(crate) fn config_file_link_count(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.nlink())
}

pub(crate) fn check_config_write_target(_target: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn write_existing_config(
    _target: &std::path::Path,
    _contents: &[u8],
) -> std::io::Result<bool> {
    // Unix keeps atomic replacement for existing files too.
    Ok(false)
}

pub(crate) fn create_config_temporary(
    path: &std::path::Path,
    private: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o666 })
        .open(path)
}

pub(crate) fn write_config_temporary(
    source: Option<&std::path::Path>,
    temporary: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temporary)?;
    if let Some(source) = source {
        let input = std::fs::File::open(source)?;
        let metadata = input.metadata()?;
        let current = output.metadata()?;
        if (metadata.uid(), metadata.gid()) != (current.uid(), current.gid()) {
            // Keep ownership before restoring mode/ACLs; chown can clear mode bits.
            if unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        // Replace inherited ACLs before enabling the original mode. Prepare all
        // access controls while the temporary is empty, before writing secrets.
        copy_config_xattrs(input.as_raw_fd(), output.as_raw_fd())?;
        output.set_permissions(metadata.permissions())?;
    }
    output.write_all(contents)?;
    output.sync_all()
}

// Access ACLs and security labels live in xattrs on Linux. Mode bits alone can
// silently broaden access, especially with a default ACL on the parent directory.
fn copy_config_xattrs(source: RawFd, destination: RawFd) -> std::io::Result<()> {
    use std::ffi::CStr;
    fn names(fd: RawFd) -> std::io::Result<Vec<u8>> {
        let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        let read = unsafe { libc::flistxattr(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    fn value(fd: RawFd, name: &CStr) -> std::io::Result<Vec<u8>> {
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        let read =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    let source_names = names(source)?;
    for bytes in names(destination)?.split_inclusive(|byte| *byte == 0) {
        if !source_names
            .split_inclusive(|byte| *byte == 0)
            .any(|name| name == bytes)
        {
            let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
            if unsafe { libc::fremovexattr(destination, name.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    for bytes in source_names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
        let original = value(source, name)?;
        // Avoid requiring relabel privileges when the inherited label already matches.
        if value(destination, name).is_ok_and(|current| current == original) {
            continue;
        }
        if unsafe {
            libc::fsetxattr(
                destination,
                name.as_ptr(),
                original.as_ptr().cast(),
                original.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn should_draw_host_cursor_by_default() -> bool {
    running_inside_wsl()
}

pub(crate) fn should_query_host_terminal_palette() -> bool {
    !running_inside_wsl()
}

fn running_inside_wsl() -> bool {
    static RUNNING_INSIDE_WSL: OnceLock<bool> = OnceLock::new();
    *RUNNING_INSIDE_WSL.get_or_init(detect_running_inside_wsl)
}

fn detect_running_inside_wsl() -> bool {
    proc_file_indicates_wsl("/proc/sys/kernel/osrelease")
        || proc_file_indicates_wsl("/proc/version")
        || WSL_MARKER_ENV_VARS
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        || std::path::Path::new("/run/WSL").exists()
}

fn proc_file_indicates_wsl(path: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text_indicates_wsl(&text))
        .unwrap_or(false)
}

fn text_indicates_wsl(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("microsoft") || text.contains("wsl")
}

fn parse_process_detection_mode(value: Option<&str>) -> Result<ProcessDetectionMode, &str> {
    match value {
        None | Some("") | Some("native") => Ok(ProcessDetectionMode::Native),
        Some("child-groups") => Ok(ProcessDetectionMode::ChildGroups),
        Some(value) => Err(value),
    }
}

fn process_detection_mode() -> ProcessDetectionMode {
    static MODE: OnceLock<ProcessDetectionMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let value = std::env::var(PROCESS_DETECTION_ENV_VAR).ok();
        parse_process_detection_mode(value.as_deref()).unwrap_or_else(|value| {
            tracing::warn!(
                variable = PROCESS_DETECTION_ENV_VAR,
                %value,
                "unknown process detection mode; using native detection"
            );
            ProcessDetectionMode::Native
        })
    })
}

pub(crate) fn interactive_shell_command(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let mut command = shell_quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Collect the foreground terminal job for a given child PID.
pub(crate) fn available_pane_shell(child_pid: u32) -> Option<String> {
    super::available_pane_shell_from_job(child_pid, foreground_job(child_pid)?)
}

pub fn foreground_job(child_pid: u32) -> Option<ForegroundJob> {
    let process_group_id = foreground_process_group_id(child_pid).or_else(|| {
        (process_detection_mode() == ProcessDetectionMode::ChildGroups)
            .then(|| child_groups_foreground_process_group(child_pid))
            .flatten()
    })?;
    foreground_job_for_group(child_pid, process_group_id)
}

fn foreground_job_for_group(child_pid: u32, process_group_id: u32) -> Option<ForegroundJob> {
    let members = foreground_process_group_members(child_pid, process_group_id)?;
    foreground_job_from_members(
        process_group_id,
        members,
        running_inside_wsl(),
        process_argv,
    )
}

fn foreground_job_from_members(
    process_group_id: u32,
    members: Vec<ProcGroupMember>,
    running_inside_wsl: bool,
    mut read_argv: impl FnMut(u32) -> Option<Vec<String>>,
) -> Option<ForegroundJob> {
    let processes = members
        .into_iter()
        .map(|member| {
            // Reading procfs cmdline enters access_remote_vm. On WSL, that read can
            // block indefinitely while a multithreaded process is exiting. A state
            // check alone has a race, so WSL uses the cheap comm-based identity when
            // it already identifies a supported agent without inspecting cmdline.
            let argv =
                process_allows_remote_memory_read(member.state, &member.comm, running_inside_wsl)
                    .then(|| read_argv(member.pid))
                    .flatten();
            ForegroundProcess {
                pid: member.pid,
                name: member.comm,
                argv0: None,
                cmdline: argv.as_ref().map(|parts| parts.join(" ")),
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

/// Best-effort foreground group for environments that do not expose terminal
/// foreground groups. This mode is explicit because background jobs cannot be
/// distinguished from foreground jobs without the native terminal signal.
fn child_groups_foreground_process_group(child_pid: u32) -> Option<u32> {
    let shell_group_id = u32::try_from(
        process_pgrp_comm_and_state(child_pid)
            .map(|(pgrp, _, _)| pgrp)
            .filter(|pgrp| *pgrp > 0)?,
    )
    .ok()?;

    child_groups_foreground_process_group_with(
        child_pid,
        shell_group_id,
        process_task_ids,
        process_task_children,
        |pid| process_pgrp_comm_and_state(pid).map(|(pgrp, _, _)| pgrp),
    )
}

fn child_groups_foreground_process_group_with(
    child_pid: u32,
    shell_group_id: u32,
    mut task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut process_group_id: impl FnMut(u32) -> Option<i32>,
) -> Option<u32> {
    let mut budget = ForegroundScanBudget::for_probe();
    let mut newest = None;
    let mut scanned = 0usize;
    for tid in task_ids(child_pid, &mut budget) {
        for child in task_children(child_pid, tid, &mut budget) {
            if scanned >= CHILD_GROUPS_SCAN_LIMIT {
                return None;
            }
            scanned += 1;

            let Some(pgrp) = process_group_id(child) else {
                continue;
            };
            if pgrp <= 0 {
                continue;
            }
            let Ok(pgrp) = u32::try_from(pgrp) else {
                continue;
            };
            if pgrp == shell_group_id {
                continue;
            }
            newest = Some(newest.map_or(pgrp, |current: u32| current.max(pgrp)));
        }
    }
    newest.or(Some(shell_group_id))
}

fn foreground_process_group_members(
    child_pid: u32,
    process_group_id: u32,
) -> Option<Vec<ProcGroupMember>> {
    foreground_process_group_members_with(
        child_pid,
        process_group_id,
        process_task_ids,
        process_task_children,
        live_process_group_member,
    )
}

fn foreground_process_group_members_with(
    child_pid: u32,
    process_group_id: u32,
    task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut live_member: impl FnMut(u32, u32) -> Option<ProcGroupMember>,
) -> Option<Vec<ProcGroupMember>> {
    // The leader is passed first; `process_tree_pids` advances both roots round-robin
    // so a truncated scan cannot let the pane shell's unrelated descendants starve
    // the foreground group, or vice versa.
    let mut members = process_tree_pids([process_group_id, child_pid], task_ids, task_children)
        .into_iter()
        .filter_map(|pid| live_member(process_group_id, pid))
        .collect::<Vec<_>>();
    members.sort_unstable_by_key(|member| member.pid);
    (!members.is_empty()).then_some(members)
}

fn process_tree_pids(
    roots: impl IntoIterator<Item = u32>,
    mut task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
) -> Vec<u32> {
    let mut visited = HashSet::new();
    let mut pids = Vec::new();
    // Keep one breadth-first frontier with its own work budget per root, so a large
    // expansion on one side cannot consume the other side's allowance. Frontier turns
    // advance round-robin, sharing the candidate ceiling between the foreground-group
    // leader's subtree and the pane shell's descendants.
    struct Frontier {
        pending: VecDeque<u32>,
        budget: ForegroundScanBudget,
    }
    let mut frontiers: Vec<Frontier> = Vec::new();
    for root in roots {
        if root > 0 && visited.insert(root) {
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
                    if child_pid > 0 && visited.insert(child_pid) {
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

fn process_task_ids(pid: u32, budget: &mut ForegroundScanBudget) -> Vec<u32> {
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

fn process_task_children(pid: u32, tid: u32, budget: &mut ForegroundScanBudget) -> Vec<u32> {
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
fn read_bounded_pid_list(mut reader: impl Read, budget: &mut ForegroundScanBudget) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut token = Vec::new();
    let mut buffer = [0_u8; 4096];

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

fn push_pid_token(pids: &mut Vec<u32>, token: &mut Vec<u8>, budget: &mut ForegroundScanBudget) {
    if token.is_empty() || budget.child_pids == 0 {
        token.clear();
        return;
    }
    if let Some(pid) = std::str::from_utf8(token)
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
    {
        budget.child_pids -= 1;
        pids.push(pid);
    }
    token.clear();
}

fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<u32> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn live_process_group_member(process_group_id: u32, pid: u32) -> Option<ProcGroupMember> {
    let (pgrp, comm, state) = process_pgrp_comm_and_state(pid)?;
    (u32::try_from(pgrp) == Ok(process_group_id)).then_some(ProcGroupMember { pid, comm, state })
}

pub fn foreground_group_leader_job(process_group_id: u32) -> Option<ForegroundJob> {
    let (pgrp, name, state) = process_pgrp_comm_and_state(process_group_id)?;
    if u32::try_from(pgrp) != Ok(process_group_id) {
        return None;
    }

    let argv = process_allows_remote_memory_read(state, &name, running_inside_wsl())
        .then(|| process_argv(process_group_id))
        .flatten();
    Some(ForegroundJob {
        process_group_id,
        processes: vec![ForegroundProcess {
            pid: process_group_id,
            name,
            argv0: None,
            cmdline: argv.as_ref().map(|parts| parts.join(" ")),
            argv,
        }],
    })
}

pub fn foreground_process_group_id(child_pid: u32) -> Option<u32> {
    // /proc/<pid>/stat format: "pid (comm) state ppid pgrp session tty_nr tpgid ..."
    // The (comm) field can contain spaces and parens, so we find the last ')' first.
    let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After (comm): state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5)
    let tpgid: i32 = fields.get(5)?.parse().ok()?;
    (tpgid > 0).then(|| u32::try_from(tpgid).unwrap_or_default())
}

pub fn foreground_process_group_id_for_tty_fd(fd: RawFd) -> Option<u32> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then(|| u32::try_from(pgid).unwrap_or_default())
}

fn process_pgrp_comm_and_state(pid: u32) -> Option<(i32, String, char)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_pgrp_comm_and_state_from_stat(&stat)
}

fn process_pgrp_comm_and_state_from_stat(stat: &str) -> Option<(i32, String, char)> {
    let close = stat.rfind(')')?;
    let comm = stat.get(1 + stat.find('(')?..close)?.to_string();
    let rest = stat.get(close + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let pgrp: i32 = fields.get(2)?.parse().ok()?;
    Some((pgrp, comm, state))
}

fn process_state_allows_remote_memory_read(state: char) -> bool {
    !matches!(state, 'D' | 'Z' | 'X' | 'x')
}

fn process_allows_remote_memory_read(state: char, comm: &str, running_inside_wsl: bool) -> bool {
    process_state_allows_remote_memory_read(state)
        && (!running_inside_wsl || crate::detect::identify_agent(comm).is_none())
}

fn process_argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
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
/// Uses /proc/<pid>/cwd symlink.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Read a Shepr agent identity hint from a process environment.
pub fn process_agent_hint(pid: u32) -> Option<crate::detect::Agent> {
    if pid == 0 {
        return None;
    }
    let (_, comm, state) = process_pgrp_comm_and_state(pid)?;
    if !process_allows_remote_memory_read(state, &comm, running_inside_wsl()) {
        return None;
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    super::parse_agent_env_hint(&environ)
}

fn signal_number(signal: Signal) -> libc::c_int {
    match signal {
        Signal::Hangup => libc::SIGHUP,
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    }
}

/// A pidfd: a handle on one specific process. A signal sent through it
/// reaches that process or nobody; it can never land on an unrelated process
/// the kernel later gave the same pid, which a plain `kill(pid)` can.
#[derive(Debug)]
pub(crate) struct ProcessHandle {
    pid: u32,
    fd: std::os::fd::OwnedFd,
}

impl ProcessHandle {
    /// Open a handle on the process that holds `pid` right now. `None` when
    /// no such process exists, or the kernel predates pidfds (Linux 5.3).
    pub(crate) fn open(pid: u32) -> Option<Self> {
        use std::os::fd::FromRawFd;

        let raw_pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: pidfd_open(2) takes a pid and a flags word and returns a new
        // close-on-exec fd or -1; it reads and writes no memory of ours.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0_u32) };
        if fd < 0 {
            return None;
        }
        let fd = RawFd::try_from(fd).ok()?;
        // SAFETY: `fd` was just returned by pidfd_open and nothing else owns it.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Some(Self { pid, fd })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    fn send(&self, signal: libc::c_int) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: pidfd_send_signal(2) with a null siginfo and no flags only
        // reads the fd, which `self` keeps open for the duration of the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        result == 0
    }

    /// Send `signal` to this process. False once it has been reaped.
    pub(crate) fn signal(&self, signal: Signal) -> bool {
        self.send(signal_number(signal))
    }

    /// Whether the process still holds its pid: running, or a zombie nobody
    /// has reaped yet. While this is true the pid cannot be reused, and
    /// neither can a process-group or session id equal to it.
    pub(crate) fn is_unreaped(&self) -> bool {
        self.send(0)
    }

    /// Whether the process has exited. A zombie counts as exited.
    pub(crate) fn has_exited(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd that lives on this stack frame; zero timeout.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        ready > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0
    }
}

/// Wait until every handle's process has exited, or `timeout` passes.
/// Returns whether they all exited.
pub(crate) fn wait_for_process_exits(
    handles: &[&ProcessHandle],
    timeout: std::time::Duration,
) -> bool {
    use std::os::fd::AsRawFd;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut descriptors: Vec<libc::pollfd> = handles
            .iter()
            .filter(|handle| !handle.has_exited())
            .map(|handle| libc::pollfd {
                fd: handle.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if descriptors.is_empty() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let wait_ms = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let count = libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: `descriptors` holds `count` initialised pollfds and outlives
        // the call; the fds are kept open by `handles`.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), count, wait_ms) };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // A failing poll must not turn this into a busy loop.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Every live process of session `session_id` other than its leader, each
/// held through a pidfd so later signals cannot reach a reused pid.
///
/// Membership is read straight from `/proc/<pid>/stat` for every process;
/// the leader's own stat is never consulted, so this works after the leader
/// has exited and been reaped. A candidate's stat is re-read after its pidfd
/// is open: if the handle is still unreaped afterwards, the pid was not
/// handed to anyone else in between and the stat described that process.
///
/// A session id is the leader's pid, and the kernel only reuses a number
/// nobody holds as a pid, process-group id or session id. So while the
/// leader is unreaped (`leader_reaped()` false) every process with this
/// session id is ours. Once it is reaped, a task that now holds pid
/// `session_id` proves the number was reused, which also proves the session
/// had no members left when that happened; nothing is returned then.
/// Remaining gap: the number is reused, the new owner starts its own session
/// and then exits and is reaped while its session lives on, all before this
/// runs. That needs a full pid wraparound between the pane's leader dying
/// and its teardown.
pub(crate) fn session_member_handles(
    session_id: u32,
    leader_reaped: impl Fn() -> bool,
) -> Vec<ProcessHandle> {
    let Ok(wanted) = i32::try_from(session_id) else {
        return Vec::new();
    };
    if wanted <= 0 {
        return Vec::new();
    }
    let mut handles = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = numeric_file_name(&entry) else {
            continue;
        };
        if pid == session_id || process_session_id(pid) != Some(wanted) {
            continue;
        }
        let Some(handle) = ProcessHandle::open(pid) else {
            continue;
        };
        if process_session_id(pid) == Some(wanted) && handle.is_unreaped() {
            handles.push(handle);
        }
    }
    if leader_reaped() && std::path::Path::new(&format!("/proc/{session_id}")).exists() {
        return Vec::new();
    }
    handles
}

/// Signal processes by bare pid. Test-only: production code signals through
/// `ProcessHandle`, which cannot hit a reused pid.
#[cfg(test)]
pub fn signal_processes(pids: &[u32], signal: Signal) {
    for &pid in pids {
        let Ok(pid) = i32::try_from(pid) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        // SAFETY: kill(2) touches no memory of this process.
        unsafe {
            libc::kill(pid, signal_number(signal));
        }
    }
}

#[cfg(test)]
pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only probes for the pid.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn write_clipboard(bytes: &[u8]) -> bool {
    write_clipboard_with(&clipboard_commands(ClipboardSession::from_env()), bytes)
}

fn write_clipboard_with(commands: &[ClipboardCommand], bytes: &[u8]) -> bool {
    commands
        .iter()
        .any(|command| run_clipboard_command(command, bytes))
}

pub fn read_clipboard_text() -> Option<String> {
    for command in read_clipboard_text_commands(ClipboardSession::from_env()) {
        if let Some(text) = read_clipboard_text_with_command(&command) {
            return Some(text);
        }
    }
    None
}

/// Which display servers the clipboard commands may talk to. Read from the
/// environment once per call and passed in, so the command lists are pure and
/// tests never have to mutate the process environment.
#[derive(Debug, Clone, Copy)]
struct ClipboardSession {
    wayland: bool,
    x11: bool,
}

impl ClipboardSession {
    fn from_env() -> Self {
        Self {
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
        }
    }
}

/// The executable's base name, so a command given by absolute path is still
/// recognised (`/usr/bin/wl-copy` is `wl-copy`).
fn clipboard_program_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn clipboard_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &["--type", "text/plain;charset=utf-8"],
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-in"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
        });
    }

    commands
}

fn read_clipboard_text_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain;charset=utf-8"],
        });
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain"],
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-out"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--output"],
        });
    }

    commands
}

fn read_clipboard_text_with_command(command: &ClipboardCommand) -> Option<String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let bytes = match read_limited_reader(stdout, MAX_CLIPBOARD_TEXT_BYTES) {
        Ok(LimitedRead::Complete(bytes)) => Some(bytes),
        Ok(LimitedRead::Empty) => None,
        // Too large, or unreadable: stop the helper rather than wait for it
        // to finish writing into a pipe nobody reads.
        Ok(LimitedRead::Oversized) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(bytes?).ok()
}

fn run_clipboard_command(command: &ClipboardCommand, bytes: &[u8]) -> bool {
    let mut child = match Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(bytes).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    drop(stdin);

    if clipboard_program_name(command.program) == "wl-copy" {
        return wait_for_wl_copy_startup(child);
    }

    child.wait().map(|status| status.success()).unwrap_or(false)
}

fn wait_for_wl_copy_startup(mut child: std::process::Child) -> bool {
    const STARTUP_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Ok(None) => return detach_clipboard_owner(child),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn detach_clipboard_owner(child: std::process::Child) -> bool {
    let pid = child.id();
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    let reaper_child = std::sync::Arc::clone(&child);
    let reaper = std::thread::Builder::new()
        .name("shepr-wl-copy-reaper".to_string())
        .spawn(move || {
            let wait_result = match reaper_child.lock() {
                Ok(mut child) => child.wait(),
                Err(poisoned) => poisoned.into_inner().wait(),
            };
            if let Err(err) = wait_result {
                tracing::warn!(pid, %err, "failed to reap wl-copy clipboard owner");
            }
        });

    if let Err(err) = reaper {
        tracing::warn!(pid, %err, "failed to start wl-copy clipboard owner reaper");
        let mut child = match child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }

    true
}

fn process_session_id(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    session_and_tty_from_stat(&stat).map(|(session, _tty)| session)
}

/// The session id and controlling-terminal device (`tty_nr`, 0 for none) from
/// a `/proc/<pid>/stat` line. The command name is skipped by its last `)`, as
/// it may itself contain spaces and parentheses.
fn session_and_tty_from_stat(stat: &str) -> Option<(i32, i32)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace().skip(3);
    let session = fields.next()?.parse().ok()?;
    let tty_nr = fields.next()?.parse().ok()?;
    Some((session, tty_nr))
}

/// Whether this server runs detached from any terminal, so closing the
/// terminal or SSH session that started it cannot hang it up. Remote attach
/// restarts a server that is not detached as a daemon.
///
/// Leading a session is not enough on its own: a terminal emulator or sshd
/// also makes the program it starts a session leader, but gives that session
/// the terminal as its controlling tty, and closing it sends SIGHUP. The
/// daemon spawn path calls setsid and never opens a terminal, so it leads its
/// session with no controlling tty; that pair is the test.
pub fn current_process_is_detached_server_daemon() -> bool {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return false;
    };
    session_and_tty_from_stat(&stat)
        .is_some_and(|(session, tty_nr)| is_detached_session(std::process::id(), session, tty_nr))
}

fn is_detached_session(pid: u32, session: i32, tty_nr: i32) -> bool {
    i64::from(session) == i64::from(pid) && tty_nr == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    #[test]
    fn wsl_marker_detection_matches_kernel_release_text() {
        assert!(text_indicates_wsl("5.15.167.4-microsoft-standard-WSL2"));
        assert!(text_indicates_wsl("4.4.0-19041-Microsoft"));
        assert!(!text_indicates_wsl("6.8.0-64-generic"));
        assert!(!text_indicates_wsl(""));
    }

    #[test]
    fn process_detection_mode_requires_explicit_child_groups_value() {
        assert_eq!(
            parse_process_detection_mode(None),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("")),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("native")),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("child-groups")),
            Ok(ProcessDetectionMode::ChildGroups)
        );
        assert_eq!(parse_process_detection_mode(Some("gvisor")), Err("gvisor"));
    }

    #[test]
    fn child_groups_foreground_group_picks_the_newest_job() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![200, 300])]);
        let groups = HashMap::from([(200, 200), (300, 300)]);

        let group = child_groups_foreground_process_group_with(
            100,
            100,
            |pid, _budget| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid, _budget| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(300));
    }

    #[test]
    fn child_groups_foreground_group_returns_to_the_shell_group() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![150, 160])]);
        let groups = HashMap::from([(150, 90), (160, 90)]);

        let group = child_groups_foreground_process_group_with(
            100,
            90,
            |pid, _budget| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid, _budget| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(90));
    }

    #[test]
    fn child_groups_foreground_group_skips_the_shell_group() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![150, 160, 300])]);
        let groups = HashMap::from([(150, 90), (160, 90), (300, 300)]);

        let group = child_groups_foreground_process_group_with(
            100,
            90,
            |pid, _budget| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid, _budget| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(300));
    }

    #[test]
    fn child_groups_foreground_group_fails_closed_at_the_scan_limit() {
        let limit = u32::try_from(CHILD_GROUPS_SCAN_LIMIT).expect("scan limit fits in u32");
        let children: Vec<u32> = (1..=(limit + 10)).collect();
        let mut inspected = 0usize;

        let group = child_groups_foreground_process_group_with(
            100,
            100,
            |_, _budget| vec![100],
            |_, _, _budget| children.clone(),
            |pid| {
                inspected += 1;
                Some(i32::try_from(pid).expect("test pid fits in i32"))
            },
        );

        assert_eq!(inspected, CHILD_GROUPS_SCAN_LIMIT);
        assert_eq!(group, None);
    }

    #[test]
    fn foreground_members_follow_the_pane_tree_and_filter_by_process_group() {
        let tasks = HashMap::from([
            (100, vec![100, 101]),
            (200, vec![200]),
            (201, vec![201]),
            (210, vec![210]),
            (220, vec![220]),
            (221, vec![221]),
            (300, vec![300]),
        ]);
        let children = HashMap::from([
            ((100, 100), vec![200, 201, 300]),
            ((100, 101), vec![210]),
            ((200, 200), vec![220]),
            ((220, 220), vec![221]),
        ]);
        let processes = HashMap::from([
            (100, (100, "shell")),
            (200, (200, "leader")),
            (201, (200, "pipeline")),
            (210, (200, "thread-child")),
            (220, (220, "intermediate")),
            (221, (200, "nested-agent")),
            (300, (300, "background")),
            (9999, (200, "unrelated-host-process")),
        ]);
        let task_reads = RefCell::new(Vec::new());
        let child_reads = RefCell::new(Vec::new());
        let member_reads = RefCell::new(Vec::new());

        let members = foreground_process_group_members_with(
            100,
            200,
            |pid, _budget| {
                task_reads.borrow_mut().push(pid);
                tasks.get(&pid).cloned().unwrap_or_default()
            },
            |pid, tid, _budget| {
                child_reads.borrow_mut().push((pid, tid));
                children.get(&(pid, tid)).cloned().unwrap_or_default()
            },
            |process_group_id, pid| {
                member_reads.borrow_mut().push(pid);
                let (pgrp, comm) = processes.get(&pid)?;
                (*pgrp == process_group_id).then(|| ProcGroupMember {
                    pid,
                    comm: (*comm).to_string(),
                    state: 'S',
                })
            },
        )
        .expect("test precondition");

        assert_eq!(
            members
                .into_iter()
                .map(|member| (member.pid, member.comm))
                .collect::<Vec<_>>(),
            vec![
                (200, "leader".to_string()),
                (201, "pipeline".to_string()),
                (210, "thread-child".to_string()),
                (221, "nested-agent".to_string()),
            ]
        );
        assert!(child_reads.borrow().contains(&(100, 101)));
        assert!(task_reads.borrow().contains(&220));
        assert!(!task_reads.borrow().contains(&9999));
        assert!(!member_reads.borrow().contains(&9999));
    }

    #[test]
    fn foreground_tree_traversal_is_bounded_by_the_scan_limit() {
        // A pane shell whose descendant tree is far larger than the bound (long-lived
        // agents accumulating children or unreaped zombies) must not make foreground
        // detection read /proc/<pid>/stat for an unbounded number of processes, and
        // the foreground group's own subtree must win the limited scan budget.
        let child_count = FOREGROUND_TREE_SCAN_LIMIT + 200;
        let child_count = u32::try_from(child_count).expect("child count fits in u32");
        let shell_children: Vec<u32> = (10..10 + child_count).collect();
        // The leader's subtree: leader(2) -> agent(9000) -> agent-child(9001). Both
        // descendants sit behind the shell's backlog and must still be reached.
        let agent_pid = 9000u32;
        let agent_child_pid = 9001u32;
        let stat_reads = RefCell::new(Vec::new());

        let members = foreground_process_group_members_with(
            1,
            2,
            // Every pid has its own single task.
            |pid, _budget| vec![pid],
            |pid, _tid, _budget| match pid {
                // The shell exposes the whole huge unrelated child list.
                1 => shell_children.clone(),
                // The leader exposes its agent child, which exposes its own child.
                2 => vec![agent_pid],
                _ if pid == agent_pid => vec![agent_child_pid],
                _ => Vec::new(),
            },
            |process_group_id, pid| {
                // Every visited pid triggers a /proc/<pid>/stat read; count them.
                stat_reads.borrow_mut().push(pid);
                (process_group_id == 2).then(|| ProcGroupMember {
                    pid,
                    comm: format!("p{pid}"),
                    state: 'S',
                })
            },
        )
        .expect("test precondition");

        // Bounded: foreground detection inspects at most the scan limit processes,
        // regardless of how large the descendant tree has grown.
        assert!(
            stat_reads.borrow().len() <= FOREGROUND_TREE_SCAN_LIMIT,
            "foreground traversal read /proc/stat for {} processes, exceeding the {} bound",
            stat_reads.borrow().len(),
            FOREGROUND_TREE_SCAN_LIMIT
        );
        assert!(members.len() <= FOREGROUND_TREE_SCAN_LIMIT);
        // The foreground-group leader and its descendants are visited before the
        // shell's unrelated backlog, so the detected agent survives truncation.
        assert!(
            members.iter().any(|member| member.pid == 2),
            "group leader must survive truncation"
        );
        assert!(
            members.iter().any(|member| member.pid == agent_child_pid),
            "leader descendants must be visited before unrelated shell descendants"
        );
    }

    #[test]
    fn foreground_tree_traversal_shares_the_scan_limit_between_roots() {
        // A foreground-group leader with more descendants than the scan limit must not
        // starve the pane shell's own foreground-group children, such as pipeline
        // members that live under the shell rather than under the leader.
        let leader_children: Vec<u32> =
            (100..100 + u32::try_from(FOREGROUND_TREE_SCAN_LIMIT).expect("limit fits") + 200)
                .collect();
        let pipeline_pid = 9000u32;
        let stat_reads = RefCell::new(Vec::new());

        let members = foreground_process_group_members_with(
            1,
            2,
            |pid, _budget| vec![pid],
            |pid, _tid, _budget| match pid {
                // The shell exposes a foreground-group pipeline child...
                1 => vec![pipeline_pid],
                // ...while the leader's own subtree already exceeds the bound.
                2 => leader_children.clone(),
                _ => Vec::new(),
            },
            |process_group_id, pid| {
                stat_reads.borrow_mut().push(pid);
                (process_group_id == 2).then(|| ProcGroupMember {
                    pid,
                    comm: format!("p{pid}"),
                    state: 'S',
                })
            },
        )
        .expect("test precondition");

        assert!(stat_reads.borrow().len() <= FOREGROUND_TREE_SCAN_LIMIT);
        assert!(
            members.iter().any(|member| member.pid == pipeline_pid),
            "shell-side foreground members must survive an oversized leader subtree"
        );
    }

    #[test]
    fn bounded_child_list_read_keeps_complete_tokens_at_eof() {
        let mut budget = ForegroundScanBudget::for_probe();
        let pids = read_bounded_pid_list(std::io::Cursor::new(b"10 20 30"), &mut budget);
        assert_eq!(pids, vec![10, 20, 30]);
        assert!(budget.child_bytes < FOREGROUND_CHILD_BYTE_LIMIT);
    }

    #[test]
    fn bounded_child_list_read_drops_a_token_cut_off_by_the_byte_budget() {
        let mut budget = ForegroundScanBudget::for_probe();
        // The byte budget ends inside the trailing pid, which must not parse as 3.
        budget.child_bytes = 8;
        let pids = read_bounded_pid_list(std::io::Cursor::new(b" 10 20 300"), &mut budget);
        assert_eq!(pids, vec![10, 20]);
        assert_eq!(budget.child_bytes, 0);
    }

    #[test]
    fn bounded_child_list_read_stops_at_the_pid_budget() {
        let mut budget = ForegroundScanBudget::for_probe();
        budget.child_pids = 2;
        let pids = read_bounded_pid_list(std::io::Cursor::new(b"10 20 30 40"), &mut budget);
        assert_eq!(pids, vec![10, 20]);
        assert_eq!(budget.child_pids, 0);
    }

    #[test]
    fn foreground_tree_traversal_reserves_enumeration_budget_per_root() {
        // The leader expansion spending its whole budget must not stop the shell root
        // from enumerating its own children.
        let leader_consumed = RefCell::new(false);
        let shell_saw_budget = RefCell::new(false);

        let members = foreground_process_group_members_with(
            1,
            2,
            |pid, budget| {
                if pid == 2 {
                    *leader_consumed.borrow_mut() = true;
                } else if pid == 1 {
                    *shell_saw_budget.borrow_mut() = budget.task_entries > 0;
                }
                budget.task_entries = 0;
                budget.child_bytes = 0;
                budget.child_pids = 0;
                vec![pid]
            },
            |_pid, _tid, _budget| Vec::new(),
            |_process_group_id, _pid| None,
        );

        assert!(*leader_consumed.borrow());
        assert!(
            *shell_saw_budget.borrow(),
            "shell root must keep its own budget after the leader spends its own"
        );
        assert!(members.is_none());
    }

    #[test]
    fn foreground_members_degrade_to_the_direct_group_leader() {
        let members = foreground_process_group_members_with(
            100,
            200,
            |_, _budget| Vec::new(),
            |_, _, _budget| Vec::new(),
            |process_group_id, pid| {
                (pid == process_group_id).then(|| ProcGroupMember {
                    pid,
                    comm: "leader".to_string(),
                    state: 'S',
                })
            },
        )
        .expect("test precondition");

        assert_eq!(
            members,
            vec![ProcGroupMember {
                pid: 200,
                comm: "leader".to_string(),
                state: 'S',
            }]
        );
    }

    #[test]
    fn foreground_members_observe_new_children_without_a_snapshot_cache() {
        let children = RefCell::new(HashMap::from([((100, 100), vec![200])]));
        let discover = || {
            foreground_process_group_members_with(
                100,
                200,
                |pid, _budget| vec![pid],
                |pid, tid, _budget| {
                    children
                        .borrow()
                        .get(&(pid, tid))
                        .cloned()
                        .unwrap_or_default()
                },
                |process_group_id, pid| {
                    [200, 201]
                        .contains(&pid)
                        .then(|| ProcGroupMember {
                            pid,
                            comm: format!("member-{pid}"),
                            state: 'S',
                        })
                        .filter(|_| process_group_id == 200)
                },
            )
            .expect("test precondition")
            .into_iter()
            .map(|member| member.pid)
            .collect::<Vec<_>>()
        };

        assert_eq!(discover(), vec![200]);
        children.borrow_mut().insert((100, 100), vec![200, 201]);
        assert_eq!(discover(), vec![200, 201]);
    }

    #[test]
    fn proc_stat_parsing_keeps_group_leader_inputs_live() {
        assert_eq!(
            process_pgrp_comm_and_state_from_stat("123 (name with ) paren) S 1 456 789 0 456"),
            Some((456, "name with ) paren".to_string(), 'S'))
        );
    }

    #[test]
    fn foreground_job_does_not_read_remote_memory_for_uninterruptible_members() {
        let argv_reads = RefCell::new(Vec::new());
        let job = foreground_job_from_members(
            200,
            vec![
                ProcGroupMember {
                    pid: 200,
                    comm: "codex".to_string(),
                    state: 'D',
                },
                ProcGroupMember {
                    pid: 201,
                    comm: "helper".to_string(),
                    state: 'S',
                },
            ],
            true,
            |pid| {
                argv_reads.borrow_mut().push(pid);
                Some(vec![format!("process-{pid}")])
            },
        )
        .expect("test precondition");

        assert_eq!(argv_reads.into_inner(), vec![201]);
        assert_eq!(job.processes[0].name, "codex");
        assert_eq!(job.processes[0].argv, None);
        assert_eq!(job.processes[1].argv, Some(vec!["process-201".to_string()]));
    }

    #[test]
    fn foreground_job_on_wsl_skips_known_agents_but_reads_wrappers() {
        let argv_reads = RefCell::new(Vec::new());
        let job = foreground_job_from_members(
            200,
            vec![
                ProcGroupMember {
                    pid: 200,
                    comm: "codex".to_string(),
                    state: 'S',
                },
                ProcGroupMember {
                    pid: 201,
                    comm: "node".to_string(),
                    state: 'S',
                },
            ],
            true,
            |pid| {
                argv_reads.borrow_mut().push(pid);
                Some(vec![format!("process-{pid}")])
            },
        )
        .expect("test precondition");

        assert_eq!(argv_reads.into_inner(), vec![201]);
        assert_eq!(job.processes[0].name, "codex");
        assert_eq!(job.processes[0].argv, None);
        assert_eq!(job.processes[1].argv, Some(vec!["process-201".to_string()]));
    }

    #[test]
    fn proc_stat_yields_session_and_controlling_tty() {
        assert_eq!(
            session_and_tty_from_stat("4242 (shepr (srv) x) S 1 4242 4242 0 -1 4194560"),
            Some((4242, 0))
        );
        assert_eq!(
            session_and_tty_from_stat("77 (shepr) S 70 77 77 34817 77 4194560"),
            Some((77, 34817))
        );
        assert_eq!(session_and_tty_from_stat("77 (shepr) S 70"), None);
    }

    #[test]
    fn only_a_session_leader_without_a_terminal_counts_as_detached() {
        // The setsid daemon spawn: leads its session, no controlling tty.
        assert!(is_detached_session(4242, 4242, 0));
        // `terminal -e shepr server` or `ssh -t host shepr server`: a session
        // leader too, but the terminal is its controlling tty.
        assert!(!is_detached_session(77, 77, 34817));
        // A server started from an interactive shell belongs to the shell's
        // session.
        assert!(!is_detached_session(90, 70, 34817));
        assert!(!is_detached_session(90, 70, 0));
    }

    #[test]
    fn remote_memory_reads_reject_dead_and_uninterruptible_states() {
        for state in ['D', 'Z', 'X', 'x'] {
            assert!(!process_state_allows_remote_memory_read(state));
        }
        for state in ['R', 'S', 'I', 'T', 't'] {
            assert!(process_state_allows_remote_memory_read(state));
        }
    }

    #[test]
    fn process_handle_follows_one_process_through_exit_and_reap() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let handle = ProcessHandle::open(child.id()).expect("pidfd_open on a live child");
        assert_eq!(handle.pid(), child.id());
        assert!(handle.is_unreaped());
        assert!(!handle.has_exited());

        assert!(handle.signal(Signal::Kill));
        assert!(wait_for_process_exits(
            &[&handle],
            std::time::Duration::from_secs(5)
        ));
        // A zombie has exited but still holds its pid.
        assert!(handle.has_exited());
        assert!(handle.is_unreaped());

        child.wait().expect("reap sleep");
        assert!(!handle.is_unreaped());
        assert!(
            !handle.signal(Signal::Kill),
            "a reaped process's handle must not signal anything"
        );
    }

    #[test]
    fn wait_for_process_exits_times_out_on_a_live_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let handle = ProcessHandle::open(child.id()).expect("pidfd_open on a live child");
        assert!(!wait_for_process_exits(
            &[&handle],
            std::time::Duration::from_millis(30)
        ));
        handle.signal(Signal::Kill);
        let _ = child.wait();
    }

    fn spawn_session_with_background_job() -> crate::pty::backend::SpawnedPty {
        let mut cmd = crate::pty::PtyCommand::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & exec sleep 30"]);
        crate::pty::backend::spawn_pty(24, 80, &cmd).expect("spawn session in a pty")
    }

    #[test]
    fn session_members_are_found_without_the_leader_and_signalled_by_handle() {
        let mut spawned = spawn_session_with_background_job();
        let leader = spawned.child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let members = loop {
            let members = session_member_handles(leader, || false);
            if !members.is_empty() || std::time::Instant::now() >= deadline {
                break members;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert!(
            members.iter().all(|member| member.pid() != leader),
            "the leader is not a member handle"
        );
        assert_eq!(members.len(), 1, "the background sleep is the only member");
        for member in &members {
            member.signal(Signal::Kill);
        }
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        let handles: Vec<&ProcessHandle> = members.iter().collect();
        assert!(wait_for_process_exits(
            &handles,
            std::time::Duration::from_secs(5)
        ));
    }

    #[test]
    fn session_members_are_withheld_when_a_reaped_leaders_pid_is_held_again() {
        let mut spawned = spawn_session_with_background_job();
        let leader = spawned.child.id();
        // The leader is alive, so from the point of view of a caller that has
        // already reaped its own leader, pid `leader` belongs to someone else.
        assert!(session_member_handles(leader, || true).is_empty());
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        // Clean up the background sleep, which outlives the leader.
        for member in session_member_handles(leader, || false) {
            member.signal(Signal::Kill);
        }
    }

    // None of these clipboard tests touch the process environment: the command
    // lists take the session as an argument and fake clipboard programs are run
    // by absolute path with their output paths baked into the script. Test
    // threads run concurrently, and a PATH or DISPLAY mutated here would leak
    // into every other test that spawns a program.

    fn fake_clipboard_dir(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should follow unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "shepr-fake-clipboard-{name}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    /// Write an executable script and return its absolute path as the
    /// `'static` program name `ClipboardCommand` wants (leaked; tests only).
    fn fake_clipboard_program(dir: &std::path::Path, name: &str, script: &str) -> &'static str {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, script).expect("fake clipboard program should be written");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake clipboard program metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions)
            .expect("fake clipboard program should be executable");
        Box::leak(path.to_string_lossy().into_owned().into_boxed_str())
    }

    fn quoted_path(path: &std::path::Path) -> String {
        shell_quote(&path.to_string_lossy())
    }

    #[test]
    fn clipboard_commands_prefer_wayland_when_available() {
        let commands = clipboard_commands(ClipboardSession {
            wayland: true,
            x11: false,
        });
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "wl-copy");
    }

    #[test]
    fn clipboard_commands_are_empty_without_a_display_server() {
        let session = ClipboardSession {
            wayland: false,
            x11: false,
        };
        assert!(clipboard_commands(session).is_empty());
        assert!(read_clipboard_text_commands(session).is_empty());
    }

    #[test]
    fn clipboard_program_name_strips_the_directory() {
        assert_eq!(clipboard_program_name("wl-copy"), "wl-copy");
        assert_eq!(clipboard_program_name("/usr/bin/wl-copy"), "wl-copy");
        assert_eq!(
            clipboard_program_name("/opt/wl-copy-wrapper"),
            "wl-copy-wrapper"
        );
    }

    #[test]
    fn wl_copy_owner_does_not_block_clipboard_write() {
        use std::path::PathBuf;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        struct Cleanup {
            temp_dir: PathBuf,
            owner_pid: Option<i32>,
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(pid) = self.owner_pid {
                    // SAFETY: kill(2) with a pid the fake wl-copy reported for
                    // itself; it touches no memory of this process.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
                let _ = std::fs::remove_dir_all(&self.temp_dir);
            }
        }

        let temp_dir = fake_clipboard_dir("wl-copy");
        let mut cleanup = Cleanup {
            temp_dir: temp_dir.clone(),
            owner_pid: None,
        };
        let marker = temp_dir.join("owner-pid");
        let payload = temp_dir.join("payload");
        let args = temp_dir.join("args");
        let fake_wl_copy = fake_clipboard_program(
            &temp_dir,
            "wl-copy",
            &format!(
                "#!/bin/sh\ncat > {payload}\nprintf '%s\\n' \"$@\" > {args}\nprintf '%s' \"$$\" > {marker}\nexec sleep 30\n",
                payload = quoted_path(&payload),
                args = quoted_path(&args),
                marker = quoted_path(&marker),
            ),
        );

        let (result_tx, result_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let command = ClipboardCommand {
                program: fake_wl_copy,
                args: &["--type", "text/plain;charset=utf-8"],
            };
            let _ = result_tx.send(run_clipboard_command(&command, b"clipboard text"));
        });

        let marker_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < marker_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let owner_pid: i32 = std::fs::read_to_string(&marker)
            .expect("fake wl-copy should enter its clipboard-owner phase")
            .parse()
            .expect("owner pid should be numeric");
        cleanup.owner_pid = Some(owner_pid);
        let returned_while_owner_running = result_rx
            .recv_timeout(Duration::from_secs(2))
            .is_ok_and(|result| result);
        let actual_payload = std::fs::read(&payload).expect("fake wl-copy should record stdin");
        let actual_args = std::fs::read_to_string(&args).expect("fake wl-copy should record args");

        // SAFETY: kill(2) with the pid the fake wl-copy reported for itself.
        unsafe {
            libc::kill(owner_pid, libc::SIGTERM);
        }
        let reap_deadline = Instant::now() + Duration::from_secs(2);
        let owner_pid_u32 = u32::try_from(owner_pid).expect("owner pid should be positive");
        while process_exists(owner_pid_u32) && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let owner_was_reaped = !process_exists(owner_pid_u32);
        cleanup.owner_pid = None;
        writer.join().expect("clipboard writer thread should join");
        drop(cleanup);

        assert!(
            returned_while_owner_running,
            "clipboard writes must return while wl-copy remains alive to own the selection"
        );
        assert_eq!(actual_payload, b"clipboard text");
        assert_eq!(actual_args, "--type\ntext/plain;charset=utf-8\n");
        assert!(
            owner_was_reaped,
            "wl-copy owner should be reaped after exit"
        );
    }

    #[test]
    fn failed_wl_copy_uses_x11_fallback() {
        let temp_dir = fake_clipboard_dir("fallback");
        let payload = temp_dir.join("xclip-payload");
        let fake_wl_copy = fake_clipboard_program(
            &temp_dir,
            "wl-copy",
            "#!/bin/sh\n/bin/cat >/dev/null\nexit 7\n",
        );
        let fake_xclip = fake_clipboard_program(
            &temp_dir,
            "xclip",
            &format!("#!/bin/sh\n/bin/cat > {}\n", quoted_path(&payload)),
        );

        // Same order `clipboard_commands` produces for a session with both a
        // Wayland and an X11 display, with the fakes standing in by path.
        let mut commands = clipboard_commands(ClipboardSession {
            wayland: true,
            x11: true,
        });
        assert_eq!(commands[0].program, "wl-copy");
        assert_eq!(commands[1].program, "xclip");
        commands[0].program = fake_wl_copy;
        commands[1].program = fake_xclip;

        let wrote = write_clipboard_with(&commands, b"clipboard fallback");
        let recorded = std::fs::read(&payload);
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(wrote);
        assert_eq!(
            recorded.expect("xclip should record stdin"),
            b"clipboard fallback"
        );
    }

    #[test]
    fn finite_clipboard_commands_report_exit_status() {
        let success = ClipboardCommand {
            program: "sh",
            args: &["-c", "cat >/dev/null"],
        };
        let failure = ClipboardCommand {
            program: "sh",
            args: &["-c", "cat >/dev/null; exit 7"],
        };

        assert!(run_clipboard_command(&success, b"clipboard text"));
        assert!(!run_clipboard_command(&failure, b"clipboard text"));
    }

    #[test]
    fn clipboard_commands_include_x11_fallbacks() {
        let commands = clipboard_commands(ClipboardSession {
            wayland: false,
            x11: true,
        });
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].program, "xclip");
        assert_eq!(commands[1].program, "xsel");
    }

    #[test]
    fn read_clipboard_text_commands_include_session_backends() {
        let commands = read_clipboard_text_commands(ClipboardSession {
            wayland: true,
            x11: true,
        });
        assert_eq!(commands[0].program, "wl-paste");
        assert_eq!(commands[1].program, "wl-paste");
        assert_eq!(commands[2].program, "xclip");
        assert_eq!(commands[3].program, "xsel");
    }

    #[test]
    fn read_clipboard_text_with_command_reads_utf8() {
        let command = ClipboardCommand {
            program: "printf",
            args: &["feature/linear-302"],
        };

        assert_eq!(
            read_clipboard_text_with_command(&command).as_deref(),
            Some("feature/linear-302")
        );
    }

    #[test]
    fn read_clipboard_text_with_command_rejects_oversized_output() {
        let command = ClipboardCommand {
            program: "sh",
            args: &["-c", "yes x | head -c 1048578"],
        };

        assert_eq!(read_clipboard_text_with_command(&command), None);
    }
}
