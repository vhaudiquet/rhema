# rhema-session-store-sqlite

The SQLite session store: one row per session, WAL, `synchronous=FULL`.

```rust
use rhema_session_store::SessionStore;
use rhema_session_store_sqlite::SqliteStore;

let store = SqliteStore::open("sessions.db")?;
store.save(&id, &session)?;
let rows = store.list()?;   // four metadata columns, no body read
```

Schema:

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    saved_at       INTEGER NOT NULL,
    bytes          INTEGER NOT NULL,
    opening_prompt TEXT,
    body           TEXT NOT NULL
) STRICT;
```

`body` is the same pretty JSON the file format holds, so a row can be
exported to a `.json` and resumed. `opening_prompt` is computed once at
save, and `list` selects the metadata columns only, which is what makes
a large picker cheap.

`save` is one `INSERT ... ON CONFLICT(id) DO UPDATE`; `Ok` means the
commit returned.

The table carries a schema version as `PRAGMA user_version`, stamped
at open: a pre-versioning file takes the stamp (1), and a file stamped
with a newer version refuses to open rather than write a schema this
build does not fully understand, leaving it untouched. SQLite writes the database and its `-wal`/`-shm`
siblings with the process umask, so the crate tightens them to `0600`
best effort after open and after the first save.

The crate is separate so a file-only frontend never compiles or links
SQLite.

Spec: `specs/rhema-session-store.md` §6.4.
