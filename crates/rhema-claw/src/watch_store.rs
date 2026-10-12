//! The watch table: agent-managed watches in SQLite, the same database
//! file the dispatcher's [`SqliteStore`](rhema_rsd::SqliteStore) uses.
//!
//! Watches are data the daemon reconfigures at runtime — the chat
//! surface's [`watch_repo`](crate::watch_tools) tool and the HTTP
//! `POST /v1/watches` route both land here — so the table is read fresh
//! on every detect pass instead of living only in the YAML config. The
//! config's `watches:` list is a one-time seed ([`ClawStore::
//! seed_from_config`]): imported when the table is still empty, ignored
//! once the table has rows.
//!
//! Two connections, one file: this store opens its own rusqlite
//! connection to the same database as the dispatcher's store — WAL and a
//! busy timeout, the pattern rhema-rsd's SQLite tests use — and the two
//! interleave through SQLite's own locking. Writes are single INSERT /
//! DELETE statements, so there is no read-modify-write window to guard.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_forge_integration::RepoRef;
use rusqlite::params;
use serde::Serialize;

use crate::config::{ForgeKind, Watch};

/// The watch table.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS watches (
    id         INTEGER PRIMARY KEY,
    forge      TEXT NOT NULL,
    repo       TEXT NOT NULL,
    host       TEXT,
    token      TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE(forge, repo)
);
";

/// Why a watch-table operation failed.
#[derive(Debug, thiserror::Error)]
pub enum WatchStoreError {
    /// The SQLite backend failed.
    #[error("watch store backend: {0}")]
    Backend(#[from] rusqlite::Error),
}

/// One watched repository, as stored. `token` never serializes: it
/// reaches the forge clients, not the wire or the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchRecord {
    /// The row id.
    pub id: i64,
    /// The forge's source name: `github`, `gitea`, or `gitlab`.
    pub forge: String,
    /// The repository, `owner/name` (GitLab: `namespace/project`).
    pub repo: String,
    /// The instance base URL; `None` for the forges with production
    /// bases. Required for gitea, which is self-hosted only.
    pub host: Option<String>,
    /// The forge token; blank means the forge's environment variable is
    /// consulted at poll time. Kept out of every serialized form.
    #[serde(skip_serializing)]
    pub token: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
}

impl WatchRecord {
    /// The repository coordinates, splitting `owner/name` — the same
    /// shape check a config watch passes. GitLab's owner keeps its full
    /// namespace path, which is why the split is lazy.
    pub fn repo_ref(&self) -> Result<RepoRef, String> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .ok_or_else(|| format!("repo must be owner/name, got {:?}", self.repo))?;
        if owner.is_empty() || name.is_empty() {
            return Err(format!("repo must be owner/name, got {:?}", self.repo));
        }
        Ok(RepoRef::new(owner, name))
    }
}

/// The persistent watch table over its own connection to the daemon's
/// SQLite file. Shareable behind an `Arc`; every operation takes `&self`.
pub struct ClawStore {
    conn: Mutex<rusqlite::Connection>,
}

impl ClawStore {
    /// Opens (creating if needed) the watch table in the database at
    /// `path` — the same file the dispatcher's store holds. WAL keeps the
    /// two connections from blocking each other's readers; the busy
    /// timeout rides out the other's write locks.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WatchStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(WatchStoreError::from)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory table: the shape without the file, for
    /// tests that need no sharing.
    pub fn in_memory() -> Result<Self, WatchStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(WatchStoreError::from)?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, WatchStoreError> {
        if durable {
            // Same pragmas as the dispatcher's store on the same file:
            // SQLite stores the journal mode per database, and both
            // connections must agree that it is WAL.
            let _mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(WatchStoreError::from)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(WatchStoreError::from)?;
        }
        conn.execute_batch(SCHEMA).map_err(WatchStoreError::from)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Adds a watch. `false` when `(forge, repo)` is already watched —
    /// the unique index turns a re-add into a no-op, so callers can be
    /// liberal.
    pub fn add(
        &self,
        forge: &str,
        repo: &str,
        host: Option<&str>,
        token: &str,
    ) -> Result<bool, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let inserted = conn.execute(
            "INSERT INTO watches (forge, repo, host, token, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(forge, repo) DO NOTHING",
            params![
                forge.trim(),
                repo.trim(),
                host.map(str::trim),
                token.trim(),
                now_millis()
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Removes the watch for `(forge, repo)`; `false` when there was
    /// none.
    pub fn remove(&self, forge: &str, repo: &str) -> Result<bool, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = conn.execute(
            "DELETE FROM watches WHERE forge = ?1 AND repo = ?2",
            params![forge.trim(), repo.trim()],
        )?;
        Ok(removed == 1)
    }

    /// Every watch, oldest first. Read fresh by every detect pass, so a
    /// watch added at runtime takes effect without a restart.
    pub fn list(&self) -> Result<Vec<WatchRecord>, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT id, forge, repo, host, token, created_at FROM watches ORDER BY id ASC")
            .map_err(WatchStoreError::from)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(WatchRecord {
                    id: row.get(0)?,
                    forge: row.get(1)?,
                    repo: row.get(2)?,
                    host: row.get(3)?,
                    token: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(WatchStoreError::from)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(WatchStoreError::from)?;
        Ok(rows)
    }

    /// The config seed: imports `watches` when the table is empty — the
    /// one-time bootstrap of a fresh deployment — and reports how many
    /// landed. A populated table is never rewritten by the seed: once
    /// any watch exists, the table rules and the config list is inert.
    /// Only the config's own token is stored; the forge's environment
    /// variable still serves at poll time when the stored one is blank.
    pub fn seed_from_config(&self, watches: &[Watch]) -> Result<usize, WatchStoreError> {
        if !self.list()?.is_empty() {
            return Ok(0);
        }
        let mut seeded = 0;
        for watch in watches {
            let token = watch.token.as_deref().map(str::trim).unwrap_or("");
            if self.add(
                watch.forge.name(),
                &watch.repo,
                watch.host.as_deref(),
                token,
            )? {
                seeded += 1;
            }
        }
        Ok(seeded)
    }
}

/// Milliseconds since the Unix epoch; stamps `created_at`.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// The shape checks every watch must pass, wherever it enters (the
/// config seed, the HTTP route, the agent tool): a parseable forge, an
/// `owner/name` repo, and — gitea being self-hosted only — a host for
/// gitea. Tokens are not required here: a blank one falls back to the
/// forge's environment variable at poll time.
pub fn validate_watch(forge: ForgeKind, repo: &str, host: Option<&str>) -> Result<(), String> {
    let watch = Watch {
        forge,
        repo: repo.trim().to_owned(),
        host: host.map(str::trim).map(str::to_owned),
        token: None,
    };
    watch.repo_ref()?;
    if forge == ForgeKind::Gitea && watch.host.as_deref().filter(|h| !h.is_empty()).is_none() {
        return Err("gitea is self-hosted: set host to the instance base URL".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_round_trips_through_the_table() {
        let store = ClawStore::in_memory().expect("store opens");
        assert!(
            store
                .add("github", "acme/widgets", None, "tok")
                .expect("add lands")
        );
        assert!(
            store
                .add("gitea", "acme/barn", Some("https://git.example.net"), "")
                .expect("add lands")
        );

        let watches = store.list().expect("list reads");
        assert_eq!(watches.len(), 2);
        assert_eq!(watches[0].forge, "github");
        assert_eq!(watches[0].repo, "acme/widgets");
        assert_eq!(watches[0].host, None);
        assert_eq!(watches[0].token, "tok");
        assert_eq!(watches[1].host.as_deref(), Some("https://git.example.net"));
        assert_eq!(watches[1].token, "", "a missing token stores blank");

        assert!(store.remove("github", "acme/widgets").expect("remove"));
        assert!(!store.remove("github", "acme/widgets").expect("re-remove"));
        assert_eq!(store.list().expect("list reads").len(), 1);
    }

    #[test]
    fn a_re_add_is_a_no_op_not_an_error() {
        let store = ClawStore::in_memory().expect("store opens");
        assert!(
            store
                .add("gitlab", "group/sub/prop", None, "")
                .expect("first")
        );
        assert!(
            !store
                .add("gitlab", "group/sub/prop", None, "")
                .expect("second")
        );
        assert_eq!(store.list().expect("list reads").len(), 1);
    }

    #[test]
    fn a_record_repo_ref_splits_owner_from_name_lazily() {
        let store = ClawStore::in_memory().expect("store opens");
        store
            .add("gitlab", "group/subgroup/widgets", None, "")
            .expect("add");
        let record = &store.list().expect("list reads")[0];
        assert_eq!(
            record.repo_ref().expect("parses").path(),
            "group/subgroup/widgets"
        );
        let bad = WatchRecord {
            id: 0,
            forge: "github".into(),
            repo: "ownerless".into(),
            host: None,
            token: String::new(),
            created_at: 0,
        };
        assert!(bad.repo_ref().is_err());
    }

    #[test]
    fn the_seed_imports_only_an_empty_table() {
        let store = ClawStore::in_memory().expect("store opens");
        let watches = vec![Watch {
            forge: ForgeKind::GitHub,
            repo: "acme/widgets".to_owned(),
            host: None,
            token: Some("  ".to_owned()),
        }];
        assert_eq!(
            store.seed_from_config(&watches).expect("seeds"),
            1,
            "the empty table takes the seed"
        );
        assert_eq!(
            store.list().expect("list reads")[0].token,
            "",
            "blank stays blank"
        );
        assert_eq!(
            store.seed_from_config(&watches).expect("skips"),
            0,
            "a populated table ignores the seed"
        );
    }

    #[test]
    fn validation_checks_shape_and_giteas_host() {
        assert!(validate_watch(ForgeKind::GitHub, "acme/widgets", None).is_ok());
        assert!(validate_watch(ForgeKind::GitHub, "ownerless", None).is_err());
        assert!(validate_watch(ForgeKind::GitHub, "acme/", None).is_err());
        assert!(validate_watch(ForgeKind::Gitea, "acme/barn", None).is_err());
        assert!(validate_watch(ForgeKind::Gitea, "acme/barn", Some("   ")).is_err());
        assert!(
            validate_watch(
                ForgeKind::Gitea,
                "acme/barn",
                Some("https://git.example.net")
            )
            .is_ok()
        );
        assert!(
            validate_watch(ForgeKind::GitLab, "group/subgroup/widgets", None).is_ok(),
            "gitlab has a production base"
        );
    }

    #[test]
    fn the_token_never_serializes() {
        let store = ClawStore::in_memory().expect("store opens");
        store
            .add("github", "acme/widgets", None, "sekrit")
            .expect("add");
        let json =
            serde_json::to_string(&store.list().expect("list reads")[0]).expect("serializes");
        assert!(!json.contains("sekrit"), "token leaked: {json}");
        assert!(json.contains("acme/widgets"), "{json}");
    }

    #[test]
    fn two_connections_share_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        let writer = ClawStore::open(&path).expect("first connection");
        let reader = ClawStore::open(&path).expect("second connection");
        writer.add("github", "acme/widgets", None, "").expect("add");
        let watches = reader.list().expect("the other connection sees it");
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].repo, "acme/widgets");
    }
}
