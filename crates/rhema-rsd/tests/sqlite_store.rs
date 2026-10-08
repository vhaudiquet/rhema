//! The SQLite [`Store`](rhema_rsd::Store): the contract suite replayed,
//! durability across a reopen, crash recovery through the dispatcher, and
//! the compare-and-swap holding across two independent connections.

#![cfg(feature = "sqlite")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rhema_core::{Message, Role};
use rhema_rsd::{
    Dispatcher, DispatcherConfig, Event, FixedClock, KeyState, ResourceKey, SqliteStore, Store,
};

#[cfg(feature = "store-contract")]
#[test]
fn sqlite_store_satisfies_the_store_contract() {
    let store = SqliteStore::in_memory().unwrap();
    rhema_rsd::store_contract::run_all(&store);
}

static NEXT_DB: AtomicU64 = AtomicU64::new(0);

/// A unique file-backed database path; WAL sidecars cleaned up with it.
fn db_path(label: &str) -> PathBuf {
    let n = NEXT_DB.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rhema-rsd-{}-{label}-{n}.db", std::process::id()))
}

/// Removes the database and its WAL sidecars, best-effort.
fn remove_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        drop(std::fs::remove_file(PathBuf::from(name)));
    }
}

/// Test-sized windows: a 100ms debounce, a 10s TTL, a 500ms admission TTL.
fn config() -> DispatcherConfig {
    DispatcherConfig {
        debounce_window_millis: 100,
        lease_ttl_millis: 10_000,
        admission_ttl_millis: 500,
        ..DispatcherConfig::default()
    }
}

fn register_github(dispatcher: &Dispatcher) {
    dispatcher.register_source(
        "github",
        Arc::new(|event: &Event| {
            ResourceKey::from(event.payload["key"].as_str().unwrap_or("github:unknown"))
        }),
    );
}

fn event(key: &str, id: &str) -> Event {
    Event {
        id: id.into(),
        source: "github".into(),
        payload: serde_json::json!({ "key": key }),
        received_at: 0,
    }
}

#[test]
fn records_survive_a_reopen() {
    let path = db_path("reopen");
    let key = ResourceKey::from("github:acme/widgets#142");

    let mut record = rhema_rsd::KeyRecord::fresh(key.clone(), 1_000);
    record.revision = 5;
    record.session = Some(rhema_rsd::Session {
        key: key.clone(),
        core: rhema_core::Session {
            history: vec![Message::text(Role::User, "half the work is done")],
            driver_state: serde_json::json!({ "attempt": 2 }),
        },
        plan: Some(serde_json::json!(["step one"])),
        artifact_ref: Some("worktrees/issue-142".into()),
        created_at: 1_000,
        last_active: 2_000,
    });
    {
        let store = SqliteStore::open(&path).unwrap();
        store.save(record.clone(), None).unwrap();
        // Dropped here: the process "dies" with the record committed.
    }

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load(&key).unwrap(), Some(record));
    assert_eq!(store.keys().unwrap(), vec![key]);
    remove_db(&path);
}

#[test]
fn a_crash_between_dispatchers_keeps_the_session_and_the_queue() {
    let path = db_path("crash");
    let key = ResourceKey::from("github:acme/widgets#142");
    let clock = Arc::new(FixedClock::new(1_000));

    // First process: dispatch, work half a turn, checkpoint, and take a
    // second event into the inbox; then die without releasing.
    {
        let store = SqliteStore::open(&path).unwrap();
        let dispatcher = Dispatcher::new(config(), clock.clone(), Arc::new(store));
        register_github(&dispatcher);
        dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
        clock.advance(100);
        dispatcher.admit().unwrap();
        let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
        assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
        session
            .core
            .history
            .push(Message::text(Role::User, "half the work is done"));
        session.artifact_ref = Some("worktrees/issue-142".into());
        dispatcher.checkpoint(&lease, session).unwrap();
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap();
    }

    // The restart's clock sits past the abandoned lease's TTL.
    clock.advance(10_001);
    let store = SqliteStore::open(&path).unwrap();
    let restarted = Dispatcher::new(config(), clock.clone(), Arc::new(store));
    register_github(&restarted);

    restarted.recover().unwrap();
    let status = restarted.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Reacquire);

    clock.advance(100);
    let admitted: Vec<ResourceKey> = restarted.admit().unwrap();
    assert_eq!(admitted, vec![key.clone()]);
    let (lease, session) = restarted.acquire(key.clone(), "handler-2").unwrap();

    // The same session resumed, and the undelivered event is still queued.
    assert_eq!(session.core.history.len(), 1);
    assert_eq!(
        session.core.history[0],
        Message::text(Role::User, "half the work is done")
    );
    assert_eq!(session.artifact_ref.as_deref(), Some("worktrees/issue-142"));
    let drained = restarted.poll_inbox(&lease).unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].event.id, "e2");

    restarted
        .release(&lease, rhema_rsd::Outcome::Handled)
        .unwrap();
    remove_db(&path);
}

#[test]
fn two_connections_cannot_lose_an_update() {
    let path = db_path("cas");
    let key = ResourceKey::from("github:acme/widgets#142");
    let first = SqliteStore::open(&path).unwrap();
    let second = SqliteStore::open(&path).unwrap();

    let record = rhema_rsd::KeyRecord::fresh(key.clone(), 1_000);
    first.save(record.clone(), None).unwrap();

    // Both connections read revision 0 and race to write revision 1.
    assert_eq!(first.load(&key).unwrap().unwrap().revision, 0);
    assert_eq!(second.load(&key).unwrap().unwrap().revision, 0);

    let mut winner = record.clone();
    winner.revision = 1;
    let mut loser = record;
    loser.revision = 1;
    loser.updated_at = 2_000;

    first.save(winner.clone(), Some(0)).unwrap();
    match second.save(loser, Some(0)) {
        Err(rhema_rsd::StoreError::RevisionMismatch {
            expected: 0,
            actual: 1,
        }) => {}
        other => panic!("expected a revision mismatch, got: {other:?}"),
    }
    assert_eq!(second.load(&key).unwrap(), Some(winner));
    remove_db(&path);
}
