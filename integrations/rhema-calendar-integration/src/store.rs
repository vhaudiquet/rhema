//! The calendar store: agent-managed calendars in SQLite, the same
//! database file the dispatcher's store uses — the pattern the claw's
//! watch table established. The config stopped carrying calendars:
//! the table is the one truth, and the source reads it fresh on every
//! detect pass, so a row added over `/v1/calendars` (or a chat tool)
//! is live without a restart. The same store owns the reminder
//! source's fed-slot ledger: the durable half of the reminder dedup,
//! so a slot does not re-fire once its turn has drained the inbox.

use crate::reminder::CalendarWatch;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// How long past its last possible due moment a fed slot's row stays
/// in the ledger. A slot only fires while its occurrence is still
/// future, so the prune rule reads `start + lead + this margin`, and
/// the margin covers a clock stepped backwards or a stalled pass
/// landing after the prune. The rows are tiny; the margin errs long.
const FED_SLOT_GRACE_SECS: i64 = 60 * 60;

/// Seconds in a minute, the unit the ledger's `lead` column carries.
const SECS_PER_MINUTE: i64 = 60;

/// The tables this store owns, created idempotently at open. The
/// dispatcher's store shares this database file, so these land beside
/// its tables, never over them.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS calendars (
    name       TEXT PRIMARY KEY,
    spec       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS fed_slots (
    calendar   TEXT NOT NULL,
    occurrence TEXT NOT NULL,
    lead       INTEGER NOT NULL,
    starts_at  INTEGER NOT NULL,
    fed_at     INTEGER NOT NULL,
    PRIMARY KEY (calendar, occurrence, lead)
);";

/// Runs the store's idempotent migration over one connection.
fn migrate(conn: &Connection) -> Result<(), std::io::Error> {
    conn.execute_batch(SCHEMA).map_err(std::io::Error::other)
}

/// An open handle on the calendar table. Its own rusqlite connection
/// to the shared database file — WAL and a busy timeout, so the
/// dispatcher's connection and this one interleave through SQLite's
/// own locking.
pub struct CalendarStore {
    conn: Mutex<Connection>,
}

impl CalendarStore {
    /// Opens (creating, if needed) the calendar table inside `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(std::io::Error::other)?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(std::io::Error::other)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// An in-memory store; tests and the static-list source's ledger
    /// use this.
    pub fn in_memory() -> Result<Self, std::io::Error> {
        let conn = Connection::open_in_memory().map_err(std::io::Error::other)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Adds (or refuses, when the name is taken) a calendar. The spec
    /// is validated first — the same door every writer shares, the
    /// route and a future chat tool alike — so an invalid calendar is
    /// loud wherever it enters.
    pub fn add(&self, calendar: &CalendarWatch) -> Result<bool, std::io::Error> {
        calendar.validate().map_err(std::io::Error::other)?;
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spec = serde_json::to_string(calendar).map_err(std::io::Error::other)?;
        let inserted = conn
            .execute(
                "INSERT INTO calendars (name, spec, created_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO NOTHING",
                rusqlite::params![
                    calendar.name.trim(),
                    spec,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                ],
            )
            .map_err(std::io::Error::other)?;
        Ok(inserted == 1)
    }

    /// Removes the named calendar; `false` when there was none.
    pub fn remove(&self, name: &str) -> Result<bool, std::io::Error> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = conn
            .execute(
                "DELETE FROM calendars WHERE name = ?1",
                rusqlite::params![name.trim()],
            )
            .map_err(std::io::Error::other)?;
        Ok(removed == 1)
    }

    /// Every calendar, oldest first. The source reads this fresh on
    /// every detect pass, so runtime adds take effect without a
    /// restart.
    pub fn list(&self) -> Result<Vec<CalendarWatch>, std::io::Error> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT spec FROM calendars ORDER BY created_at ASC, name ASC")
            .map_err(std::io::Error::other)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(std::io::Error::other)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(std::io::Error::other)?;
        rows.iter()
            .map(|spec| serde_json::from_str(spec).map_err(std::io::Error::other))
            .collect()
    }

    /// Whether this slot has already fed. The dispatcher dedups a fed
    /// id only while the slot sits in its inbox; this ledger is what
    /// keeps the next tick from re-ingesting a slot whose turn has
    /// drained.
    pub(crate) fn slot_is_fed(
        &self,
        calendar: &str,
        occurrence: &str,
        lead: u64,
    ) -> Result<bool, std::io::Error> {
        // rusqlite binds no unsigned ints; the column is INTEGER.
        let lead = i64::try_from(lead).map_err(std::io::Error::other)?;
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fed: Option<()> = conn
            .query_row(
                "SELECT 1 FROM fed_slots
                 WHERE calendar = ?1 AND occurrence = ?2 AND lead = ?3",
                rusqlite::params![calendar, occurrence, lead],
                |_| Ok(()),
            )
            .optional()
            .map_err(std::io::Error::other)?;
        Ok(fed.is_some())
    }

    /// Records a slot as fed; `false` when the ledger already held it.
    /// The dedup id stays `{calendar}:{occurrence}:{lead}` downstream;
    /// this table is the source's own memory of what it has fed.
    pub(crate) fn mark_slot_fed(
        &self,
        slot: &FedSlot,
        fed_at: i64,
    ) -> Result<bool, std::io::Error> {
        // rusqlite binds no unsigned ints; the column is INTEGER.
        let lead = i64::try_from(slot.lead).map_err(std::io::Error::other)?;
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inserted = conn
            .execute(
                "INSERT INTO fed_slots (calendar, occurrence, lead, starts_at, fed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(calendar, occurrence, lead) DO NOTHING",
                rusqlite::params![slot.calendar, slot.occurrence, lead, slot.starts_at, fed_at],
            )
            .map_err(std::io::Error::other)?;
        Ok(inserted == 1)
    }

    /// Deletes the ledger's spent rows: slots whose occurrence start,
    /// plus the lead, plus [`FED_SLOT_GRACE_SECS`] is already past
    /// `now`. Such a slot can never be due again — a started event is
    /// never fed — so pruning keeps the table bounded without ever
    /// resurrecting a reminder. Returns how many rows went.
    pub(crate) fn prune_fed_slots(&self, now: i64) -> Result<usize, std::io::Error> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "DELETE FROM fed_slots WHERE starts_at + lead * ?1 + ?2 < ?3",
            rusqlite::params![SECS_PER_MINUTE, FED_SLOT_GRACE_SECS, now],
        )
        .map_err(std::io::Error::other)
    }
}

/// One reminder slot the source has fed, as the ledger holds it: the
/// dedup key (calendar, occurrence, lead) plus the times the prune
/// rule needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FedSlot {
    /// The calendar's name, as the dedup id spells it.
    pub(crate) calendar: String,
    /// The occurrence's stable vocabulary id.
    pub(crate) occurrence: String,
    /// The slot's lead, in minutes.
    pub(crate) lead: u64,
    /// When the occurrence starts, epoch seconds: what the prune rule
    /// measures the slot's lifetime against.
    pub(crate) starts_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CalendarKind;

    /// 2026-10-20T00:00:00Z, in seconds.
    const NOW_SECS: i64 = 1_792_444_800;

    fn ics(name: &str) -> CalendarWatch {
        serde_json::from_str(&format!(
            "{{\"name\": \"{name}\", \"kind\": \"ics\", \"url\": \"https://example.net/f.ics\"}}"
        ))
        .expect("parses")
    }

    #[test]
    fn a_calendar_round_trips_through_the_table() {
        let store = CalendarStore::in_memory().expect("store opens");
        assert!(store.add(&ics("team")).expect("add lands"));
        assert!(
            !store.add(&ics("team")).expect("add again"),
            "a taken name is refused, not overwritten"
        );
        let listed = store.list().expect("list reads");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "team");
        assert_eq!(listed[0].kind, CalendarKind::Ics);
        assert!(store.remove("team").expect("remove lands"));
        assert!(store.list().expect("list reads").is_empty());
    }

    #[test]
    fn an_invalid_calendar_is_loud_wherever_it_enters() {
        let store = CalendarStore::in_memory().expect("store opens");
        let blank: CalendarWatch =
            serde_json::from_str("{\"name\": \"  \", \"kind\": \"ics\"}").expect("parses");
        let err = store.add(&blank).expect_err("a blank name is refused");
        assert!(err.to_string().contains("name"), "got: {err}");
        let no_url: CalendarWatch =
            serde_json::from_str("{\"name\": \"x\", \"kind\": \"ics\"}").expect("parses");
        let err = store.add(&no_url).expect_err("an urlless ics is refused");
        assert!(err.to_string().contains("url"), "got: {err}");
    }

    #[test]
    fn a_fed_slot_is_recorded_once_per_key() {
        let store = CalendarStore::in_memory().expect("store opens");
        let slot = FedSlot {
            calendar: "home".to_owned(),
            occurrence: "ics:standup@1792445400".to_owned(),
            lead: 60,
            starts_at: NOW_SECS + 15 * SECS_PER_MINUTE,
        };
        assert!(
            store.mark_slot_fed(&slot, NOW_SECS).expect("mark lands"),
            "the first insert is new"
        );
        assert!(
            !store.mark_slot_fed(&slot, NOW_SECS).expect("mark again"),
            "the key dedups: the row is never rewritten"
        );
        assert!(
            store
                .slot_is_fed("home", "ics:standup@1792445400", 60)
                .expect("read lands"),
        );
        assert!(
            !store
                .slot_is_fed("home", "ics:standup@1792445400", 15)
                .expect("read lands"),
            "another lead is its own slot"
        );
        assert!(
            !store
                .slot_is_fed("work", "ics:standup@1792445400", 60)
                .expect("read lands"),
            "another calendar is its own slot"
        );
    }

    #[test]
    fn the_ledger_prunes_slots_whose_occurrence_is_long_past() {
        let store = CalendarStore::in_memory().expect("store opens");
        // Fed two days ago, for an occurrence that started shortly
        // after: every lead's moment is long gone.
        let spent = FedSlot {
            calendar: "home".to_owned(),
            occurrence: "ics:old@1792272600".to_owned(),
            lead: 15,
            starts_at: NOW_SECS - 47 * 3600,
        };
        store
            .mark_slot_fed(&spent, NOW_SECS - 47 * 3600)
            .expect("mark lands");
        // Still ahead of itself: the slot's last due moment is
        // `starts_at + lead + grace`, and that is in the future.
        let live = FedSlot {
            calendar: "home".to_owned(),
            occurrence: "ics:soon@1792483200".to_owned(),
            lead: 15,
            starts_at: NOW_SECS + 3600,
        };
        store.mark_slot_fed(&live, NOW_SECS).expect("mark lands");

        let pruned = store.prune_fed_slots(NOW_SECS).expect("prune lands");
        assert_eq!(pruned, 1, "the spent row goes, and only it");
        assert!(
            !store
                .slot_is_fed("home", "ics:old@1792272600", 15)
                .expect("read lands"),
        );
        assert!(
            store
                .slot_is_fed("home", "ics:soon@1792483200", 15)
                .expect("read lands"),
            "a slot that can still be due survives the prune"
        );
    }
}
