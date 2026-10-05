//! The one owner of a session's files.
//!
//! A [`SessionPersister`] holds the data directory lease and the session
//! writer, on a thread of its own. The event loop only captures: the
//! structural snapshot and, per pane, a handle to its shell's cwd
//! ([`super::PendingCwds`]). Everything that costs time (reading cwds from
//! /proc, serializing, writing and syncing the layout file) happens on the
//! persister's thread, one job at a time and in the order they were
//! submitted. The exception is an inline persister (the thread could not be started): its jobs, expensive
//! work included, run on the submitting thread, which for a server is the
//! event loop.
//!
//! The lease is released only when the persister is retired, after every
//! submitted job has finished or been abandoned.
//! If a job panics, on either kind of worker, it fails closed: it reports a
//! refusal for that job and later submissions, keeps the writer (and lease)
//! alive, and releases the lease only after the persister is retired.
//!
//! Every job that ends, whether it finished, failed or was abandoned, fires
//! the completion signal the persister was built with, after its result is
//! in place, so an event loop waiting on that signal wakes to reap it rather
//! than polling for it.

use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use tokio::sync::Notify;

use super::capture::PendingCwds;
use super::error::{SaveError, SaveRefusal};
use super::lock::DataDirLease;
use super::schema::SessionSnapshot;
use super::writer::SessionWriter;

/// What one save puts on disk: the layout, with the shell cwd reads still to
/// do on it.
pub struct SessionBundle {
    pub snapshot: SessionSnapshot,
    /// The shell cwd reads still to do, applied to `snapshot` where the save
    /// runs.
    pub cwds: PendingCwds,
}

/// A job for the persister.
pub enum PersistJob {
    /// Write the bundle over the saved session.
    Save(SessionBundle),
    /// Remove the saved session: nothing is left to restore.
    Clear,
}

/// Where a submitted job's result arrives.
pub struct PendingSave(mpsc::Receiver<Result<(), SaveError>>);

/// The sending half of a [`PendingSave`]: whoever runs the job reports its
/// result through it. Dropping it unreported reports a failure. Either way,
/// once it is gone its completion signal (if it has one) fires.
pub struct SaveCompletion {
    /// Taken when the result is sent, or dropped before the signal fires, so
    /// a woken waiter always finds the result or the disconnect.
    result: Option<mpsc::Sender<Result<(), SaveError>>>,
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
    pub fn try_finish(&self) -> Option<Result<(), SaveError>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(abandoned())),
        }
    }

    /// Blocks until the job has finished.
    pub fn wait(self) -> Result<(), SaveError> {
        self.0.recv().unwrap_or_else(|_| Err(abandoned()))
    }
}

impl SaveCompletion {
    /// Reports the job's result; the signal fires when `self` drops here,
    /// after the result is readable.
    pub fn complete(mut self, result: Result<(), SaveError>) {
        if let Some(sender) = self.result.take() {
            // The submitter may have stopped waiting (a shutdown that gave up
            // on the result); nobody is left to tell.
            sender.send(result).ok();
        }
    }
}

impl Drop for SaveCompletion {
    /// Fires on every way a job ends: reported, or dropped unreported when the
    /// persister went away before running it, which the pending side reads
    /// as abandoned. `notify_one`
    /// keeps a permit when nobody is waiting yet, so a job that ends between
    /// the loop's check and its wait still wakes it.
    fn drop(&mut self) {
        drop(self.result.take());
        if let Some(signal) = &self.signal {
            signal.notify_one();
        }
    }
}

fn abandoned() -> SaveError {
    SaveError::Abandoned
}

/// The result of a job that panicked, and of every job after it: the
/// persister is still alive and holds the lease, but runs no more saves.
fn stopped_after_panic() -> SaveError {
    SaveError::Refused(SaveRefusal::StoppedAfterPanic)
}

/// The state the persister's thread owns.
struct PersistState {
    writer: SessionWriter,
    /// Cleared when a job panics: the state may be half updated, so no later
    /// job runs against it. The writer, and its lease, stay alive.
    accepting_jobs: bool,
}

impl PersistState {
    fn new(writer: SessionWriter) -> Self {
        Self {
            writer,
            accepting_jobs: true,
        }
    }

    /// Runs `work` under the persister's panic contract, whichever worker owns
    /// the state. Work that panics fails, as does every later one, and the
    /// state is kept so the lease stays held until retirement. For an inline
    /// persister this means a panicking save no longer ends the server: it
    /// keeps running with saves off, which the error log says once.
    fn run_guarded(&mut self, work: Work) -> Result<(), SaveError> {
        if !self.accepting_jobs {
            return Err(stopped_after_panic());
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the persister owns its job panics: a caught panic fails that job and refuses every later one, while the lease stays held until retirement"
        )]
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(self)));
        outcome.unwrap_or_else(|_| {
            // The state stays outside the unwind boundary. Its writer owns
            // the lease, so a failed persister cannot let another server
            // restore stale files while this server still owns live panes.
            self.accepting_jobs = false;
            tracing::error!(
                event = "persist.actor",
                subsystem = "persist",
                outcome = "panicked",
                "session persister stopped after a job panicked; holding the data directory lease until retirement"
            );
            Err(stopped_after_panic())
        })
    }

    fn run(&mut self, job: PersistJob, now: SystemTime) -> Result<(), SaveError> {
        match job {
            PersistJob::Clear => self.writer.clear(now),
            PersistJob::Save(SessionBundle { mut snapshot, cwds }) => {
                // The /proc reads the event loop left for here.
                cwds.resolve(&mut snapshot);
                self.writer.save(&snapshot, now)
            }
        }
    }

    fn retire(self) {
        self.writer.retire();
    }
}

/// One unit the worker runs against its state: a submitted job, bound to its
/// time stamp, or in tests whatever the test needs to run there.
type Work = Box<dyn FnOnce(&mut PersistState) -> Result<(), SaveError> + Send>;

struct Command {
    work: Work,
    done: SaveCompletion,
}

enum Worker {
    Thread {
        commands: mpsc::Sender<Command>,
        thread: std::thread::JoinHandle<()>,
    },
    /// No thread could be started: jobs run on the caller's thread.
    Inline(Box<PersistState>),
    /// Terminal state: submitted work is refused because no worker will run it.
    Retired,
}

/// Owns a session's files; see the module docs.
pub struct SessionPersister {
    worker: Worker,
    /// Completion wakeup; unobserved completions coalesce into one `Notify` permit.
    finished: Arc<Notify>,
}

impl SessionPersister {
    /// Takes over the data directory `lease` guards. `backup_policy`: the
    /// first save must copy the session file aside before replacing it (it
    /// could not be loaded, or restore dropped part of it). `finished` is
    /// fired each time a submitted job ends, once its result can be read.
    pub fn spawn(
        lease: DataDirLease,
        backup_policy: super::recovery::SessionBackupPolicy,
        finished: Arc<Notify>,
    ) -> Self {
        let state = PersistState::new(SessionWriter::new(lease, backup_policy));
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
                while let Ok(Command { work, done }) = command_receiver.recv() {
                    done.complete(state.run_guarded(work));
                }
                // Every sender is gone: the persister was retired or dropped,
                // after the jobs queued before it were completed or abandoned.
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
    /// refused without touching the files. After a worker job panics, this and later jobs fail while
    /// the worker keeps the lease until retirement. Every job
    /// fires the completion signal when it ends, including one that was
    /// refused.
    pub fn submit(&mut self, job: PersistJob, now: SystemTime) -> PendingSave {
        self.submit_work(Box::new(move |state| state.run(job, now)))
    }

    fn submit_work(&mut self, work: Work) -> PendingSave {
        let (done, pending) = PendingSave::signalled_channel(Some(Arc::clone(&self.finished)));
        match &mut self.worker {
            Worker::Thread { commands, .. } => {
                if let Err(mpsc::SendError(command)) = commands.send(Command { work, done }) {
                    command.done.complete(Err(abandoned()));
                }
            }
            Worker::Inline(state) => done.complete(state.run_guarded(work)),
            Worker::Retired => done.complete(Err(SaveError::Refused(SaveRefusal::Retired))),
        }
        pending
    }

    /// Finishes or abandons every queued job, then releases the data directory
    /// lease. Later jobs are refused.
    pub fn retire(&mut self) {
        match std::mem::replace(&mut self.worker, Worker::Retired) {
            Worker::Thread { commands, thread } => {
                drop(commands);
                if thread.join().is_err() {
                    // A job's panic is caught on the thread; one outside a job
                    // (retiring the writer) dropped its state, lease included.
                    tracing::error!(
                        event = "persist.actor",
                        subsystem = "persist",
                        outcome = "panicked",
                        "session persister thread panicked"
                    );
                }
            }
            Worker::Inline(state) => (*state).retire(),
            Worker::Retired => {}
        }
    }
}

impl Drop for SessionPersister {
    /// Dropping without retiring still drains the queue and releases the
    /// lease before returning, so the directory is free for the next owner.
    /// This can block until a queued save finishes. Callers that need to keep
    /// a runtime worker responsive must move retirement itself to a blocking
    /// thread; detaching here would release the lease while a write could
    /// still be changing the files.
    fn drop(&mut self) {
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// The inline fallback, built directly: jobs run on the submitting
    /// thread, and `finished` fires before `submit` returns.
    fn inline(
        lease: DataDirLease,
        backup_policy: super::super::recovery::SessionBackupPolicy,
        finished: Arc<Notify>,
    ) -> SessionPersister {
        SessionPersister {
            worker: Worker::Inline(Box::new(PersistState::new(SessionWriter::new(
                lease,
                backup_policy,
            )))),
            finished,
        }
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::schema::SNAPSHOT_VERSION,
            "host_theme": super::super::schema::SavedHostTheme::default(),
            "workspaces": [{
                "id": "w1",
                "name": "w",
                "next_public_pane_number": 2,
                "layout": {
                    "Pane": {
                        "cwd": "/shepr-persister-test",
                        "public_number": 1,
                        "label": null
                    }
                },
                "zoomed": false,
                "focused": 1,
                "root_pane": 1
            }],
            "active": 0
        }))
        .expect("test precondition")
    }

    #[test]
    fn jobs_run_in_order_and_retiring_releases_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister");
        let directory = scratch.join("data");
        let lease = DataDirLease::acquire(&directory).expect("lease");
        let mut persister = SessionPersister::spawn(
            lease,
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            signal(),
        );
        let now = SystemTime::now();
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
            }),
            now,
        );
        let cleared = persister.submit(PersistJob::Clear, now);
        saved.wait().expect("save");
        cleared.wait().expect("clear");
        assert!(
            !directory
                .join(shepr_paths::SESSION_FILE_NAME)
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
            }),
            now,
        );
        persister.retire();
        assert!(
            last.try_finish().is_some_and(|result| result.is_ok()),
            "retiring finishes the queued save first"
        );
        DataDirLease::acquire(&directory).expect("the lease is free after retiring");
        assert!(matches!(
            persister.submit(PersistJob::Clear, now).wait(),
            Err(SaveError::Refused(SaveRefusal::Retired))
        ));
        assert!(
            directory
                .join(shepr_paths::SESSION_FILE_NAME)
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
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            signal(),
        );
        drop(persister);
        DataDirLease::acquire(&directory).expect("the lease is free after the drop");
    }

    #[test]
    fn an_inline_persister_runs_jobs_on_the_caller_and_releases_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister-inline");
        let directory = scratch.join("data");
        let mut persister = inline(
            DataDirLease::acquire(&directory).expect("lease"),
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            signal(),
        );
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
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

    /// A panicking job fails, every later job fails without running, and the
    /// lease stays held until the persister is retired.
    fn assert_a_panicking_job_stops_saves_and_keeps_the_lease(
        directory: &std::path::Path,
        mut persister: SessionPersister,
    ) {
        let now = SystemTime::now();
        let panicked = persister
            .submit_work(Box::new(|_| panic!("test job panicked")))
            .wait();
        assert_eq!(
            panicked.expect_err("the panicking job fails").to_string(),
            stopped_after_panic().to_string()
        );
        let later = persister
            .submit(
                PersistJob::Save(SessionBundle {
                    snapshot: snapshot(),
                    cwds: PendingCwds::default(),
                }),
                now,
            )
            .wait();
        assert_eq!(
            later.expect_err("a job after the panic fails").to_string(),
            stopped_after_panic().to_string()
        );
        assert!(
            !directory
                .join(shepr_paths::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat"),
            "the job after the panic did not run"
        );
        assert_eq!(
            DataDirLease::acquire(directory).err().map(|err| err.kind()),
            Some(io::ErrorKind::ResourceBusy),
            "the persister still holds the lease after the panic"
        );
        persister.retire();
        DataDirLease::acquire(directory).expect("the lease is free after retiring");
    }

    #[test]
    fn a_panicking_job_on_the_thread_stops_saves_and_keeps_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister-panic-thread");
        let directory = scratch.join("data");
        let persister = SessionPersister::spawn(
            DataDirLease::acquire(&directory).expect("lease"),
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            signal(),
        );
        assert!(matches!(persister.worker, Worker::Thread { .. }));
        assert_a_panicking_job_stops_saves_and_keeps_the_lease(&directory, persister);
    }

    #[test]
    fn a_panicking_inline_job_stops_saves_and_keeps_the_lease() {
        let scratch = crate::test_support::ScratchDir::new("persister-panic-inline");
        let directory = scratch.join("data");
        let persister = inline(
            DataDirLease::acquire(&directory).expect("lease"),
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            signal(),
        );
        assert_a_panicking_job_stops_saves_and_keeps_the_lease(&directory, persister);
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
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
            Arc::clone(&finished),
        );
        let saved = persister.submit(
            PersistJob::Save(SessionBundle {
                snapshot: snapshot(),
                cwds: PendingCwds::default(),
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
        let mut persister = inline(
            DataDirLease::acquire(&directory).expect("lease"),
            super::super::recovery::SessionBackupPolicy::NoBackupNeeded,
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
