//! The consent round-trip: how a running turn asks the human for
//! something only a human can give — a credential to store, permission
//! to act — and waits, inside the tool call, for the answer.
//!
//! The shape is one table plus one bridge. [`ConsentStore`] keeps the
//! request rows: what was asked, of which session, and how it
//! resolved — never the secret itself, which travels once, from the
//! answering client through the HTTP route into the identity store,
//! and is gone. [`WaitBridge`] is how the HTTP route wakes the tool
//! that awaits its row's resolution; the tool re-reads the row around
//! every wake, so a resolution that raced the waiter's registration is
//! caught by the store, not lost.
//!
//! Unanswered requests expire by age: [`CONSENT_WAIT`] bounds both the
//! tool's patience and the `pending` list a UI polls, so a daemon that
//! restarted under a waiting tool (the waiter is gone) stops offering
//! a stale card. The tool answers a denial or an expiry in-band — a
//! turn that asked and was refused is a turn that can say so.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::hub::{EventHub, TurnEvent};
use rhema_rsd::ResourceKey;

/// How long a consent request stays answerable — the tool's patience
/// and the pending list's horizon, one number so neither lies about
/// the other.
pub const CONSENT_WAIT: Duration = Duration::from_secs(15 * 60);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// What kind of answer a request waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentKind {
    /// A credential: the human pastes a secret, it lands in the
    /// identity store, the tool learns only the account id.
    Secret,
    /// Permission: the human approves or denies an action.
    Phone,
}

impl ConsentKind {
    /// The wire spelling, in consent rows and on the SSE stream.
    pub fn as_str(self) -> &'static str {
        match self {
            ConsentKind::Secret => "secret",
            ConsentKind::Phone => "phone",
        }
    }

    /// Parses the stored spelling; `None` for anything else.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "secret" => Some(ConsentKind::Secret),
            "phone" => Some(ConsentKind::Phone),
            _ => None,
        }
    }
}

/// One consent request: asked, and maybe answered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Consent {
    /// The request's id (`con-…`).
    pub id: String,
    /// What kind of answer it waits for.
    pub kind: ConsentKind,
    /// The one-line ask, as the UI shows it.
    pub summary: String,
    /// The ask's context — the model's own reason.
    pub detail: String,
    /// The session that asked.
    pub session_key: String,
    /// For a secret request: the account fields to store the answer
    /// under (label, service, url, username, secret_kind) — the draft
    /// without its secret. `None` for permission requests.
    pub draft: Option<Value>,
    /// When the request was made, epoch millis.
    pub created_at: u64,
    /// When it resolved, epoch millis, if it did.
    pub resolved_at: Option<u64>,
    /// Whether it was granted, if it resolved.
    pub granted: Option<bool>,
    /// For a resolved secret request: the account the secret landed in.
    pub account_id: Option<String>,
}

/// Why a consent-store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConsentStoreError {
    /// The backend failed to read or write.
    #[error("consent store failed: {0}")]
    Backend(String),
}

/// The consent table over the claw's shared database.
pub struct ConsentStore {
    conn: Mutex<rusqlite::Connection>,
}

impl ConsentStore {
    /// Opens (creating if needed) the consent table on the database at
    /// `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, ConsentStoreError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|err| ConsentStoreError::Backend(err.to_string()))?;
        Self::init(conn)
    }

    /// Opens the consent table over an in-memory database.
    pub fn in_memory() -> Result<Self, ConsentStoreError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|err| ConsentStoreError::Backend(err.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, ConsentStoreError> {
        let conn = conn;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| ConsentStoreError::Backend(err.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS consents (
                id          TEXT PRIMARY KEY,
                kind        TEXT NOT NULL,
                summary     TEXT NOT NULL,
                detail      TEXT NOT NULL,
                session_key TEXT NOT NULL,
                draft       TEXT,
                created_at  INTEGER NOT NULL,
                resolved_at INTEGER,
                granted     INTEGER,
                account_id  TEXT
            );",
        )
        .map_err(|err| ConsentStoreError::Backend(err.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Records a request, durable before it returns. The id is unique
    /// per request (a wall-clock millisecond plus a process counter).
    pub fn create(
        &self,
        kind: ConsentKind,
        summary: &str,
        detail: &str,
        session_key: &str,
        draft: Option<Value>,
    ) -> Result<Consent, ConsentStoreError> {
        static CONSENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = now_millis();
        let id = format!(
            "con-{}-{:04x}",
            now,
            CONSENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO consents (id, kind, summary, detail, session_key, draft, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                kind.as_str(),
                summary,
                detail,
                session_key,
                draft.as_ref().map(Value::to_string),
                now as i64,
            ],
        )
        .map_err(|err| ConsentStoreError::Backend(err.to_string()))?;
        Ok(Consent {
            id,
            kind,
            summary: summary.to_owned(),
            detail: detail.to_owned(),
            session_key: session_key.to_owned(),
            draft,
            created_at: now,
            resolved_at: None,
            granted: None,
            account_id: None,
        })
    }

    /// The request with this id, when it exists.
    pub fn get(&self, id: &str) -> Result<Option<Consent>, ConsentStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, summary, detail, session_key, draft, created_at,
                        resolved_at, granted, account_id
                 FROM consents WHERE id = ?1",
            )
            .map_err(backend)?;
        let mut rows = stmt.query_map(params![id], row_consent).map_err(backend)?;
        rows.next().transpose().map_err(backend)
    }

    /// Every unanswered, unexpired request, oldest first — the list a
    /// UI polls after a reload, so a card is never lost to one.
    pub fn pending(&self) -> Result<Vec<Consent>, ConsentStoreError> {
        let horizon = (now_millis().saturating_sub(CONSENT_WAIT.as_millis() as u64)) as i64;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, summary, detail, session_key, draft, created_at,
                        resolved_at, granted, account_id
                 FROM consents
                 WHERE resolved_at IS NULL AND created_at >= ?1
                 ORDER BY created_at, id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![horizon], row_consent)
            .map_err(backend)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(backend)
    }

    /// Marks a secret request resolved and names the account the secret
    /// landed in. `false` when the id is unknown or already resolved —
    /// the answer is consumed exactly once.
    pub fn resolve_secret(&self, id: &str, account_id: &str) -> Result<bool, ConsentStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let updated = conn
            .execute(
                "UPDATE consents SET resolved_at = ?2, granted = 1, account_id = ?3
                 WHERE id = ?1 AND resolved_at IS NULL",
                params![id, now_millis() as i64, account_id],
            )
            .map_err(backend)?;
        Ok(updated > 0)
    }

    /// Marks a permission request granted or denied. `false` when the id
    /// is unknown or already resolved.
    pub fn resolve_decision(&self, id: &str, granted: bool) -> Result<bool, ConsentStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let updated = conn
            .execute(
                "UPDATE consents SET resolved_at = ?2, granted = ?3
                 WHERE id = ?1 AND resolved_at IS NULL",
                params![id, now_millis() as i64, granted],
            )
            .map_err(backend)?;
        Ok(updated > 0)
    }
}

fn backend(err: rusqlite::Error) -> ConsentStoreError {
    ConsentStoreError::Backend(err.to_string())
}

fn row_consent(row: &rusqlite::Row<'_>) -> rusqlite::Result<Consent> {
    let kind: String = row.get(1)?;
    Ok(Consent {
        id: row.get(0)?,
        kind: ConsentKind::parse(&kind).unwrap_or(ConsentKind::Secret),
        summary: row.get(2)?,
        detail: row.get(3)?,
        session_key: row.get(4)?,
        draft: row
            .get::<_, Option<String>>(5)?
            .map(|json| serde_json::from_str(&json).unwrap_or(Value::Null)),
        created_at: row.get::<_, i64>(6)?.max(0) as u64,
        resolved_at: row
            .get::<_, Option<i64>>(7)?
            .map(|millis| millis.max(0) as u64),
        granted: row.get::<_, Option<i64>>(8)?.map(|flag| flag != 0),
        account_id: row.get(9)?,
    })
}

/// Wakes tools waiting on a row's resolution. A wake for an id nobody
/// waits on yet is not an error and not lost: the waiter re-reads its
/// row around every wait, so the store, not the signal, is the truth.
#[derive(Default)]
pub struct WaitBridge {
    waiters: Mutex<HashMap<String, Arc<Notify>>>,
}

impl WaitBridge {
    /// An empty bridge.
    pub fn new() -> Self {
        Self::default()
    }

    /// The notify handle for `id`, creating it on first use. A waiter
    /// holds this handle across its wait; the resolver and the phone
    /// command wait both wait through it.
    pub(crate) fn waiter(&self, id: &str) -> Arc<Notify> {
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    /// Wakes whoever waits on `id`, if anyone.
    pub fn wake(&self, id: &str) {
        let waiter = self
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned();
        if let Some(waiter) = waiter {
            waiter.notify_one();
        }
    }
}

/// What a wait came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Resolved in favor: for a secret request, the account id the
    /// answer was stored under.
    Granted(Option<String>),
    /// Resolved against.
    Denied,
    /// [`CONSENT_WAIT`] elapsed with no answer.
    TimedOut,
}

/// Waits until `id`'s row resolves or the budget runs out. The row —
/// not the wake — is what the answer reads, so a resolution that raced
/// the waiter is still seen.
pub async fn wait_for_resolution(
    store: &ConsentStore,
    bridge: &WaitBridge,
    id: &str,
    budget: Duration,
) -> WaitOutcome {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(Some(consent)) = store.get(id) {
            if let Some(granted) = consent.granted {
                return if granted {
                    WaitOutcome::Granted(consent.account_id)
                } else {
                    WaitOutcome::Denied
                };
            }
        }
        let waiter = bridge.waiter(id);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, waiter.notified()).await {
            Ok(_) => continue,
            Err(_) => return WaitOutcome::TimedOut,
        }
    }
}

/// The tool's name on the wire.
pub const ASK_SECRET: &str = "ask_secret";

/// `ask_secret`: store an account through the human. The tool asks, the
/// chat surface shows a card, the human pastes the secret, the HTTP
/// route stores it into the identity store, and the tool wakes with the
/// account id — never the secret, which nothing here re-renders.
struct AskSecret {
    consents: Arc<ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
    key: ResourceKey,
    /// The tool's patience; [`CONSENT_WAIT`] in production, a sliver in
    /// tests.
    wait: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskSecretArgs {
    service: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    secret_kind: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[async_trait]
impl Tool for AskSecret {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            ASK_SECRET,
            "Ask the human for a credential and store it in the agent \
             accounts: a card appears in their chat where they paste the \
             secret. Use it when a task needs an account (a forge token, \
             an API key) that identity_list does not show yet — never ask \
             for something already stored. The answer carries no secret, \
             only whether it was stored and the account's id. The human \
             may decline, or answer never; both come back in-band. Act \
             with the stored account afterwards through identity_exec.",
            json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string",
                        "description": "The service the credential is for, \
                            e.g. 'github' or an internal tool's name." },
                    "label": { "type": "string",
                            "description": "Display name for the account; \
                                defaults to the service name." },
                    "username": { "type": "string",
                        "description": "The login name on the service." },
                    "url": { "type": "string",
                        "description": "Base URL, for self-hosted instances." },
                    "secret_kind": { "type": "string",
                        "enum": ["token", "password"],
                        "description": "What the secret is; default token." },
                    "reason": { "type": "string",
                        "description": "Why you need it — shown on the card." }
                },
                "required": ["service"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: AskSecretArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let service = args.service.trim();
        if service.is_empty() {
            return Err("service must not be empty".to_owned());
        }
        // Validate the kind spelling before the card exists: a typo'd
        // enum must fail the call, not the human's answer.
        let secret_kind = crate::accounts_tools::parse_kind(args.secret_kind.as_deref())?;
        let label = args
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(service)
            .to_owned();
        let summary = match args
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            Some(username) => format!("secret for {service} ({username})"),
            None => format!("secret for {service}"),
        };
        let detail = args
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("no reason given")
            .to_owned();
        let draft = json!({
            "label": label,
            "service": service,
            "url": args.url,
            "username": args.username,
            "secret_kind": match secret_kind {
                rhema_identity::SecretKind::Token => "token",
                rhema_identity::SecretKind::Password => "password",
            },
        });
        let consent = self
            .consents
            .create(
                ConsentKind::Secret,
                &summary,
                &detail,
                self.key.as_str(),
                Some(draft),
            )
            .map_err(|err| err.to_string())?;
        self.hub.emit(
            &self.key,
            TurnEvent::ConsentRequest {
                id: consent.id.clone(),
                kind: consent.kind.as_str().to_owned(),
                summary: consent.summary.clone(),
                detail: consent.detail.clone(),
                created_at: consent.created_at,
            },
        );
        match wait_for_resolution(&self.consents, &self.bridge, &consent.id, self.wait).await {
            WaitOutcome::Granted(account_id) => Ok(json!({
                "stored": true,
                "account_id": account_id,
                "service": service,
            })
            .to_string()),
            WaitOutcome::Denied => Ok(json!({"granted": false}).to_string()),
            WaitOutcome::TimedOut => Ok(json!({"timed_out": true}).to_string()),
        }
    }
}

/// The consent tool set over `store`: `ask_secret`. Built per dispatch,
/// so the request names the session that asked.
pub fn registry(
    consents: Arc<ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
    key: ResourceKey,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(AskSecret {
        consents,
        bridge,
        hub,
        key,
        wait: CONSENT_WAIT,
    }));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<ConsentStore> {
        Arc::new(ConsentStore::in_memory().expect("store opens"))
    }

    #[test]
    fn a_created_request_round_trips_and_starts_pending() {
        let store = store();
        let consent = store
            .create(
                ConsentKind::Secret,
                "secret for github",
                "to post",
                "interactive:web",
                Some(json!({"label": "github"})),
            )
            .expect("creates");
        assert_eq!(consent.kind, ConsentKind::Secret);
        assert_eq!(consent.granted, None);
        assert_eq!(consent.session_key, "interactive:web");
        let fetched = store.get(&consent.id).expect("reads").expect("exists");
        assert_eq!(fetched, consent);
        assert_eq!(store.pending().expect("reads"), vec![consent]);
    }

    #[test]
    fn a_secret_resolution_names_the_account_exactly_once() {
        let store = store();
        let consent = store
            .create(ConsentKind::Secret, "s", "d", "interactive:web", None)
            .expect("creates");
        assert!(
            store
                .resolve_secret(&consent.id, "acc-1")
                .expect("resolves"),
            "the first resolution lands"
        );
        assert!(
            !store
                .resolve_secret(&consent.id, "acc-2")
                .expect("resolves"),
            "the answer is consumed once"
        );
        let resolved = store.get(&consent.id).expect("reads").expect("exists");
        assert_eq!(resolved.account_id.as_deref(), Some("acc-1"));
        assert_eq!(resolved.granted, Some(true));
        assert!(
            store.pending().expect("reads").is_empty(),
            "resolved rows leave the pending list"
        );
    }

    #[test]
    fn a_decision_resolves_once_with_its_verdict() {
        let store = store();
        let consent = store
            .create(
                ConsentKind::Phone,
                "phone dnd_on",
                "quiet",
                "interactive:web",
                None,
            )
            .expect("creates");
        assert!(
            store
                .resolve_decision(&consent.id, false)
                .expect("resolves"),
            "a denial lands"
        );
        let resolved = store.get(&consent.id).expect("reads").expect("exists");
        assert_eq!(resolved.granted, Some(false));
        assert_eq!(resolved.account_id, None);
        assert!(
            !store.resolve_decision(&consent.id, true).expect("resolves"),
            "a denial cannot be flipped"
        );
    }

    #[test]
    fn an_expired_request_leaves_the_pending_list() {
        let store = store();
        {
            let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
            conn.execute(
                "INSERT INTO consents (id, kind, summary, detail, session_key, created_at)
                 VALUES ('con-old', 'secret', 's', 'd', 'interactive:web', 1)",
                [],
            )
            .expect("writes raw");
        }
        let fresh = store
            .create(ConsentKind::Secret, "s", "d", "interactive:web", None)
            .expect("creates");
        let pending = store.pending().expect("reads");
        assert_eq!(pending, vec![fresh], "only the unexpired request answers");
        assert!(
            store.get("con-old").expect("reads").is_some(),
            "the expired row still reads"
        );
    }

    /// The full round-trip, tool-side: the ask creates the row and the
    /// stream event, the human's answer (the route's half: resolve,
    /// wake) ends the wait, and the answer names the account — the
    /// secret never passes through the tool.
    #[tokio::test]
    async fn an_ask_stored_through_a_consent_answers_with_the_account_id() {
        use crate::hub::EventHub;

        let store = store();
        let bridge = Arc::new(WaitBridge::new());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = AskSecret {
            consents: store.clone(),
            bridge: bridge.clone(),
            hub: hub.clone(),
            key: key.clone(),
            wait: std::time::Duration::from_secs(10),
        };
        let mut receiver = hub.subscribe(&key);
        let task = tokio::spawn(async move {
            tool.invoke(json!({
                "service": "github",
                "username": "octocat",
                "reason": "to file the issue you asked for",
            }))
            .await
        });
        // The ask lands: one pending row, one stream event.
        let pending = loop {
            let pending = store.pending().expect("reads");
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(pending[0].summary, "secret for github (octocat)");
        assert!(matches!(
            receiver.try_recv(),
            Ok(TurnEvent::ConsentRequest { .. })
        ));
        // The route's half: store the secret elsewhere, resolve, wake.
        store
            .resolve_secret(&pending[0].id, "acc-9")
            .expect("resolves");
        bridge.wake(&pending[0].id);
        let answer = task.await.expect("the tool returns").expect("in-band ok");
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap(),
            json!({"stored": true, "account_id": "acc-9", "service": "github"}),
            "{answer}"
        );
    }

    #[tokio::test]
    async fn a_denied_ask_answers_in_band_and_not_as_an_error() {
        let store = store();
        let bridge = Arc::new(WaitBridge::new());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = AskSecret {
            consents: store.clone(),
            bridge: bridge.clone(),
            hub,
            key,
            wait: std::time::Duration::from_secs(10),
        };
        let task = tokio::spawn(async move { tool.invoke(json!({"service": "gitlab"})).await });
        let pending = loop {
            let pending = store.pending().expect("reads");
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        store
            .resolve_decision(&pending[0].id, false)
            .expect("resolves");
        bridge.wake(&pending[0].id);
        let answer = task.await.expect("the tool returns").expect("in-band ok");
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap(),
            json!({"granted": false}),
            "{answer}"
        );
    }

    #[tokio::test]
    async fn an_unanswered_ask_times_out_in_band() {
        let store = store();
        let bridge = Arc::new(WaitBridge::new());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = AskSecret {
            consents: store,
            bridge,
            hub,
            key,
            wait: std::time::Duration::from_millis(50),
        };
        let answer = tool
            .invoke(json!({"service": "github"}))
            .await
            .expect("in-band ok");
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap(),
            json!({"timed_out": true}),
            "{answer}"
        );
    }
}
