use super::*;

/// OSC reports and save observations belong to one cwd arbitration state.
/// Keep the locks separate: a /proc read never holds either, and callers that
/// need both take reported before remembered.
#[derive(Default)]
pub(super) struct PaneCwdState {
    pub(super) reported: Mutex<Option<ReportedCwd>>,
    pub(super) remembered: Mutex<Option<PersistedCwd>>,
}

impl PaneCwdState {
    pub(super) fn remembered_cwd(&self) -> Option<std::path::PathBuf> {
        let reported = shepr_core::locks::lock_auxiliary(&self.reported);
        let remembered = shepr_core::locks::lock_auxiliary(&self.remembered);
        remembered_cwd_for_save(reported.clone(), remembered.clone())
    }

    pub(super) fn resolve(
        &self,
        shell_cwd: Option<std::path::PathBuf>,
    ) -> Option<std::path::PathBuf> {
        ReportedCwd::resolve(
            shepr_core::locks::lock_auxiliary(&self.reported).as_ref(),
            shell_cwd,
        )
    }
}

/// Reads a pane shell's live working directory from any thread, so a save can
/// take the probe on the event loop and do the /proc read where the save runs.
pub struct PaneCwdProbe {
    child_liveness: Arc<ChildLiveness>,
    cwd: Arc<PaneCwdState>,
}

impl PaneCwdProbe {
    /// The best cwd known for this save. A usable /proc read is arbitrated
    /// against OSC 7 exactly as it is for a live pane, then remembered. If the
    /// child is gone or its cwd cannot be used, retain the saved observation
    /// rather than replacing it with an unavailable process path.
    pub fn read(&self) -> Option<std::path::PathBuf> {
        let Some(shell_cwd) = self
            .child_liveness
            .observe(super::process_probe::usable_process_cwd)
            .flatten()
        else {
            return self.remembered_cwd();
        };
        let reported = shepr_core::locks::lock_auxiliary(&self.cwd.reported).clone();
        let cwd = ReportedCwd::resolve(reported.as_ref(), Some(shell_cwd.into_path_buf()))?;
        *shepr_core::locks::lock_auxiliary(&self.cwd.remembered) = Some(PersistedCwd {
            path: cwd.clone(),
            report_generation: reported.map(|reported| reported.generation),
        });
        Some(cwd)
    }

    pub(super) fn remembered_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd.remembered_cwd()
    }
}

/// The last accepted OSC 7 report, with the pane shell's /proc cwd sampled
/// when it arrived.
///
/// OSC 7 carries what /proc cannot: a logical path through symlinks, or the
/// directory of a program the pane shell's /proc entry does not describe (a
/// nested shell, a root shell under `sudo`). It goes stale when the shell
/// changes directory without emitting a new report. The sample tells the two
/// apart: while the shell's /proc cwd still equals it, nothing the shell did
/// is newer than the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReportedCwd {
    pub(super) path: std::path::PathBuf,
    pub(super) shell_cwd_at_report: Option<std::path::PathBuf>,
    pub(super) generation: u64,
}

/// A save's last cwd observation and the OSC 7 report current when it read
/// /proc. The generation lets a report that arrived later replace this older
/// fallback even when the next /proc read is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PersistedCwd {
    pub(super) path: std::path::PathBuf,
    pub(super) report_generation: Option<u64>,
}

impl ReportedCwd {
    /// The pane cwd given the shell's current /proc cwd: the report while the
    /// shell has not moved since it arrived, otherwise the shell's own cwd.
    pub(super) fn resolve(
        reported: Option<&Self>,
        shell_cwd: Option<std::path::PathBuf>,
    ) -> Option<std::path::PathBuf> {
        match (shell_cwd, reported) {
            (Some(shell_cwd), Some(reported))
                if reported.shell_cwd_at_report.as_ref() == Some(&shell_cwd) =>
            {
                Some(reported.path.clone())
            }
            (Some(shell_cwd), _) => Some(shell_cwd),
            (None, reported) => reported.map(|reported| reported.path.clone()),
        }
    }
}

pub(super) fn remembered_cwd_for_save(
    reported: Option<ReportedCwd>,
    persisted: Option<PersistedCwd>,
) -> Option<std::path::PathBuf> {
    match (reported, persisted) {
        (Some(reported), Some(persisted)) => {
            let report_is_newer = persisted
                .report_generation
                .is_none_or(|generation| reported.generation > generation);
            if report_is_newer {
                Some(reported.path)
            } else {
                ReportedCwd::resolve(Some(&reported), Some(persisted.path))
            }
        }
        (Some(reported), None) => Some(reported.path),
        (None, Some(persisted)) => Some(persisted.path),
        (None, None) => None,
    }
}

pub(super) fn follow_cwd_from_groups(
    shell_group: Option<shepr_platform::Pgid>,
    foreground_pgid: Option<shepr_platform::Pgid>,
    pane_cwd: impl FnOnce() -> Option<std::path::PathBuf>,
    foreground_group_cwd: impl FnOnce(shepr_platform::Pgid) -> Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    match (shell_group, foreground_pgid) {
        (Some(shell_group), Some(foreground_pgid)) if shell_group != foreground_pgid => {
            foreground_group_cwd(foreground_pgid).or_else(pane_cwd)
        }
        _ => pane_cwd(),
    }
}

pub(super) fn publish_reported_cwd(
    pane_id: PaneId,
    child_liveness: &ChildLiveness,
    cwd: std::path::PathBuf,
    reported_cwd: &Mutex<Option<ReportedCwd>>,
    events: &crate::events::EventSender,
) {
    let Some(cwd) = UsableCwd::new(cwd) else {
        return;
    };
    // One readlink per OSC 7, sampled before taking the lock.
    let shell_cwd_at_report = child_liveness.observe(readlink_process_cwd).flatten();
    let mut last_reported = shepr_core::locks::lock_auxiliary(reported_cwd);
    if let Some(last) = last_reported.as_mut()
        && last.path == cwd.as_path()
    {
        // A repeated report is not a new event, but it is fresh evidence
        // that the path is current wherever the shell now is.
        last.shell_cwd_at_report = shell_cwd_at_report;
        last.generation = last.generation.saturating_add(1);
        return;
    }
    // The dedupe slot is updated only once the event is queued: if the shared
    // channel is full, the next identical OSC 7 must retry instead of being
    // swallowed as a duplicate of a report AppState never saw. Keep the lock
    // through the nonblocking enqueue and store so concurrent publishers queue
    // cwd changes in the same order they update the dedupe slot.
    match events.try_send(crate::events::RuntimeEvent::TerminalCwdReported { cwd: cwd.clone() }) {
        Ok(()) => {
            let generation = last_reported
                .as_ref()
                .map_or(0, |last| last.generation.saturating_add(1));
            *last_reported = Some(ReportedCwd {
                path: cwd.into_path_buf(),
                shell_cwd_at_report,
                generation,
            });
        }
        Err(err) => {
            drop(last_reported);
            warn!(
                pane = pane_id.raw(),
                error = %err,
                "failed to send terminal cwd report"
            );
        }
    }
}

impl PaneRuntime {
    /// Get the current working directory of the child shell process.
    ///
    /// The latest OSC 7 report wins while the shell's /proc cwd is unchanged
    /// since that report arrived; once the shell has moved without reporting,
    /// its /proc cwd wins. One /proc readlink per call and no stat
    /// (`readlink_process_cwd`): this runs on the event loop.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let shell_cwd = self.child_liveness.observe(readlink_process_cwd).flatten();
        self.cwd.resolve(shell_cwd)
    }

    /// The cwd a save can use without a /proc read, using the same OSC 7
    /// arbitration as [`Self::cwd`]. A save's capture takes this on the event
    /// loop and lets [`PaneCwdProbe::read`] refresh it where the save runs.
    pub fn remembered_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd.remembered_cwd()
    }

    /// What another thread needs to resolve this pane's best saved cwd (see
    /// [`PaneCwdProbe`]); taking it reads nothing.
    pub fn cwd_probe(&self) -> PaneCwdProbe {
        PaneCwdProbe {
            child_liveness: Arc::clone(&self.child_liveness),
            cwd: Arc::clone(&self.cwd),
        }
    }

    /// The cwd to inherit when a split or new workspace follows this pane.
    /// The shell's OSC 7 arbitration applies while its own process group is in
    /// the foreground; a foreground job's group leader takes precedence while
    /// a different group owns the terminal.
    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        if self.child_liveness.live_process_id().is_none() {
            return self.cwd.resolve(None);
        }
        // The shell need not remain its group's leader. ProcStat already
        // supplies its current group, so no membership scan is needed here.
        self.child_liveness
            .observe(|pid| {
                let groups = shepr_platform::ProcStat::read(pid).ok();
                let shell_group = groups.as_ref().map(|stat| stat.process_group);
                let foreground_group = groups.and_then(|stat| stat.foreground_group);
                follow_cwd_from_groups(
                    shell_group,
                    foreground_group,
                    || self.cwd(),
                    |group| readlink_process_cwd(group.leader_pid()),
                )
            })
            .flatten()
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        self.child_liveness
            .observe(|pid| {
                let foreground_pgid = shepr_platform::foreground_process_group_id(pid);
                let leader_cwd =
                    foreground_pgid.and_then(|group| readlink_process_cwd(group.leader_pid()));

                // Prefer the leader; a helper's private chdir is not the job cwd.
                leader_cwd.or_else(|| {
                    let shell_cwd = readlink_process_cwd(pid);
                    foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref())
                })
            })
            .flatten()
    }
}
