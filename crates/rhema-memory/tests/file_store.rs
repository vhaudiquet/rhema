//! The file store against the shared contract, plus the behavior only a
//! filesystem backend can have: frontmatter round-trips, corrupt files
//! that never take search down, durability across store instances, and
//! re-scoping that rewrites in place.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use rhema_memory::{
    FileMemoryStore, MemoryDraft, MemoryError, MemoryQuery, MemoryRecord, MemoryStore, Scope,
    store_contract,
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
    store_contract::rank_orders_terms_recency_and_tags(&store(&dir));
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
