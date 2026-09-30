//! The one owner of a session's files.
//!
//! A [`SessionPersister`] holds the data directory lease, the session writer
//! and the pane history carried between saves, on a thread of its own. The
//! event loop only captures: the structural snapshot and, per pane, a handle
//! to its terminal ([`super::PendingHistory`]) and to its shell's cwd
//! ([`super::PendingCwds`]). Everything that costs time (reading cwds from
//! /proc, formatting new scrollback, serializing, writing and syncing the
//! layout and history files as one bundle) happens on the persister's thread, one
//! job at a time and in the order they were submitted, so a history is always
//! resolved against the carry state its predecessor left.
//!
//! The lease is released only when the persister is retired, after every job
//! submitted before has finished.
//!
//! Every job that ends, whether it finished, failed or was abandoned, fires
//! the completion signal the persister was built with, after its result is
//! in place, so an event loop waiting on that signal wakes to reap it rather
//! than polling for it.

use std::io;
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use tokio::sync::Notify;

use super::lock::DataDirLease;
use super::snapshot::{
    HistoryCarry, PendingCwds, PendingHistory, ResolvedHistory, SessionSnapshot,
};
use super::writer::SessionWriter;

/// What one save puts on disk: the layout and the pane history captured with
/// it, written together.
pub struct SessionBundle {
    pub snapshot: SessionSnapshot,
    /// The shell cwd reads still to do, applied to `snapshot` where the save
    /// runs.
    pub cwds: PendingCwds,
    /// `None` when pane history is not persisted.
    pub history: Option<PendingHistory>,
}

/// A job for the persister.
pub enum PersistJob {
    /// Write the bundle over the saved session.
    Save(SessionBundle),
    /// Remove the saved session: nothing is left to restore.
    Clear,
}

/// Where a submitted job's result arrives.
pub struct PendingSave(mpsc::Receiver<io::Result<()>>);

/// The sending half of a [`PendingSave`]: whoever runs the job reports its
/// result through it. Dropping it unreported reports a failure. Either way,
/// once it is gone its completion signal (if it has one) fires.
pub struct SaveCompletion {
    /// Taken when the result is sent, or dropped before the signal fires, so
    /// a woken waiter always finds the result or the disconnect.
    result: Option<mpsc::Sender<io::Result<()>>>,
    signal: Option<Arc<Notify>>,
}

impl PendingSave {
    /// A pending result and the completion that settles it, with no
    /// completion signal.
    pub fn channel() -> (SaveCompletion, Self) {
        Self::signalled_channel(None)
    }

    fn signalled_channel(signal: Option<Arc<Notify>>) -> (SaveCompletion, Self) {
        let (result, receiver) = mpsc::channel();
        (
            SaveCompletion {
                result: Some(result),
                signal,
            },
            Self(receiver),
        )
    }

    /// The job's result once it has finished, without waiting.
    pub fn try_finish(&self) -> Option<io::Result<()>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(abandoned())),
        }
    }

    /// Blocks until the job has finished.
    pub fn wait(self) -> io::Result<()> {
        self.0.recv().unwrap_or_else(|_| Err(abandoned()))
    }
}

impl SaveCompletion {
    /// Reports the job's result; the signal fires when `self` drops here,
    /// after the result is readable.
    pub fn complete(mut self, result: io::Result<()>) {
        if let Some(sender) = self.result.take() {
            // The submitter may have stopped waiting (a shutdown that gave up
            // on the result); nobody is left to tell.
            sender.send(result).ok();
        }
    }
}

impl Drop for SaveCompletion {
    /// Fires on every way a job ends: reported, or dropped unreported (a
    /// panic on the persister's thread unwinds through the job's
    /// completion), which the pending side reads as abandoned. `notify_one`
    /// keeps a permit when nobody is waiting yet, so a job that ends between
    /// the loop's check and its wait still wakes it.
    fn drop(&mut self) {
        drop(self.result.take());
        if let Some(signal) = &self.signal {
            signal.notify_one();
        }
    }
}

fn abandoned() -> io::Error {
    io::Error::other("session persister ended before finishing the save")
}

/// The state the persister's thread owns.
struct PersistState {
    writer: SessionWriter,
    history: HistoryCarry,
}

impl PersistState {
    fn run(&mut self, job: PersistJob, now: SystemTime) -> io::Result<()> {
        match job {
            PersistJob::Clear => {
                self.history.clear();
                self.writer.clear(now)
            }
            PersistJob::Save(SessionBundle {
                mut snapshot,
                cwds,
                history,
            }) => {
                // The /proc reads the event loop left for here.
                cwds.resolve(&mut snapshot);
                let result = match history {
                    None => {
                        self.history.forget_saved();
                        self.writer.save(&snapshot, None, now)
                    }
                    Some(history) => {
                        // Formatting pane history is the expensive part of a
                        // save; a history that is the file's already is
                        // neither assembled, serialized nor hashed.
                        let resolved = history.resolve_for_save(
                            &snapshot,
                            &mut self.history,
                            self.writer.history_is_current(),
                        );
                        match resolved {
                            ResolvedHistory::Unchanged => {
                                self.writer.save_keeping_history(&snapshot, now)
                            }
                            ResolvedHistory::Changed(history) => {
                                self.writer.save(&snapshot, Some(&history), now)
                            }
                        }
                    }
                };
                match &result {
                    Ok(()) => self.history.note_saved(),
                    Err(_) => self.history.forget_saved(),
                }
                result
            }
        }
    }

    fn retire(&mut self) {
        self.writer.retire();
    }
}

struct Command {
    job: PersistJob,
    now: SystemTime,
    done: SaveCompletion,
}

enum Worker {
    Thread {
        commands: mpsc::Sender<Command>,
        thread: std::thread::JoinHandle<()>,
    },
    /// No thread (none could be started, or none was asked for): jobs run on
    /// the caller's thread.
    Inline(Box<PersistState>),
    Retired,
}

/// Owns a session's files; see the module docs.
pub struct SessionPersister {
    worker: Worker,
    /// Fired once per submitted job, when it ends.
    finished: Arc<Notify>,
}

impl SessionPersister {
    /// Takes over the data directory `lease` guards without a thread: jobs run
    /// on the submitting thread. For an owner that persists nothing, which
    /// only holds the lease, so it does not pay a thread for it. `finished`
    /// fires like a threaded persister's, before `submit` returns.
    pub fn inline(
        lease: DataDirLease,
        protect_unloaded: bool,
        history: HistoryCarry,
        finished: Arc<Notify>,
    ) -> Self {
        Self {
            worker: Worker::Inline(Box::new(PersistState {
                writer: SessionWriter::new(lease, protect_unloaded),
                history,
            })),
            finished,
        }
    }

    /// Takes over the data directory `lease` guards. `protect_unloaded`: the
    /// first save must copy the session file aside before replacing it (it
    /// could not be loaded, or restore dropped part of it). `history` is the
    /// carried history restore produced. `finished` is fired each time a
    /// submitted job ends, once its result can be read.
    pub fn spawn(
        lease: DataDirLease,
        protect_unloaded: bool,
        history: HistoryCarry,
        finished: Arc<Notify>,
    ) -> Self {
        let state = PersistState {
            writer: SessionWriter::new(lease, protect_unloaded),
            history,
        };
        // The state is handed over only once the thread runs, so a failed
        // spawn leaves it here for the inline fallback.
        let (state_sender, state_receiver) = mpsc::channel::<PersistState>();
        let (commands, command_receiver) = mpsc::channel::<Command>();
        let spawned = std::thread::Builder::new()
            .name("shepr-persist".into())
            .spawn(move || {
                let Ok(mut state) = state_receiver.recv() else {
                    return;
                };
                while let Ok(Command { job, now, done }) = command_receiver.recv() {
                    done.complete(state.run(job, now));
                }
                // Every sender is gone: the persister was retired or dropped,
                // after the jobs queued before it.
                state.retire();
            });
        let worker = match spawned {
            Ok(thread) => match state_sender.send(state) {
                Ok(()) => Worker::Thread { commands, thread },
                // The thread exited before taking the state; it cannot have
                // done anything else.
                Err(mpsc::SendError(state)) => Worker::Inline(Box::new(state)),
            },
            Err(error) => {
                tracing::error!(
                    event = "persist.actor", subsystem = "persist", outcome = "spawn_error",
                    %error,
                    "failed to start the session persister thread; saving on the event loop"
                );
                // The closure, and the receiver the state was to go through,
                // went with the failed spawn; the state was never sent.
                Worker::Inline(Box::new(state))
            }
        };
        Self { worker, finished }
    }

    /// Queues `job`, stamped `now` (the time used for recovery-copy naming
    /// and cadence). Jobs run in submission order. After retirement a job is
    /// accepted and does nothing, like a retired writer's. Every job fires
    /// the completion signal when it ends, including one that was refused.
    pub fn submit(&mut self, job: PersistJob, now: SystemTime) -> PendingSave {
        let (done, pending) = PendingSave::signalled_channel(Some(Arc::clone(&self.finished)));
        match &mut self.worker {
            Worker::Thread { commands, .. } => {
                if let Err(mpsc::SendError(command)) = commands.send(Command { job, now, done }) {
                    command.done.complete(Err(abandoned()));
                }
            }
            Worker::Inline(state) => done.complete(state.run(job, now)),
            Worker::Retired => done.complete(Ok(())),
        }
        pending
    }

    /// Finishes every queued job, then releases the data directory lease.
    /// Later jobs are ignored.
    pub fn retire(&mut self) {
        match std::mem::replace(&mut self.worker, Worker::Retired) {
            Worker::Thread { commands, thread } => {
                drop(commands);
                if thread.join().is_err() {
                    // A panic on the thread dropped its state, lease included.
                    tracing::error!(
                        event = "persist.actor",
                        subsystem = "persist",
                        outcome = "panicked",
                        "session persister thread panicked"
                    );
                }
            }
            Worker::Inline(mut state) => state.retire(),
            Worker::Retired => {}
        }
    }
}

impl Drop for SessionPersister {
    /// Dropping without retiring still drains the queue and releases the
    /// lease before returning, so the directory is free for the next owner.
    fn drop(&mut self) {
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::snapshot::SNAPSHOT_VERSION,
            "workspaces": [{
                "id": "w1",
                "identity_cwd": "/shepr-persister-test",
                "layout": { "Pane": 0 },
                "panes": { "0": { "cwd": "/shepr-persister-test" } },
                "zoomed": false,
                "focused": 0,
                "root_pane": 0
            }],
            "active": 0,
            "selected": 0
        }))
        .expect("test precondition")
    }

    #[test]
    fn jobs_run_in_order_and_retiring_releases_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister");
        let directory = scratch.join("data");
        let lease = DataDirLease::acquire(&directory).expect("lease");
        let mut persister =
            SessionPersister::spawn(lease, false, HistoryCarry::default(), signal());
        let now = SystemTime::now();
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
                history: None,
            }),
            now,
        );
        let cleared = persister.submit(PersistJob::Clear, now);
        saved.wait().expect("save");
        cleared.wait().expect("clear");
        assert!(
            !directory
                .join(super::super::io::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat"),
            "the clear ran after the save"
        );
        assert_eq!(
            DataDirLease::acquire(&directory)
                .err()
                .map(|err| err.kind()),
            Some(io::ErrorKind::ResourceBusy),
            "the persister holds the lease"
        );

        let last = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
                history: None,
            }),
            now,
        );
        persister.retire();
        assert!(
            last.try_finish().is_some_and(|result| result.is_ok()),
            "retiring finishes the queued save first"
        );
        DataDirLease::acquire(&directory).expect("the lease is free after retiring");
        persister
            .submit(PersistJob::Clear, now)
            .wait()
            .expect("a retired persister ignores jobs");
        assert!(
            directory
                .join(super::super::io::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat"),
            "a retired persister writes nothing"
        );
    }

    #[test]
    fn dropping_the_persister_releases_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister-drop");
        let directory = scratch.join("data");
        let persister = SessionPersister::spawn(
            DataDirLease::acquire(&directory).expect("lease"),
            false,
            HistoryCarry::default(),
            signal(),
        );
        drop(persister);
        DataDirLease::acquire(&directory).expect("the lease is free after the drop");
    }

    #[test]
    fn an_inline_persister_runs_jobs_on_the_caller_and_releases_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister-inline");
        let directory = scratch.join("data");
        let mut persister = SessionPersister::inline(
            DataDirLease::acquire(&directory).expect("lease"),
            false,
            HistoryCarry::default(),
            signal(),
        );
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
                history: None,
            }),
            SystemTime::now(),
        );
        assert!(
            saved.try_finish().is_some_and(|result| result.is_ok()),
            "the job had finished when submit returned"
        );
        drop(persister);
        DataDirLease::acquire(&directory).expect("the lease is free after the drop");
    }

    fn signal() -> Arc<Notify> {
        Arc::new(Notify::new())
    }

    /// Waits for one firing of `finished`, bounded so a missing signal fails
    /// the test instead of hanging it.
    async fn signalled(finished: &Notify) {
        tokio::time::timeout(std::time::Duration::from_secs(5), finished.notified())
            .await
            .expect("the completion signal fired");
    }

    #[tokio::test]
    async fn a_finished_job_fires_the_signal_after_its_result_is_readable() {
        let scratch = crate::test_support::ScratchDir::new("persister-signal");
        let directory = scratch.join("data");
        let finished = signal();
        let mut persister = SessionPersister::spawn(
            DataDirLease::acquire(&directory).expect("lease"),
            false,
            HistoryCarry::default(),
            Arc::clone(&finished),
        );
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
                history: None,
            }),
            SystemTime::now(),
        );
        signalled(&finished).await;
        assert!(
            saved.try_finish().is_some_and(|result| result.is_ok()),
            "the result is in place when the signal fires"
        );
        drop(persister);
    }

    #[tokio::test]
    async fn an_inline_job_leaves_a_permit_for_a_later_wait() {
        let scratch = crate::test_support::ScratchDir::new("persister-inline-signal");
        let directory = scratch.join("data");
        let finished = signal();
        let mut persister = SessionPersister::inline(
            DataDirLease::acquire(&directory).expect("lease"),
            false,
            HistoryCarry::default(),
            Arc::clone(&finished),
        );
        let cleared = persister.submit(PersistJob::Clear, SystemTime::now());
        // Nobody was waiting when the job ended; the stored permit wakes the
        // first wait after it.
        signalled(&finished).await;
        assert!(cleared.try_finish().is_some());
        drop(persister);
    }

    #[tokio::test]
    async fn a_completion_dropped_unreported_fires_the_signal_as_abandoned() {
        let finished = signal();
        let (completion, pending) = PendingSave::signalled_channel(Some(Arc::clone(&finished)));
        drop(completion);
        signalled(&finished).await;
        assert!(
            pending.try_finish().is_some_and(|result| result.is_err()),
            "the pending side reads the dropped completion as abandoned"
        );
    }
}
