//! The usage worker: one scheduler thread that owns every source, credential
//! generation and account, and short-lived threads for everything that
//! blocks (credential reads, the registry load, the curl probe, requests).
//! The scheduler itself never touches the filesystem or the network.
//!
//! Every completion is checked against what it captured when it started:
//!
//! - A credential read carries its source incarnation, its attempt id and the
//!   reconciliation epoch. A read of a removed source, a read past its
//!   deadline, or a read started before a pause is dropped (its reader slot
//!   is still released).
//! - A request carries its credential generation and account. Credential
//!   refusals (401, 403) mark only that generation, kept as a tombstone that
//!   outlives the credential; throttling, failures and measurements are
//!   evidence about the account and apply to it.
//!
//! Every thread delivers a completion even if it panics, so no slot, probe
//! or host stays held by a thread that died.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime};

use crate::credentials::{Credential, Generation, ParsedCredentials};
use crate::limits::{
    MAX_CREDENTIAL_READERS, MAX_EVENTS_PER_PASS, MAX_GATES, MAX_TOMBSTONES, WORKER_MIN_SLEEP,
};
use crate::model::{
    AccountKey, AccountView, Availability, ClaudeMeasurement, CodexMeasurement, EndpointHealth,
    FailureClass, IdentityProblem, Measurement, Plan, ReadFailure, RegistryHealth, SourceView,
    TransportHealth, UsageSnapshot,
};
use crate::reader::ReadResult;
use crate::registry::RegistryWriter;
use crate::schedule::{Backoff, Gate, Timing};
use crate::secret::Secret;
use crate::source::{DiscoveredSource, Provider, SourceLocator, SourceOrigin};
use crate::transport::{CurlProblem, Host, Outcome, Request, RetryAfter};
use crate::{claude_api, codex_api};

/// The blocking operations the worker delegates. Production uses the real
/// filesystem and curl; tests stand a double in.
pub(crate) trait UsageIo: Send + Sync + 'static {
    fn read(&self, locator: &SourceLocator) -> ReadResult;
    fn probe(&self) -> Result<(), CurlProblem>;
    fn fetch(&self, request: Request) -> Outcome;
}

/// The real filesystem and the system curl.
pub(crate) struct SystemIo {
    curl: Mutex<Option<crate::transport::Curl>>,
    user_agent: String,
}

impl SystemIo {
    pub(crate) fn new(user_agent: String) -> Self {
        Self {
            curl: Mutex::new(None),
            user_agent,
        }
    }

    fn curl(&self) -> MutexGuard<'_, Option<crate::transport::Curl>> {
        // The value is replaced whole, so a panic cannot leave it torn.
        self.curl.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UsageIo for SystemIo {
    fn read(&self, locator: &SourceLocator) -> ReadResult {
        crate::reader::read_source(locator)
    }

    fn probe(&self) -> Result<(), CurlProblem> {
        let curl = crate::transport::find_curl()?;
        *self.curl() = Some(curl);
        Ok(())
    }

    fn fetch(&self, request: Request) -> Outcome {
        let curl = self.curl().clone();
        match curl {
            Some(curl) => curl.fetch(request, &self.user_agent),
            None => Outcome::Failed(FailureClass::Transport),
        }
    }
}

/// Runs a closure if dropped while armed: how a thread that unwinds still
/// delivers its completion.
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

enum Event {
    /// The control state changed; read it.
    Control,
    Shutdown,
    RegistryLoaded(std::io::Result<Vec<SourceLocator>>),
    ReadDone {
        locator: SourceLocator,
        incarnation: u64,
        attempt: u64,
        result: ReadResult,
    },
    ProbeDone(Result<(), CurlProblem>),
    FetchDone {
        attempt: u64,
        outcome: Outcome,
        received_at: SystemTime,
    },
    RegistryWritten(bool),
}

/// What the server last asked for. Latest value wins: a burst of changes is
/// one read of this, never a queue.
#[derive(Default)]
struct Control {
    sources: Option<Vec<DiscoveredSource>>,
    /// The latest active value, when one was set since the last read.
    active: Option<bool>,
    /// How many times activity was set on. Coalescing keeps only the latest
    /// value, so this is what shows an off-and-on that happened in between:
    /// every activation forces credentials to be read again.
    activations: u64,
}

struct ControlShared {
    control: Mutex<Control>,
    /// A `Control` event is queued and not yet read.
    signalled: AtomicBool,
}

impl ControlShared {
    fn lock(&self) -> MutexGuard<'_, Control> {
        // Fields are replaced whole.
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the server configures.
pub struct UsageConfig {
    /// The honest User-Agent sent with every request (`shepr/<version>`).
    pub user_agent: String,
    /// Where remembered sources live; `None` remembers nothing across runs.
    pub registry: Option<PathBuf>,
}

/// The handle to the usage worker. Dropping it stops the scheduler; threads
/// it started finish on their own and their results are dropped.
pub struct UsageWorker {
    events: mpsc::Sender<Event>,
    control: Arc<ControlShared>,
    shared: Arc<Mutex<Arc<UsageSnapshot>>>,
}

impl UsageWorker {
    /// Starts a worker. `wake` is called, on the worker's thread, after each
    /// new snapshot is published; it must not block (a `try_send`).
    pub fn start(config: UsageConfig, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let io = Arc::new(SystemIo::new(config.user_agent));
        Self::start_with(io, config.registry, Timing::default(), Box::new(wake))
    }

    pub(crate) fn start_with(
        io: Arc<dyn UsageIo>,
        registry: Option<PathBuf>,
        timing: Timing,
        wake: Box<dyn Fn() + Send>,
    ) -> std::io::Result<Self> {
        let (events, receiver) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Arc::new(UsageSnapshot::default())));
        let control = Arc::new(ControlShared {
            control: Mutex::new(Control::default()),
            signalled: AtomicBool::new(false),
        });
        let incarnation = next_incarnation();
        let thread_shared = Arc::clone(&shared);
        let thread_control = Arc::clone(&control);
        let thread_events = events.clone();
        std::thread::Builder::new()
            .name("usage-worker".into())
            .spawn(move || {
                let mut worker = Worker::new(
                    io,
                    timing,
                    thread_events,
                    thread_control,
                    thread_shared,
                    wake,
                    incarnation,
                    registry,
                );
                worker.run(&receiver);
            })?;
        Ok(Self {
            events,
            control,
            shared,
        })
    }

    /// Replaces the set of sources the server discovered. Remembered sources
    /// are added by the worker itself.
    pub fn set_sources(&self, sources: Vec<DiscoveredSource>) {
        self.control.lock().sources = Some(sources);
        self.signal();
    }

    /// Whether any client is attached. Inactive means no reads and no
    /// requests; the last snapshot stays readable.
    pub fn set_active(&self, active: bool) {
        {
            let mut control = self.control.lock();
            control.active = Some(active);
            if active {
                control.activations += 1;
            }
        }
        self.signal();
    }

    fn signal(&self) {
        if !self.control.signalled.swap(true, Ordering::SeqCst) {
            self.events.send(Event::Control).ok();
        }
    }

    /// The latest published snapshot.
    pub fn snapshot(&self) -> Arc<UsageSnapshot> {
        Arc::clone(&lock(&self.shared))
    }
}

impl Drop for UsageWorker {
    fn drop(&mut self) {
        self.events.send(Event::Shutdown).ok();
    }
}

fn lock(shared: &Mutex<Arc<UsageSnapshot>>) -> MutexGuard<'_, Arc<UsageSnapshot>> {
    // The guarded value is an `Arc` replaced whole.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next_incarnation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy)]
struct ReadAttempt {
    id: u64,
    started: Instant,
    epoch: u64,
}

struct SourceState {
    origin: SourceOrigin,
    incarnation: u64,
    reading: Option<ReadAttempt>,
    last_read: Option<Instant>,
    credential: Option<Credential>,
    /// What the last accepted read said, when not a usable credential.
    state: SourceReadState,
    /// Active time at the first failed read of the current failing streak.
    failing_since: Option<Duration>,
    /// The last read could not start: every reader slot was taken.
    readers_exhausted: bool,
    /// The epoch of the last accepted read. Below the worker's epoch, the
    /// credential held predates a pause and serves no request.
    reconciled_epoch: u64,
    /// The last read was discarded (late or from before a pause): the
    /// credential held is unconfirmed and serves no request until a read is
    /// accepted. Releasing the reader slot does not restore it.
    unconfirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceReadState {
    NotYetRead,
    Usable,
    Missing,
    LoggedOut,
    ApiKey,
    Unsupported,
    Failed(ReadFailure),
}

impl SourceState {
    fn stalled(&self, now: Instant, timing: &Timing) -> bool {
        self.reading
            .is_some_and(|read| now.saturating_duration_since(read.started) >= timing.read_deadline)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    /// Claude: `/profile` has not answered for this generation yet.
    Pending,
    Known(AccountKey),
    Unresolved(IdentityProblem),
}

struct GenerationState {
    provider: Provider,
    identity: Identity,
    email: Option<String>,
    plan: Option<String>,
}

impl GenerationState {
    fn account(&self, generation: Generation) -> Option<AccountKey> {
        match &self.identity {
            Identity::Pending => None,
            Identity::Known(key) => Some(key.clone()),
            Identity::Unresolved(_) => Some(AccountKey::Unresolved {
                provider: self.provider,
                generation,
            }),
        }
    }

    /// Whether `/profile` should be asked to establish (or retry) identity.
    fn wants_identity(&self) -> bool {
        self.provider == Provider::Claude
            && matches!(
                self.identity,
                Identity::Pending | Identity::Unresolved(IdentityProblem::ProfileIncomplete)
            )
    }
}

struct AccountState {
    label: Option<String>,
    plan: Plan,
    measurement: Option<Measurement>,
    usage: EndpointHealth,
    profile: Option<EndpointHealth>,
    identity_problem: Option<IdentityProblem>,
}

impl AccountState {
    fn new(key: &AccountKey, identity_problem: Option<IdentityProblem>) -> Self {
        Self {
            label: None,
            plan: Plan::default(),
            measurement: None,
            usage: EndpointHealth::Pending,
            profile: matches!(key, AccountKey::Claude { .. }).then_some(EndpointHealth::Pending),
            identity_problem,
        }
    }
}

/// What a gate schedules. Gates live apart from accounts and generations so
/// no garbage collection, rotation or rediscovery resets a backoff.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GateKey {
    Usage(AccountKey),
    Profile(AccountKey),
    Identity(Generation),
}

#[derive(Default)]
struct HostState {
    in_flight: Option<u64>,
    /// No request starts before this (spacing, or a held slot's retry).
    not_before: Option<Instant>,
}

#[derive(Debug, Clone)]
enum Job {
    Identity(Generation),
    Usage(AccountKey),
    Profile(AccountKey),
}

impl Job {
    fn gate(&self) -> GateKey {
        match self {
            Self::Identity(generation) => GateKey::Identity(*generation),
            Self::Usage(key) => GateKey::Usage(key.clone()),
            Self::Profile(key) => GateKey::Profile(key.clone()),
        }
    }
}

struct Pending {
    host: Host,
    job: Job,
    generation: Generation,
}

enum Transport {
    Unprobed,
    Probing,
    Ready,
    Failed {
        problem: CurlProblem,
        retry: Instant,
    },
}

enum RegistryLoad {
    /// Loading on its thread; nothing is saved until it finishes, so a
    /// partial set never overwrites the file.
    Loading,
    Loaded,
    Disabled,
}

/// Generations refused by the provider, kept after their credential is
/// gone so the same digest returning stays refused. Bounded; the oldest go.
#[derive(Default)]
struct Tombstones {
    entries: HashMap<Generation, u64>,
    next: u64,
}

impl Tombstones {
    /// Records a refusal. Past the bound the oldest tombstone whose
    /// credential no source holds any more is forgotten; one still held is
    /// never evicted, as that would make an unchanged refused credential
    /// eligible again.
    fn insert(&mut self, generation: Generation, held: &BTreeSet<Generation>) {
        self.next += 1;
        self.entries.insert(generation, self.next);
        if self.entries.len() > MAX_TOMBSTONES
            && let Some(oldest) = self
                .entries
                .iter()
                .filter(|(generation, _)| !held.contains(*generation))
                .min_by_key(|(_, order)| **order)
                .map(|(generation, _)| *generation)
        {
            self.entries.remove(&oldest);
        }
    }

    fn contains(&self, generation: Generation) -> bool {
        self.entries.contains_key(&generation)
    }
}

struct Worker {
    io: Arc<dyn UsageIo>,
    timing: Timing,
    events: mpsc::Sender<Event>,
    control: Arc<ControlShared>,
    shared: Arc<Mutex<Arc<UsageSnapshot>>>,
    wake: Box<dyn Fn() + Send>,
    incarnation: u64,
    revision: u64,
    published: Arc<UsageSnapshot>,
    active: bool,
    active_time: Duration,
    last_tick: Instant,
    /// Raised on every activation: reads started before it are discarded.
    epoch: u64,
    seen_activations: u64,
    discovered: Vec<DiscoveredSource>,
    remembered: BTreeSet<SourceLocator>,
    registry: Option<RegistryWriter>,
    registry_load: RegistryLoad,
    registry_health: RegistryHealth,
    sources: BTreeMap<SourceLocator, SourceState>,
    /// The reader thread alive per locator, whatever its source's
    /// incarnation: a re-added source never gets an overlapping reader.
    live_reads: HashMap<SourceLocator, u64>,
    readers: usize,
    next_id: u64,
    generations: HashMap<Generation, GenerationState>,
    rejected: Tombstones,
    denied: Tombstones,
    gates: HashMap<GateKey, Gate>,
    accounts: BTreeMap<AccountKey, AccountState>,
    hosts: HashMap<Host, HostState>,
    pending: HashMap<u64, Pending>,
    transport: Transport,
}

impl Worker {
    fn new(
        io: Arc<dyn UsageIo>,
        timing: Timing,
        events: mpsc::Sender<Event>,
        control: Arc<ControlShared>,
        shared: Arc<Mutex<Arc<UsageSnapshot>>>,
        wake: Box<dyn Fn() + Send>,
        incarnation: u64,
        registry_path: Option<PathBuf>,
    ) -> Self {
        let mut registry = None;
        let mut registry_load = RegistryLoad::Disabled;
        let mut registry_health = RegistryHealth::Disabled;
        if let Some(path) = registry_path {
            registry_load = RegistryLoad::Loading;
            registry_health = RegistryHealth::Ok;
            let load_events = events.clone();
            let load_path = path.clone();
            let loading = std::thread::Builder::new()
                .name("usage-registry".into())
                .spawn(move || {
                    let unwind_events = load_events.clone();
                    let guard = OnUnwind(Some(move || {
                        unwind_events
                            .send(Event::RegistryLoaded(Err(std::io::Error::other(
                                "the registry load panicked",
                            ))))
                            .ok();
                    }));
                    let loaded = crate::registry::load(&load_path);
                    load_events.send(Event::RegistryLoaded(loaded)).ok();
                    guard.defuse();
                });
            if loading.is_err() {
                registry_load = RegistryLoad::Loaded;
                registry_health = RegistryHealth::ReadFailed;
            }
            let report = events.clone();
            match RegistryWriter::start(path, move |ok| {
                report.send(Event::RegistryWritten(ok)).ok();
            }) {
                Ok(writer) => registry = Some(writer),
                Err(error) => {
                    shepr_platform::structured_log!(
                        WARN,
                        event = usage.registry_write,
                        outcome = Error,
                        error = %error,
                        "could not start the remembered usage sources writer"
                    );
                    registry_health = RegistryHealth::WriteFailed;
                }
            }
        }
        // clock-io-ok: the worker samples the clock once per pass.
        let now = Instant::now();
        Self {
            io,
            timing,
            events,
            control,
            shared,
            wake,
            incarnation,
            revision: 0,
            published: Arc::new(UsageSnapshot::default()),
            active: false,
            active_time: Duration::ZERO,
            last_tick: now,
            epoch: 0,
            seen_activations: 0,
            discovered: Vec::new(),
            remembered: BTreeSet::new(),
            registry,
            registry_load,
            registry_health,
            sources: BTreeMap::new(),
            live_reads: HashMap::new(),
            readers: 0,
            next_id: 0,
            generations: HashMap::new(),
            rejected: Tombstones::default(),
            denied: Tombstones::default(),
            gates: HashMap::new(),
            accounts: BTreeMap::new(),
            hosts: HashMap::new(),
            pending: HashMap::new(),
            transport: Transport::Unprobed,
        }
    }

    fn run(&mut self, receiver: &mpsc::Receiver<Event>) {
        loop {
            // clock-io-ok: the worker samples the clock once per pass.
            let now = Instant::now();
            let wait = self.next_wake(now).saturating_duration_since(now);
            let first = match receiver.recv_timeout(wait) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            // clock-io-ok: as above.
            let now = Instant::now();
            // clock-io-ok: wall time for observations and expiry.
            let wall = SystemTime::now();
            self.account_active_time(now);
            // A bounded batch per pass: a burst of completions cannot starve
            // scheduling, and the rest is taken on the next pass at once.
            let batch = first
                .into_iter()
                .chain(receiver.try_iter().take(MAX_EVENTS_PER_PASS));
            for event in batch.collect::<Vec<_>>() {
                if !self.handle(event, now, wall) {
                    return;
                }
            }
            self.tick(now, wall);
            self.publish(now, wall);
        }
    }

    fn account_active_time(&mut self, now: Instant) {
        if self.active {
            self.active_time += now.saturating_duration_since(self.last_tick);
        }
        self.last_tick = now;
    }

    /// Applies one event; false to stop.
    fn handle(&mut self, event: Event, now: Instant, wall: SystemTime) -> bool {
        match event {
            Event::Shutdown => return false,
            Event::Control => self.apply_control(),
            Event::RegistryLoaded(loaded) => self.registry_loaded(loaded),
            Event::ReadDone {
                locator,
                incarnation,
                attempt,
                result,
            } => self.read_done(&locator, incarnation, attempt, result, now),
            Event::ProbeDone(result) => {
                self.transport = match result {
                    Ok(()) => Transport::Ready,
                    Err(problem) => {
                        shepr_platform::structured_log!(
                            WARN,
                            event = usage.transport,
                            outcome = Unavailable,
                            error = ?problem,
                            "no usable curl for usage requests; retrying later"
                        );
                        Transport::Failed {
                            problem,
                            retry: now + self.timing.probe_retry,
                        }
                    }
                };
            }
            Event::FetchDone {
                attempt,
                outcome,
                received_at,
            } => self.fetch_done(attempt, outcome, received_at, now, wall),
            Event::RegistryWritten(ok) => {
                self.registry_health = if ok {
                    RegistryHealth::Ok
                } else {
                    RegistryHealth::WriteFailed
                };
            }
        }
        true
    }

    fn apply_control(&mut self) {
        // Clear the flag before reading, so a change made after this read
        // signals again.
        self.control.signalled.store(false, Ordering::SeqCst);
        let (sources, active, activations) = {
            let mut control = self.control.lock();
            (
                control.sources.take(),
                control.active.take(),
                control.activations,
            )
        };
        if activations != self.seen_activations {
            // Every activation, however coalesced: credentials read before it
            // serve nothing until a fresh read lands; missed polls are not
            // queued.
            self.seen_activations = activations;
            self.epoch += 1;
            for source in self.sources.values_mut() {
                source.last_read = None;
            }
        }
        if let Some(active) = active {
            self.active = active;
        }
        if let Some(sources) = sources {
            self.discovered = sources;
            self.apply_sources();
        }
    }

    fn registry_loaded(&mut self, loaded: std::io::Result<Vec<SourceLocator>>) {
        let unsaved = !self.remembered.is_empty();
        match loaded {
            Ok(sources) => {
                self.remembered.extend(sources);
            }
            Err(error) => {
                shepr_platform::structured_log!(
                    WARN,
                    event = usage.registry_read,
                    outcome = Error,
                    error = %error,
                    "could not read remembered usage sources; starting without them"
                );
                self.registry_health = RegistryHealth::ReadFailed;
            }
        }
        self.registry_load = RegistryLoad::Loaded;
        self.apply_sources();
        if unsaved {
            self.save_registry();
        }
    }

    /// Rebuilds the source set from the server's discovery and the
    /// remembered sources. A newly seen agent source is remembered.
    fn apply_sources(&mut self) {
        let mut wanted: BTreeMap<SourceLocator, SourceOrigin> = BTreeMap::new();
        let mut remembered_changed = false;
        for source in &self.discovered {
            if source.origin == SourceOrigin::Agent
                && self.remembered.len() < crate::limits::MAX_REMEMBERED_SOURCES
                && self.remembered.insert(source.locator.clone())
            {
                remembered_changed = true;
            }
            let origin = wanted
                .entry(source.locator.clone())
                .or_insert(source.origin);
            *origin = (*origin).min(source.origin);
        }
        for locator in &self.remembered {
            wanted
                .entry(locator.clone())
                .or_insert(SourceOrigin::Remembered);
        }
        // A removed source takes its evidence with it: recompute what it held.
        let mut orphaned = Vec::new();
        self.sources.retain(|locator, source| {
            let keep = wanted.contains_key(locator);
            if !keep && let Some(credential) = &source.credential {
                orphaned.push(credential.generation);
            }
            keep
        });
        for generation in orphaned {
            self.reconcile_generation(generation);
        }
        for (locator, origin) in wanted {
            match self.sources.get_mut(&locator) {
                Some(source) => source.origin = origin,
                None => {
                    self.next_id += 1;
                    self.sources.insert(
                        locator,
                        SourceState {
                            origin,
                            incarnation: self.next_id,
                            reading: None,
                            last_read: None,
                            credential: None,
                            state: SourceReadState::NotYetRead,
                            failing_since: None,
                            readers_exhausted: false,
                            reconciled_epoch: self.epoch,
                            unconfirmed: false,
                        },
                    );
                }
            }
        }
        if remembered_changed {
            self.save_registry();
        }
        self.collect_garbage();
    }

    fn save_registry(&self) {
        if !matches!(self.registry_load, RegistryLoad::Loaded) {
            return;
        }
        if let Some(writer) = &self.registry {
            writer.save(self.remembered.iter().cloned().collect());
        }
    }

    fn read_done(
        &mut self,
        locator: &SourceLocator,
        incarnation: u64,
        attempt: u64,
        result: ReadResult,
        now: Instant,
    ) {
        // The reader thread is gone whatever its result is worth.
        self.readers = self.readers.saturating_sub(1);
        if self.live_reads.get(locator) == Some(&attempt) {
            self.live_reads.remove(locator);
        }
        let epoch = self.epoch;
        let deadline = self.timing.read_deadline;
        let Some(source) = self.sources.get_mut(locator) else {
            return;
        };
        let Some(read) = source.reading.filter(|read| read.id == attempt) else {
            return;
        };
        if source.incarnation != incarnation {
            return;
        }
        source.reading = None;
        source.last_read = Some(now);
        // Too late, or from before a pause: discarded, and the next read is
        // the one that counts.
        if read.epoch != epoch || now.saturating_duration_since(read.started) > deadline {
            source.unconfirmed = true;
            if read.epoch != epoch {
                source.last_read = None;
            }
            return;
        }
        source.reconciled_epoch = epoch;
        source.unconfirmed = false;
        match result {
            ReadResult::Parsed(ParsedCredentials::Usable(credential)) => {
                source.failing_since = None;
                source.state = SourceReadState::Usable;
                let generation = credential.generation;
                let previous = source
                    .credential
                    .replace(credential)
                    .map(|credential| credential.generation);
                // Both sides of a change: the generation left behind loses a
                // holder and its evidence too.
                self.reconcile_generation(generation);
                if let Some(previous) = previous.filter(|previous| *previous != generation) {
                    self.reconcile_generation(previous);
                }
            }
            ReadResult::Parsed(other) => {
                // A valid parse showing logout or another mode ends the
                // binding at once; no grace applies.
                let previous = source
                    .credential
                    .take()
                    .map(|credential| credential.generation);
                source.failing_since = None;
                source.state = match other {
                    ParsedCredentials::LoggedOut => SourceReadState::LoggedOut,
                    ParsedCredentials::ApiKey => SourceReadState::ApiKey,
                    ParsedCredentials::Unsupported | ParsedCredentials::Usable(_) => {
                        SourceReadState::Unsupported
                    }
                };
                if let Some(previous) = previous {
                    self.reconcile_generation(previous);
                }
            }
            ReadResult::Missing => {
                let previous = source
                    .credential
                    .take()
                    .map(|credential| credential.generation);
                source.failing_since = None;
                source.state = SourceReadState::Missing;
                if source.origin == SourceOrigin::Remembered && self.remembered.remove(locator) {
                    self.sources.remove(locator);
                    self.save_registry();
                }
                if let Some(previous) = previous {
                    self.reconcile_generation(previous);
                }
            }
            ReadResult::Failed(failure) => {
                // Requests stop at the first failure; the binding and history
                // stay until the grace in active time has passed.
                source.state = SourceReadState::Failed(failure);
                let since = *source.failing_since.get_or_insert(self.active_time);
                if self.active_time.saturating_sub(since) >= self.timing.unreadable_grace
                    && let Some(previous) = source.credential.take()
                {
                    self.reconcile_generation(previous.generation);
                }
            }
        }
        self.collect_garbage();
    }

    /// Recomputes a generation's facts from every source currently holding
    /// it. For Codex, identity is the binding evidence of all of them: it is
    /// resolved only when each names the same bound principal and account,
    /// and it is reconsidered on every read, since the id token is outside
    /// the digest and can change while the token stays.
    fn reconcile_generation(&mut self, generation: Generation) {
        let holders: Vec<&Credential> = self
            .sources
            .values()
            .filter_map(|source| source.credential.as_ref())
            .filter(|credential| credential.generation == generation)
            .collect();
        let Some(first) = holders.first() else {
            return;
        };
        let provider = first.provider;
        let email = holders
            .iter()
            .find_map(|credential| credential.identity.email.clone());
        let plan = holders
            .iter()
            .find_map(|credential| credential.identity.plan.clone());
        let codex_identity = (provider == Provider::Codex).then(|| {
            let mut identities = holders.iter().map(|credential| codex_identity(credential));
            let first = identities
                .next()
                .unwrap_or(Identity::Unresolved(IdentityProblem::MissingClaims));
            if identities.all(|identity| identity == first) {
                first
            } else {
                Identity::Unresolved(IdentityProblem::IdTokenMismatch)
            }
        });
        let state = self
            .generations
            .entry(generation)
            .or_insert_with(|| GenerationState {
                provider,
                identity: Identity::Pending,
                email: None,
                plan: None,
            });
        state.email = email;
        state.plan = plan;
        if let Some(identity) = codex_identity {
            state.identity = identity;
        }
        self.ensure_account_for(generation);
    }

    fn ensure_account_for(&mut self, generation: Generation) {
        let Some(state) = self.generations.get(&generation) else {
            return;
        };
        let Some(key) = state.account(generation) else {
            return;
        };
        let problem = match &state.identity {
            Identity::Unresolved(problem) => Some(*problem),
            Identity::Pending | Identity::Known(_) => None,
        };
        let email = state.email.clone();
        let plan = state.plan.clone();
        let account = self
            .accounts
            .entry(key.clone())
            .or_insert_with(|| AccountState::new(&key, problem));
        account.identity_problem = problem;
        // Codex hints come from the credential; Claude's come from
        // `/profile` and are not overwritten here.
        if key.provider() == Provider::Codex {
            if email.is_some() {
                account.label = email;
            }
            if plan.is_some() && account.plan.kind.is_none() {
                account.plan.kind = plan;
            }
        }
    }

    /// Drops generations no source supplies and no request holds, and
    /// accounts with no source and no measurement. Tombstones and gates are
    /// not touched here: they outlive what they describe.
    fn collect_garbage(&mut self) {
        let live: BTreeSet<Generation> = self
            .sources
            .values()
            .filter_map(|source| {
                source
                    .credential
                    .as_ref()
                    .map(|credential| credential.generation)
            })
            .chain(self.pending.values().map(|pending| pending.generation))
            .collect();
        self.generations
            .retain(|generation, _| live.contains(generation));
        let supplied: BTreeSet<AccountKey> = self
            .generations
            .iter()
            .filter_map(|(generation, state)| state.account(*generation))
            .collect();
        self.accounts
            .retain(|key, account| supplied.contains(key) || account.measurement.is_some());
    }

    /// Forgets gates that constrain nothing any more: their account or
    /// generation is gone and their due time has passed. A live gate, or one
    /// still holding a future bound, is never dropped, so no cleanup resets a
    /// cadence or a backoff. Past the bound with only such gates left, the
    /// map grows and the condition is logged.
    fn prune_gates(&mut self, now: Instant) {
        if self.gates.len() <= MAX_GATES {
            return;
        }
        let accounts = &self.accounts;
        let generations = &self.generations;
        self.gates.retain(|key, gate| {
            let live = match key {
                GateKey::Usage(account) | GateKey::Profile(account) => {
                    accounts.contains_key(account)
                }
                GateKey::Identity(generation) => generations.contains_key(generation),
            };
            live || gate.due_at().is_some_and(|due| due > now)
        });
        if self.gates.len() > MAX_GATES {
            shepr_platform::structured_log!(
                WARN,
                event = usage.schedule,
                outcome = Exhausted,
                gates = self.gates.len(),
                "more usage schedules are live than the bound; keeping them all"
            );
        }
    }

    /// Ends the binding of every source whose failing streak has outlasted
    /// the grace in active time, whether or not another read has finished:
    /// a hung reader must not keep an unconfirmed binding forever. Measurements
    /// stay with their accounts; the stalled reader keeps its slot until it
    /// really ends.
    fn expire_failed_reads(&mut self) {
        let active_time = self.active_time;
        let grace = self.timing.unreadable_grace;
        let mut released = Vec::new();
        for source in self.sources.values_mut() {
            if let Some(since) = source.failing_since
                && active_time.saturating_sub(since) >= grace
                && let Some(credential) = source.credential.take()
            {
                released.push(credential.generation);
            }
        }
        for generation in released {
            self.reconcile_generation(generation);
        }
        self.collect_garbage();
    }

    /// The generations sources hold now, which tombstones keep.
    fn held_generations(&self) -> BTreeSet<Generation> {
        self.sources
            .values()
            .filter_map(|source| {
                source
                    .credential
                    .as_ref()
                    .map(|credential| credential.generation)
            })
            .collect()
    }

    fn tick(&mut self, now: Instant, wall: SystemTime) {
        self.prune_gates(now);
        self.expire_failed_reads();
        if !self.active {
            return;
        }
        self.start_reads(now);
        // curl is probed only once there is something to ask: a host with no
        // usable Claude or Codex credential never runs it.
        let has_work = [Host::Anthropic, Host::ChatGpt]
            .into_iter()
            .any(|host| !self.candidate_jobs(host, now, wall).is_empty());
        match &self.transport {
            Transport::Unprobed if has_work => self.start_probe(now),
            Transport::Failed { retry, .. } if has_work && now >= *retry => self.start_probe(now),
            Transport::Ready => {
                for host in [Host::Anthropic, Host::ChatGpt] {
                    self.start_request(host, now, wall);
                }
            }
            Transport::Unprobed | Transport::Probing | Transport::Failed { .. } => {}
        }
    }

    fn start_reads(&mut self, now: Instant) {
        let reread = self.timing.reread;
        let due: Vec<SourceLocator> = self
            .sources
            .iter()
            .filter(|(locator, source)| {
                source.reading.is_none()
                    && !self.live_reads.contains_key(*locator)
                    && source
                        .last_read
                        .is_none_or(|last| now.saturating_duration_since(last) >= reread)
            })
            .map(|(locator, _)| locator.clone())
            .collect();
        for locator in due {
            if self.readers >= MAX_CREDENTIAL_READERS {
                if let Some(source) = self.sources.get_mut(&locator) {
                    source.readers_exhausted = true;
                }
                continue;
            }
            let Some(source) = self.sources.get_mut(&locator) else {
                continue;
            };
            self.next_id += 1;
            let attempt = self.next_id;
            let incarnation = source.incarnation;
            let io = Arc::clone(&self.io);
            let events = self.events.clone();
            let read_locator = locator.clone();
            let started = std::thread::Builder::new()
                .name("usage-read".into())
                .spawn(move || {
                    let unwind_events = events.clone();
                    let unwind_locator = read_locator.clone();
                    let guard = OnUnwind(Some(move || {
                        unwind_events
                            .send(Event::ReadDone {
                                locator: unwind_locator,
                                incarnation,
                                attempt,
                                result: ReadResult::Failed(ReadFailure::Io(
                                    std::io::ErrorKind::Other,
                                )),
                            })
                            .ok();
                    }));
                    let result = io.read(&read_locator);
                    events
                        .send(Event::ReadDone {
                            locator: read_locator,
                            incarnation,
                            attempt,
                            result,
                        })
                        .ok();
                    guard.defuse();
                });
            match started {
                Ok(_) => {
                    self.readers += 1;
                    self.live_reads.insert(locator, attempt);
                    source.readers_exhausted = false;
                    source.reading = Some(ReadAttempt {
                        id: attempt,
                        started: now,
                        epoch: self.epoch,
                    });
                }
                Err(_) => source.readers_exhausted = true,
            }
        }
    }

    fn start_probe(&mut self, now: Instant) {
        let io = Arc::clone(&self.io);
        let events = self.events.clone();
        let started = std::thread::Builder::new()
            .name("usage-probe".into())
            .spawn(move || {
                let unwind_events = events.clone();
                let guard = OnUnwind(Some(move || {
                    unwind_events
                        .send(Event::ProbeDone(Err(CurlProblem::Unusable)))
                        .ok();
                }));
                events.send(Event::ProbeDone(io.probe())).ok();
                guard.defuse();
            });
        self.transport = match started {
            Ok(_) => Transport::Probing,
            Err(_) => Transport::Failed {
                problem: CurlProblem::Unusable,
                retry: now + self.timing.probe_retry,
            },
        };
    }

    fn gate(&self, key: &GateKey) -> Option<&Gate> {
        self.gates.get(key)
    }

    /// The credential that may serve a request for `generation` now.
    fn credential_for_generation(
        &self,
        generation: Generation,
        now: Instant,
        wall: SystemTime,
    ) -> Option<&Credential> {
        if self.rejected.contains(generation) || self.denied.contains(generation) {
            return None;
        }
        self.sources.values().find_map(|source| {
            let credential = source.credential.as_ref()?;
            let fresh = source.reconciled_epoch == self.epoch
                && !source.unconfirmed
                && source.failing_since.is_none()
                && !source.readers_exhausted
                && !source.stalled(now, &self.timing);
            (credential.generation == generation
                && fresh
                && credential.expires_at.is_none_or(|expiry| wall < expiry))
            .then_some(credential)
        })
    }

    /// A generation that resolves to `key` and may serve a request now.
    fn generation_for_account(
        &self,
        key: &AccountKey,
        now: Instant,
        wall: SystemTime,
    ) -> Option<Generation> {
        let mut candidates: Vec<Generation> = self
            .generations
            .iter()
            .filter(|(generation, state)| state.account(**generation).as_ref() == Some(key))
            .map(|(generation, _)| *generation)
            .collect();
        candidates.sort();
        candidates.into_iter().find(|generation| {
            self.credential_for_generation(*generation, now, wall)
                .is_some()
        })
    }

    fn host_of(provider: Provider) -> Host {
        match provider {
            Provider::Claude => Host::Anthropic,
            Provider::Codex => Host::ChatGpt,
        }
    }

    /// Every job for `host` that a usable credential could serve, with when
    /// its gate is due (`None`: now).
    fn candidate_jobs(
        &self,
        host: Host,
        now: Instant,
        wall: SystemTime,
    ) -> Vec<(u8, Option<Instant>, Job, Generation)> {
        let mut jobs = Vec::new();
        if host == Host::Anthropic {
            for (generation, state) in &self.generations {
                if state.wants_identity()
                    && self
                        .credential_for_generation(*generation, now, wall)
                        .is_some()
                {
                    let key = GateKey::Identity(*generation);
                    jobs.push((
                        0,
                        self.gate(&key).and_then(Gate::due_at),
                        Job::Identity(*generation),
                        *generation,
                    ));
                }
            }
        }
        for key in self.accounts.keys() {
            if Self::host_of(key.provider()) != host {
                continue;
            }
            let Some(generation) = self.generation_for_account(key, now, wall) else {
                continue;
            };
            let usage = GateKey::Usage(key.clone());
            jobs.push((
                1,
                self.gate(&usage).and_then(Gate::due_at),
                Job::Usage(key.clone()),
                generation,
            ));
            if matches!(key, AccountKey::Claude { .. }) {
                let profile = GateKey::Profile(key.clone());
                jobs.push((
                    2,
                    self.gate(&profile).and_then(Gate::due_at),
                    Job::Profile(key.clone()),
                    generation,
                ));
            }
        }
        jobs
    }

    /// The most urgent due job for `host`: identity first, then usage, then
    /// the routine profile refresh; within each, the longest overdue.
    fn next_job(&self, host: Host, now: Instant, wall: SystemTime) -> Option<(Job, Generation)> {
        let mut due: Vec<_> = self
            .candidate_jobs(host, now, wall)
            .into_iter()
            .filter(|(_, at, _, _)| at.is_none_or(|at| at <= now))
            .collect();
        due.sort_by_key(|(priority, at, _, _)| (*priority, *at));
        due.into_iter()
            .next()
            .map(|(_, _, job, generation)| (job, generation))
    }

    fn start_request(&mut self, host: Host, now: Instant, wall: SystemTime) {
        let state = self.hosts.entry(host).or_default();
        if state.in_flight.is_some() || state.not_before.is_some_and(|at| now < at) {
            return;
        }
        let Some((job, generation)) = self.next_job(host, now, wall) else {
            return;
        };
        let Some(credential) = self.credential_for_generation(generation, now, wall) else {
            return;
        };
        let request = build_request(&job, credential);
        self.next_id += 1;
        let attempt = self.next_id;
        let io = Arc::clone(&self.io);
        let events = self.events.clone();
        let started = std::thread::Builder::new()
            .name("usage-request".into())
            .spawn(move || {
                let unwind_events = events.clone();
                let guard = OnUnwind(Some(move || {
                    unwind_events
                        .send(Event::FetchDone {
                            attempt,
                            outcome: Outcome::Failed(FailureClass::Transport),
                            // clock-io-ok: when the failed request ended.
                            received_at: SystemTime::now(),
                        })
                        .ok();
                }));
                let outcome = io.fetch(request);
                // clock-io-ok: the response's receipt time is its observation time.
                let received_at = SystemTime::now();
                events
                    .send(Event::FetchDone {
                        attempt,
                        outcome,
                        received_at,
                    })
                    .ok();
                guard.defuse();
            });
        let state = self.hosts.entry(host).or_default();
        // Spacing counts from the start, whatever the request turns out to be.
        state.not_before = Some(now + self.timing.host_spacing);
        if started.is_ok() {
            state.in_flight = Some(attempt);
            self.pending.insert(
                attempt,
                Pending {
                    host,
                    job,
                    generation,
                },
            );
        }
    }

    fn fetch_done(
        &mut self,
        attempt: u64,
        outcome: Outcome,
        received_at: SystemTime,
        now: Instant,
        wall: SystemTime,
    ) {
        let Some(pending) = self.pending.remove(&attempt) else {
            return;
        };
        let host = self.hosts.entry(pending.host).or_default();
        if host.in_flight == Some(attempt) {
            host.in_flight = None;
        }
        let generation = pending.generation;
        // The binding the request was made under must still hold for its
        // result to count as this account's evidence. A refusal is about the
        // credential and applies regardless; anything else from a binding
        // that has since changed is dropped, and the job runs again.
        let bound = match &pending.job {
            Job::Identity(_) => true,
            Job::Usage(key) | Job::Profile(key) => {
                self.generations
                    .get(&generation)
                    .and_then(|state| state.account(generation))
                    .as_ref()
                    == Some(key)
            }
        };
        let credential_refusal = matches!(
            outcome,
            Outcome::Refused(_)
                | Outcome::Status {
                    status: 401 | 403,
                    ..
                }
        );
        if !bound && !credential_refusal {
            if matches!(outcome, Outcome::HostBlocked) {
                let retry = now + self.timing.host_blocked_retry;
                let host = self.hosts.entry(pending.host).or_default();
                host.not_before = Some(host.not_before.map_or(retry, |at| at.max(retry)));
            }
            self.collect_garbage();
            return;
        }
        match outcome {
            Outcome::HostBlocked => {
                let retry = now + self.timing.host_blocked_retry;
                let host = self.hosts.entry(pending.host).or_default();
                host.not_before = Some(host.not_before.map_or(retry, |at| at.max(retry)));
                self.set_health(&pending.job, EndpointHealth::HostBlocked);
            }
            // Credential-specific refusals mark that generation only, never
            // the account: another generation of it may still be fine.
            Outcome::Refused(_) | Outcome::Status { status: 403, .. } => {
                shepr_platform::structured_log!(
                    WARN,
                    event = usage.credential,
                    outcome = Refused,
                    generation = %generation.short(),
                    "a usage credential was refused; waiting for it to change"
                );
                let held = self.held_generations();
                self.denied.insert(generation, &held);
            }
            Outcome::Status { status: 401, .. } => {
                shepr_platform::structured_log!(
                    WARN,
                    event = usage.credential,
                    outcome = Refused,
                    generation = %generation.short(),
                    "a usage credential was rejected; waiting for the agent to renew it"
                );
                let held = self.held_generations();
                self.rejected.insert(generation, &held);
            }
            Outcome::Status {
                status: 429,
                retry_after,
            } => {
                let timing = self.timing;
                let Backoff { retry_at, streak } = self
                    .gates
                    .entry(pending.job.gate())
                    .or_default()
                    .throttled(now, &timing, retry_after);
                self.set_health(
                    &pending.job,
                    EndpointHealth::Throttled {
                        retry_at: wall_of(retry_at, now, wall),
                        streak,
                    },
                );
            }
            Outcome::Status {
                status,
                retry_after,
            } => self.failed(
                &pending.job,
                FailureClass::Status(status),
                retry_after,
                now,
                wall,
            ),
            Outcome::Failed(class) => self.failed(&pending.job, class, RetryAfter::None, now, wall),
            Outcome::Success(body) => {
                self.succeeded(&pending.job, generation, &body, received_at, now, wall);
            }
        }
        self.collect_garbage();
    }

    fn failed(
        &mut self,
        job: &Job,
        failure: FailureClass,
        hint: RetryAfter,
        now: Instant,
        wall: SystemTime,
    ) {
        let timing = self.timing;
        let Backoff { retry_at, streak } = self
            .gates
            .entry(job.gate())
            .or_default()
            .failed(now, &timing, hint);
        self.set_health(
            job,
            EndpointHealth::Failing {
                failure,
                retry_at: wall_of(retry_at, now, wall),
                streak,
            },
        );
    }

    /// Records an endpoint's health. Usage and profile health are separate:
    /// one endpoint's outcome never touches the other's.
    fn set_health(&mut self, job: &Job, health: EndpointHealth) {
        match job {
            Job::Identity(_) => {}
            Job::Usage(key) => {
                if let Some(account) = self.accounts.get_mut(key) {
                    account.usage = health;
                }
            }
            Job::Profile(key) => {
                if let Some(account) = self.accounts.get_mut(key) {
                    account.profile = Some(health);
                }
            }
        }
    }

    fn succeeded(
        &mut self,
        job: &Job,
        generation: Generation,
        body: &serde_json::Value,
        received_at: SystemTime,
        now: Instant,
        wall: SystemTime,
    ) {
        let timing = self.timing;
        match job {
            Job::Identity(_) | Job::Profile(_) => {
                let Some(profile) = claude_api::parse_profile(body) else {
                    self.failed(job, FailureClass::Response, RetryAfter::None, now, wall);
                    return;
                };
                self.gates
                    .entry(job.gate())
                    .or_default()
                    .succeeded_after(now, timing.profile_refresh);
                let identity = match (&profile.account_uuid, &profile.organization_uuid) {
                    (Some(account_uuid), Some(organization_uuid)) => {
                        Identity::Known(AccountKey::Claude {
                            account_uuid: account_uuid.clone(),
                            organization_uuid: organization_uuid.clone(),
                        })
                    }
                    _ => Identity::Unresolved(IdentityProblem::ProfileIncomplete),
                };
                let Some(state) = self.generations.get_mut(&generation) else {
                    return;
                };
                // The token's account is whatever the provider says now.
                state.identity = identity;
                let Some(key) = state.account(generation) else {
                    return;
                };
                self.ensure_account_for(generation);
                if let Some(account) = self.accounts.get_mut(&key) {
                    if profile.email.is_some() {
                        account.label = profile.email;
                    }
                    account.plan = profile.plan;
                    if matches!(key, AccountKey::Claude { .. }) {
                        account.profile = Some(EndpointHealth::Ok { at: received_at });
                        // An identity answer refreshes the profile too, so the
                        // routine refresh can wait; it never shortens an
                        // outstanding profile backoff.
                        let gate = self.gates.entry(GateKey::Profile(key)).or_default();
                        if matches!(job, Job::Identity(_)) {
                            gate.extend_to(now + timing.profile_refresh);
                        } else {
                            gate.succeeded_after(now, timing.profile_refresh);
                        }
                    }
                }
            }
            Job::Usage(key) => {
                let measurement = match key.provider() {
                    Provider::Claude => {
                        claude_api::parse_usage(body, received_at).map(Measurement::Claude)
                    }
                    Provider::Codex => {
                        codex_api::parse_usage(body, received_at).map(Measurement::Codex)
                    }
                };
                let Some(measurement) = measurement else {
                    self.failed(job, FailureClass::Response, RetryAfter::None, now, wall);
                    return;
                };
                let gate = self.gates.entry(job.gate()).or_default();
                gate.succeeded(now, &timing, key);
                for reset in resets(&measurement) {
                    if let Ok(until) = reset.duration_since(wall) {
                        gate.follow_reset(now + until, &timing);
                    }
                }
                let account = self
                    .accounts
                    .entry(key.clone())
                    .or_insert_with(|| AccountState::new(key, None));
                if let Measurement::Codex(codex) = &measurement
                    && codex.plan_type.is_some()
                {
                    account.plan.kind.clone_from(&codex.plan_type);
                }
                account.measurement = Some(measurement);
                account.usage = EndpointHealth::Ok { at: received_at };
            }
        }
    }

    /// The next instant something can change without an event: only future
    /// timers count, and only for work that could actually start.
    fn next_wake(&self, now: Instant) -> Instant {
        let mut wake = now + self.timing.idle_wake;
        if !self.active {
            return wake;
        }
        let mut consider = |at: Instant| {
            if at > now {
                wake = wake.min(at);
            }
        };
        for (locator, source) in &self.sources {
            match source.reading {
                Some(read) => consider(read.started + self.timing.read_deadline),
                None if !self.live_reads.contains_key(locator) => {
                    if let Some(last) = source.last_read {
                        consider(last + self.timing.reread);
                    }
                }
                None => {}
            }
        }
        if let Transport::Failed { retry, .. } = &self.transport {
            consider(*retry);
        }
        // A failing streak's grace ends in active time; while active, that
        // is real time from now.
        for source in self.sources.values() {
            if let Some(since) = source.failing_since
                && source.credential.is_some()
            {
                let elapsed = self.active_time.saturating_sub(since);
                consider(now + self.timing.unreadable_grace.saturating_sub(elapsed));
            }
        }
        if matches!(self.transport, Transport::Ready) {
            // clock-io-ok: expiry is judged against the wall clock.
            let wall = SystemTime::now();
            for host in [Host::Anthropic, Host::ChatGpt] {
                let state = self.hosts.get(&host);
                if state.is_some_and(|state| state.in_flight.is_some()) {
                    // Its completion is an event.
                    continue;
                }
                let free_at = state.and_then(|state| state.not_before);
                for (_, due, _, _) in self.candidate_jobs(host, now, wall) {
                    let start = match (due, free_at) {
                        (Some(due), Some(free)) => due.max(free),
                        (Some(at), None) | (None, Some(at)) => at,
                        (None, None) => now,
                    };
                    consider(start);
                }
            }
        }
        wake.max(now + WORKER_MIN_SLEEP.min(self.timing.idle_wake))
    }

    fn publish(&mut self, now: Instant, wall: SystemTime) {
        let mut snapshot = self.build_snapshot(now, wall);
        snapshot.incarnation = self.incarnation;
        snapshot.revision = self.revision;
        if snapshot == *self.published {
            return;
        }
        self.revision += 1;
        snapshot.revision = self.revision;
        let snapshot = Arc::new(snapshot);
        self.published = Arc::clone(&snapshot);
        *lock(&self.shared) = snapshot;
        (self.wake)();
    }

    fn build_snapshot(&self, now: Instant, wall: SystemTime) -> UsageSnapshot {
        let sources = self
            .sources
            .iter()
            .map(|(locator, source)| {
                let account = source.credential.as_ref().and_then(|credential| {
                    self.generations
                        .get(&credential.generation)
                        .and_then(|state| state.account(credential.generation))
                });
                SourceView {
                    locator: locator.clone(),
                    origin: source.origin,
                    availability: self.availability(source, now, wall),
                    account,
                }
            })
            .collect::<Vec<_>>();
        let accounts = self
            .accounts
            .iter()
            .map(|(key, account)| AccountView {
                key: key.clone(),
                label: account.label.clone(),
                plan: account.plan.clone(),
                measurement: account.measurement.clone(),
                has_usable_credential: self.generation_for_account(key, now, wall).is_some(),
                usage: account.usage.clone(),
                profile: account.profile.clone(),
                sources: sources
                    .iter()
                    .filter(|view| view.account.as_ref() == Some(key))
                    .map(|view| view.locator.clone())
                    .collect(),
                identity_problem: account.identity_problem,
            })
            .collect();
        UsageSnapshot {
            incarnation: 0,
            revision: 0,
            accounts,
            sources,
            transport: match &self.transport {
                Transport::Unprobed | Transport::Probing => TransportHealth::Unprobed,
                Transport::Ready => TransportHealth::Ready,
                Transport::Failed {
                    problem: CurlProblem::Missing,
                    ..
                } => TransportHealth::Missing,
                Transport::Failed {
                    problem: CurlProblem::Unusable,
                    ..
                } => TransportHealth::Unusable,
            },
            registry: self.registry_health,
        }
    }

    fn availability(&self, source: &SourceState, now: Instant, wall: SystemTime) -> Availability {
        // Persistence is the failing streak's grace in active time, whatever
        // the current read is doing.
        let persistent = source.failing_since.is_some_and(|since| {
            self.active_time.saturating_sub(since) >= self.timing.unreadable_grace
        });
        if source.stalled(now, &self.timing) || source.unconfirmed {
            return Availability::Unreadable {
                reason: ReadFailure::Stalled,
                persistent,
            };
        }
        if source.readers_exhausted {
            return Availability::Unreadable {
                reason: ReadFailure::ReadersExhausted,
                persistent,
            };
        }
        match source.state {
            SourceReadState::NotYetRead => Availability::NotYetRead,
            SourceReadState::Missing => Availability::Missing,
            SourceReadState::LoggedOut => Availability::LoggedOut,
            SourceReadState::ApiKey => Availability::ApiKey,
            SourceReadState::Unsupported => Availability::Unsupported,
            SourceReadState::Failed(reason) => Availability::Unreadable { reason, persistent },
            SourceReadState::Usable => {
                let Some(credential) = &source.credential else {
                    return Availability::NotYetRead;
                };
                if self.rejected.contains(credential.generation) {
                    Availability::Rejected
                } else if self.denied.contains(credential.generation) {
                    Availability::Denied
                } else if credential.expires_at.is_some_and(|expiry| wall >= expiry) {
                    Availability::Expired
                } else {
                    Availability::Usable {
                        expires_at: credential.expires_at,
                    }
                }
            }
        }
    }
}

/// Codex identity from one credential: resolved only with proven binding
/// between the id token and the access token, and both identifiers present.
fn codex_identity(credential: &Credential) -> Identity {
    let hints = &credential.identity;
    match (
        &hints.principal,
        &credential.routing.account_id,
        hints.id_token_bound,
    ) {
        (Some(principal), Some(account_id), Some(true)) => Identity::Known(AccountKey::Codex {
            principal: principal.clone(),
            account_id: account_id.clone(),
        }),
        (_, _, Some(false)) => Identity::Unresolved(IdentityProblem::IdTokenMismatch),
        _ => Identity::Unresolved(IdentityProblem::MissingClaims),
    }
}

fn build_request(job: &Job, credential: &Credential) -> Request {
    let bearer = Secret::new(credential.access_token.expose().to_vec());
    match job {
        Job::Identity(_) | Job::Profile(_) => Request {
            host: Host::Anthropic,
            url: claude_api::PROFILE_URL,
            bearer,
            headers: Vec::new(),
        },
        Job::Usage(key) => match key.provider() {
            Provider::Claude => Request {
                host: Host::Anthropic,
                url: claude_api::USAGE_URL,
                bearer,
                headers: vec![(
                    claude_api::BETA_HEADER.0,
                    claude_api::BETA_HEADER.1.to_owned(),
                )],
            },
            Provider::Codex => {
                let mut headers = Vec::new();
                if let Some(account_id) = &credential.routing.account_id {
                    headers.push((codex_api::ACCOUNT_HEADER, account_id.clone()));
                }
                if credential.routing.fedramp {
                    headers.push((
                        codex_api::FEDRAMP_HEADER.0,
                        codex_api::FEDRAMP_HEADER.1.to_owned(),
                    ));
                }
                Request {
                    host: Host::ChatGpt,
                    url: codex_api::USAGE_URL,
                    bearer,
                    headers,
                }
            }
        },
    }
}

/// The reset times a measurement names, absolute or derived from
/// `reset_after` relative to when it was observed.
fn resets(measurement: &Measurement) -> Vec<SystemTime> {
    match measurement {
        Measurement::Claude(ClaudeMeasurement { limits, .. }) => {
            limits.iter().filter_map(|limit| limit.resets_at).collect()
        }
        Measurement::Codex(CodexMeasurement {
            observed_at,
            groups,
            ..
        }) => groups
            .iter()
            .flat_map(|group| [&group.primary, &group.secondary])
            .flatten()
            .filter_map(|window| {
                window.reset_at.or_else(|| {
                    window
                        .reset_after
                        .and_then(|after| observed_at.checked_add(after))
                })
            })
            .collect(),
    }
}

/// A monotonic instant expressed as wall time, for publication.
fn wall_of(at: Instant, now: Instant, wall: SystemTime) -> SystemTime {
    wall.checked_add(at.saturating_duration_since(now))
        .unwrap_or(wall)
}

#[cfg(test)]
mod tests;
