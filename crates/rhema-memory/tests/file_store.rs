//! The file store against the shared contract, plus the behavior only a
//! filesystem backend can have: frontmatter round-trips, corrupt files
//! that never take search down, durability across store instances, and
//! re-scoping that rewrites in place.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use rhema_memory::{
    Decay, FileMemoryStore, MemoryDraft, MemoryError, MemoryQuery, MemoryRecord, MemoryStore,
    Scope, Source, Status, VerifyHint, store_contract,
};

fn store(dir: &tempfile::TempDir) -> FileMemoryStore {
    FileMemoryStore::new(dir.path()).unwrap()
}

#[test]
fn the_contract_holds() {
    let dir = tempfile::tempdir().unwrap();
    store_contract::run_all(&store(&dir));
}

#[test]
fn ranking_orders_terms_recency_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    store_contract::run_rank_orders(&store(&dir));
}

#[test]
fn a_body_of_delimiters_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let body = "usage:\n---\nrun `just release`\n---\nthen push the tag\n";
    let record = store
        .put(MemoryDraft::new(Scope::Global, body).with_tags(["release"]))
        .unwrap();
    assert_eq!(store.get(&record.id).unwrap().unwrap().body, body);
    let found = store.search(MemoryQuery::new("release")).unwrap();
    assert_eq!(found[0].body, body);
}

#[test]
fn a_corrupt_file_never_takes_search_down() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let good = store
        .put(MemoryDraft::new(Scope::Global, "a good fact"))
        .unwrap();

    let junk = dir.path().join("20261008T000000-000000ff.md");
    fs::write(&junk, "not frontmatter at all\n").unwrap();

    let bodies: Vec<String> = store
        .search(MemoryQuery::new(""))
        .unwrap()
        .into_iter()
        .map(|record| record.body)
        .collect();
    assert_eq!(bodies, vec!["a good fact".to_owned()]);

    let err = store.get("20261008T000000-000000ff").unwrap_err();
    assert!(matches!(err, MemoryError::Corrupt { .. }), "{err:?}");
    assert!(store.forget("20261008T000000-000000ff").unwrap());
    assert!(!junk.exists());
    let _ = good;
}

#[test]
fn foreign_files_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    store
        .put(MemoryDraft::new(Scope::Global, "the only fact"))
        .unwrap();
    fs::write(dir.path().join("notes.txt"), "not a record").unwrap();
    // A leftover from a crashed write is skipped too.
    fs::write(dir.path().join("20261008T000000-000000fe.md.tmp"), "half").unwrap();

    let found = store.search(MemoryQuery::new("")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].body, "the only fact");
}

#[test]
fn a_put_survives_a_new_store_instance() {
    let dir = tempfile::tempdir().unwrap();
    let record = {
        let store = store(&dir);
        store
            .put(MemoryDraft::new(Scope::Global, "durable fact").with_tags(["durable"]))
            .unwrap()
    };

    // A new instance has a reset id counter; the fresh id still must not
    // collide, even within the same wall-clock second.
    let reopened = store(&dir);
    let loaded = reopened.get(&record.id).unwrap().unwrap();
    assert_eq!(loaded, record);
    let next = reopened
        .put(MemoryDraft::new(Scope::Global, "the next fact"))
        .unwrap();
    assert_ne!(next.id, record.id);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn rescoping_rewrites_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let created = store
        .put(MemoryDraft::new(Scope::Global, "moved fact"))
        .unwrap();

    let moved = store
        .put(
            MemoryDraft::new(
                Scope::Workspace(PathBuf::from("/somewhere/else")),
                "moved fact",
            )
            .with_id(created.id.clone()),
        )
        .unwrap();
    assert_eq!(
        moved.scope,
        Scope::Workspace(PathBuf::from("/somewhere/else"))
    );
    assert_eq!(moved.created_at, created.created_at);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

    let query =
        MemoryQuery::new("moved").with_scopes([Scope::Workspace(PathBuf::from("/somewhere/else"))]);
    let found: Vec<MemoryRecord> = store.search(query).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, created.id);
}

#[test]
fn an_update_of_a_corrupt_record_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let record = store
        .put(MemoryDraft::new(Scope::Global, "about to be mangled"))
        .unwrap();
    fs::write(dir.path().join(format!("{}.md", record.id)), "mangled").unwrap();

    let err = store
        .put(MemoryDraft::new(Scope::Global, "fix").with_id(record.id.clone()))
        .unwrap_err();
    assert!(matches!(err, MemoryError::Corrupt { .. }), "{err:?}");
    // `forget` does not parse, so it is the way out.
    assert!(store.forget(&record.id).unwrap());
}

#[test]
fn a_v1_file_without_temporal_fields_reads_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Exactly what the pre-validity store wrote: no decay, no status, no
    // validity window, no provenance.
    fs::write(
        root.join("20261001T000000-00000001.md"),
        "---\n id: 20261001T000000-00000001\n scope: Global\n tags: [migration]\n created_at: 1759276800000\n updated_at: 1759276800000\n---\nthe old way of deploying\n",
    )
    .unwrap();

    let store = store(&dir);
    let record = store
        .get("20261001T000000-00000001")
        .unwrap()
        .expect("reads unchanged");
    // The body is whatever followed the frontmatter, verbatim.
    assert_eq!(record.body, "the old way of deploying\n");
    assert_eq!(
        record.valid_from, record.created_at,
        "validity from creation"
    );
    assert_eq!(record.valid_until, None);
    assert_eq!(record.decay, Decay::Volatile, "derived from the tags");
    assert_eq!(record.source, Source::default(), "provenance unknown");
    assert_eq!(record.status, Status::Active);
    assert_eq!(record.verified_at, None);
    assert_eq!(record.subject, None);
    assert!(store.search(MemoryQuery::new("deploying")).unwrap().len() == 1);
}

#[test]
fn temporal_fields_round_trip_through_the_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let day = 86_400_000u64;
    let now = store
        .put(MemoryDraft::new(Scope::Global, "clock probe"))
        .unwrap()
        .created_at;
    let saved = store
        .put(
            MemoryDraft::new(Scope::Global, "the lease runs out")
                .with_tags(["home"])
                .with_subject("home/accommodation")
                .with_decay(Decay::Long)
                .with_source(Source::Told {
                    by: "the landlord".to_owned(),
                })
                .with_valid_from(now - day)
                .with_valid_until(now + 90 * day)
                .with_verify_hint(VerifyHint::Manual),
        )
        .unwrap();
    // A write marks the frontmatter with the temporal facts.
    let raw = fs::read_to_string(dir.path().join(format!("{}.md", saved.id))).unwrap();
    assert!(raw.contains("subject: home/accommodation"), "{raw}");
    assert!(raw.contains("decay: long"), "{raw}");
    assert!(raw.contains("kind: told"), "{raw}");

    let loaded = store.get(&saved.id).unwrap().unwrap();
    assert_eq!(loaded, saved);
    assert_eq!(loaded.decay, Decay::Long, "declared decay, not derived");
    assert_eq!(loaded.subject.as_deref(), Some("home/accommodation"));
    assert_eq!(loaded.verify_hint, Some(VerifyHint::Manual));
    assert_eq!(loaded.valid_from, now - day);
}

#[test]
fn a_shared_arc_serves_concurrent_consumers() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(store(&dir));
    let mut handles = Vec::new();
    for n in 0..8 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            store
                .put(MemoryDraft::new(Scope::Global, format!("fact {n}")))
                .unwrap()
        }));
    }
    let ids: std::collections::BTreeSet<String> =
        handles.into_iter().map(|h| h.join().unwrap().id).collect();
    assert_eq!(ids.len(), 8);
    assert_eq!(store.search(MemoryQuery::new("fact")).unwrap().len(), 8);
}
