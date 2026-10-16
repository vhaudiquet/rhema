//! The replayable [`IdentityStore`] behavioral suite.
//!
//! Every function here is a plain assertion-based test body taking any
//! `&dyn IdentityStore`; both bundled stores replay the exact same suite
//! from this crate's own tests, and a future backend (an OS keychain,
//! say) can do the same from its own. Each function's records carry
//! unique markers so they compose on one shared store; `run_all` runs
//! the set in order.

use crate::record::{AccountDraft, Secret, SecretKind, check_id};
use crate::store::{IdentityError, IdentityStore};

fn draft(label: &str, secret: &str) -> AccountDraft {
    AccountDraft::new(label, "github", Secret::new(SecretKind::Token, secret))
}

/// `put` creates; `get` returns the redacted account and `secret` the
/// credential, field-for-field, timestamps assigned.
pub fn put_then_get_round_trips(store: &dyn IdentityStore) {
    let account = store
        .put(draft("contract-rt", "tok-1").with_username("octocat"))
        .unwrap();
    assert_eq!(account.label, "contract-rt");
    assert_eq!(account.service, "github");
    assert_eq!(account.username.as_deref(), Some("octocat"));
    assert_eq!(account.secret_kind, SecretKind::Token);
    assert_eq!(account.created_at, account.updated_at);
    assert_eq!(store.get(&account.id).unwrap(), Some(account.clone()));
    let secret = store.secret(&account.id).unwrap().expect("stored");
    assert_eq!(secret.kind, SecretKind::Token);
    assert_eq!(secret.value, "tok-1");
}

/// `get` and `secret` on an id the store never issued are `None`, not an
/// error.
pub fn get_missing_returns_none(store: &dyn IdentityStore) {
    assert_eq!(store.get("19700101T000000-00000000").unwrap(), None);
    assert_eq!(store.secret("19700101T000000-00000000").unwrap(), None);
}

/// A malformed id is rejected before any lookup, on every entry point.
pub fn malformed_ids_are_rejected(store: &dyn IdentityStore) {
    for id in ["", "../escape", "has_underscore", "has space"] {
        assert!(
            matches!(store.get(id), Err(IdentityError::InvalidId { .. })),
            "get({id:?})"
        );
        assert!(
            matches!(store.forget(id), Err(IdentityError::InvalidId { .. })),
            "forget({id:?})"
        );
        assert!(
            matches!(store.secret(id), Err(IdentityError::InvalidId { .. })),
            "secret({id:?})"
        );
        assert!(
            matches!(
                store.set_secret(id, Secret::new(SecretKind::Token, "t")),
                Err(IdentityError::InvalidId { .. })
            ),
            "set_secret({id:?})"
        );
        assert!(
            matches!(
                store.put(draft("bad", "t").with_id(id)),
                Err(IdentityError::InvalidId { .. })
            ),
            "put({id:?})"
        );
    }
}

/// Creates get distinct, well-shaped, time-ordered ids.
pub fn put_create_assigns_distinct_ids(store: &dyn IdentityStore) {
    let mut ids = Vec::new();
    for label in ["distinct-1", "distinct-2", "distinct-3"] {
        ids.push(store.put(draft(label, "t")).unwrap().id);
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

/// An update keeps the id and `created_at`, swaps metadata and secret,
/// and `updated_at` advances.
pub fn put_update_preserves_created_at_and_swaps_secret(store: &dyn IdentityStore) {
    let created = store.put(draft("contract-update", "old-tok")).unwrap();
    let updated = store
        .put(
            AccountDraft::new(
                "contract-update-2",
                "gitea",
                Secret::new(SecretKind::Password, "new-tok"),
            )
            .with_id(created.id.clone()),
        )
        .unwrap();
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);
    assert_eq!(updated.label, "contract-update-2");
    assert_eq!(updated.service, "gitea");
    assert_eq!(updated.secret_kind, SecretKind::Password);
    assert_eq!(store.get(&created.id).unwrap(), Some(updated));
    let secret = store.secret(&created.id).unwrap().expect("stored");
    assert_eq!(secret.value, "new-tok");
}

/// An update naming an absent id is `UnknownId`, not a silent create.
pub fn put_update_unknown_id_errors(store: &dyn IdentityStore) {
    let err = store
        .put(draft("ghost", "t").with_id("19700101T000000-00000001"))
        .unwrap_err();
    assert!(
        matches!(err, IdentityError::UnknownId { ref id } if id == "19700101T000000-00000001"),
        "got {err:?}"
    );
}

/// `forget` removes once — secret included — then reports absence
/// idempotently.
pub fn forget_removes_then_reports_absent(store: &dyn IdentityStore) {
    let account = store.put(draft("contract-forget", "tok")).unwrap();
    assert!(store.forget(&account.id).unwrap());
    assert_eq!(store.get(&account.id).unwrap(), None);
    assert_eq!(store.secret(&account.id).unwrap(), None);
    assert!(!store.forget(&account.id).unwrap());
}

/// Empty secrets, oversized secrets, and empty labels/services are
/// rejected by every store, before the backend is touched.
pub fn limits_are_enforced(store: &dyn IdentityStore) {
    assert!(matches!(
        store.put(draft("contract-limits", "   ")),
        Err(IdentityError::EmptySecret)
    ));
    let long = Secret::new(SecretKind::Token, "x".repeat(crate::SECRET_LIMIT + 1));
    assert!(matches!(
        store.put(AccountDraft::new("contract-limits", "github", long)),
        Err(IdentityError::SecretTooLong { .. })
    ));
    assert!(matches!(
        store.put(AccountDraft::new(
            "  ",
            "github",
            Secret::new(SecretKind::Token, "t")
        )),
        Err(IdentityError::EmptyLabel)
    ));
    assert!(matches!(
        store.put(AccountDraft::new(
            "x",
            "  ",
            Secret::new(SecretKind::Token, "t")
        )),
        Err(IdentityError::EmptyService)
    ));
}

/// The stored service is the normalized form, whatever the draft said.
pub fn service_is_normalized(store: &dyn IdentityStore) {
    let account = store
        .put(AccountDraft::new(
            "contract-norm",
            "  GitLab ",
            Secret::new(SecretKind::Token, "t"),
        ))
        .unwrap();
    assert_eq!(account.service, "gitlab");
}

/// `set_secret` rotates the credential (kind included) without touching
/// metadata, and `updated_at` advances.
pub fn set_secret_rotates(store: &dyn IdentityStore) {
    let created = store.put(draft("contract-rotate", "old")).unwrap();
    let rotated = store
        .set_secret(&created.id, Secret::new(SecretKind::Password, "new-pass"))
        .unwrap();
    assert_eq!(rotated.id, created.id);
    assert_eq!(rotated.label, created.label);
    assert_eq!(rotated.created_at, created.created_at);
    assert!(rotated.updated_at >= created.updated_at);
    assert_eq!(rotated.secret_kind, SecretKind::Password);
    let secret = store.secret(&created.id).unwrap().expect("stored");
    assert_eq!(secret.kind, SecretKind::Password);
    assert_eq!(secret.value, "new-pass");
}

/// A rotation naming an absent id is `UnknownId`, not a create.
pub fn set_secret_unknown_id_errors(store: &dyn IdentityStore) {
    let err = store
        .set_secret(
            "19700101T000000-00000002",
            Secret::new(SecretKind::Token, "t"),
        )
        .unwrap_err();
    assert!(
        matches!(err, IdentityError::UnknownId { .. }),
        "got {err:?}"
    );
}

/// `list` returns every account, redacted — the two the function creates
/// are found by id, since the suite composes on one shared store.
pub fn list_returns_every_account(store: &dyn IdentityStore) {
    let first = store.put(draft("contract-list-1", "t")).unwrap();
    let second = store
        .put(AccountDraft::new(
            "contract-list-2",
            "smtp",
            Secret::new(SecretKind::Password, "p"),
        ))
        .unwrap();
    let listed = store.list().unwrap();
    let one = listed.iter().find(|a| a.id == first.id).expect("listed");
    let two = listed.iter().find(|a| a.id == second.id).expect("listed");
    assert_eq!(one.secret_kind, SecretKind::Token);
    assert_eq!(two.secret_kind, SecretKind::Password);
}

/// Runs the whole suite against `store`, which must be fresh.
pub fn run_all(store: &dyn IdentityStore) {
    put_then_get_round_trips(store);
    get_missing_returns_none(store);
    malformed_ids_are_rejected(store);
    put_create_assigns_distinct_ids(store);
    put_update_preserves_created_at_and_swaps_secret(store);
    put_update_unknown_id_errors(store);
    forget_removes_then_reports_absent(store);
    limits_are_enforced(store);
    service_is_normalized(store);
    set_secret_rotates(store);
    set_secret_unknown_id_errors(store);
    list_returns_every_account(store);
}
