//! The phone seam: what the claw knows about the user's phones, and the
//! consent-gated way it acts on them.
//!
//! The Android client reports its own state — battery, charging,
//! network, do-not-disturb, location, the foreground app — as
//! `phone:<device>:<topic>` events through `/v1/ingest`. Those events
//! are also conversation (the phone dispatches like any resident key),
//! but the conversation is the wrong place to answer "what's my
//! battery": a status question needs the latest observation, not a
//! transcript. [`PhoneStore`] keeps the latest payload per device and
//! topic, fed at the ingest door, plus a registry of the devices that
//! have ever reported ([`DeviceRecord`]).
//!
//! A device's identity lives for the life of one install: a reinstall,
//! a cleared app data, or a second phone each arrive under a fresh id.
//! The registry is what keeps the fleet honest — [`PhoneStore::devices`]
//! answers who reported, how it names itself (the `device` topic's
//! manufacturer and model), and when it was last heard — and
//! [`PhoneStore::forget_device`] retires an identity: its observations,
//! its queued commands, and its registry row go together. A forgotten
//! phone that still reports simply re-registers. Both surfaces —
//! `GET`/`DELETE /v1/phones` for the client's settings screen, the
//! `phone` tool's `devices`/`forget` actions for the agent — read and
//! write this one registry.
//!
//! Reports are observations, not conversation: a topic-shaped key
//! (`phone:<device>:<topic>`) lands in the tables and the ingest door
//! answers "observed" without dispatching a turn. The transition rules
//! bound the writes, but they cannot bound *value* — a decile drop
//! from 80 is not news, and each dispatched turn's reply auto-notifies
//! the phone, so per-flip turns would be a model call and a push
//! notification per flip. Anything the agent could do with a flip it
//! can do on demand: the `phone` tool reads the same snapshot at
//! decision time, and a scheduled task can check it and notify
//! deliberately. A bare `phone:<device>` key (no topic) is a message on
//! the device's resident key and dispatches like any resident key.
//!
//! Acting on the phone crosses a line reading never does, so it is
//! consent-gated end to end: a control action raises a consent
//! request ([`crate::consents`]), the human approves or denies it in
//! the chat surface, and only then does the action join a command
//! queue the device polls ([`Command`]). Forgetting a device is
//! consent-gated the same way, minus the queue: nothing is sent to the
//! device, its records are dropped. The phone executes on its own
//! clock — a WorkManager floor of fifteen minutes — so the tool's
//! answer may honestly be "queued, not yet confirmed".

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rhema_autonomous_agent::{ToolOp, ToolRegistry, async_trait};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::consents::{CONSENT_WAIT, ConsentKind, WaitBridge, wait_for_resolution};
use crate::hub::{EventHub, TurnEvent};
use rhema_rsd::ResourceKey;

/// The device a status or action addresses when the tool call names
/// none — the same default the notification queue and the auto-notify
/// rule use.
pub const DEFAULT_DEVICE: &str = crate::notifications::DEFAULT_DEVICE;

/// How long a queued command waits for the device's confirmation. The
/// phone polls commands on the WorkManager floor (fifteen minutes), so
/// this is the honest latency of the channel, not pessimism.
pub const COMMAND_WAIT: Duration = Duration::from_secs(20 * 60);

/// The control actions a consent gate stands in front of. Status reads
/// never appear here: observing the phone is what the tool is for;
/// acting on it is what needs asking.
pub const CONTROL_ACTIONS: [&str; 3] = ["dnd_on", "dnd_off", "alert"];

/// The source name the phone client's reports ingest under.
pub const SOURCE: &str = "phone";

/// The topic that names a device: the client sends it once per status
/// run (it changes with the app version, so an upgrade re-sends it) and
/// its payload is where [`device_label`] reads the fleet's labels from.
pub const DEVICE_TOPIC: &str = "device";

/// Splits a `phone:<device>:<topic>` key into its device and topic;
/// `None` for anything else — the shape the Android client's status
/// reports use, and the only keys that feed [`PhoneStore::observe`].
pub fn phone_key_parts(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("phone:")?;
    let (device, topic) = rest.split_once(':')?;
    (!device.is_empty() && !topic.is_empty()).then_some((device, topic))
}

/// The label a `device` payload names: `<manufacturer> <model>`, with
/// whichever part is missing simply left out. Anything else (a bare
/// "unknown", an empty payload) yields no label, and the fleet row
/// keeps whatever it had.
fn device_label(payload: &Value) -> Option<String> {
    let field = |name: &str| {
        payload[name]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("unknown"))
    };
    let manufacturer = field("manufacturer");
    let model = field("model");
    match (manufacturer, model) {
        (Some(manufacturer), Some(model)) if model.starts_with(manufacturer) => {
            Some(model.to_owned())
        }
        (Some(manufacturer), Some(model)) => Some(format!("{manufacturer} {model}")),
        (Some(_), None) => manufacturer.map(str::to_owned),
        (None, Some(model)) => Some(model.to_owned()),
        (None, None) => None,
    }
}

/// Why a phone-store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PhoneStoreError {
    /// The backend failed to read or write.
    #[error("phone store failed: {0}")]
    Backend(String),
}

fn backend(err: rusqlite::Error) -> PhoneStoreError {
    PhoneStoreError::Backend(err.to_string())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// One queued device command: what to do, and what the device said.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Command {
    /// The command's id (`cmd-…`), the result route's address.
    pub id: String,
    /// The device that should execute it.
    pub device: String,
    /// The action: one of [`CONTROL_ACTIONS`].
    pub action: String,
    /// Action arguments (an alert's title/body, say).
    pub args: Value,
    /// When the command was queued, epoch millis.
    pub created_at: u64,
    /// Whether execution succeeded, once the device reported.
    pub result_ok: Option<bool>,
    /// What the device said about it.
    pub result_detail: Option<String>,
}

/// One identity in the fleet: the client's persistent install id, the
/// name it reported for itself, and the two clocks — first and last
/// report. A raw UUID says nothing a human can match to a phone in a
/// drawer, so the label is what the settings screens lead with.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceRecord {
    /// The device id the client generated for this install.
    pub device: String,
    /// The self-reported name (`<manufacturer> <model>`), when the
    /// client sent a `device` topic.
    pub label: Option<String>,
    /// The first report, epoch millis.
    pub first_seen: u64,
    /// The latest report of any topic, epoch millis.
    pub last_seen: u64,
}

/// The phone tables over the claw's shared database: the latest
/// observation per device and topic, the command queue, and the device
/// registry.
pub struct PhoneStore {
    conn: Mutex<rusqlite::Connection>,
}

impl PhoneStore {
    /// Opens (creating if needed) the phone tables on the database at
    /// `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, PhoneStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(backend)?;
        Self::init(conn)
    }

    /// Opens the phone tables over an in-memory database.
    pub fn in_memory() -> Result<Self, PhoneStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(backend)?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, PhoneStoreError> {
        let conn = conn;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(backend)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS phone_state (
                device     TEXT NOT NULL,
                topic      TEXT NOT NULL,
                payload    TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (device, topic)
            );
            CREATE TABLE IF NOT EXISTS phone_commands (
                id            TEXT PRIMARY KEY,
                device        TEXT NOT NULL,
                action        TEXT NOT NULL,
                args          TEXT NOT NULL,
                created_at    INTEGER NOT NULL,
                result_ok     INTEGER,
                result_detail TEXT,
                result_at     INTEGER
            );
            CREATE TABLE IF NOT EXISTS phone_devices (
                device     TEXT PRIMARY KEY,
                label      TEXT,
                first_seen INTEGER NOT NULL,
                last_seen  INTEGER NOT NULL
            );",
        )
        .map_err(backend)?;
        // One-time migration for databases that predate the registry:
        // every identity still present in the state table becomes a
        // registry row. A later forget_device deletes the state rows
        // with the identity, so the seed cannot resurrect it.
        conn.execute_batch(
            "INSERT OR IGNORE INTO phone_devices (device, label, first_seen, last_seen)
             SELECT device, NULL, MIN(updated_at), MAX(updated_at)
             FROM phone_state GROUP BY device",
        )
        .map_err(backend)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Records the latest payload for one device topic — the ingest
    /// door's write, one upsert per report. The report also registers
    /// the device: first report opens the registry row, every report
    /// refreshes `last_seen`, and a `device` payload names the label.
    pub fn observe(
        &self,
        device: &str,
        topic: &str,
        payload: &Value,
    ) -> Result<(), PhoneStoreError> {
        let now = now_millis() as i64;
        let label = if topic == DEVICE_TOPIC {
            device_label(payload)
        } else {
            None
        };
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO phone_state (device, topic, payload, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device, topic) DO UPDATE SET payload = ?3, updated_at = ?4",
            params![device, topic, payload.to_string(), now],
        )
        .map_err(backend)?;
        conn.execute(
            "INSERT INTO phone_devices (device, label, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(device) DO UPDATE SET
                 last_seen = ?3,
                 label = COALESCE(?2, label)",
            params![device, label, now],
        )
        .map_err(backend)?;
        Ok(())
    }

    /// The latest observations for one device, topic name to payload —
    /// the status action's answer.
    pub fn snapshot(&self, device: &str) -> Result<Value, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT topic, payload, updated_at FROM phone_state WHERE device = ?1")
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![device], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(backend)?;
        let mut topics = serde_json::Map::new();
        for row in rows {
            let (topic, payload, updated_at) = row.map_err(backend)?;
            let mut payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            if let Some(object) = payload.as_object_mut() {
                object.insert("observed_at".into(), json!(updated_at.max(0) as u64));
            }
            topics.insert(topic, payload);
        }
        Ok(json!({ "device": device, "topics": topics }))
    }

    /// The fleet: every device that has ever reported, oldest first.
    pub fn devices(&self) -> Result<Vec<DeviceRecord>, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT device, label, first_seen, last_seen FROM phone_devices ORDER BY first_seen, device")
            .map_err(backend)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DeviceRecord {
                    device: row.get(0)?,
                    label: row.get(1)?,
                    first_seen: row.get::<_, i64>(2)?.max(0) as u64,
                    last_seen: row.get::<_, i64>(3)?.max(0) as u64,
                })
            })
            .map_err(backend)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(backend)
    }

    /// Retires a device: its observations, its queued commands, and its
    /// registry row go together. Answers whether the identity was
    /// known — `false` for an unknown name, which deleted nothing but
    /// is still a valid answer, never an error. A phone that reports
    /// after being forgotten re-registers itself.
    pub fn forget_device(&self, device: &str) -> Result<bool, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute("DELETE FROM phone_state WHERE device = ?1", params![device])
            .map_err(backend)?;
        conn.execute(
            "DELETE FROM phone_commands WHERE device = ?1",
            params![device],
        )
        .map_err(backend)?;
        let removed = conn
            .execute(
                "DELETE FROM phone_devices WHERE device = ?1",
                params![device],
            )
            .map_err(backend)?;
        Ok(removed > 0)
    }

    /// Queues a command for `device`, durable before it returns. The id
    /// is unique per command (a wall-clock millisecond plus a process
    /// counter).
    pub fn enqueue_command(
        &self,
        device: &str,
        action: &str,
        args: Value,
    ) -> Result<Command, PhoneStoreError> {
        static COMMAND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = now_millis();
        let id = format!(
            "cmd-{}-{:04x}",
            now,
            COMMAND_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO phone_commands (id, device, action, args, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, device, action, args.to_string(), now as i64],
        )
        .map_err(backend)?;
        Ok(Command {
            id,
            device: device.to_owned(),
            action: action.to_owned(),
            args,
            created_at: now,
            result_ok: None,
            result_detail: None,
        })
    }

    /// The commands a device has not confirmed yet, oldest first — the
    /// poll list the client executes from.
    pub fn pending_commands(&self, device: &str) -> Result<Vec<Command>, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, device, action, args, created_at, result_ok, result_detail
                 FROM phone_commands
                 WHERE device = ?1 AND result_at IS NULL
                 ORDER BY created_at, id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![device], row_command)
            .map_err(backend)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(backend)
    }

    /// Records a command's execution result. `false` when the id is
    /// unknown or already confirmed — a result is consumed once, and a
    /// redelivered report does not overwrite the first verdict.
    pub fn record_result(
        &self,
        id: &str,
        ok: bool,
        detail: Option<&str>,
    ) -> Result<bool, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let updated = conn
            .execute(
                "UPDATE phone_commands SET result_ok = ?2, result_detail = ?3, result_at = ?4
                 WHERE id = ?1 AND result_at IS NULL",
                params![id, ok, detail, now_millis() as i64],
            )
            .map_err(backend)?;
        Ok(updated > 0)
    }

    /// The command with this id, when it exists.
    pub fn get_command(&self, id: &str) -> Result<Option<Command>, PhoneStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, device, action, args, created_at, result_ok, result_detail
                 FROM phone_commands WHERE id = ?1",
            )
            .map_err(backend)?;
        let mut rows = stmt.query_map(params![id], row_command).map_err(backend)?;
        rows.next().transpose().map_err(backend)
    }
}

fn row_command(row: &rusqlite::Row<'_>) -> rusqlite::Result<Command> {
    Ok(Command {
        id: row.get(0)?,
        device: row.get(1)?,
        action: row.get(2)?,
        args: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(Value::Null),
        created_at: row.get::<_, i64>(4)?.max(0) as u64,
        result_ok: row.get::<_, Option<i64>>(5)?.map(|flag| flag != 0),
        result_detail: row.get(6)?,
    })
}

/// What a command wait came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The device reported.
    Confirmed {
        /// Whether execution succeeded.
        ok: bool,
        /// What the device said about it.
        detail: Option<String>,
    },
    /// [`COMMAND_WAIT`] elapsed without a report; the command stays
    /// queued and the device may still execute it.
    Unconfirmed,
}

/// Waits until a queued command confirms or the budget runs out — the
/// consent wait's shape over the command table.
pub async fn wait_for_command_result(
    store: &PhoneStore,
    bridge: &WaitBridge,
    id: &str,
    budget: Duration,
) -> CommandOutcome {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(Some(command)) = store.get_command(id) {
            if let Some(ok) = command.result_ok {
                return CommandOutcome::Confirmed {
                    ok,
                    detail: command.result_detail,
                };
            }
        }
        let waiter = bridge.waiter(id);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, waiter.notified()).await {
            Ok(_) => continue,
            Err(_) => return CommandOutcome::Unconfirmed,
        }
    }
}

/// The tool's name on the wire.
pub const PHONE: &str = "phone";

/// `phone`: read the device's reported state, or — through a consent
/// request the human answers in chat — act on it.
struct PhoneTool {
    store: Arc<PhoneStore>,
    consents: Arc<crate::consents::ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
    key: ResourceKey,
    /// The consent patience; [`CONSENT_WAIT`] in production, a sliver
    /// in tests.
    wait: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PhoneArgs {
    action: String,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    /// An alert's headline and body; the device renders what it gets.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[async_trait]
impl ToolOp for PhoneTool {
    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: PhoneArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let device = args
            .device
            .as_deref()
            .map(str::trim)
            .filter(|device| !device.is_empty())
            .unwrap_or(DEFAULT_DEVICE)
            .to_owned();
        let action = args.action.trim().to_owned();
        match action.as_str() {
            "status" => {
                let snapshot = self
                    .store
                    .snapshot(&device)
                    .map_err(|err| err.to_string())?;
                if snapshot["topics"]
                    .as_object()
                    .is_some_and(|topics| topics.is_empty())
                {
                    return Ok(json!({"observed": false, "device": device,
                        "note": "this device has never reported its status"})
                    .to_string());
                }
                Ok(json!({"observed": true, "snapshot": snapshot}).to_string())
            }
            "devices" => {
                let devices = self.store.devices().map_err(|err| err.to_string())?;
                Ok(json!({ "devices": devices }).to_string())
            }
            "forget" => self.forget(&device, args).await,
            action if CONTROL_ACTIONS.contains(&action) => {
                self.control(action, &device, args).await
            }
            other => Err(format!(
                "unknown action {other:?}: status, devices, forget, or one of {CONTROL_ACTIONS:?}"
            )),
        }
    }
}

impl PhoneTool {
    /// One control action, consent first, confirmation second. Both
    /// waits answer in-band: a refused action is a turn that can say so.
    async fn control(&self, action: &str, device: &str, args: PhoneArgs) -> Result<String, String> {
        match self
            .ask(
                &format!("phone {action} on {device}"),
                action,
                device,
                &args,
            )
            .await?
        {
            Ask::Granted => {}
            Ask::Refused(answer) => return Ok(answer),
        }
        let command_args = match action {
            "alert" => json!({
                "title": args.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("claw"),
                "body": args.body.as_deref().map(str::trim).filter(|b| !b.is_empty()).unwrap_or(action),
            }),
            _ => json!({}),
        };
        let command = self
            .store
            .enqueue_command(device, action, command_args)
            .map_err(|err| err.to_string())?;
        match wait_for_command_result(&self.store, &self.bridge, &command.id, COMMAND_WAIT).await {
            CommandOutcome::Confirmed { ok, detail } => Ok(json!({
                "granted": true,
                "done": ok,
                "detail": detail,
                "command_id": command.id,
            })
            .to_string()),
            CommandOutcome::Unconfirmed => Ok(json!({
                "granted": true,
                "done": false,
                "queued": true,
                "command_id": command.id,
                "note": "the device has not confirmed yet; it executes on its own poll",
            })
            .to_string()),
        }
    }

    /// Retires a device's records after the human approves. Nothing is
    /// sent to the device and nothing waits for it: a grant drops the
    /// identity, its observations, and its queued commands at once. The
    /// answer says whether the name was known, so the turn can tell a
    /// typo from a retirement.
    async fn forget(&self, device: &str, args: PhoneArgs) -> Result<String, String> {
        match self
            .ask(
                &format!("phone forget on {device}"),
                "forget",
                device,
                &args,
            )
            .await?
        {
            Ask::Granted => {}
            Ask::Refused(answer) => return Ok(answer),
        }
        let forgotten = self
            .store
            .forget_device(device)
            .map_err(|err| err.to_string())?;
        Ok(json!({"granted": true, "forgotten": forgotten}).to_string())
    }

    /// The consent round-trip both mutating actions share: raise the
    /// card, wait, and classify the human's answer. `Err` is the store
    /// failing, not a refusal — a refusal is an [`Ask::Refused`] with
    /// the in-band answer.
    async fn ask(
        &self,
        summary: &str,
        action: &str,
        device: &str,
        args: &PhoneArgs,
    ) -> Result<Ask, String> {
        let reason = args
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("no reason given");
        let detail = format!("{reason} (action: {action}, device: {device})");
        let consent = self
            .consents
            .create(
                ConsentKind::Phone,
                summary,
                &detail,
                self.key.as_str(),
                None,
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
        Ok(
            match wait_for_resolution(&self.consents, &self.bridge, &consent.id, self.wait).await {
                crate::consents::WaitOutcome::Granted(_) => Ask::Granted,
                crate::consents::WaitOutcome::Denied => {
                    Ask::Refused(json!({"granted": false}).to_string())
                }
                crate::consents::WaitOutcome::TimedOut => {
                    Ask::Refused(json!({"timed_out": true}).to_string())
                }
            },
        )
    }
}

/// What [`PhoneTool::ask`] came back with: a grant, or the in-band
/// answer a refusal or a silence turns into.
enum Ask {
    Granted,
    Refused(String),
}

/// The phone tool set over `store`: `phone`. Built per dispatch — a
/// consent ask must name the session that asked.
/// The compiled operation behind the manifest, by `run.builtin` id.
fn builtins(
    store: Arc<PhoneStore>,
    consents: Arc<crate::consents::ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
    key: ResourceKey,
) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([(
        "phone.call".to_owned(),
        Arc::new(PhoneTool {
            store,
            consents,
            bridge,
            hub,
            key,
            wait: CONSENT_WAIT,
        }) as Arc<dyn ToolOp>,
    )])
}

/// The phone tool's `TOOL.yaml`, embedded so the daemon ships it with
/// no filesystem layout.
pub const MANIFESTS: &[(&str, &str)] = &[("phone", include_str!("../tools/phone/TOOL.yaml"))];

/// The phone tool set over `store`: `phone` — its spec from the
/// embedded manifest, the operation compiled here.
pub fn registry(
    store: Arc<PhoneStore>,
    consents: Arc<crate::consents::ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
    key: ResourceKey,
) -> ToolRegistry {
    let (tools, skipped) = rhema_autonomous_agent::manifest::builtin_tools(
        MANIFESTS,
        &builtins(store, consents, bridge, hub, key),
    );
    assert!(
        skipped.is_empty(),
        "the phone tool manifest must load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_autonomous_agent::Allowlist;

    #[test]
    fn the_manifest_loads_against_its_builtin_and_serves_the_registry() {
        let resolver = registry(
            store(),
            Arc::new(crate::consents::ConsentStore::in_memory().expect("store opens")),
            Arc::new(WaitBridge::new()),
            Arc::new(EventHub::with_capacity(16)),
            ResourceKey::new("r"),
        )
        .resolver_for(&Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("phone"),
            rhema_core::ReplaySafety::Unsafe,
            "a resumed run must not re-queue a control or re-retire a device"
        );
    }

    fn store() -> Arc<PhoneStore> {
        Arc::new(PhoneStore::in_memory().expect("store opens"))
    }

    #[test]
    fn an_observed_topic_is_the_snapshot_until_replaced() {
        let store = store();
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "80"}),
            )
            .expect("observes");
        store
            .observe(
                "pixel",
                "network",
                &json!({"topic": "network", "value": "wifi"}),
            )
            .expect("observes");
        let snapshot = store.snapshot("pixel").expect("reads");
        assert_eq!(snapshot["topics"]["battery"]["value"], "80");
        assert_eq!(snapshot["topics"]["network"]["value"], "wifi");
        assert!(snapshot["topics"]["battery"]["observed_at"].is_u64());
        assert!(
            store.snapshot("other").expect("reads")["topics"]
                .as_object()
                .expect("a map")
                .is_empty()
        );
        // A newer report replaces, never appends.
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "70"}),
            )
            .expect("observes");
        assert_eq!(
            store.snapshot("pixel").expect("reads")["topics"]["battery"]["value"],
            "70"
        );
        let devices = store.devices().expect("reads");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, "pixel");
    }

    #[test]
    fn a_report_registers_the_device_and_its_label_rides_the_device_topic() {
        let store = store();
        store
            .observe(
                "pixel",
                "device",
                &json!({"topic": "device", "manufacturer": "Google", "model": "Pixel 9"}),
            )
            .expect("observes");
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "80"}),
            )
            .expect("observes");
        let devices = store.devices().expect("reads");
        assert_eq!(devices.len(), 1, "two topics, one device");
        assert_eq!(devices[0].label.as_deref(), Some("Google Pixel 9"));
        assert!(devices[0].first_seen <= devices[0].last_seen);

        // A second install is a second identity, oldest first.
        store
            .observe(
                "pixel-2",
                "device",
                &json!({"topic": "device", "model": "Pixel 8"}),
            )
            .expect("observes");
        let devices = store.devices().expect("reads");
        assert_eq!(
            devices
                .iter()
                .map(|d| d.device.as_str())
                .collect::<Vec<_>>(),
            vec!["pixel", "pixel-2"]
        );
        assert_eq!(devices[1].label.as_deref(), Some("Pixel 8"));

        // Other topics never clobber the label.
        store
            .observe(
                "pixel",
                "network",
                &json!({"topic": "network", "value": "wifi"}),
            )
            .expect("observes");
        assert_eq!(
            store.devices().expect("reads")[0].label.as_deref(),
            Some("Google Pixel 9")
        );
    }

    #[test]
    fn forgetting_a_device_takes_its_state_its_commands_and_its_identity() {
        let store = store();
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "10"}),
            )
            .expect("observes");
        store
            .enqueue_command("pixel", "dnd_on", json!({}))
            .expect("queues");
        store
            .observe(
                "pixel-2",
                "battery",
                &json!({"topic": "battery", "value": "90"}),
            )
            .expect("observes");

        assert!(
            !store.forget_device("stranger").expect("forgets"),
            "an unknown name forgets nothing"
        );
        assert!(store.forget_device("pixel").expect("forgets"));
        assert!(
            store.snapshot("pixel").expect("reads")["topics"]
                .as_object()
                .expect("a map")
                .is_empty()
        );
        assert!(store.pending_commands("pixel").expect("reads").is_empty());
        assert_eq!(
            store
                .devices()
                .expect("reads")
                .iter()
                .map(|d| d.device.as_str())
                .collect::<Vec<_>>(),
            vec!["pixel-2"]
        );

        // A forgotten phone that still reports re-registers.
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "9"}),
            )
            .expect("observes");
        assert_eq!(store.devices().expect("reads").len(), 2);
    }

    #[test]
    fn a_store_reopened_over_an_old_database_seeds_the_registry_from_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("phones.db");
        {
            let store = PhoneStore::open(&path).expect("opens");
            store
                .observe(
                    "legacy",
                    "battery",
                    &json!({"topic": "battery", "value": "5"}),
                )
                .expect("observes");
        }
        let store = PhoneStore::open(&path).expect("reopens");
        let devices = store.devices().expect("reads");
        assert_eq!(devices.len(), 1, "the migration backfills the registry");
        assert_eq!(devices[0].device, "legacy");
        assert_eq!(devices[0].label, None);
    }

    #[test]
    fn a_device_label_reads_the_payload_and_ignores_placeholder_parts() {
        assert_eq!(
            device_label(&json!({"manufacturer": "Google", "model": "Pixel 9"})).as_deref(),
            Some("Google Pixel 9")
        );
        assert_eq!(
            device_label(&json!({"manufacturer": "Google", "model": "Google Pixel 9"})).as_deref(),
            Some("Google Pixel 9"),
            "a model that repeats the manufacturer is not doubled"
        );
        assert_eq!(
            device_label(&json!({"model": "Pixel 9"})).as_deref(),
            Some("Pixel 9")
        );
        assert_eq!(
            device_label(&json!({"manufacturer": "unknown", "model": "Pixel 9"})).as_deref(),
            Some("Pixel 9")
        );
        assert_eq!(device_label(&json!({})), None);
    }

    #[test]
    fn a_command_confirms_exactly_once() {
        let store = store();
        let command = store
            .enqueue_command("pixel", "dnd_on", json!({}))
            .expect("queues");
        assert_eq!(
            store.pending_commands("pixel").expect("reads").len(),
            1,
            "unconfirmed commands stay on the poll list"
        );
        assert!(
            store
                .record_result(&command.id, true, Some("dnd set"))
                .expect("records"),
            "the first report lands"
        );
        assert!(
            !store
                .record_result(&command.id, false, Some("late"))
                .expect("records"),
            "a redelivery does not flip the verdict"
        );
        let confirmed = store
            .get_command(&command.id)
            .expect("reads")
            .expect("exists");
        assert_eq!(confirmed.result_ok, Some(true));
        assert_eq!(confirmed.result_detail.as_deref(), Some("dnd set"));
        assert!(store.pending_commands("pixel").expect("reads").is_empty());
    }

    /// The full control path, tool-side: consent first, then the queued
    /// command, then the device's confirmation — the tool answers with
    /// all three facts and never the human's absence of an answer.
    #[tokio::test]
    async fn a_control_action_is_consent_gated_then_confirmed() {
        let store = store();
        let consents = Arc::new(crate::consents::ConsentStore::in_memory().expect("opens"));
        let bridge = Arc::new(WaitBridge::new());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = PhoneTool {
            store: store.clone(),
            consents: consents.clone(),
            bridge: bridge.clone(),
            hub: hub.clone(),
            key: key.clone(),
            wait: std::time::Duration::from_secs(10),
        };
        let mut receiver = hub.subscribe(&key);
        let task = tokio::spawn(async move {
            tool.invoke(json!({
                "action": "dnd_on",
                "device": "pixel",
                "reason": "the movie starts",
            }))
            .await
        });

        // The consent card goes out; the human approves.
        let pending = loop {
            let pending = consents.pending().expect("reads");
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(pending[0].kind, ConsentKind::Phone);
        assert_eq!(pending[0].summary, "phone dnd_on on pixel");
        assert!(matches!(
            receiver.try_recv(),
            Ok(TurnEvent::ConsentRequest { .. })
        ));
        consents
            .resolve_decision(&pending[0].id, true)
            .expect("resolves");
        bridge.wake(&pending[0].id);

        // The command queues; the device confirms on its poll.
        let queued = loop {
            let queued = store.pending_commands("pixel").expect("reads");
            if !queued.is_empty() {
                break queued;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(queued[0].action, "dnd_on");
        store
            .record_result(&queued[0].id, true, None)
            .expect("records");
        bridge.wake(&queued[0].id);

        let answer = task.await.expect("the tool returns").expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("parses");
        assert_eq!(answer["granted"], true);
        assert_eq!(answer["done"], true);
    }

    #[tokio::test]
    async fn a_denied_control_never_queues_a_command() {
        let store = store();
        let consents = Arc::new(crate::consents::ConsentStore::in_memory().expect("opens"));
        let bridge = Arc::new(WaitBridge::new());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = PhoneTool {
            store: store.clone(),
            consents: consents.clone(),
            bridge: bridge.clone(),
            hub,
            key,
            wait: std::time::Duration::from_secs(10),
        };
        let task = tokio::spawn(async move {
            tool.invoke(json!({"action": "alert", "reason": "found the keys"}))
                .await
        });
        let pending = loop {
            let pending = consents.pending().expect("reads");
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        consents
            .resolve_decision(&pending[0].id, false)
            .expect("resolves");
        bridge.wake(&pending[0].id);
        let answer = task.await.expect("the tool returns").expect("in-band ok");
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap(),
            json!({"granted": false}),
            "{answer}"
        );
        assert!(
            store.pending_commands("phone").expect("reads").is_empty(),
            "nothing queues behind a denial"
        );
    }

    /// The full forget path: the consent card first, and only a grant
    /// drops the identity. The answer carries whether the name was
    /// known, so a typo reads as data, not as silence.
    #[tokio::test]
    async fn forgetting_is_consent_gated_then_drops_the_identity() {
        let store = store();
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "1"}),
            )
            .expect("observes");
        store
            .enqueue_command("pixel", "alert", json!({}))
            .expect("queues");
        let consents = Arc::new(crate::consents::ConsentStore::in_memory().expect("opens"));
        let bridge = Arc::new(WaitBridge::new());
        let tool = PhoneTool {
            store: store.clone(),
            consents: consents.clone(),
            bridge: bridge.clone(),
            hub: Arc::new(EventHub::default()),
            key: ResourceKey::from("interactive:web"),
            wait: std::time::Duration::from_secs(10),
        };
        let task = tokio::spawn(async move {
            tool.invoke(json!({
                "action": "forget",
                "device": "pixel",
                "reason": "the old install, superseded",
            }))
            .await
        });

        // Until the human answers, nothing is dropped.
        let pending = loop {
            let pending = consents.pending().expect("reads");
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(pending[0].summary, "phone forget on pixel");
        assert!(
            store.devices().expect("reads").len() == 1
                && !store.pending_commands("pixel").expect("reads").is_empty(),
            "a waiting consent has dropped nothing yet"
        );
        consents
            .resolve_decision(&pending[0].id, true)
            .expect("resolves");
        bridge.wake(&pending[0].id);

        let answer = task.await.expect("the tool returns").expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("parses");
        assert_eq!(answer["forgotten"], true, "{answer}");
        assert!(store.devices().expect("reads").is_empty());
        assert!(
            store.snapshot("pixel").expect("reads")["topics"]
                .as_object()
                .expect("a map")
                .is_empty()
        );
        assert!(store.pending_commands("pixel").expect("reads").is_empty());
    }

    #[tokio::test]
    async fn status_reads_without_asking() {
        let store = store();
        store
            .observe(
                "pixel",
                "battery",
                &json!({"topic": "battery", "value": "42", "charging": false}),
            )
            .expect("observes");
        let tool = PhoneTool {
            store,
            consents: Arc::new(crate::consents::ConsentStore::in_memory().expect("opens")),
            bridge: Arc::new(WaitBridge::new()),
            hub: Arc::new(EventHub::default()),
            key: ResourceKey::from("interactive:web"),
            wait: std::time::Duration::from_secs(1),
        };
        let answer = tool
            .invoke(json!({"action": "status", "device": "pixel"}))
            .await
            .expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("parses");
        assert_eq!(answer["observed"], true);
        assert_eq!(answer["snapshot"]["topics"]["battery"]["value"], "42");
        // No consent row: reading never asks.
        assert!(tool.consents.pending().expect("reads").is_empty());
    }
}
