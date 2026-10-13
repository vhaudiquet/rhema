//! Offline integration tests: the [`Driver`] end to end over a
//! memory-store dispatcher with a hand-driven clock — burst coalescing,
//! the live inbox and lease renewal, session continuity, outcome
//! bookkeeping, the capacity/race/panic edges. Zero network, zero real
//! time.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{DEBOUNCE, TTL, config, event, register, setup, setup_with, the_key};
use rhema_autonomous_agent::async_trait;
use rhema_autonomous_agent::{Driver, Handler, SessionContext};
use rhema_rsd::{DispatchError, DispatcherConfig, IngestResult, KeyState, Outcome, ResourceKey};
use tokio::sync::{Notify, mpsc};

/// A handler the test paces step by step: it waits at `start` before its
/// first poll and at `resume` before its last act — a final poll when
/// `poll_after_resume`, else straight to `outcome`. Every drain's size is
/// reported on the channel.
struct GatedHandler {
    report: mpsc::UnboundedSender<usize>,
    start: Arc<Notify>,
    resume: Arc<Notify>,
    poll_after_resume: bool,
    outcome: Outcome,
}

#[async_trait]
impl Handler for GatedHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        self.start.notified().await;
        let _ = self.report.send(ctx.poll_inbox().await.unwrap().len());
        self.resume.notified().await;
        if self.poll_after_resume {
            let _ = self.report.send(ctx.poll_inbox().await.unwrap().len());
        }
        self.outcome.clone()
    }
}

/// Wires a [`GatedHandler`]: the notifies let the test pace the run, the
/// channel reports every drain.
fn gated(
    outcome: Outcome,
    poll_after_resume: bool,
) -> (
    Arc<Notify>,
    Arc<Notify>,
    mpsc::UnboundedReceiver<usize>,
    Arc<GatedHandler>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handler = Arc::new(GatedHandler {
        report: tx,
        start: Arc::new(Notify::new()),
        resume: Arc::new(Notify::new()),
        poll_after_resume,
        outcome,
    });
    (handler.start.clone(), handler.resume.clone(), rx, handler)
}

/// A handler that drains once — its trigger, normally — and reports the
/// batch size, for tests with no mid-run coordination.
struct SimpleHandler {
    report: mpsc::UnboundedSender<usize>,
    outcome: Outcome,
}

#[async_trait]
impl Handler for SimpleHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        let _ = self.report.send(ctx.poll_inbox().await.unwrap().len());
        self.outcome.clone()
    }
}

/// A plain handler: drain, then fail.
struct FailAfterPoll;

#[async_trait]
impl Handler for FailAfterPoll {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        ctx.poll_inbox().await.unwrap();
        Outcome::Failed("the diff is unreviewable".to_owned())
    }
}

/// A handler that panics before doing anything: the lease dies with it.
struct Panic;

#[async_trait]
impl Handler for Panic {
    async fn handle(&self, _ctx: &mut SessionContext) -> Outcome {
        panic!("deliberate boom");
    }
}

/// Bumps a counter the session carries across runs, recording what each
/// run read and the session's creation instant.
struct ContinuityHandler {
    runs: Arc<Mutex<Vec<(u64, u64)>>>,
}

#[async_trait]
impl Handler for ContinuityHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        ctx.poll_inbox().await.unwrap();
        let mut seen = ctx.session.core.driver_state["seen"].as_u64().unwrap_or(0);
        seen += 1;
        ctx.session.core.driver_state = serde_json::json!({ "seen": seen });
        ctx.checkpoint().unwrap();
        self.runs
            .lock()
            .unwrap()
            .push((seen, ctx.session.created_at));
        Outcome::Handled
    }
}

/// Tracks how many runs are live at once and which keys were served,
/// draining exactly one trigger per run.
struct CappedHandler {
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
    runs: Arc<Mutex<Vec<ResourceKey>>>,
}

#[async_trait]
impl Handler for CappedHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        let now_live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(now_live, Ordering::SeqCst);
        // Room for a sibling run to go live while this one holds its slot.
        tokio::task::yield_now().await;
        let items = ctx.poll_inbox().await.unwrap();
        assert_eq!(items.len(), 1, "each run drains exactly its own trigger");
        self.runs.lock().unwrap().push(ctx.key.clone());
        self.live.fetch_sub(1, Ordering::SeqCst);
        Outcome::Handled
    }
}

/// Waits until the spawned run holds the lease: every deadline the test
/// then sets is measured from a known instant.
async fn until_leased(dispatcher: &rhema_rsd::Dispatcher, key: &ResourceKey) {
    for _ in 0..1_000 {
        if dispatcher.status(key).unwrap().state == KeyState::Leased {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the run never acquired {key}");
}

#[tokio::test]
async fn a_burst_on_one_key_becomes_one_run_whose_first_poll_drains_the_batch() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();

    for id in ["e1", "e2", "e3"] {
        assert_eq!(
            dispatcher.ingest(event(key.as_str(), id)).unwrap(),
            IngestResult::DebouncePending
        );
    }
    assert_eq!(dispatcher.status(&key).unwrap().inbox_depth, 3);

    clock.advance(DEBOUNCE);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let driver = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: tx,
            outcome: Outcome::Handled,
        }),
    );
    assert_eq!(
        driver.run_once().await.unwrap(),
        vec![Outcome::Handled],
        "one burst on one key: one run"
    );
    assert_eq!(
        rx.recv().await,
        Some(3),
        "the first poll returns the whole trigger batch for one reasoning step"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    assert_eq!(status.revision, 1);
}

#[tokio::test]
async fn an_event_ingested_mid_run_reaches_the_next_poll_which_renewed_the_lease() {
    // A 1s TTL makes the renewal observable: the mid-run ingest below
    // happens past the deadline `acquire` set, so it lands live only if
    // the poll renewed the lease before waiting.
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        lease_ttl_millis: 1_000,
        ..config()
    });
    register(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "trigger")).unwrap();
    clock.advance(DEBOUNCE);
    let (start, resume, mut rx, handler) = gated(Outcome::Handled, true);
    let driver = Driver::new(dispatcher.clone(), handler).with_holder("test-driver");
    let runner = tokio::spawn(async move { driver.run_once().await.unwrap() });
    until_leased(&dispatcher, &key).await;
    let active = dispatcher.list_active().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].lease.as_ref().unwrap().holder, "test-driver");

    // First poll at t = 1_800: drains the trigger and renews the lease to
    // 1_800 + 1_000 = 2_800.
    clock.advance(700);
    start.notify_one();
    assert_eq!(rx.recv().await, Some(1));

    // t = 2_700: past the unrenewed deadline (2_100), before the renewed
    // one (2_800). The event lands live — no second dispatch — only
    // because poll_inbox renewed the lease.
    clock.advance(900);
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "mid-run")).unwrap(),
        IngestResult::DeliveredLive
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Leased);
    assert_eq!(status.inbox_depth, 1);

    resume.notify_one();
    assert_eq!(
        rx.recv().await,
        Some(1),
        "the mid-run event reaches the live run"
    );
    assert_eq!(runner.await.unwrap(), vec![Outcome::Handled]);
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
}

#[tokio::test]
async fn the_session_survives_across_runs_and_the_revision_advances() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();
    let runs = Arc::new(Mutex::new(Vec::new()));
    let driver = Driver::new(
        dispatcher.clone(),
        Arc::new(ContinuityHandler { runs: runs.clone() }),
    );

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    assert_eq!(driver.run_once().await.unwrap(), vec![Outcome::Handled]);

    dispatcher.ingest(event(key.as_str(), "e2")).unwrap();
    clock.advance(DEBOUNCE);
    assert_eq!(driver.run_once().await.unwrap(), vec![Outcome::Handled]);

    let runs = runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].0, 1);
    assert_eq!(
        runs[1].0, 2,
        "run 2 read run 1's driver_state off the session"
    );
    assert_eq!(
        runs[0].1, runs[1].1,
        "both runs resumed the same persistent session"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.revision, 2, "one acquisition per run");
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.last_outcome, Some(Outcome::Handled));
}

#[tokio::test]
async fn a_failed_run_on_a_drained_inbox_returns_the_key_to_idle() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    let driver = Driver::new(dispatcher.clone(), Arc::new(FailAfterPoll));
    assert_eq!(
        driver.run_once().await.unwrap(),
        vec![Outcome::Failed("the diff is unreviewable".to_owned())]
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    assert_eq!(
        status.last_outcome,
        Some(Outcome::Failed("the diff is unreviewable".to_owned()))
    );
}

#[tokio::test]
async fn a_failed_run_with_a_pending_event_rearms_the_key_for_reacquire() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    let (start, resume, mut rx, handler) =
        gated(Outcome::Failed("tooling broke".to_owned()), false);
    let driver = Driver::new(dispatcher.clone(), handler);
    let runner = tokio::spawn(async move { driver.run_once().await.unwrap() });
    until_leased(&dispatcher, &key).await;
    start.notify_one();
    assert_eq!(rx.recv().await, Some(1));

    // The event slips in after the handler's last poll and before its
    // release: the failing run must not swallow it.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap(),
        IngestResult::DeliveredLive
    );
    resume.notify_one();
    assert_eq!(
        runner.await.unwrap(),
        vec![Outcome::Failed("tooling broke".to_owned())]
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Reacquire);
    assert_eq!(status.inbox_depth, 1);
    assert_eq!(
        status.last_outcome,
        Some(Outcome::Failed("tooling broke".to_owned()))
    );

    // The re-armed key dispatches again after the window: the pending
    // event is still there for the next run.
    clock.advance(DEBOUNCE);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let retry = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: tx,
            outcome: Outcome::Handled,
        }),
    );
    assert_eq!(retry.run_once().await.unwrap(), vec![Outcome::Handled]);
    assert_eq!(
        rx.recv().await,
        Some(1),
        "the next run sees the event the failure left"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.revision, 2);
}

#[tokio::test]
async fn a_key_parked_at_capacity_skips_the_pass_and_runs_later() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: 1,
        ..config()
    });
    register(&dispatcher);
    let held_key = ResourceKey::from("github:acme/widgets#1");
    let key = ResourceKey::from("github:acme/widgets#2");

    // An out-of-band lease fills the only slot before the driver runs.
    let (lease, _) = dispatcher.acquire(held_key.clone(), "external").unwrap();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);

    let (tx, _rx) = mpsc::unbounded_channel();
    let driver = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: tx,
            outcome: Outcome::Handled,
        }),
    );
    assert!(
        driver.run_once().await.unwrap().is_empty(),
        "no free slot: the key never became a run, so no outcome"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert!(
        status.admitted_at.is_some(),
        "the key waits in the admission queue, never dropped"
    );

    dispatcher.release(&lease, Outcome::Handled).unwrap();
    assert_eq!(driver.run_once().await.unwrap(), vec![Outcome::Handled]);
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.revision, 1);
    assert_eq!(status.last_outcome, Some(Outcome::Handled));
}

#[tokio::test]
async fn a_key_lost_to_a_race_reports_no_outcome_and_is_served_once() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);

    // The inline pass is polled before any spawned task runs, so its drive
    // runs first and its handler parks holding the lease while both admits
    // have offered the key. The spawned pass's acquire then lands on the
    // held lease and reports nothing; a notifier task un-parks the winner
    // once it holds the lease.
    let (start, resume, mut rx, handler) = gated(Outcome::Handled, true);
    let (loser_tx, _loser_rx) = mpsc::unbounded_channel();
    let loser_dispatcher = dispatcher.clone();
    let loser = tokio::spawn(async move {
        Driver::new(
            loser_dispatcher,
            Arc::new(SimpleHandler {
                report: loser_tx,
                outcome: Outcome::Handled,
            }),
        )
        .run_once()
        .await
        .unwrap()
    });
    let gate_dispatcher = dispatcher.clone();
    let gate_key = key.clone();
    let notifier = tokio::spawn(async move {
        for _ in 0..1_000 {
            if gate_dispatcher.status(&gate_key).unwrap().state == KeyState::Leased {
                break;
            }
            tokio::task::yield_now().await;
        }
        start.notify_one();
        resume.notify_one();
    });
    let winner = Driver::new(dispatcher.clone(), handler);
    assert_eq!(winner.run_once().await.unwrap(), vec![Outcome::Handled]);
    assert!(
        loser.await.unwrap().is_empty(),
        "the losing acquire reports nothing"
    );
    notifier.await.unwrap();
    assert_eq!(
        rx.recv().await,
        Some(1),
        "the winner drained the trigger, once"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.revision, 1);
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
}

#[tokio::test]
async fn a_panicking_handler_fails_its_run_and_the_key_recovers_after_ttl() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    let driver = Driver::new(dispatcher.clone(), Arc::new(Panic));
    let outcomes = driver.run_once().await.unwrap();
    assert!(
        matches!(outcomes.as_slice(), [Outcome::Failed(msg)] if msg.contains("panicked")),
        "the panic surfaces as the run's outcome: {outcomes:?}"
    );
    // The lease died with the task and is not swept until something
    // touches the key.
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);

    clock.advance(TTL + 1);
    let (tx, _rx) = mpsc::unbounded_channel();
    let sweeper = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: tx,
            outcome: Outcome::Handled,
        }),
    );
    sweeper.bootstrap().await.unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(
        status.state,
        KeyState::Reacquire,
        "the trigger was never drained"
    );
    assert_eq!(status.inbox_depth, 1);
    assert_eq!(
        status.last_outcome, None,
        "a panicked run records no outcome"
    );

    // The re-armed key dispatches normally on the next pass.
    clock.advance(DEBOUNCE);
    let (retry_tx, mut retry_rx) = mpsc::unbounded_channel();
    let retry = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: retry_tx,
            outcome: Outcome::Handled,
        }),
    );
    assert_eq!(retry.run_once().await.unwrap(), vec![Outcome::Handled]);
    assert_eq!(
        retry_rx.recv().await,
        Some(1),
        "the re-armed run sees its trigger"
    );
    assert_eq!(dispatcher.status(&key).unwrap().revision, 2);
}

#[tokio::test]
async fn a_run_that_outlives_its_lease_fails_loudly_instead_of_releasing() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        lease_ttl_millis: 1_000,
        ..config()
    });
    register(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    let (start, resume, mut rx, handler) = gated(Outcome::Handled, false);
    let driver = Driver::new(dispatcher.clone(), handler);
    let runner = tokio::spawn(async move { driver.run_once().await });
    until_leased(&dispatcher, &key).await;

    // First poll at t = 1_100 renews to 2_100; the run then parks past it.
    start.notify_one();
    assert_eq!(rx.recv().await, Some(1));
    clock.advance(1_001);
    resume.notify_one();

    // The final checkpoint is past the TTL: the run must abort with the
    // dispatcher's expiry, not finalize over a dead lease (RSD §5).
    let err = runner.await.unwrap().unwrap_err();
    assert!(matches!(err, DispatchError::Expired(_)));
    assert_eq!(
        dispatcher.status(&key).unwrap().state,
        KeyState::Leased,
        "nothing was persisted past the dead lease"
    );

    let (tx, _rx) = mpsc::unbounded_channel();
    let sweeper = Driver::new(
        dispatcher.clone(),
        Arc::new(SimpleHandler {
            report: tx,
            outcome: Outcome::Handled,
        }),
    );
    sweeper.bootstrap().await.unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(
        status.state,
        KeyState::Idle,
        "the sweep ended the dead lease on a drained inbox"
    );
    assert_eq!(status.last_outcome, None, "a lapsed run records no outcome");
}

#[tokio::test]
async fn concurrent_runs_serve_every_key_once_under_the_cap() {
    const KEYS: usize = 6;
    const CAP: usize = 2;

    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: CAP as u32,
        debounce_window_millis: 0,
        ..config()
    });
    register(&dispatcher);
    let keys: Vec<ResourceKey> = (0..KEYS)
        .map(|i| ResourceKey::from(format!("github:acme/widgets#{i}")))
        .collect();
    for (i, key) in keys.iter().enumerate() {
        dispatcher
            .ingest(event(key.as_str(), &format!("e{i}")))
            .unwrap();
    }
    clock.advance(1);

    let live = Arc::new(AtomicUsize::new(0));
    let max_live = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(Mutex::new(Vec::new()));
    let driver = Driver::new(
        dispatcher.clone(),
        Arc::new(CappedHandler {
            live: live.clone(),
            max_live: max_live.clone(),
            runs: runs.clone(),
        }),
    );

    // One pass serves at most CAP keys; the rest wait in the admission
    // queue and are offered on later passes.
    let mut outcomes = Vec::new();
    for _ in 0..KEYS {
        outcomes.extend(driver.run_once().await.unwrap());
        if outcomes.len() == KEYS {
            break;
        }
    }
    assert_eq!(outcomes.len(), KEYS);
    assert!(outcomes.iter().all(|o| *o == Outcome::Handled));

    let mut ran = runs.lock().unwrap().clone();
    ran.sort();
    assert_eq!(ran, keys, "every key served exactly once");
    assert!(
        max_live.load(Ordering::SeqCst) <= CAP,
        "the global cap held under concurrent runs"
    );
    for key in &keys {
        let status = dispatcher.status(key).unwrap();
        assert_eq!(status.state, KeyState::Idle);
        assert_eq!(status.revision, 1);
        assert_eq!(status.inbox_depth, 0);
        assert_eq!(status.last_outcome, Some(Outcome::Handled));
    }
}

/// Two drivers over one store, each owning half the key space: an event
/// for the other's key is not driven here, is not consumed, and is
/// driven — once — by the driver that owns it. This is the claw's
/// forge/resident split, where an unfiltered pass would run, fail, and
/// consume the other side's event.
#[tokio::test]
async fn a_driver_leaves_the_keys_it_does_not_own_for_the_driver_that_does() {
    let (dispatcher, clock) = setup();
    register(&dispatcher);

    struct WhoAmI {
        mine: &'static str,
    }
    #[async_trait]
    impl Handler for WhoAmI {
        async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
            let _ = ctx.poll_inbox().await;
            Outcome::Failed(self.mine.to_owned())
        }
    }

    let mine = the_key(); // github:acme/widgets#142
    let theirs = ResourceKey::from("interactive:web");
    dispatcher.ingest(event(mine.as_str(), "e-mine")).unwrap();
    dispatcher
        .ingest(event(theirs.as_str(), "e-theirs"))
        .unwrap();
    // Past the debounce window, both keys are due.
    clock.advance(common::DEBOUNCE);

    let only_mine = Arc::new(WhoAmI { mine: "mine" });
    let only_theirs = Arc::new(WhoAmI { mine: "theirs" });
    let driver_a = Arc::new(Driver::new(dispatcher.clone(), only_mine).owning(Arc::new(
        |key: &ResourceKey| key.as_str().starts_with("github:"),
    )));
    let driver_b = Arc::new(
        Driver::new(dispatcher.clone(), only_theirs).owning(Arc::new(|key: &ResourceKey| {
            key.as_str().starts_with("interactive:")
        })),
    );

    // A pass of the github driver answers only its own key; the
    // interactive key's event is untouched.
    let outcomes = driver_a.run_once().await.unwrap();
    assert_eq!(
        outcomes,
        vec![Outcome::Failed("mine".to_owned())],
        "one run, the owned key only"
    );
    let status = dispatcher.status(&theirs).unwrap();
    assert_eq!(status.inbox_depth, 1, "the other driver's event is intact");
    assert_eq!(status.state, KeyState::Idle);

    // The owning driver picks it up on its own pass.
    let outcomes = driver_b.run_once().await.unwrap();
    assert_eq!(outcomes, vec![Outcome::Failed("theirs".to_owned())]);
    let status = dispatcher.status(&theirs).unwrap();
    assert_eq!(status.inbox_depth, 0, "driven exactly once");
    assert_eq!(
        status.last_outcome,
        Some(Outcome::Failed("theirs".to_owned()))
    );
}
