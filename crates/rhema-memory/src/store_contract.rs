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

use crate::record::{BODY_LIMIT, Decay, MemoryDraft, Scope, Source, Status, VerifyHint, check_id};
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

/// A create carries the declared validity, provenance, and hint, and
/// derives the rest: `valid_from` from creation, decay from the tag
/// namespace, unknown source, active status.
pub fn put_carries_validity_provenance_and_defaults(store: &dyn MemoryStore) {
    let day = 86_400_000;
    let record = store
        .put(
            MemoryDraft::new(Scope::Global, "temporal fact")
                .with_tags(["Infrastructure"])
                .with_subject("home/accommodation")
                .with_valid_until(90 * day)
                .with_decay(Decay::Permanent)
                .with_source(Source::Told {
                    by: "the user".to_owned(),
                })
                .with_verify_hint(VerifyHint::Manual),
        )
        .unwrap();
    assert_eq!(record.valid_from, record.created_at);
    assert_eq!(record.valid_until, Some(90 * day));
    assert_eq!(record.decay, Decay::Permanent, "declared decay wins");
    assert_eq!(
        record.source,
        Source::Told {
            by: "the user".to_owned()
        }
    );
    assert_eq!(record.subject.as_deref(), Some("home/accommodation"));
    assert_eq!(record.status, Status::Active);
    assert_eq!(record.verified_at, None);
    assert_eq!(record.verify_hint, Some(VerifyHint::Manual));

    let derived = store
        .put(MemoryDraft::new(Scope::Global, "derived fact").with_tags(["migration"]))
        .unwrap();
    assert_eq!(derived.decay, Decay::Volatile, "derived from the tags");
    assert_eq!(derived.source, Source::default(), "provenance unknown");
    assert_eq!(derived.status, Status::Active);
    assert_eq!(derived.subject, None);

    let bad = MemoryDraft::new(Scope::Global, "bad subject").with_subject("Has Spaces");
    assert!(
        matches!(store.put(bad), Err(MemoryError::InvalidSubject { .. })),
        "malformed subject rejected"
    );
}

/// The default search rules: only active records inside their validity
/// window answer; `as_of` replays any instant; `include_superseded`
/// widens the status rule but not the window; `fresh_only` drops records
/// past their decay while staleness itself never hides anything.
pub fn search_applies_the_validity_rules(store: &dyn MemoryStore) {
    let day = 86_400_000;
    let now = now_of(store);
    for (body, draft) in [
        (
            "validity stable record",
            MemoryDraft::new(Scope::Global, "validity stable record"),
        ),
        (
            "validity expired record",
            MemoryDraft::new(Scope::Global, "validity expired record").with_valid_until(now - day),
        ),
        (
            "validity unborn record",
            MemoryDraft::new(Scope::Global, "validity unborn record")
                .with_valid_from(now + 2 * day),
        ),
        (
            "validity volatile record",
            MemoryDraft::new(Scope::Global, "validity volatile record").with_tags(["migration"]),
        ),
    ] {
        store.put(draft).unwrap_or_else(|e| panic!("{body}: {e}"));
    }

    let bodies = |query: MemoryQuery| -> Vec<String> {
        store
            .search(query)
            .unwrap()
            .into_iter()
            .map(|record| record.body)
            .collect()
    };

    let default = bodies(MemoryQuery::new("validity"));
    assert!(
        default.contains(&"validity stable record".to_owned()),
        "{default:?}"
    );
    assert!(
        default.contains(&"validity volatile record".to_owned()),
        "{default:?}"
    );
    assert!(
        !default.contains(&"validity expired record".to_owned()),
        "the horizon has passed: hidden, not killed"
    );
    assert!(
        !default.contains(&"validity unborn record".to_owned()),
        "not valid yet"
    );

    let replay = bodies(MemoryQuery::new("validity").as_of(now + day));
    assert!(
        !replay.contains(&"validity unborn record".to_owned()),
        "still unborn at day 1: {replay:?}"
    );

    let later = bodies(MemoryQuery::new("validity").as_of(now + 31 * day));
    assert!(
        later.contains(&"validity unborn record".to_owned()),
        "born by day 31: {later:?}"
    );
    assert!(
        later.contains(&"validity volatile record".to_owned()),
        "staleness annotates, it never hides: {later:?}"
    );

    let fresh = bodies(
        MemoryQuery::new("validity")
            .fresh_only()
            .as_of(now + 31 * day),
    );
    assert!(
        !fresh.contains(&"validity volatile record".to_owned()),
        "fresh_only drops the stale: {fresh:?}"
    );
    assert!(
        fresh.contains(&"validity stable record".to_owned()),
        "{fresh:?}"
    );

    let widened = bodies(MemoryQuery::new("validity").include_superseded());
    assert_eq!(
        widened.len(),
        default.len(),
        "widening the status rule changes nothing here: the window rule holds"
    );
}

/// A record created and retired inside the test runs is replayable:
/// `as_of` before the retirement still shows it as it was believed then.
pub fn as_of_replay_sees_through_supersession(store: &dyn MemoryStore) {
    // Replay distinguishes instants at the store's millisecond
    // granularity, so the three writes need distinct stamps.
    let gap = std::time::Duration::from_millis(2);
    let old = store
        .put(MemoryDraft::new(Scope::Global, "replay old fact"))
        .unwrap();
    std::thread::sleep(gap);
    let new = store
        .put(MemoryDraft::new(Scope::Global, "replay new fact"))
        .unwrap();
    std::thread::sleep(gap);
    let marked = store.supersede(&old.id, &new.id).unwrap();
    let retired_at = match marked.status {
        Status::Superseded { at, .. } => at,
        ref other => panic!("expected superseded, got {other:?}"),
    };

    let bodies = |at: u64| -> Vec<String> {
        store
            .search(MemoryQuery::new("replay").as_of(at))
            .unwrap()
            .into_iter()
            .map(|record| record.body)
            .collect()
    };
    let before = bodies(old.created_at - 1);
    assert!(before.is_empty(), "nothing existed yet: {before:?}");
    let between = bodies(retired_at - 1);
    assert!(
        between.contains(&"replay old fact".to_owned()),
        "the old fact was still believed: {between:?}"
    );
    let after = bodies(retired_at + 1);
    assert_eq!(
        after,
        vec!["replay new fact".to_owned()],
        "only the replacement answers once superseded"
    );
}

/// Superseding keeps the record, marks what replaced it, and hides it
/// from plain search while it stays reachable; a dead record never
/// outranks a live one on a widened query.
pub fn supersede_keeps_the_record_and_hides_it(store: &dyn MemoryStore) {
    let old = store
        .put(MemoryDraft::new(Scope::Global, "supersede old").with_tags(["contract-supersede"]))
        .unwrap();
    let new = store
        .put(MemoryDraft::new(Scope::Global, "supersede new").with_tags(["contract-supersede"]))
        .unwrap();
    let marked = store.supersede(&old.id, &new.id).unwrap();
    assert!(matches!(
        marked.status,
        Status::Superseded { ref by, .. } if by == &new.id
    ));
    assert_eq!(marked.body, "supersede old");
    assert_eq!(
        store.get(&old.id).unwrap().unwrap().body,
        "supersede old",
        "the body survives: what was believed stays readable"
    );

    let live: Vec<String> = store
        .search(MemoryQuery::new("supersede"))
        .unwrap()
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(
        live,
        vec![new.id.clone()],
        "the dead record stops answering"
    );

    let widened = store
        .search(MemoryQuery::new("supersede").include_superseded())
        .unwrap();
    let ids: Vec<String> = widened.into_iter().map(|record| record.id).collect();
    assert!(ids.contains(&new.id) && ids.contains(&old.id), "{ids:?}");
    assert!(
        matches!(
            store.supersede("19700101T000000-00000009", &new.id),
            Err(MemoryError::UnknownId { .. })
        ),
        "superseding an absent record reports it"
    );
}

/// On a widened search a live record outranks a dead one with the same
/// keyword hits. Order-sensitive, so — like
/// [`rank_orders_terms_recency_and_tags`] — it is *not* part of
/// [`run_all`]: a store with a different ranking method cannot replay it.
pub fn superseded_records_never_outrank_active(store: &dyn MemoryStore) {
    let old = store
        .put(MemoryDraft::new(Scope::Global, "outrank old"))
        .unwrap();
    let new = store
        .put(MemoryDraft::new(Scope::Global, "outrank new"))
        .unwrap();
    store.supersede(&old.id, &new.id).unwrap();
    let found: Vec<String> = store
        .search(MemoryQuery::new("outrank").include_superseded())
        .unwrap()
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(
        found.first(),
        Some(&new.id),
        "a keyword hit on a dead fact must not bury the live one"
    );
}

/// Retracting keeps the record and the reason: a human's no outlives
/// the model's yes.
pub fn retract_records_the_reason(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "retract me"))
        .unwrap();
    let retracted = store.retract(&record.id, "the user says so").unwrap();
    assert!(matches!(
        retracted.status,
        Status::Retracted { ref why, at } if why == "the user says so" && at > 0
    ));
    assert!(retracted.status.is_dead());
    assert!(
        store
            .search(MemoryQuery::new("retract me"))
            .unwrap()
            .is_empty(),
        "the retracted record stops answering"
    );
    let kept = store
        .search(MemoryQuery::new("retract me").include_superseded())
        .unwrap();
    assert_eq!(kept.len(), 1, "kept, not deleted");
    assert!(matches!(
        store.retract("19700101T000000-00000009", "why"),
        Err(MemoryError::UnknownId { .. })
    ));
}

/// Verification moves only `verified_at` — the belief is re-confirmed,
/// not rewritten.
pub fn verify_sets_verified_at_only(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "verify me"))
        .unwrap();
    let verified = store.verify(&record.id).unwrap();
    assert!(verified.verified_at.unwrap_or(0) >= verified.created_at);
    assert_eq!(verified.body, record.body);
    assert_eq!(verified.created_at, record.created_at);
    assert_eq!(verified.updated_at, record.updated_at);
    assert!(matches!(
        store.verify("19700101T000000-00000009"),
        Err(MemoryError::UnknownId { .. })
    ));
}

/// A body edit voids the verification (nobody checked the new wording);
/// a retag does not. An edit never resurrects a dead record.
pub fn updates_void_verification_only_when_the_body_changes(store: &dyn MemoryStore) {
    let record = store
        .put(MemoryDraft::new(Scope::Global, "voiding update"))
        .unwrap();
    store.verify(&record.id).unwrap();

    let retagged = store
        .put(
            MemoryDraft::new(Scope::Global, "voiding update")
                .with_tags(["retagged"])
                .with_id(record.id.clone()),
        )
        .unwrap();
    assert!(
        retagged.verified_at.is_some(),
        "same body: the verification holds"
    );

    let rewritten = store
        .put(MemoryDraft::new(Scope::Global, "voiding update, reworded").with_id(record.id.clone()))
        .unwrap();
    assert!(
        rewritten.verified_at.is_none(),
        "new wording, nobody has checked it"
    );
    assert!(
        rewritten.valid_from >= rewritten.created_at,
        "the new wording holds from when it was written"
    );

    store.retract(&record.id, "wrong").unwrap();
    let edited = store
        .put(MemoryDraft::new(Scope::Global, "voiding update, edited").with_id(record.id.clone()))
        .unwrap();
    assert!(
        edited.status.is_dead(),
        "an edit does not resurrect a retracted record"
    );
}

/// The sweep reports stale and horizon-past records, most overdue first,
/// and never reports a dead one.
pub fn due_for_review_reports_stale_and_expired(store: &dyn MemoryStore) {
    let day = 86_400_000;
    let fresh = store
        .put(MemoryDraft::new(Scope::Global, "sweep fresh record"))
        .unwrap();
    let volatile = store
        .put(
            MemoryDraft::new(Scope::Global, "sweep volatile record")
                .with_tags(["infrastructure"])
                .with_verify_hint(VerifyHint::Manual),
        )
        .unwrap();
    // Probed after the writes, so "now" is at or after every creation.
    let now = now_of(store);
    store
        .put(
            MemoryDraft::new(Scope::Global, "sweep horizon record")
                .with_decay(Decay::Permanent)
                .with_valid_until(now + day),
        )
        .unwrap();

    // The suite shares one store, so earlier functions' records may be
    // due too; the assertions below stay on this function's markers.
    let items = store.due_for_review(now).unwrap();
    assert!(
        items
            .iter()
            .all(|item| item.record.id != fresh.id && item.record.id != volatile.id),
        "a fresh record and a 30-day record are not due right after writing"
    );

    let items = store.due_for_review(now + 31 * day).unwrap();
    let position = |body: &str| {
        items
            .iter()
            .position(|item| item.record.body == body)
            .unwrap()
    };
    assert!(
        position("sweep horizon record") < position("sweep volatile record"),
        "most overdue first: the horizon lapsed a day after writing, the \
         30-day decay only at day 30"
    );
    assert!(
        !items.iter().any(|item| item.record.id == fresh.id),
        "the fresh record waits"
    );
    let volatile_item = items
        .iter()
        .find(|item| item.record.body == "sweep volatile record")
        .unwrap();
    assert_eq!(volatile_item.verify_hint, Some(VerifyHint::Manual));

    // Supersession takes a record out of the sweep.
    store.supersede(&volatile.id, &fresh.id).unwrap();
    let items = store.due_for_review(now + 31 * day).unwrap();
    assert!(
        !items.iter().any(|item| item.record.id == volatile.id),
        "the superseded record leaves the sweep"
    );
    assert!(
        items
            .iter()
            .any(|item| item.record.body == "sweep horizon record"),
        "the horizon stays due"
    );
}

/// Runs the order-sensitive rank assertions against `store` — the
/// complement to [`run_all`] for rankers that honor the documented
/// ordering (tag hits over body hits, recency ties by id, live over
/// dead).
pub fn run_rank_orders(store: &dyn MemoryStore) {
    rank_orders_terms_recency_and_tags(store);
    superseded_records_never_outrank_active(store);
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
    put_carries_validity_provenance_and_defaults(store);
    search_applies_the_validity_rules(store);
    as_of_replay_sees_through_supersession(store);
    supersede_keeps_the_record_and_hides_it(store);
    retract_records_the_reason(store);
    verify_sets_verified_at_only(store);
    updates_void_verification_only_when_the_body_changes(store);
    due_for_review_reports_stale_and_expired(store);
}
/// The store's wall clock, as far as the contract's relative-time tests
/// need it: the same vocabulary the stores stamp records with.
fn now_of(store: &dyn MemoryStore) -> u64 {
    let probe = store
        .put(MemoryDraft::new(Scope::Global, "contract clock probe"))
        .unwrap();
    probe.created_at
}
