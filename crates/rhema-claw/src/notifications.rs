//! The notification table: outbound claw-to-device messages in SQLite,
//! the same database file the dispatcher's
//! [`SqliteStore`](rhema_rsd::SqliteStore) and the watch table use.
//!
//! Notifications are how the resident reaches a device that is not the
//! live chat surface: the `notify` agent tool enqueues, a task or phone
//! turn's reply auto-notifies, and the device polls
//! `GET /v1/notifications` and acks what it delivered. Delivery is
//! at-least-once until the ack — an unacked row keeps answering the
//! poll, a crash between delivery and ack re-delivers.
//!
//! Two connections, one file: this store opens its own rusqlite
//! connection to the same database as the dispatcher's store — WAL and a
//! busy timeout, the pattern [`ClawStore`](crate::watch_store::ClawStore)
//! established — and the two interleave through SQLite's own locking.
//! Writes are single INSERT / UPDATE statements, so there is no
//! read-modify-write window to guard.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;

/// The device a notification reaches when its producer names none: the
/// tool's default, the auto-notify rule's fallback, and the poll
/// route's default — one phone, one user (spec §7).
pub const DEFAULT_DEVICE: &str = "phone";

/// The notification table.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notifications (
    id           INTEGER PRIMARY KEY,
    device       TEXT NOT NULL DEFAULT 'phone',
    title        TEXT NOT NULL,
    body         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    delivered_at INTEGER NULL
);
";

/// Why a notification-table operation failed.
#[derive(Debug, thiserror::Error)]
pub enum NotificationStoreError {
    /// The SQLite backend failed.
    #[error("notification store backend: {0}")]
    Backend(#[from] rusqlite::Error),
}

/// One outbound notification, as stored. Serializes whole: nothing here
/// is secret — this is the shape the poll route serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotificationRecord {
    /// The row id; the ack route addresses it, `since` filters by it.
    pub id: i64,
    /// The device it is bound for (`phone` unless told otherwise).
    pub device: String,
    /// The one-line headline.
    pub title: String,
    /// The message body.
    pub body: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
    /// When the device acked delivery; `None` while outstanding.
    pub delivered_at: Option<i64>,
}

/// The persistent notification table over its own connection to the
/// daemon's SQLite file. Shareable behind an `Arc`; every operation
/// takes `&self`.
pub struct NotificationStore {
    conn: Mutex<rusqlite::Connection>,
}

impl NotificationStore {
    /// Opens (creating if needed) the notification table in the database
    /// at `path` — the same file the dispatcher's store holds. WAL keeps
    /// the connections from blocking each other's readers; the busy
    /// timeout rides out the other's write locks.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, NotificationStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(NotificationStoreError::from)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory table: the shape without the file, for
    /// tests that need no sharing.
    pub fn in_memory() -> Result<Self, NotificationStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(NotificationStoreError::from)?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, NotificationStoreError> {
        if durable {
            // Same pragmas as the dispatcher's store on the same file:
            // SQLite stores the journal mode per database, and both
            // connections must agree that it is WAL.
            let _mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(NotificationStoreError::from)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(NotificationStoreError::from)?;
        }
        conn.execute_batch(SCHEMA)
            .map_err(NotificationStoreError::from)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Enqueues one notification and returns it with its row id. The
    /// row is outstanding (`delivered_at` unset) until
    /// [`Self::ack`].
    pub fn push(
        &self,
        device: &str,
        title: &str,
        body: &str,
    ) -> Result<NotificationRecord, NotificationStoreError> {
        let created_at = now_millis();
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO notifications (device, title, body, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![device, title, body, created_at],
        )?;
        Ok(NotificationRecord {
            id: conn.last_insert_rowid(),
            device: device.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
            created_at,
            delivered_at: None,
        })
    }

    /// The device's outstanding notifications with an id over `since`,
    /// oldest first — exactly what a device that acks what it delivers
    /// polls for. Delivered rows stop answering the poll; an unacked row
    /// keeps coming back (at-least-once until the ack).
    pub fn pending(
        &self,
        device: &str,
        since: i64,
    ) -> Result<Vec<NotificationRecord>, NotificationStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, device, title, body, created_at, delivered_at
                 FROM notifications
                 WHERE device = ?1 AND delivered_at IS NULL AND id > ?2
                 ORDER BY id ASC",
            )
            .map_err(NotificationStoreError::from)?;
        let rows = stmt
            .query_map(params![device, since], |row| {
                Ok(NotificationRecord {
                    id: row.get(0)?,
                    device: row.get(1)?,
                    title: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    delivered_at: row.get(5)?,
                })
            })
            .map_err(NotificationStoreError::from)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(NotificationStoreError::from)?;
        Ok(rows)
    }

    /// Marks the notification delivered. Idempotent: `false` when the id
    /// is unknown or already acked, never an error.
    pub fn ack(&self, id: i64) -> Result<bool, NotificationStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let acked = conn.execute(
            "UPDATE notifications SET delivered_at = ?2
             WHERE id = ?1 AND delivered_at IS NULL",
            params![id, now_millis()],
        )?;
        Ok(acked == 1)
    }
}

/// Milliseconds since the Unix epoch; stamps `created_at` and
/// `delivered_at`.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_polls_until_acked() {
        let store = NotificationStore::in_memory().expect("store opens");
        let first = store
            .push(DEFAULT_DEVICE, "claw", "first")
            .expect("push lands");
        let second = store
            .push(DEFAULT_DEVICE, "claw", "second")
            .expect("push lands");
        assert!(first.id < second.id, "ids ascend");
        assert_eq!(first.delivered_at, None, "outstanding until acked");

        // The poll answers outstanding rows oldest first.
        let pending = store.pending(DEFAULT_DEVICE, 0).expect("poll reads");
        assert_eq!(
            pending
                .iter()
                .map(|record| record.body.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"],
        );

        // `since` skips what the device already saw.
        assert_eq!(
            store
                .pending(DEFAULT_DEVICE, first.id)
                .expect("poll reads")
                .len(),
            1,
        );

        // Acking delivers; the delivered row stops answering, and a
        // re-ack is a quiet false, not an error.
        assert!(store.ack(first.id).expect("ack lands"));
        let pending = store.pending(DEFAULT_DEVICE, 0).expect("poll reads");
        assert_eq!(
            pending.iter().map(|record| record.id).collect::<Vec<_>>(),
            [second.id],
        );
        assert!(!store.ack(first.id).expect("re-ack is quiet"));
        assert!(!store.ack(10_000).expect("unknown id is quiet"));
    }

    #[test]
    fn devices_do_not_hear_each_other() {
        let store = NotificationStore::in_memory().expect("store opens");
        store.push(DEFAULT_DEVICE, "claw", "phone").expect("push");
        store.push("desk", "claw", "desk").expect("push");
        let phones = store.pending(DEFAULT_DEVICE, 0).expect("poll reads");
        assert_eq!(phones.len(), 1);
        assert_eq!(phones[0].device, DEFAULT_DEVICE);
        assert_eq!(phones[0].body, "phone");
        assert_eq!(store.pending("desk", 0).expect("poll reads").len(), 1);
        assert_eq!(store.pending("watch", 0).expect("poll reads").len(), 0);
    }

    #[test]
    fn two_connections_share_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        let writer = NotificationStore::open(&path).expect("first connection");
        let reader = NotificationStore::open(&path).expect("second connection");
        writer.push(DEFAULT_DEVICE, "claw", "hello").expect("push");
        let pending = reader.pending(DEFAULT_DEVICE, 0).expect("poll reads");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "hello");
    }
}
