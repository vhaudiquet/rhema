//! The calendar table: agent-managed calendars in SQLite, the same
//! database file the dispatcher's store uses — the pattern the claw's
//! watch table established. The config stopped carrying calendars:
//! the table is the one truth, and the source reads it fresh on every
//! detect pass, so a row added over `/v1/calendars` (or a chat tool)
//! is live without a restart.

use crate::reminder::CalendarWatch;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS calendars (
                 name       TEXT PRIMARY KEY,
                 spec       TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );",
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// An in-memory table; tests use this.
    pub fn in_memory() -> Result<Self, std::io::Error> {
        let conn = Connection::open_in_memory().map_err(std::io::Error::other)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS calendars (
                 name       TEXT PRIMARY KEY,
                 spec       TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );",
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Adds (or refuses, when the name is taken) a calendar. The spec
    /// is validated first — the same door every writer shares, the
    /// route and a future chat tool alike — so an invalid calendar is
    /// loud wherever it enters.
    pub fn add(&self, calendar: &CalendarWatch) -> Result<bool, std::io::Error> {
        calendar
            .validate()
            .map_err(std::io::Error::other)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CalendarKind;

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
}
