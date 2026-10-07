use super::*;
use std::collections::VecDeque;

use base64::Engine as _;

/// A double for the filesystem and curl: credential files by locator, and
/// queued outcomes by URL (the last one repeats).
#[derive(Default)]
struct FakeIo {
    files: Mutex<HashMap<SourceLocator, Vec<u8>>>,
    outcomes: Mutex<HashMap<&'static str, VecDeque<Outcome>>>,
    requests: Mutex<Vec<SeenRequest>>,
}

#[derive(Debug, Clone)]
struct SeenRequest {
    url: &'static str,
    bearer: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

impl FakeIo {
    fn write(&self, locator: &SourceLocator, bytes: &[u8]) {
        self.files
            .lock()
            .expect("files")
            .insert(locator.clone(), bytes.to_vec());
    }

    fn respond(&self, url: &'static str, outcomes: Vec<Outcome>) {
        self.outcomes
            .lock()
            .expect("outcomes")
            .insert(url, outcomes.into());
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl UsageIo for FakeIo {
    fn read(&self, locator: &SourceLocator) -> ReadResult {
        match self.files.lock().expect("files").get(locator) {
            Some(bytes) => match crate::credentials::parse(locator.provider, bytes) {
                Ok(parsed) => ReadResult::Parsed(parsed),
                Err(_) => ReadResult::Failed(ReadFailure::NotJson),
            },
            None => ReadResult::Missing,
        }
    }

    fn probe(&self) -> Result<(), CurlProblem> {
        Ok(())
    }

    fn fetch(&self, request: Request) -> Outcome {
        self.requests.lock().expect("requests").push(SeenRequest {
            url: request.url,
            bearer: request.bearer.expose().to_vec(),
            headers: request.headers.clone(),
        });
        let mut outcomes = self.outcomes.lock().expect("outcomes");
        let queue = outcomes.entry(request.url).or_default();
        if queue.len() > 1 {
            queue
                .pop_front()
                .unwrap_or(Outcome::Failed(FailureClass::Transport))
        } else {
            queue
                .front()
                .cloned()
                .unwrap_or(Outcome::Failed(FailureClass::Transport))
        }
    }
}

fn fast_timing() -> Timing {
    Timing {
        poll_cadence: Duration::from_millis(300),
        jitter_percent: 0,
        host_spacing: Duration::from_millis(5),
        profile_refresh: Duration::from_secs(3600),
        throttle_ladder: [Duration::from_secs(3600); 5],
        failure_start: Duration::from_secs(3600),
        failure_max: Duration::from_secs(3600),
        reset_follow_up: Duration::from_millis(10),
        reread: Duration::from_millis(40),
        read_deadline: Duration::from_secs(5),
        unreadable_grace: Duration::from_millis(200),
        probe_retry: Duration::from_secs(3600),
        idle_wake: Duration::from_millis(20),
        host_blocked_retry: Duration::from_millis(50),
    }
}

fn start(io: &Arc<FakeIo>) -> UsageWorker {
    let io: Arc<FakeIo> = Arc::clone(io);
    let io: Arc<dyn UsageIo> = io;
    UsageWorker::start_with(io, None, fast_timing(), Box::new(|| {})).expect("worker starts")
}

fn source(provider: Provider, directory: &str) -> DiscoveredSource {
    DiscoveredSource {
        locator: SourceLocator {
            provider,
            directory: PathBuf::from(directory),
        },
        origin: SourceOrigin::Server,
    }
}

fn wait_for(
    worker: &UsageWorker,
    what: &str,
    done: impl Fn(&UsageSnapshot) -> bool,
) -> Arc<UsageSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = worker.snapshot();
        if done(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {snapshot:#?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn jwt(claims: &serde_json::Value) -> String {
    let encode = |value: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(value).expect("json"))
    };
    format!(
        "{}.{}.sig",
        encode(&serde_json::json!({"alg": "none"})),
        encode(claims)
    )
}

fn codex_file_with_id(user: &str, account: &str, nonce: &str, id_user: &str) -> Vec<u8> {
    let access = jwt(&serde_json::json!({
        "nonce": nonce,
        "https://api.openai.com/auth": {"chatgpt_user_id": user}
    }));
    let id = jwt(&serde_json::json!({
        "https://api.openai.com/auth": {"chatgpt_user_id": id_user},
        "email": format!("{id_user}@example.com")
    }));
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {"access_token": access, "id_token": id, "account_id": account}
    }))
    .expect("json")
}

/// A Codex file whose id token is bound to its access token.
fn codex_file(user: &str, account: &str, nonce: &str) -> Vec<u8> {
    codex_file_with_id(user, account, nonce, user)
}

fn claude_file(token: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "claudeAiOauth": {"accessToken": token, "scopes": ["user:profile", "user:inference"]}
    }))
    .expect("json")
}

fn codex_usage(percent: u32) -> Outcome {
    Outcome::Success(serde_json::json!({
        "plan_type": "pro",
        "rate_limit": {"allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": percent, "limit_window_seconds": 18000,
                               "reset_after_seconds": 100, "reset_at": 4_000_000_000_u64}}
    }))
}

#[test]
fn a_codex_source_is_polled_with_its_routing_and_measured() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.respond(codex_api::USAGE_URL, vec![codex_usage(42)]);
    let worker = start(&io);
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);

    let snapshot = wait_for(&worker, "a measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    let account = &snapshot.accounts[0];
    assert_eq!(
        account.key,
        AccountKey::Codex {
            principal: "user-1".into(),
            account_id: "acct-1".into()
        }
    );
    assert_eq!(account.sources, vec![codex.locator.clone()]);
    let Some(Measurement::Codex(measurement)) = &account.measurement else {
        panic!("codex measurement");
    };
    let primary = measurement.groups[0].primary.as_ref().expect("primary");
    assert_eq!(primary.used_percent, Some(42.0));
    let request = &io.requests()[0];
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| *name == codex_api::ACCOUNT_HEADER && value == "acct-1")
    );
}

#[test]
fn claude_identity_comes_from_profile_before_usage_is_asked() {
    let io = Arc::new(FakeIo::default());
    let claude = source(Provider::Claude, "/home/u/.claude");
    io.write(&claude.locator, &claude_file("token-a"));
    io.respond(
        claude_api::PROFILE_URL,
        vec![Outcome::Success(serde_json::json!({
            "account": {"uuid": "acc", "email": "a@example.com"},
            "organization": {"uuid": "org", "organization_type": "claude_max"}
        }))],
    );
    io.respond(
        claude_api::USAGE_URL,
        vec![Outcome::Success(serde_json::json!({
            "limits": [{"kind": "session", "percent": 12}]
        }))],
    );
    let worker = start(&io);
    worker.set_sources(vec![claude]);
    worker.set_active(true);

    let snapshot = wait_for(&worker, "a claude measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    let account = &snapshot.accounts[0];
    assert_eq!(
        account.key,
        AccountKey::Claude {
            account_uuid: "acc".into(),
            organization_uuid: "org".into()
        }
    );
    assert_eq!(account.label.as_deref(), Some("a@example.com"));
    assert_eq!(account.plan.kind.as_deref(), Some("claude_max"));
    let urls: Vec<&str> = io.requests().iter().map(|request| request.url).collect();
    assert_eq!(urls[0], claude_api::PROFILE_URL);
    assert!(urls.contains(&claude_api::USAGE_URL));
    for request in io.requests() {
        assert_eq!(request.bearer, b"token-a");
    }
}

#[test]
fn a_rejected_generation_stops_until_the_token_changes() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "old"));
    io.respond(
        codex_api::USAGE_URL,
        vec![Outcome::Status {
            status: 401,
            retry_after: RetryAfter::None,
        }],
    );
    let worker = start(&io);
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);

    wait_for(&worker, "the rejection", |snapshot| {
        snapshot.sources.first().map(|view| &view.availability) == Some(&Availability::Rejected)
    });
    let rejected_requests = io.requests().len();
    // Re-reads of the same token keep it suspended.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(io.requests().len(), rejected_requests);

    io.respond(codex_api::USAGE_URL, vec![codex_usage(7)]);
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "renewed"));
    wait_for(&worker, "the renewed token's measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
}

#[test]
fn two_directories_with_one_codex_account_share_one_account_and_one_gate() {
    let io = Arc::new(FakeIo::default());
    let first = source(Provider::Codex, "/srv/codex-a");
    let second = source(Provider::Codex, "/srv/codex-b");
    io.write(&first.locator, &codex_file("user-1", "acct-1", "a"));
    io.write(&second.locator, &codex_file("user-1", "acct-1", "b"));
    io.respond(codex_api::USAGE_URL, vec![codex_usage(5)]);
    let worker = start(&io);
    worker.set_sources(vec![first, second]);
    worker.set_active(true);

    let snapshot = wait_for(&worker, "both sources on one account", |snapshot| {
        snapshot.accounts.len() == 1
            && snapshot.accounts[0].sources.len() == 2
            && snapshot.accounts[0].measurement.is_some()
    });
    assert_eq!(snapshot.sources.len(), 2);
    // One poll per cadence, not one per directory.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(io.requests().len(), 1);
}

#[test]
fn an_inactive_worker_reads_and_asks_nothing() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.respond(codex_api::USAGE_URL, vec![codex_usage(1)]);
    let worker = start(&io);
    worker.set_sources(vec![codex]);
    std::thread::sleep(Duration::from_millis(150));
    assert!(io.requests().is_empty());
    let snapshot = worker.snapshot();
    assert!(
        snapshot
            .sources
            .iter()
            .all(|view| view.availability == Availability::NotYetRead)
    );
}

#[test]
fn a_mismatched_id_token_keeps_its_measurement_unmerged() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    let access = jwt(&serde_json::json!({"sub": "user-1"}));
    let id = jwt(&serde_json::json!({"sub": "user-2"}));
    io.write(
        &codex.locator,
        &serde_json::to_vec(&serde_json::json!({
            "tokens": {"access_token": access, "id_token": id, "account_id": "acct-1"}
        }))
        .expect("json"),
    );
    io.respond(codex_api::USAGE_URL, vec![codex_usage(3)]);
    let worker = start(&io);
    worker.set_sources(vec![codex]);
    worker.set_active(true);
    let snapshot = wait_for(&worker, "an unresolved measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    assert!(matches!(
        snapshot.accounts[0].key,
        AccountKey::Unresolved { .. }
    ));
    assert_eq!(
        snapshot.accounts[0].identity_problem,
        Some(IdentityProblem::IdTokenMismatch)
    );
}

#[test]
fn a_torn_read_stops_requests_at_once_and_unbinds_after_the_grace() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.respond(codex_api::USAGE_URL, vec![codex_usage(9)]);
    let worker = start(&io);
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);
    wait_for(&worker, "a first measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });

    io.write(&codex.locator, b"{\"tokens\": {\"acc");
    wait_for(&worker, "the source to read as unreadable", |snapshot| {
        matches!(
            snapshot.sources[0].availability,
            Availability::Unreadable {
                reason: ReadFailure::NotJson,
                ..
            }
        )
    });
    let requests = io.requests().len();
    let snapshot = wait_for(&worker, "the grace to pass", |snapshot| {
        matches!(
            snapshot.sources[0].availability,
            Availability::Unreadable {
                persistent: true,
                ..
            }
        )
    });
    assert_eq!(
        io.requests().len(),
        requests,
        "no request during the failing streak"
    );
    // The account keeps its history and its last polling health, but has no
    // usable credential.
    assert!(snapshot.accounts[0].measurement.is_some());
    assert!(!snapshot.accounts[0].has_usable_credential);
    assert!(matches!(
        snapshot.accounts[0].usage,
        EndpointHealth::Ok { .. }
    ));
}

#[test]
fn a_codex_credential_without_an_id_token_is_never_merged() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    let access = jwt(&serde_json::json!({
        "https://api.openai.com/auth": {"chatgpt_user_id": "user-1"}
    }));
    io.write(
        &codex.locator,
        &serde_json::to_vec(&serde_json::json!({
            "tokens": {"access_token": access, "account_id": "acct-1"}
        }))
        .expect("json"),
    );
    io.respond(codex_api::USAGE_URL, vec![codex_usage(3)]);
    let worker = start(&io);
    worker.set_sources(vec![codex]);
    worker.set_active(true);
    let snapshot = wait_for(&worker, "an unresolved account", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    assert!(matches!(
        snapshot.accounts[0].key,
        AccountKey::Unresolved { .. }
    ));
    assert_eq!(
        snapshot.accounts[0].identity_problem,
        Some(IdentityProblem::MissingClaims)
    );
}

#[test]
fn binding_is_reconsidered_when_the_id_token_changes_under_the_same_token() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.write(&codex.locator, &codex_file("user-1", "acct-1", "same"));
    io.respond(codex_api::USAGE_URL, vec![codex_usage(3)]);
    let worker = start(&io);
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);
    wait_for(&worker, "a resolved source", |snapshot| {
        matches!(
            snapshot
                .sources
                .first()
                .and_then(|view| view.account.as_ref()),
            Some(AccountKey::Codex { .. })
        )
    });
    // Same access token (same generation), id token now names someone else.
    io.write(
        &codex.locator,
        &codex_file_with_id("user-1", "acct-1", "same", "user-2"),
    );
    wait_for(&worker, "the source to become unresolved", |snapshot| {
        matches!(
            snapshot
                .sources
                .first()
                .and_then(|view| view.account.as_ref()),
            Some(AccountKey::Unresolved { .. })
        )
    });
}

#[test]
fn a_rejection_outlives_the_credential_disappearing_and_returning() {
    let io = Arc::new(FakeIo::default());
    let codex = source(Provider::Codex, "/home/u/.codex");
    let file = codex_file("user-1", "acct-1", "old");
    io.write(&codex.locator, &file);
    io.respond(
        codex_api::USAGE_URL,
        vec![Outcome::Status {
            status: 401,
            retry_after: RetryAfter::None,
        }],
    );
    let worker = start(&io);
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);
    wait_for(&worker, "the rejection", |snapshot| {
        snapshot.sources.first().map(|view| &view.availability) == Some(&Availability::Rejected)
    });
    let requests = io.requests().len();
    io.files.lock().expect("files").remove(&codex.locator);
    wait_for(&worker, "the file to read as missing", |snapshot| {
        snapshot.sources.first().map(|view| &view.availability) == Some(&Availability::Missing)
    });
    io.write(&codex.locator, &file);
    wait_for(&worker, "the same token, still rejected", |snapshot| {
        snapshot.sources.first().map(|view| &view.availability) == Some(&Availability::Rejected)
    });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(io.requests().len(), requests);
}

#[test]
fn a_403_from_an_old_generation_leaves_the_account_health_alone() {
    let io = Arc::new(FakeIo::default());
    let first = source(Provider::Codex, "/srv/codex-a");
    let second = source(Provider::Codex, "/srv/codex-b");
    io.write(&first.locator, &codex_file("user-1", "acct-1", "a"));
    io.write(&second.locator, &codex_file("user-1", "acct-1", "b"));
    io.respond(
        codex_api::USAGE_URL,
        vec![
            Outcome::Status {
                status: 403,
                retry_after: RetryAfter::None,
            },
            codex_usage(4),
        ],
    );
    let worker = start(&io);
    worker.set_sources(vec![first, second]);
    worker.set_active(true);
    let snapshot = wait_for(&worker, "the other generation's measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    let account = &snapshot.accounts[0];
    assert!(matches!(account.usage, EndpointHealth::Ok { .. }));
    assert!(account.has_usable_credential);
    assert!(
        snapshot
            .sources
            .iter()
            .any(|view| view.availability == Availability::Denied)
    );
}

/// A double whose reads can be made slow, counting the reads it serves.
struct SlowIo {
    inner: FakeIo,
    delay_ms: std::sync::atomic::AtomicU64,
    reads: std::sync::atomic::AtomicUsize,
}

impl SlowIo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: FakeIo::default(),
            delay_ms: std::sync::atomic::AtomicU64::new(0),
            reads: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl UsageIo for SlowIo {
    fn read(&self, locator: &SourceLocator) -> ReadResult {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay_ms.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(delay));
        self.inner.read(locator)
    }
    fn probe(&self) -> Result<(), CurlProblem> {
        Ok(())
    }
    fn fetch(&self, request: Request) -> Outcome {
        self.inner.fetch(request)
    }
}

#[test]
fn a_read_that_finishes_late_does_not_revive_the_old_credential() {
    let io = SlowIo::new();
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.inner
        .write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.inner.respond(codex_api::USAGE_URL, vec![codex_usage(1)]);
    let timing = Timing {
        read_deadline: Duration::from_millis(60),
        poll_cadence: Duration::from_millis(50),
        ..fast_timing()
    };
    let shared: Arc<SlowIo> = Arc::clone(&io);
    let worker =
        UsageWorker::start_with(shared, None, timing, Box::new(|| {})).expect("worker starts");
    worker.set_sources(vec![codex]);
    worker.set_active(true);
    wait_for(&worker, "a first measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    // Every read from now on overruns its deadline.
    io.delay_ms.store(200, Ordering::SeqCst);
    wait_for(&worker, "a discarded late read", |snapshot| {
        matches!(
            snapshot.sources[0].availability,
            Availability::Unreadable {
                reason: ReadFailure::Stalled,
                ..
            }
        )
    });
    let requests = io.inner.requests().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        io.inner.requests().len(),
        requests,
        "no request rides on a credential no read has confirmed"
    );
}

#[test]
fn the_failed_read_grace_ends_even_while_the_next_read_hangs() {
    let io = SlowIo::new();
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.inner
        .write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.inner.respond(codex_api::USAGE_URL, vec![codex_usage(1)]);
    let timing = Timing {
        unreadable_grace: Duration::from_millis(150),
        read_deadline: Duration::from_millis(300),
        ..fast_timing()
    };
    let shared: Arc<SlowIo> = Arc::clone(&io);
    let worker =
        UsageWorker::start_with(shared, None, timing, Box::new(|| {})).expect("worker starts");
    worker.set_sources(vec![codex.clone()]);
    worker.set_active(true);
    wait_for(&worker, "a first measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    io.inner.write(&codex.locator, b"{\"tokens\": {\"acc");
    wait_for(&worker, "the failing streak", |snapshot| {
        matches!(
            snapshot.sources[0].availability,
            Availability::Unreadable {
                reason: ReadFailure::NotJson,
                ..
            }
        )
    });
    // The next read hangs past the grace.
    io.delay_ms.store(60_000, Ordering::SeqCst);
    wait_for(
        &worker,
        "the binding to end without another read",
        |snapshot| {
            snapshot.sources[0].account.is_none()
                && snapshot
                    .accounts
                    .first()
                    .is_some_and(|account| !account.has_usable_credential)
        },
    );
    // Past the hung reader's deadline the source shows stalled, and still
    // persistently unreadable: the grace has passed.
    wait_for(&worker, "a persistent stall", |snapshot| {
        snapshot.sources[0].availability
            == Availability::Unreadable {
                reason: ReadFailure::Stalled,
                persistent: true,
            }
    });
}

#[test]
fn a_quick_off_and_on_still_rereads_credentials() {
    let io = SlowIo::new();
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.inner
        .write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.inner.respond(codex_api::USAGE_URL, vec![codex_usage(1)]);
    let timing = Timing {
        reread: Duration::from_secs(3600),
        poll_cadence: Duration::from_secs(3600),
        ..fast_timing()
    };
    let shared: Arc<SlowIo> = Arc::clone(&io);
    let worker =
        UsageWorker::start_with(shared, None, timing, Box::new(|| {})).expect("worker starts");
    worker.set_sources(vec![codex]);
    worker.set_active(true);
    wait_for(&worker, "a first measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    let reads = io.reads.load(Ordering::SeqCst);
    // Coalesced into one control read, but the activation still counts.
    worker.set_active(false);
    worker.set_active(true);
    let deadline = Instant::now() + Duration::from_secs(30);
    while io.reads.load(Ordering::SeqCst) == reads {
        assert!(Instant::now() < deadline, "the activation forced no read");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_idle_active_worker_does_not_spin() {
    struct CountingIo {
        inner: FakeIo,
        reads: std::sync::atomic::AtomicUsize,
    }
    impl UsageIo for CountingIo {
        fn read(&self, locator: &SourceLocator) -> ReadResult {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read(locator)
        }
        fn probe(&self) -> Result<(), CurlProblem> {
            Ok(())
        }
        fn fetch(&self, request: Request) -> Outcome {
            self.inner.fetch(request)
        }
    }
    let io = Arc::new(CountingIo {
        inner: FakeIo::default(),
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let codex = source(Provider::Codex, "/home/u/.codex");
    io.inner
        .write(&codex.locator, &codex_file("user-1", "acct-1", "a"));
    io.inner.respond(codex_api::USAGE_URL, vec![codex_usage(1)]);
    let timing = Timing {
        reread: Duration::from_secs(3600),
        poll_cadence: Duration::from_secs(3600),
        idle_wake: Duration::from_secs(3600),
        ..fast_timing()
    };
    let shared: Arc<CountingIo> = Arc::clone(&io);
    let worker =
        UsageWorker::start_with(shared, None, timing, Box::new(|| {})).expect("worker starts");
    worker.set_sources(vec![codex]);
    worker.set_active(true);
    wait_for(&worker, "a measurement", |snapshot| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.measurement.is_some())
    });
    // With nothing due for an hour the worker sleeps: one read, one request.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(io.reads.load(Ordering::SeqCst), 1);
    assert_eq!(io.inner.requests().len(), 1);
}
