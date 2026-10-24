//! The watch table: agent-managed watches in SQLite, the same database
//! file the dispatcher's [`SqliteStore`](rhema_rsd::SqliteStore) uses.
//!
//! Watches are data the daemon reconfigures at runtime — the chat
//! surface's [`watch_repo`](crate::watch_tools) tool and the HTTP
//! `POST /v1/watches` route both land here — so the table is read fresh
//! on every detect pass instead of living only in a static list. The
//! config no longer carries watches at all — the table is the one
//! truth, managed over `/v1/watches` and (from chat) the watch tools.
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

use rhema_forge_integration::{ForgeKind, resolve_token};

/// One watched repository — the row's vocabulary, and the type the
/// routes parse into. It used to live in the config as the YAML seed's
/// shape; the config no longer carries watches (the table is managed
/// over `/v1/watches`), and the type moved here with the rest of the
/// forge state. Unknown keys are rejected: a typo'd key must not
/// silently read as unset.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Watch {
    /// Which forge this watch polls.
    pub forge: ForgeKind,
    /// The repository, `owner/name` (GitLab: `namespace/project`).
    pub repo: String,
    /// The instance base URL. Required for gitea (self-hosted only);
    /// github and gitlab default to their production bases.
    pub host: Option<String>,
    /// The forge token. Falls back to `GITHUB_TOKEN` / `GITEA_TOKEN` /
    /// `GITLAB_TOKEN`; a blank either way counts as unset.
    pub token: Option<String>,
}

impl Watch {
    /// The repository coordinates, splitting `owner/name`. GitLab's owner
    /// keeps its full namespace path, which is why the split is lazy: the
    /// first `/` separates owner from name, never a deeper one.
    pub fn repo_ref(&self) -> Result<RepoRef, WatchShapeError> {
        let (owner, name) =
            self.repo
                .split_once('/')
                .ok_or_else(|| WatchShapeError::RepoShape {
                    repo: self.repo.clone(),
                })?;
        if owner.is_empty() || name.is_empty() {
            return Err(WatchShapeError::RepoShape {
                repo: self.repo.clone(),
            });
        }
        Ok(RepoRef::new(owner, name))
    }

    /// The token to poll with: the stored value, then the forge's env
    /// variable; whitespace-only counts as unset everywhere.
    pub fn resolve_token(&self) -> Option<String> {
        resolve_token(
            |name| std::env::var(name),
            self.forge,
            self.token.as_deref(),
        )
    }
}

/// The watch table, and the account-watch table beside it: one row per
/// identity whose forge notifications the daemon polls. An account watch
/// is derived state — it names the [`rhema_identity`] account the
/// notifications belong to, and its token is resolved through the
/// identity store at poll time, never stored here.
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
CREATE TABLE IF NOT EXISTS account_watches (
    id         INTEGER PRIMARY KEY,
    forge      TEXT NOT NULL,
    host       TEXT,
    account_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(forge, account_id)
);
";

/// Why a watch-table operation failed.
#[derive(Debug, thiserror::Error)]
pub enum WatchStoreError {
    /// The SQLite backend failed.
    #[error("watch store backend: {0}")]
    Backend(#[from] rusqlite::Error),
}

/// Why a watch's coordinates failed their shape checks: a repo that is
/// not `owner/name`, or a gitea watch with no instance base URL. One
/// type for every entry path — the HTTP route, the agent tool, the
/// config watch — so the refusals read the same everywhere.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatchShapeError {
    /// The repo field is not `owner/name` (GitLab's owner may itself
    /// carry slashes; the first `/` still separates).
    #[error("repo must be owner/name, got {repo:?}")]
    RepoShape {
        /// The repo value as stored, for the error message.
        repo: String,
    },
    /// A gitea watch with no host: gitea is self-hosted only, so there
    /// is nothing to poll.
    #[error("gitea is self-hosted: set host to the instance base URL")]
    MissingHost,
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
    pub fn repo_ref(&self) -> Result<RepoRef, WatchShapeError> {
        let (owner, name) =
            self.repo
                .split_once('/')
                .ok_or_else(|| WatchShapeError::RepoShape {
                    repo: self.repo.clone(),
                })?;
        if owner.is_empty() || name.is_empty() {
            return Err(WatchShapeError::RepoShape {
                repo: self.repo.clone(),
            });
        }
        Ok(RepoRef::new(owner, name))
    }
}

/// The persistent watch table over its own connection to the daemon's
/// SQLite file. Shareable behind an `Arc`; every operation takes `&self`.
pub struct ClawStore {
    conn: Mutex<rusqlite::Connection>,
}

/// One watched identity: the daemon polls this forge's notifications as
/// the named account. `account_id` is the [`rhema_identity`] store's id —
/// the credential stays there, never in this table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountWatchRecord {
    /// The row id.
    pub id: i64,
    /// The forge's source name: `github`, `gitea`, or `gitlab`.
    pub forge: String,
    /// The instance base URL; `None` for the forges with production
    /// bases. Required for gitea, which is self-hosted only.
    pub host: Option<String>,
    /// The identity store's account id the watch polls as.
    pub account_id: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
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

    /// Registers the account watch: the daemon polls `forge`'s
    /// notifications as the account. `false` when the account is already
    /// watched on that forge. The identity's own secret is *not* passed
    /// here — the watch names the account, and the token is resolved
    /// through the identity store at poll time.
    pub fn add_account_watch(
        &self,
        forge: &str,
        host: Option<&str>,
        account_id: &str,
    ) -> Result<bool, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let inserted = conn.execute(
            "INSERT INTO account_watches (forge, host, account_id, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(forge, account_id) DO NOTHING",
            params![
                forge.trim(),
                host.map(str::trim),
                account_id.trim(),
                now_millis()
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Removes the account watch for `(forge, account_id)`; `false` when
    /// there was none.
    pub fn remove_account_watch(
        &self,
        forge: &str,
        account_id: &str,
    ) -> Result<bool, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = conn.execute(
            "DELETE FROM account_watches WHERE forge = ?1 AND account_id = ?2",
            params![forge.trim(), account_id.trim()],
        )?;
        Ok(removed == 1)
    }

    /// Removes every account watch of one account — the cascade its
    /// `identity_forget` runs. Returns how many rows went.
    pub fn remove_account_watches_for(&self, account_id: &str) -> Result<usize, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = conn.execute(
            "DELETE FROM account_watches WHERE account_id = ?1",
            params![account_id.trim()],
        )?;
        Ok(removed)
    }

    /// Every account watch, oldest first. Read fresh by every detect
    /// pass, like the repo watches.
    pub fn list_account_watches(&self) -> Result<Vec<AccountWatchRecord>, WatchStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, forge, host, account_id, created_at
                 FROM account_watches ORDER BY id ASC",
            )
            .map_err(WatchStoreError::from)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AccountWatchRecord {
                    id: row.get(0)?,
                    forge: row.get(1)?,
                    host: row.get(2)?,
                    account_id: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(WatchStoreError::from)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(WatchStoreError::from)?;
        Ok(rows)
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
/// HTTP route, the agent tool): a parseable forge, an
/// `owner/name` repo, and — gitea being self-hosted only — a host for
/// gitea. Tokens are not required here: a blank one falls back to the
/// forge's environment variable at poll time.
pub fn validate_watch(
    forge: ForgeKind,
    repo: &str,
    host: Option<&str>,
) -> Result<(), WatchShapeError> {
    let watch = Watch {
        forge,
        repo: repo.trim().to_owned(),
        host: host.map(str::trim).map(str::to_owned),
        token: None,
    };
    watch.repo_ref()?;
    if forge == ForgeKind::Gitea && watch.host.as_deref().filter(|h| !h.is_empty()).is_none() {
        return Err(WatchShapeError::MissingHost);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_fails_a_watch_loudly_and_specificly() {
        assert_eq!(
            validate_watch(ForgeKind::GitHub, "acme", None).unwrap_err(),
            WatchShapeError::RepoShape {
                repo: "acme".to_owned()
            },
            "an ownerless repo is loud"
        );
        assert_eq!(
            validate_watch(ForgeKind::Gitea, "acme/barn", None).unwrap_err(),
            WatchShapeError::MissingHost,
            "a hostless gitea watch is loud"
        );
        validate_watch(ForgeKind::GitHub, "acme/widgets", None).expect("github validates");
        validate_watch(
            ForgeKind::Gitea,
            "acme/barn",
            Some("https://git.example.net"),
        )
        .expect("gitea validates");
    }

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
}
