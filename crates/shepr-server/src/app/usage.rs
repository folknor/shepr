//! Subscription usage tracking as the server wires it: which credential
//! directories the `shepr-usage` worker is given, whether anyone is attached,
//! and the latest snapshot it published. The worker owns credentials,
//! requests and scheduling; nothing here touches a credential.
//!
//! Directories come from the server's own environment (the default
//! directories under HOME, and any valid override) and from each pane
//! running Claude or Codex. A pane's scan runs on a capped discovery thread,
//! off the event loop and off the detection tick: it recognizes the agent
//! process itself (the pane's reported agent is a scheduling hint), reads
//! that process's allowlisted environment, recognizes it again, and resolves
//! the directory by the provider's rules.
//!
//! A scan result counts only if it is the pane's current attempt, arrived
//! within the scan deadline, names the provider the pane was hinted with
//! (still hinted now), and the pane still runs the very shell incarnation
//! the scan was scheduled against. Anything else is uncertain and retried
//! with backoff. A scan that outlives its deadline keeps its slot until its
//! thread ends. The worker remembers every agent directory it is given.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use shepr_agent::Agent;
use shepr_core::layout::PaneId;
use shepr_platform::{Pid, ProcessInstance};
use shepr_usage::{
    DiscoveredSource, Provider, SourceLocator, SourceOrigin, UsageConfig, UsageSnapshot,
    UsageWorker,
};

use super::App;
use crate::limits::{
    MAX_USAGE_DISCOVERY_SCANS, USAGE_DISCOVERY_INTERVAL, USAGE_DISCOVERY_POLL,
    USAGE_DISCOVERY_RETRY_START, USAGE_DISCOVERY_SCAN_DEADLINE, USAGE_SNAPSHOT_READ_INTERVAL,
};

pub(crate) struct UsageTracking {
    worker: UsageWorker,
    latest: Arc<UsageSnapshot>,
    server_sources: Vec<DiscoveredSource>,
    panes: HashMap<PaneId, PaneDiscovery>,
    sent: Option<Vec<DiscoveredSource>>,
    active: Option<bool>,
    /// Scan threads alive, stuck or invalidated ones included; a slot frees
    /// only when its thread delivers its completion.
    scans_alive: usize,
    /// The scan thread alive per pane, kept apart from the pane's state so
    /// invalidating an attempt, or dropping and re-adding the pane, never
    /// lets a second scan overlap a thread still running.
    live: HashMap<PaneId, u64>,
    next_attempt: u64,
    results: mpsc::Receiver<ScanResult>,
    results_tx: mpsc::Sender<ScanResult>,
    next_snapshot_read: Instant,
}

/// One pane's discovery state.
struct PaneDiscovery {
    /// The provider the pane reports and its shell, as last sampled.
    hint: Provider,
    shell: Pid,
    /// When the pane's next scan is due; a changed hint makes it due now.
    due: Instant,
    /// Consecutive uncertain scans, for backoff.
    uncertain_streak: u32,
    attempt: Option<Attempt>,
    /// The directory its agent was last found using.
    found: Option<DiscoveredSource>,
}

#[derive(Clone, Copy)]
struct Attempt {
    id: u64,
    started: Instant,
    provider: Provider,
    shell: ProcessInstance,
}

struct ScanResult {
    pane: PaneId,
    attempt: u64,
    outcome: ScanOutcome,
}

/// What a scan of one pane found.
enum ScanOutcome {
    Found(DiscoveredSource),
    /// Nothing usable this time; retried with backoff.
    Uncertain(&'static str),
}

/// Runs a closure if dropped while armed: a scan thread that unwinds still
/// delivers a completion, so its slot is never held by a dead thread.
struct OnUnwind<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnUnwind<F> {
    fn defuse(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnUnwind<F> {
    fn drop(&mut self) {
        if let Some(deliver) = self.0.take() {
            deliver();
        }
    }
}

impl App {
    /// Starts usage tracking for this server. Called once by the bootstrap;
    /// a worker that cannot start leaves tracking off, logged.
    pub(crate) fn start_usage_tracking(&mut self) {
        let config = UsageConfig {
            user_agent: format!("shepr/{}", env!("CARGO_PKG_VERSION")),
            registry: Some(self.paths.usage_sources_path()),
        };
        let worker = match UsageWorker::start(config, || {}) {
            Ok(worker) => worker,
            Err(error) => {
                shepr_platform::structured_log!(
                    WARN,
                    event = usage.worker_start,
                    outcome = Error,
                    error = %error,
                    "could not start usage tracking"
                );
                return;
            }
        };
        let (results_tx, results) = mpsc::channel();
        self.usage = Some(UsageTracking {
            worker,
            latest: Arc::new(UsageSnapshot::default()),
            server_sources: server_sources(),
            panes: HashMap::new(),
            sent: None,
            active: None,
            scans_alive: 0,
            live: HashMap::new(),
            next_attempt: 0,
            results,
            results_tx,
            next_snapshot_read: self.clock.now,
        });
    }

    /// One pass of usage tracking on the event loop: the active flag, scan
    /// results, new scans, the source set and the latest snapshot. Its only
    /// blocking work is reading one short `/proc` stat record per scan
    /// scheduled or accepted, the pane shell's start time.
    pub(crate) fn service_usage(&mut self, active: bool) {
        let now = self.clock.now;
        let hinted = self.usage_hinted_panes();
        let Some(usage) = self.usage.as_mut() else {
            return;
        };
        if usage.active != Some(active) {
            usage.active = Some(active);
            usage.worker.set_active(active);
        }
        usage.refresh_panes(&hinted, now);
        usage.take_results(now);
        if active {
            usage.expire_attempts(now);
            usage.start_scans(now);
        }
        usage.send_sources();
        if now >= usage.next_snapshot_read {
            usage.next_snapshot_read = now + USAGE_SNAPSHOT_READ_INTERVAL;
            let snapshot = usage.worker.snapshot();
            if (snapshot.incarnation, snapshot.revision)
                != (usage.latest.incarnation, usage.latest.revision)
            {
                log_snapshot(&snapshot);
                usage.latest = snapshot;
            }
        }
    }

    /// When the loop must next run usage tracking, if it is on.
    pub(crate) fn usage_deadline(&self) -> Option<Instant> {
        let usage = self.usage.as_ref()?;
        let now = self.clock.now;
        let mut deadline = usage.next_snapshot_read;
        if usage.active == Some(true) {
            let room = usage.scans_alive < MAX_USAGE_DISCOVERY_SCANS;
            for (id, pane) in &usage.panes {
                match pane.attempt {
                    Some(attempt) => {
                        deadline = deadline.min(attempt.started + USAGE_DISCOVERY_SCAN_DEADLINE);
                    }
                    // A waiting pane counts only when a scan could start;
                    // at the cap, the result poll below is the next look.
                    None if room && !usage.live.contains_key(id) => {
                        deadline = deadline.min(pane.due);
                    }
                    None => {}
                }
            }
        }
        if usage.scans_alive > 0 {
            deadline = deadline.min(now + USAGE_DISCOVERY_POLL);
        }
        Some(deadline.max(now))
    }

    /// Every pane whose agent is Claude or Codex, with its shell's pid.
    fn usage_hinted_panes(&self) -> HashMap<PaneId, (Pid, Provider)> {
        let mut hinted = HashMap::new();
        for workspace in self.state.workspaces().iter() {
            for pane_id in workspace.tree().pane_ids() {
                let Some(pane) = self.state.pane(pane_id) else {
                    continue;
                };
                let ownership = pane.terminal().ownership();
                let Some(provider) = ownership
                    .effective_agent()
                    .or(ownership.detected_agent())
                    .and_then(provider_of)
                else {
                    continue;
                };
                let Some(shell) = self
                    .lookup_runtime(pane_id)
                    .and_then(shepr_mux::pane::PaneRuntime::child_pid)
                else {
                    continue;
                };
                hinted.insert(pane_id, (shell, provider));
            }
        }
        hinted
    }
}

impl UsageTracking {
    /// Follows the panes' hints: a new pane or a changed hint or shell is due
    /// at once; a pane no longer running a tracked agent is dropped (the
    /// worker still remembers its directory).
    fn refresh_panes(&mut self, hinted: &HashMap<PaneId, (Pid, Provider)>, now: Instant) {
        self.panes.retain(|pane, _| hinted.contains_key(pane));
        for (pane, (shell, provider)) in hinted {
            match self.panes.get_mut(pane) {
                Some(state) if state.hint == *provider && state.shell == *shell => {}
                Some(state) => {
                    // A new target: the attempt for the old one is void, and
                    // its completion will change nothing (its thread still
                    // holds the pane's live slot until it ends).
                    state.hint = *provider;
                    state.shell = *shell;
                    state.due = now;
                    state.uncertain_streak = 0;
                    state.found = None;
                    state.attempt = None;
                }
                None => {
                    self.panes.insert(
                        *pane,
                        PaneDiscovery {
                            hint: *provider,
                            shell: *shell,
                            due: now,
                            uncertain_streak: 0,
                            attempt: None,
                            found: None,
                        },
                    );
                }
            }
        }
    }

    fn take_results(&mut self, now: Instant) {
        while let Ok(result) = self.results.try_recv() {
            // The thread has ended, whatever its result is worth.
            self.scans_alive = self.scans_alive.saturating_sub(1);
            if self.live.get(&result.pane) == Some(&result.attempt) {
                self.live.remove(&result.pane);
            }
            let Some(pane) = self.panes.get_mut(&result.pane) else {
                continue;
            };
            let Some(attempt) = pane.attempt.filter(|attempt| attempt.id == result.attempt) else {
                continue;
            };
            pane.attempt = None;
            let in_time =
                now.saturating_duration_since(attempt.started) <= USAGE_DISCOVERY_SCAN_DEADLINE;
            let same_hint = attempt.provider == pane.hint;
            // The pane still runs the shell incarnation the scan was for.
            let same_shell = pane.shell == attempt.shell.pid && attempt.shell.is_live();
            let accepted = match result.outcome {
                ScanOutcome::Found(source)
                    if in_time
                        && same_hint
                        && same_shell
                        && source.locator.provider == pane.hint =>
                {
                    pane.found = Some(source);
                    true
                }
                ScanOutcome::Found(_) => {
                    tracing::debug!(?result.pane, "usage discovery result no longer matches its pane");
                    false
                }
                ScanOutcome::Uncertain(reason) => {
                    tracing::debug!(?result.pane, reason, "usage discovery found no directory");
                    false
                }
            };
            if accepted {
                pane.uncertain_streak = 0;
                pane.due = now + USAGE_DISCOVERY_INTERVAL;
            } else {
                pane.uncertain(now);
            }
        }
    }

    /// Invalidates attempts past their deadline: their evidence will not be
    /// taken, the pane is retried, and the thread keeps its slot until it
    /// ends.
    fn expire_attempts(&mut self, now: Instant) {
        for pane in self.panes.values_mut() {
            if let Some(attempt) = pane.attempt
                && now.saturating_duration_since(attempt.started) > USAGE_DISCOVERY_SCAN_DEADLINE
            {
                pane.attempt = None;
                pane.uncertain(now);
            }
        }
    }

    /// Admits due panes, longest overdue first, while slots are free. A pane
    /// left waiting stays due and is admitted as slots free up.
    fn start_scans(&mut self, now: Instant) {
        let mut due: Vec<(Instant, PaneId)> = self
            .panes
            .iter()
            .filter(|(id, pane)| {
                pane.attempt.is_none() && pane.due <= now && !self.live.contains_key(*id)
            })
            .map(|(id, pane)| (pane.due, *id))
            .collect();
        due.sort_by_key(|(due, _)| *due);
        for (_, pane_id) in due {
            if self.scans_alive >= MAX_USAGE_DISCOVERY_SCANS {
                return;
            }
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            // The shell incarnation the scan is for, sampled now while the
            // pane runtime holds the child unreaped.
            let Ok(shell) = ProcessInstance::current(pane.shell) else {
                pane.uncertain(now);
                continue;
            };
            self.next_attempt += 1;
            let attempt = Attempt {
                id: self.next_attempt,
                started: now,
                provider: pane.hint,
                shell,
            };
            let tx = self.results_tx.clone();
            let started = std::thread::Builder::new()
                .name("usage-discover".into())
                .spawn(move || {
                    let unwind_tx = tx.clone();
                    let guard = OnUnwind(Some(move || {
                        unwind_tx
                            .send(ScanResult {
                                pane: pane_id,
                                attempt: attempt.id,
                                outcome: ScanOutcome::Uncertain("the scan panicked"),
                            })
                            .ok();
                    }));
                    let outcome = scan(attempt.shell, attempt.provider);
                    tx.send(ScanResult {
                        pane: pane_id,
                        attempt: attempt.id,
                        outcome,
                    })
                    .ok();
                    guard.defuse();
                });
            if started.is_ok() {
                self.scans_alive += 1;
                self.live.insert(pane_id, attempt.id);
                pane.attempt = Some(attempt);
            } else {
                // A thread could not start: retried after a backoff, so the
                // loop does not spin on it.
                pane.uncertain(now);
                return;
            }
        }
    }

    fn send_sources(&mut self) {
        let mut sources: Vec<DiscoveredSource> = self
            .server_sources
            .iter()
            .cloned()
            .chain(self.panes.values().filter_map(|pane| pane.found.clone()))
            .collect();
        sources.sort_by(|left, right| {
            (&left.locator, left.origin).cmp(&(&right.locator, right.origin))
        });
        sources.dedup();
        if self.sent.as_ref() != Some(&sources) {
            self.worker.set_sources(sources.clone());
            self.sent = Some(sources);
        }
    }
}

impl PaneDiscovery {
    /// Schedules a retry after an uncertain outcome, the delay doubling with
    /// each consecutive one up to the discovery interval.
    fn uncertain(&mut self, now: Instant) {
        self.uncertain_streak = self.uncertain_streak.saturating_add(1);
        self.due = now + retry_delay(self.uncertain_streak);
    }
}

fn retry_delay(streak: u32) -> Duration {
    USAGE_DISCOVERY_RETRY_START
        .saturating_mul(1 << streak.min(8))
        .min(USAGE_DISCOVERY_INTERVAL)
}

fn provider_of(agent: Agent) -> Option<Provider> {
    match agent {
        Agent::Claude => Some(Provider::Claude),
        Agent::Codex => Some(Provider::Codex),
        _ => None,
    }
}

/// The directories the server's own environment names: each provider's
/// default under HOME, and its override when the environment registry
/// accepts one. A refused override names nothing, and never displaces the
/// default, which is tracked regardless.
fn server_sources() -> Vec<DiscoveredSource> {
    use shepr_core::env::EnvVar;
    let path = |var: EnvVar| match shepr_core::env::read_path(var) {
        Ok(value) => value.map(|value| value.as_os_str().as_bytes().to_vec()),
        Err(error) => {
            shepr_platform::structured_log!(
                WARN,
                event = usage.discovery,
                outcome = Refused,
                %error,
                "ignoring a refused environment value for usage discovery"
            );
            None
        }
    };
    let home = path(EnvVar::Home);
    let mut sources = Vec::new();
    for (provider, var) in [
        (Provider::Claude, EnvVar::ClaudeConfigDir),
        (Provider::Codex, EnvVar::CodexHome),
    ] {
        let candidates = [
            shepr_usage::resolve_config_directory(provider, None, home.as_deref()),
            match path(var) {
                Some(value) => {
                    shepr_usage::resolve_config_directory(provider, Some(&value), home.as_deref())
                }
                None => Err(shepr_usage::Unresolved::NoHome),
            },
        ];
        for directory in candidates.into_iter().flatten() {
            sources.push(DiscoveredSource {
                locator: SourceLocator {
                    provider,
                    directory,
                },
                origin: SourceOrigin::Server,
            });
        }
    }
    sources
}

/// Recognizes the agent in a pane's foreground job, by the same selection
/// as detection, and resolves the directory it uses from its environment.
/// Blocking: runs on a discovery thread.
fn scan(shell: ProcessInstance, hint: Provider) -> ScanOutcome {
    if !shell.is_live() {
        return ScanOutcome::Uncertain("the pane shell is gone");
    }
    // The leader job first, as detection does; the shell's job only when the
    // leader names no agent.
    let leader_job = shepr_platform::foreground_process_group_id(shell.pid)
        .and_then(shepr_platform::foreground_group_leader_job);
    let from_leader = leader_job
        .as_ref()
        .and_then(shepr_detect::select_agent_process_in_job)
        .map(|selected| (selected.agent, selected.process.clone()));
    let selected = match from_leader {
        Some(selected) => Some(selected),
        None => shepr_platform::foreground_job(shell.pid).and_then(|job| {
            shepr_detect::select_agent_process_in_job(&job)
                .map(|selected| (selected.agent, selected.process.clone()))
        }),
    };
    let Some((agent, process)) = selected else {
        return ScanOutcome::Uncertain("no agent process in the foreground");
    };
    let Some(provider) = provider_of(agent) else {
        return ScanOutcome::Uncertain("the foreground agent is not tracked");
    };
    if provider != hint {
        return ScanOutcome::Uncertain("the foreground agent is not the pane's reported agent");
    }
    let names: [&[u8]; 2] = [provider.override_variable().as_bytes(), b"HOME"];
    let Ok(values) = shepr_platform::read_allowlisted_environ(process.instance(), &names) else {
        return ScanOutcome::Uncertain("the agent environment could not be read");
    };
    // The read is evidence only if the process is still what was recognized
    // and the pane still runs the same shell.
    let fresh = shepr_platform::process_facts(process.pid);
    let same = fresh.as_ref().is_some_and(|fresh| {
        *fresh == process && shepr_detect::identify_agent_process(fresh) == Some(agent)
    });
    if !same || !shell.is_live() {
        return ScanOutcome::Uncertain("the agent process changed during the scan");
    }
    let mut values = values.into_iter();
    let override_value = values.next().flatten();
    let home = values.next().flatten();
    match shepr_usage::resolve_config_directory(
        provider,
        override_value.as_deref(),
        home.as_deref(),
    ) {
        Ok(directory) => ScanOutcome::Found(DiscoveredSource {
            locator: SourceLocator {
                provider,
                directory,
            },
            origin: SourceOrigin::Agent,
        }),
        Err(_) => ScanOutcome::Uncertain("the agent environment names no usable directory"),
    }
}

fn log_snapshot(snapshot: &UsageSnapshot) {
    let measured = snapshot
        .accounts
        .iter()
        .filter(|account| account.measurement.is_some())
        .count();
    shepr_platform::structured_log!(
        INFO,
        event = usage.snapshot,
        outcome = Ok,
        accounts = snapshot.accounts.len(),
        measured,
        sources = snapshot.sources.len(),
        transport = ?snapshot.transport,
        "usage tracking updated"
    );
}
