//! The Postgres store, against a real database — one container, the
//! schema migration, a record round-trip, the compare-and-swap, the
//! conflict, and the meta table. Gated: the test runs when
//! `RHEMA_TEST_PG_URL` names a Postgres, and reports a plain pass
//! otherwise, so `cargo test --workspace` stays offline-first.

use rhema_rsd::{KeyRecord, ResourceKey, Store};
use rhema_vercel_agent::PgStore;

/// The URL the CI (or a developer) provides.
fn pg_url() -> Option<String> {
    std::env::var("RHEMA_TEST_PG_URL")
        .ok()
        .filter(|url| !url.is_empty())
}

fn a_record(key: &str, revision: u64) -> KeyRecord {
    let mut record = KeyRecord::fresh(ResourceKey::new(key.to_owned()), 1_000);
    record.revision = revision;
    record
}

#[test]
fn the_store_round_trips_a_record_and_enforces_the_revision() {
    let Some(url) = pg_url() else {
        eprintln!("RHEMA_TEST_PG_URL unset: the Postgres store test is skipped");
        return;
    };
    // A unique schema space per run: the table names are fixed, so a
    // concurrent run would collide. The test drops its rows at the
    // end and keys them with a run-specific prefix.
    let store = PgStore::open(&url).expect("the store opens");
    let run = format!(
        "test-{}-",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let key = ResourceKey::new(format!("{run}session-a"));

    // Never written: None.
    assert_eq!(store.load(&key).unwrap(), None);

    // First write: the key must not exist yet.
    let record = a_record(key.as_str(), 1);
    store.save(record.clone(), None).unwrap();
    let loaded = store.load(&key).unwrap().unwrap();
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.key, key);

    // A second "create" is a conflict naming both revisions.
    let err = store.save(a_record(key.as_str(), 1), None).unwrap_err();
    assert!(
        matches!(
            &err,
            rhema_rsd::StoreError::RevisionMismatch {
                expected: 0,
                actual: 1
            }
        ),
        "{err}"
    );

    // The compare-and-swap: right revision advances, wrong one fails.
    let mut next = a_record(key.as_str(), 2);
    next.state = loaded.state;
    store.save(next, Some(1)).unwrap();
    let err = store.save(a_record(key.as_str(), 3), Some(1)).unwrap_err();
    assert!(
        matches!(
            &err,
            rhema_rsd::StoreError::RevisionMismatch {
                expected: 1,
                actual: 2
            }
        ),
        "{err}"
    );
    assert_eq!(store.load(&key).unwrap().unwrap().revision, 2);

    // The keys listing carries the run's key, in order.
    let keys = store.keys().unwrap();
    assert!(keys.contains(&key));

    // Cleanup: the run's rows go away, through this test's own
    // connection.
    let mut client = raw_client(&url);
    client
        .execute(
            "DELETE FROM records WHERE key LIKE $1",
            &[&format!("{run}%")],
        )
        .unwrap();
    client
        .execute("DELETE FROM meta WHERE key LIKE $1", &[&format!("{run}%")])
        .unwrap();
}

/// This test's own connection, for setup and cleanup.
fn raw_client(url: &str) -> postgres::Client {
    let connector =
        postgres_native_tls::MakeTlsConnector::new(native_tls::TlsConnector::new().unwrap());
    postgres::Client::connect(url, connector).unwrap()
}

#[test]
fn the_meta_table_round_trips_and_replaces() {
    let Some(url) = pg_url() else {
        eprintln!("RHEMA_TEST_PG_URL unset: the Postgres store test is skipped");
        return;
    };
    let store = PgStore::open(&url).unwrap();
    let run = format!(
        "test-{}-",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    assert_eq!(store.meta(&format!("{run}config")).unwrap(), None);
    store
        .set_meta(&format!("{run}config"), "{\"a\":1}")
        .unwrap();
    store
        .set_meta(&format!("{run}config"), "{\"a\":2}")
        .unwrap();
    assert_eq!(
        store.meta(&format!("{run}config")).unwrap().as_deref(),
        Some("{\"a\":2}")
    );
    raw_client(&url)
        .execute("DELETE FROM meta WHERE key LIKE $1", &[&format!("{run}%")])
        .unwrap();
}
