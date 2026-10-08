//! The replayable [`MemoryStore`] behavioral suite.
//!
//! Every function here is a plain assertion-based test body taking any
//! `&dyn MemoryStore`; both bundled stores replay the exact same suite
//! from this crate's own tests, and a future backend can do the same from
//! its own. Each function's records carry unique markers so they compose
//! on one shared store; `run_all` runs the order-agnostic set in order.
//!
//! The rank-order assertions live in
//! [`rank_orders_terms_recency_and_tags`], kept out of [`run_all`]
//! deliberately: a store with a different ranking method — the semantic
//! ranker the framework spec defers to later — must be able to replay
//! the whole order-agnostic suite unchanged. Swapping the ranker may
//! never break a caller.

use std::path::PathBuf;

use crate::record::{BODY_LIMIT, MemoryDraft, Scope, check_id};
use crate::search::MemoryQuery;
use crate::store::{MemoryError, MemoryStore};

fn ws(path: &str) -> Scope {
    Scope::Workspace(PathBuf::from(path))
}

/// `put` creates; `get` returns it field-for-field, timestamps assigned.
pub fn put_then_get_round_trips(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "one fact").with_tags(["round-trip"]))
        .unwrap();
    assert_eq!(record.body, "one fact");
    assert_eq!(record.tags, vec!["round-trip".to_owned()]);
    assert_eq!(record.created_at, record.updated_at);
    assert_eq!(store.get(&record.id).unwrap(), Some(record));
}

/// `get` on an id the store never issued is `None`, not an error.
pub fn get_missing_returns_none(store: &dyn MemoryStore) {
    assert_eq!(store.get("19700101T000000-00000000").unwrap(), None);
}

/// A malformed id is rejected before any lookup, on every entry point.
pub fn malformed_ids_are_rejected(store: &dyn MemoryStore) {
    for id in ["", "../escape", "has_underscore", "has space"] {
        assert!(
            matches!(store.get(id), Err(MemoryError::InvalidId { .. })),
            "get({id:?})"
        );
        assert!(
            matches!(store.forget(id), Err(MemoryError::InvalidId { .. })),
            "forget({id:?})"
        );
        let draft = MemoryDraft::new(Scope::Global, "body").with_id(id);
        assert!(
            matches!(store.put(draft), Err(MemoryError::InvalidId { .. })),
            "put({id:?})"
        );
    }
}

/// Creates get distinct, well-shaped, time-ordered ids.
pub fn put_create_assigns_distinct_ids(store: &dyn MemoryStore) {
    let mut ids = Vec::new();
    for body in ["distinct-1", "distinct-2", "distinct-3"] {
        ids.push(store.put(MemoryDraft::new(Scope::Global, body)).unwrap().id);
    }
    let unique: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(unique.len(), 3, "ids must differ: {ids:?}");
    for id in &ids {
        assert!(check_id(id).is_ok(), "{id} must fit the id shape");
    }
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "creation order is id order");
}

/// An update keeps the id and `created_at`, swaps the content, and may
/// re-aim the scope.
pub fn put_update_preserves_created_at_and_id(store: &dyn MemoryStore) {
    let created = store
        .put(MemoryDraft::new(Scope::Global, "update-me v1").with_tags(["update-marker"]))
        .unwrap();
    let updated = store
        .put(
            MemoryDraft::new(ws("/contract/update"), "update-me v2")
                .with_tags(["update-marker"])
                .with_id(created.id.clone()),
        )
        .unwrap();
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);
    assert_eq!(updated.body, "update-me v2");
    assert_eq!(updated.scope, ws("/contract/update"));
    assert_eq!(store.get(&created.id).unwrap(), Some(updated));
}

/// An update naming an absent id is `UnknownId`, not a silent create.
pub fn put_update_unknown_id_errors(store: &dyn MemoryStore) {
    let err = store
        .put(MemoryDraft::new(Scope::Global, "ghost").with_id("19700101T000000-00000001"))
        .unwrap_err();
    assert!(
        matches!(err, MemoryError::UnknownId { ref id } if id == "19700101T000000-00000001"),
        "got {err:?}"
    );
}

/// `forget` removes once, then reports absence idempotently.
pub fn forget_removes_then_reports_absent(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "forget-me"))
        .unwrap();
    assert!(store.forget(&record.id).unwrap());
    assert_eq!(store.get(&record.id).unwrap(), None);
    assert!(!store.forget(&record.id).unwrap());
}

/// Empty and oversized bodies are rejected by every store, before the
/// backend is touched.
pub fn body_limits_are_enforced(store: &dyn MemoryStore) {
    assert!(matches!(
        store.put(MemoryDraft::new(Scope::Global, "   ")),
        Err(MemoryError::EmptyBody)
    ));
    let long = "x".repeat(BODY_LIMIT + 1);
    assert!(matches!(
        store.put(MemoryDraft::new(Scope::Global, long)),
        Err(MemoryError::BodyTooLong {
            limit: BODY_LIMIT,
            ..
        })
    ));
}

/// The stored tags are the normalized set, whatever the draft said.
pub fn tags_are_normalized(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "tagged fact").with_tags([
            "Commits",
            " commits ",
            "",
            "CONVENTION",
            "commits",
        ]))
        .unwrap();
    assert_eq!(
        record.tags,
        vec!["commits".to_owned(), "convention".to_owned()]
    );
}

/// A search spans exactly the scopes it names — a workspace record never
/// surfaces in another workspace's search.
pub fn search_respects_scopes(store: &dyn MemoryStore) {
    store
        .put(MemoryDraft::new(Scope::Global, "scopes-alpha fact"))
        .unwrap();
    store
        .put(MemoryDraft::new(
            ws("/contract/scopes-a"),
            "scopes-beta fact",
        ))
        .unwrap();
    store
        .put(MemoryDraft::new(
            ws("/contract/scopes-b"),
            "scopes-gamma fact",
        ))
        .unwrap();

    let bodies = |scopes: Vec<Scope>| {
        let query = MemoryQuery::new("scopes").with_scopes(scopes);
        store
            .search(query)
            .unwrap()
            .into_iter()
            .map(|record| record.body)
            .collect::<Vec<_>>()
    };
    let global_and_a = bodies(vec![Scope::Global, ws("/contract/scopes-a")]);
    assert!(global_and_a.contains(&"scopes-alpha fact".to_owned()));
    assert!(global_and_a.contains(&"scopes-beta fact".to_owned()));
    assert!(!global_and_a.contains(&"scopes-gamma fact".to_owned()));

    let only_b = bodies(vec![ws("/contract/scopes-b")]);
    assert_eq!(only_b, vec!["scopes-gamma fact".to_owned()]);
}

/// The tag filter is a conjunction over normalized tags.
pub fn search_filters_by_tags(store: &dyn MemoryStore) {
    store
        .put(
            MemoryDraft::new(Scope::Global, "tag-filter wanted").with_tags(["contract-tag-filter"]),
        )
        .unwrap();
    store
        .put(MemoryDraft::new(Scope::Global, "tag-filter unwanted"))
        .unwrap();
    let query = MemoryQuery::new("tag-filter").with_tags(["contract-tag-filter"]);
    let found = store.search(query).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].body, "tag-filter wanted");
}

/// The limit truncates the ranked results.
pub fn search_respects_limit(store: &dyn MemoryStore) {
    for n in 1..=3 {
        store
            .put(MemoryDraft::new(
                Scope::Global,
                format!("contractlimit {n}"),
            ))
            .unwrap();
    }
    let query = MemoryQuery::new("contractlimit").with_limit(2);
    let found = store.search(query).unwrap();
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|record| record.body.contains("contractlimit"))
    );
}

/// The last record written leads an empty query, and `put` after `get`
/// needs no clock: id order is creation order. Runs last in [`run_all`],
/// so "last written" means the final record of the whole suite.
pub fn search_empty_query_returns_recent(store: &dyn MemoryStore) {
    let first = store
        .put(MemoryDraft::new(Scope::Global, "recent-marker first"))
        .unwrap();
    let second = store
        .put(MemoryDraft::new(Scope::Global, "recent-marker second"))
        .unwrap();
    let found = store.search(MemoryQuery::new("")).unwrap();
    assert_eq!(found[0].id, second.id);
    assert_eq!(found[1].id, first.id);
}

/// Term hits outrank body hits, and equal scores fall back to recency.
/// Order-sensitive, so it is *not* part of [`run_all`]: a store with a
/// different ranking method cannot replay it, by design.
pub fn rank_orders_terms_recency_and_tags(store: &dyn MemoryStore) {
    store
        .put(MemoryDraft::new(Scope::Global, "deploy the old way"))
        .unwrap();
    store
        .put(MemoryDraft::new(Scope::Global, "deploy the new way"))
        .unwrap();
    // Created last, but its tag hit outranks either body hit regardless
    // of recency.
    store
        .put(MemoryDraft::new(Scope::Global, "ship it").with_tags(["deploy"]))
        .unwrap();
    let found = store.search(MemoryQuery::new("deploy")).unwrap();
    let bodies: Vec<String> = found.into_iter().map(|record| record.body).collect();
    assert_eq!(
        bodies,
        vec![
            "ship it".to_owned(),            // tag hit outranks either body hit
            "deploy the new way".to_owned(), // equal scores: newer first
            "deploy the old way".to_owned(),
        ]
    );
}

/// Runs the whole order-agnostic suite against `store`, which must be
/// fresh. The rank-order assertions live in
/// [`rank_orders_terms_recency_and_tags`]; see the module docs for why.
pub fn run_all(store: &dyn MemoryStore) {
    put_then_get_round_trips(store);
    get_missing_returns_none(store);
    malformed_ids_are_rejected(store);
    put_create_assigns_distinct_ids(store);
    put_update_preserves_created_at_and_id(store);
    put_update_unknown_id_errors(store);
    forget_removes_then_reports_absent(store);
    body_limits_are_enforced(store);
    tags_are_normalized(store);
    search_respects_scopes(store);
    search_filters_by_tags(store);
    search_respects_limit(store);
    search_empty_query_returns_recent(store);
}
