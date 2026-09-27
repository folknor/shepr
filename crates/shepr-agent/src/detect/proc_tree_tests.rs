// ---------------------------------------------------------------------------
// Foreground job detection
// ---------------------------------------------------------------------------

use super::*;
use std::{cell::RefCell, collections::HashMap};

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
        (100..100 + u32::try_from(FOREGROUND_TREE_SCAN_LIMIT).expect("limit fits") + 200).collect();
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
fn remote_memory_reads_reject_dead_and_uninterruptible_states() {
    for state in ['D', 'Z', 'X', 'x'] {
        assert!(!process_state_allows_remote_memory_read(state));
    }
    for state in ['R', 'S', 'I', 'T', 't'] {
        assert!(process_state_allows_remote_memory_read(state));
    }
}

#[test]
fn pane_shell_process_names_reject_exec_replacement_programs() {
    for shell in ["bash", "-zsh", "/bin/fish"] {
        assert!(is_pane_shell_process_name(shell), "{shell}");
    }
    for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
        assert!(!is_pane_shell_process_name(program), "{program}");
    }
}

#[test]
fn parse_agent_env_hint_accepts_known_agents() {
    assert_eq!(
        parse_agent_env_hint(b"PATH=/bin\0SHEPR_AGENT=claude\0TERM=xterm\0"),
        Some(crate::detect::Agent::Claude)
    );
    assert_eq!(
        parse_agent_env_hint(b"SHEPR_AGENT=codex"),
        Some(crate::detect::Agent::Codex)
    );
}

#[test]
fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
    assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
    assert_eq!(parse_agent_env_hint(b"SHEPR_AGENT=not-an-agent\0"), None);
}
