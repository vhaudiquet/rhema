# rhema-session-store, one durable store for saved sessions

> **Status:** v1 draft, 2026-11-16, for review. Nothing implemented yet.
> **Framework grounding:** SPEC.md §6 (the Session is the durable record;
> the driver checkpoints it at step boundaries), `rhema_core::CommitHook`
> (the engine's persistence seam), `rhema-rsd::Store` (the existing
> store-contract pattern the seam crate copies).
> **One-line promise:** one seam, one crate per backend, and no frontend
> writes session bytes again.

---

## 0. Scope

Three crates, one seam and one crate per backend, the same shape as
`rhema-provider-*` and `rhema-websearch-*`:

- **`rhema-session-store`** is the seam only: `SessionStore`,
  `SessionId`, `Meta`, `StoreError`, the opening-prompt helper, and the
  replayable contract suite. It depends on `rhema-core`, `serde`,
  `serde_json`, and `thiserror`, and on nothing else. No
  `rhema-config`, no `rusqlite`, no filesystem code.
- **`rhema-session-store-file`** is the reference backend:
  `FileStore` (the managed layout the TUI and ACP already use, one
  bare-JSON `Session` per file, atomic temp-plus-rename with fsync) and
  `PathStore` (the single explicit path `--save`/`--resume` take). It
  owns the writer, `next_id`, `encode_workspace`, and the `0700`/`0600`
  modes.
- **`rhema-session-store-sqlite`** is one row per session, WAL,
  `synchronous=FULL`, body as the same JSON the file format holds. The
  table's schema is stamped as `PRAGMA user_version` at open: an
  unstamped file takes the stamp, and a file stamped above this
  build's version refuses to open untouched.

**In v1:**

- The seam, its contract suite, and id validation.
- The file backend, migrated under ACP, `rhema-host-agent`'s
  save/load helpers, headless, and the TUI, plus the missing ACP commit
  hook.
- The SQLite backend, tested against the same suite, adopted by no
  frontend yet.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Compare-and-swap saves.** No second writer shares one id today:
  the TUI allocates a fresh id per conversation, ACP keys by client id
  with one process per session, and headless is one process. Cross-
  process resume of the same id is last-writer-wins, as it already is.
  Adding CAS now forces a decision about storing a revision beside a
  bare-JSON file. The trait grows `save_if_unchanged(id, session,
  expect)` when a second writer actually exists; SQLite can then
  implement it in one transaction.
- ❌ **Submissions, job records, or any driver state.** Those live in
  `Session::driver_state` (the core docs bless exactly that) and ride
  the whole-session commit. A separate table reopens the crash window
  between the answer landing in history and the record saying "done".
- ❌ **A JSONL backend.** The file backend is the compatibility path.
  An append-only backend is the answer if fsync of a large whole file
  proves too slow, and it gets its own spec then.
- ❌ **An RSD adapter.** `rhema_rsd::Store` holds `KeyRecord`s, not bare
  sessions. Wrapping one as the other hides lease and inbox state.
- ❌ **Encryption, compression, retention, garbage collection, stale
  temp sweeping, cross-scope listing, migration of old layouts.**
- ❌ **Async methods.** `CommitHook` is synchronous and the engine calls
  it on the turn task. A store that blocks is the existing deal; an
  async wrapper can be layered above.
- ❌ **A facade or registry crate.** The backend is chosen at compile
  time by the frontend; a runtime registry buys nothing until a
  frontend picks a store from configuration, and then it is one small
  crate of its own.

### Cargo

```toml
# rhema-session-store
[dependencies]
rhema-core = { path = "../rhema-core", version = "0.3.0" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }

[features]
# The reusable contract suite, for replay against any backend.
store-contract = []
```

```toml
# rhema-session-store-file
[dependencies]
rhema-session-store = { path = "../rhema-session-store", version = "0.3.0" }
rhema-core = { path = "../rhema-core", version = "0.3.0" }
rhema-config = { path = "../rhema-config", version = "0.3.0" }
serde_json = { workspace = true }

[dev-dependencies]
rhema-session-store = { path = "../rhema-session-store", version = "0.3.0", features = ["store-contract"] }
tempfile = "3"
```

```toml
# rhema-session-store-sqlite
[dependencies]
rhema-session-store = { path = "../rhema-session-store", version = "0.3.0" }
rhema-core = { path = "../rhema-core", version = "0.3.0" }
# Bundled: no system sqlite, and the same version rhema-rsd locks, so
# the workspace builds one copy.
rusqlite = { version = "0.40", features = ["bundled"] }
serde_json = { workspace = true }

[dev-dependencies]
rhema-session-store = { path = "../rhema-session-store", version = "0.3.0", features = ["store-contract"] }
```

A consumer that only needs the trait depends on the seam alone and
pulls neither rusqlite nor `rhema-config`.

---

## 1. The problem

There are three session writers in the tree, and they disagree:

- `rhema-code/src/sessions.rs`: `<cache>/rhema/sessions/<encoded
  workspace>/<millis:016>-<n:02>.json`, atomic temp-plus-rename,
  `0600`, listing by mtime with a snippet parsed from each file.
- `rhema-acp/src/store.rs`: `<cache>/rhema/acp-sessions/<id>.json`,
  `private_write` (truncate in place, **not** atomic), id validation,
  load by id.
- `rhema-host-agent`: `save_session_to` / `load_session_from`, the
  same pretty JSON over an arbitrary path, atomic.

The duplication is only the symptom. The failures it causes:

1. **ACP has no commit hook.** A turn that dies mid-step loses the
   whole turn, while the TUI and headless keep every stream boundary.
   `app.persist` runs only after `Some(Ok(_))`.
2. **ACP truncates in place.** A crash mid-save can leave a half file,
   the one shape a session store must never produce.
3. **The picker pays for every byte.** `list_sessions` parses every
   full session to get a 48-character snippet. `docs/compaction-audit.md`
   counts 40 sessions holding 28.7 MB of message bytes, 19 over 256 KB
   and one at 15.16 MB: drawing the list reads and parses all of it.
4. **No reusable seam.** A new frontend (cloud, a server, a test
   harness) must copy one of the three, and each copy drifts.

---

## 2. Goals

- One implementation of the file layout, the atomic write, the privacy
  modes, the listing, and the id rules.
- The on-disk format does not change: a file this crate writes is the
  same bare-JSON `Session` `--resume` takes today, and a file written
  by the old code loads through the new crate.
- A `save` that returns `Ok` survives process death and, for the file
  backend, power loss (fsync of the file and its directory).
- The trait is small, synchronous, object-safe, and passes the same
  behavioral suite on every backend.
- Deleting ACP's non-atomic writer and adding its commit hook is the
  first consumer-visible win.

**Non-goals:** multi-writer coordination, transactional multi-record
writes, cross-process notification, or being the only persistence in the
framework. RSD keeps its own store for dispatch state.

---

## 3. Vocabulary

| Term | Meaning |
|---|---|
| **Session value** | A `rhema_core::Session`: history, opaque driver state, cost, usage stats. |
| **Session id** | An opaque, validated file-safe string identifying one saved session. TUI: `<millis:016>-<n:02>`. ACP: the client's session id. `--save`: the file stem. |
| **Store** | A collection of saved sessions, one directory or one SQLite database. |
| **Path store** | A single explicit file; the identity is the path, not an id. |
| **Managed layout** | `<root>/<id>.json` or `<root>/<encoded-scope>/<id>.json`. |
| **Scope** | An optional grouping directory, the TUI's encoded workspace. ACP uses none. |
| **Saved at** | Milliseconds since the Unix epoch, from the file's mtime (file) or the save clock (SQLite). |
| **Opening prompt** | The first non-empty user text in history, cut to 48 chars, the picker row. |

---

## 4. The trait

```rust
pub trait SessionStore: Send + Sync {
    /// The session saved under `id`; `None` when nothing was ever saved
    /// there. A file that exists but does not parse is `Err`, never
    /// `None`: the listing tolerates corruption, a resume surfaces it.
    fn load(&self, id: &SessionId) -> Result<Option<Session>, StoreError>;

    /// Atomically replace the session saved under `id`. On `Ok` the
    /// write is committed: a reader sees the old value or the new one,
    /// never a mix, and the backend's durability level has been met.
    fn save(&self, id: &SessionId, session: &Session) -> Result<(), StoreError>;

    /// Remove `id`. `Ok(true)` when a record existed and is gone,
    /// `Ok(false)` when there was none. Removing a missing id is not an
    /// error.
    fn delete(&self, id: &SessionId) -> Result<bool, StoreError>;

    /// Every saved session in this store, newest first, ties broken by
    /// id ascending. Never fails because one record is corrupt: the
    /// corrupt entry still lists, with `opening_prompt: None`.
    fn list(&self) -> Result<Vec<Meta>, StoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub id: SessionId,
    /// Milliseconds since the Unix epoch.
    pub saved_at: u64,
    /// The file's or body's byte length.
    pub bytes: u64,
    /// The first non-empty user text, cut to 48 characters.
    pub opening_prompt: Option<String>,
}
```

Why these choices:

- **Sync.** The engine's `CommitHook` is `Fn(&Session) -> Result<(),
  CommitError>`, called inside `Agent::step`. An async trait would be
  driven with `block_on` at exactly one place per frontend, which is
  worse than letting the backend block. Backends must not spawn.
- **`&Session`, not `&mut`.** The hook hands a shared reference; the
  store serializes it and returns.
- **No revision.** See §0; last writer wins in v1.
- **`Meta` is metadata only.** The listing must not deserialize the
  body where the backend can avoid it. SQLite reads four columns; the
  file backend parses to derive the snippet (today's behavior), and
  this spec records that cost rather than inventing a sidecar.
- **An empty store is `Ok(vec![])`, not an error.** A missing directory
  is "nothing saved yet". `load` of a missing id is `Ok(None)`.

`StoreError`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session store io at {path}: {source}")]
    Io { path: PathBuf, #[source] source: io::Error },
    #[error("session {id} does not parse: {source}")]
    Corrupt { id: SessionId, #[source] source: serde_json::Error },
    #[error("invalid session id {0:?}")]
    InvalidId(String),
    #[error("session store backend: {0}")]
    Backend(String),
}

impl From<StoreError> for rhema_core::CommitError {
    fn from(err: StoreError) -> Self { CommitError(err.to_string()) }
}
```

The `From` impl is the whole hook glue:

```rust
agent.set_commit_hook(Some(Box::new(move |session| {
    store.save(&id, session).map_err(Into::into)
})));
```

---

## 5. `SessionId`

```rust
pub struct SessionId(String);

impl SessionId {
    pub fn new(raw: impl Into<String>) -> Result<Self, StoreError>;
    pub fn as_str(&self) -> &str;
}
```

Rules, enforced on construction and on every trait method:

- 1 to 128 bytes.
- Every byte inside `[-._0-9A-Za-z]`.
- No leading `.`. A dot-prefixed name is how temp files hide, and
  `list` skips dot entries; allowing one would produce a session that
  saves but never lists. The old ACP `valid_id` allowed it; no real id
  has one, and tightening is the honest fix.
- `Display`, `AsRef<str>`, `Serialize`, `Deserialize` (deserialization
  validates).

`SessionId` never reaches a path without validation. `list` ignores
filenames that fail these rules; a file with a valid id but no valid
body is a session that lists and fails on `load`, not a filename the
store silently hides.

---

## 6. The backends

| Crate | Holds | Depends on |
|---|---|---|
| `rhema-session-store` | the trait, ids, `Meta`, `StoreError`, `opening_prompt`, the suite | `rhema-core`, `serde`, `serde_json`, `thiserror` |
| `rhema-session-store-file` | `FileStore`, `PathStore`, the writer, `next_id`, `encode_workspace` | the seam, `rhema-core`, `rhema-config`, `serde_json` |
| `rhema-session-store-sqlite` | `SqliteStore` | the seam, `rhema-core`, `rusqlite`, `serde_json` |

No re-exports. The seam owns `SessionId`, `Meta`, `StoreError`, and
the trait; a backend exports only its own names. A frontend depends on
both crates and imports `FileStore` from the file crate and `SessionId`
/ `StoreError` from the seam, the way `rhema-websearch-registry`
imports `BraveProvider` from `rhema-websearch-brave` and `Chain` from
`rhema_websearch`. The file backend is the reference implementation:
every consumer today uses it, and it is the one the suite was written
against.

`rhema-rsd` bundles its volatile and SQLite stores in one crate, and
that is not a contradiction: its `Store` is an internal seam behind a
dispatch primitive, while this one is a public storage seam whose
backends an embedder picks at compile time. The family convention
(`rhema-provider-*`, `rhema-websearch-*`, `rhema-sandbox-*`) is one
backend per crate for exactly that choice.

### 6.1 `FileStore`, the managed layout

```rust
impl FileStore {
    /// `<base>/<id>.json` (ACP's shape).
    pub fn new(base: impl Into<PathBuf>) -> Self;
    /// `<base>/<encoded-workspace>/<id>.json` (the TUI's shape).
    pub fn scoped(base: impl Into<PathBuf>, workspace: &Path) -> Self;
    pub fn dir(&self) -> &Path;
    pub fn path_for(&self, id: &SessionId) -> PathBuf;
    /// The next free `<millis:016>-<n:02>` id. Creates the directory,
    /// writes nothing else.
    pub fn next_id(&self) -> Result<SessionId, StoreError>;
}
```

- **Layout is frozen.** The workspace directory name is the absolute
  workspace path with every byte outside `[-._0-9A-Za-z]`
  percent-encoded, exactly `encode_workspace` today. Do not "improve"
  the encoding: old directories must resolve.
- **The file is a bare `Session`.** Pretty JSON, no wrapper, no
  envelope, no trailing marker. The same bytes `--resume`, `--save`,
  and a hand edit expect.
- **Ids are allocated, never guessed.** `next_id` takes the current
  millisecond zero-padded to 16 digits and the smallest `n`, zero-padded
  to 2 digits (`0001759000000000-00`, `0001759000000000-01`, ...),
  whose file does not exist. It creates the directory `0700` and
  returns without writing. The fixed widths are why lexicographic name
  order is time order, a property the old picker relies on. Two
  processes can race into the same name in the same millisecond; the
  second `save` wins, matching today's behavior. A fresh
  conversation's id is stable for its whole life.
- **`load`** opens that one file, reads, parses, returns `None` on
  `NotFound`. It never falls back to a different file.
- **`list`** reads the directory, skips dot entries and names that
  fail `SessionId` validation, stats each file for `saved_at`/`bytes`,
  parses it for `opening_prompt`, and sorts. `saved_at` is the mtime;
  when the mtime cannot be read it falls back to the leading
  millisecond field when the id matches the allocated
  `<16 digits>-<2 digits>` shape, and to `0` otherwise, so a record
  never vanishes from the list because its clock is unreadable. A file that
  fails to parse still lists with `opening_prompt: None`; `load` is
  where the error surfaces. A missing directory lists empty. Unlike the
  old picker, which skipped any name it could not read a timestamp
  from, a stray `foo.json` with a valid id now lists (and fails on
  resume) instead of hiding; the TUI writes no such file.
- **`delete`** removes the file; `NotFound` is `Ok(false)`.
- **`save`** creates the directory if needed, then follows §6.2.

### 6.2 The write protocol

One internal function serves `FileStore` and `PathStore`:

1. Serialize with `serde_json::to_writer_pretty` directly into the temp
   file. Do not build a `String` first; the 15.16 MB session in the
   audit should not cost a second 15 MB allocation per stream
   boundary.
2. Create the temp as `.<id>.<pid>.<seq>.tmp` in the same directory,
   mode `0600`, with `create_new(true)`. `seq` is a process-wide atomic
   counter, so two threads saving the same id in one process never
   collide on the temp name.
3. `flush` and `sync_all` the temp.
4. `rename` over the target.
5. `sync_all` the parent directory, best effort. Ignore only
   `InvalidInput`/`Unsupported` (platforms that refuse directory
   fsync); any other error is reported. Where the directory fsync
   works, the rename itself is durable.
6. On any failure: remove the temp, return the error with the path, and
   leave the previous file untouched.

This is the one behavioral change from today's TUI writer: it fsyncs.
The TUI commits two or three times per step, so the cost is real and
must be measured before merge (§12).

A crash may leave a temp behind. Dot entries never list, never load,
and never match a valid id, so a leftover is inert. Sweeping is a
follow-up.

### 6.3 `PathStore`, the explicit path

```rust
impl PathStore {
    pub fn new(path: impl Into<PathBuf>) -> Self;
    pub fn path(&self) -> &Path;
    pub fn load(&self) -> Result<Option<Session>, StoreError>;
    pub fn save(&self, session: &Session) -> Result<(), StoreError>;
}
```

`PathStore` does **not** implement `SessionStore`. Its identity is the
path, and `--save`/`--resume` accept paths outside any managed root.
Forcing an id parameter would be a lie. It shares §6.2's writer, so the
atomicity and durability are identical. `save` creates the parent
directory (`0700`) when it is missing, so `--save` into a fresh
`.rhema/runs/` works without a separate setup step. `load` returns
`None` on `NotFound`, so `--resume` keeps its current "cannot resume"
error wording.

### 6.4 `SqliteStore` (sqlite crate)

```rust
impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError>;
    pub fn in_memory() -> Result<Self, StoreError>; // tests
}
```

Schema, created on open:

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    saved_at       INTEGER NOT NULL,
    bytes          INTEGER NOT NULL,
    opening_prompt TEXT,
    body           TEXT NOT NULL
) STRICT;
```

- `body` is the same pretty JSON as the file format, so a row can be
  exported to a `.json` and resumed, and a corrupt body reports the
  same `Corrupt` error as a file.
- Open with `journal_mode=WAL`, `synchronous=FULL`, and a busy timeout,
  the same settings `rhema-rsd`'s store uses. `save` is one
  `INSERT ... ON CONFLICT(id) DO UPDATE` in one transaction; `Ok`
  means committed.
- `saved_at` is `SystemTime` at save. `bytes` is the body length.
  `opening_prompt` is computed once, at save, so `list` never parses.
- `list` selects the four metadata columns, no `body`; this is the
  backend that makes a large picker cheap.
- `delete` uses the statement's change count for the boolean.
- After open, tighten the database file to `0600` best effort, and
  after the first save tighten the `-wal`/`-shm` siblings it created.
  SQLite writes them with the process umask, so the tighten is
  best-effort by contract, not a guarantee.
- This backend lives in its own crate so a file-only frontend never
  compiles or links rusqlite, with no feature-unification surprise:
  `cargo tree -p rhema-code | rg rusqlite` finds no match, by
  construction.

---

## 7. Listing and the picker

`Meta` replaces `SavedSession` at the store boundary. The TUI keeps
`SavedSession` as its own view type because it carries a `PathBuf` the
picker needs for `--resume`. `FileStore` comes from
`rhema_session_store_file`:

```rust
let store = FileStore::scoped(base, &workspace);
let rows = store.list()?;
let saved: Vec<SavedSession> = rows.into_iter()
    .map(|meta| SavedSession {
        path: store.path_for(&meta.id),
        saved_at: meta.saved_at,
        snippet: meta.opening_prompt,
    })
    .collect();
```

Ordering is part of the contract: `saved_at` descending, then
`id` ascending. The TUI's current tie-break on path is equivalent
because the path is the directory plus the id.

The 48-character width stays. `truncate_chars` moves into the seam
crate beside `opening_prompt`; the TUI re-exports nothing and uses the
seam's helper only if it still needs a snippet elsewhere.

---

## 8. Wiring

### 8.1 ACP (the first consumer, and the bug fix)

- Delete `crates/rhema-acp/src/store.rs`; keep `base_dir` and
  `AcpOptions::session_dir` in `serve` as the managed base.
- ACP depends on the seam (`SessionId`, `StoreError`) and on the file
  crate (`FileStore`).
- `session/load` calls `FileStore::new(base).load(&SessionId::new(id)?)`.
- **Install a commit hook before `agent.turn` and clear it after the
  turn**, exactly like `rhema-code/src/turn.rs` and headless do. The
  store must be shared with the turn task (`Arc<FileStore>`; `FileStore`
  is a path and cheap to clone).
- Keep the post-turn `app.persist` call. It is not redundant: the sink's
  `SessionMeter` records cost on `StepEnd`, and the engine's last
  commit fires **before** `StepEnd` is emitted. Without the post-turn
  save the final cost/usage totals never reach disk. This is also why
  the TUI keeps its post-turn save.
- Test: a provider that fails after a committed tool call leaves a
  file whose last assistant message carries the call and whose tool
  result is missing; a reopened agent's `revive_dangling_calls` closes
  it.

### 8.2 `rhema-host-agent`

`save_session_to` and `load_session_from` delegate to
`PathStore::save`/`load` (`rhema-session-store-file`). This removes
the third copy of the JSON writer and gives the helpers fsync. The
public signatures do not change:
`load_session_from` still returns `Ok(Session)` and maps `None` (missing
file) to an `io::ErrorKind::NotFound` error, preserving current
behavior.

### 8.3 Headless

`--save` and the failure checkpoint both become `PathStore::new(path)`.
The warning strings and the printed resume hint stay. The failure path
(`.rhema/runs/failed-<secs>.json`) keeps allocating its own name.

### 8.4 TUI

- `sessions.rs` keeps `base_dir`, `SavedSession`, and `ago`, and becomes
  a thin adapter over `FileStore`/`PathStore` (§7).
- `session_file: Option<PathBuf>` stays in `App` for v1. Allocation
  uses `FileStore::scoped(base, workspace).next_id()` then
  `path_for(&id)`; checkpointing uses `PathStore::new(path)`. This is the
  smallest migration that removes the duplicate writer without
  redesigning the app's session identity. A follow-up can carry
  `(FileStore, SessionId)` instead of a path, once submissions land.
- The exit save uses `PathStore`, unchanged in behavior.
- The "an empty history leaves no file" rule in `save_session` is
  product policy and stays in `rhema-code`; the store writes whatever
  it is given.
- Tests that call `sessions::write_session` / `save_session` /
  `list_sessions` move to the store, keeping `SavedSession`'s shape so
  picker tests do not churn.

### 8.5 Cloud and RSD

No code change in v1. `rhema-cloud-agent` continues to receive a
`Session` from its platform and can adopt `SqliteStore` over its own
storage later. `rhema-rsd` keeps its own `Store`; it stores `KeyRecord`s,
and unifying the two would hide lease and inbox state.

---

## 9. The contract suite

The suite lives in the seam crate behind the `store-contract` feature,
off by default, the same pattern as `rhema_rsd::store_contract`: plain
assertion functions taking `&dyn SessionStore`, plus `run_all`. Each
backend dev-depends on the seam with the feature on, and a third-party
backend can do the same.

```rust
#[cfg(feature = "store-contract")]
pub mod contract {
    pub fn load_missing_returns_none(store: &dyn SessionStore);
    pub fn save_then_load_round_trips(store: &dyn SessionStore);
    pub fn save_replaces_whole_value(store: &dyn SessionStore);
    pub fn delete_removes_and_reports(store: &dyn SessionStore);
    pub fn list_is_newest_first_and_ordered(store: &dyn SessionStore);
    pub fn run_all(store: &dyn SessionStore);
}
```

The suite uses its own id namespace per function so one store can run
everything, and `run_all` expects a fresh store. Id validation is tested
on `SessionId` itself, not through a backend, and the ordering test
sleeps 2 ms between saves so the mtimes differ. Backend-specific tests,
outside the suite:

- File: mtime-based ordering with distinct timestamps; a leftover
  `.tmp` is ignored by `list`; a save over an existing file leaves no
  temp; a corrupt file lists with no snippet and `load` returns
  `Corrupt`; `next_id` never returns a taken name; the directory is
  `0700` and files `0600` (unix); a pre-existing bare-JSON file written
  by hand loads unchanged; a save from a second thread cannot interleave
  a partial read.
- SQLite: WAL and FULL are actually set; `list` does not read `body`
  (query plan or a body with invalid JSON proves it); a `0600` tighten;
  a file store's `Session` and a SQLite `Session` round-trip equal;
  a row dumped to a file resumes.
- `PathStore`: missing is `None`; save/load round-trip; temp cleanup;
  a failed serialize leaves the old file.

The suite must pass from the seam crate (`--features store-contract`)
and from each backend's test build. The SQLite-specific tests live in
the SQLite crate and never touch the file one.

---

## 10. Invariants

1. A reader never sees a partial session; a writer never removes the
   only good copy before the new one is renamed.
2. `save` returns `Ok` only after the backend's durability point (file:
   fsync done; SQLite: transaction committed).
3. A missing store or id is absence, not an error (`Ok(None)`,
   `Ok(vec![])`, `Ok(false)`).
4. Corruption is reported by `load`, tolerated by `list`.
5. An id never escapes its directory: validation is the only path
   construction, and it admits no separator, no dot prefix, no `..`.
6. The on-disk format is versionless by design: a bare `Session` is the
   format, and additive fields ride `#[serde(default)]` in core.
7. Private by default: directories `0700`, files `0600` on unix.
8. No panics on input, paths, or corrupt files; every failure is a
   `StoreError`.

---

## 11. Migration plan

One commit per step, each independently reviewable:

1. **Seam crate.** `rhema-session-store`: `SessionId`, `StoreError`,
   `Meta`, the trait, `opening_prompt`, the contract suite behind
   `store-contract`, and unit tests for the id rules and the prompt
   helper. No backend, no consumer changes.
2. **File crate.** `rhema-session-store-file`: `FileStore`,
   `PathStore`, the shared writer, `next_id`, `encode_workspace`, plus
   the file-specific tests and the suite. Add
   `rhema_config::privacy::private_create(path) -> io::Result<File>`
   (create-new, `0600`, the mode applied at creation) so the mode rules
   stay in one module; the writer calls it for every temp file. No
   consumer changes yet.
3. **`rhema-host-agent`.** Delegate `save_session_to` /
   `load_session_from` to `PathStore`; add the file crate; keep
   the public API and tests green.
4. **Headless.** Move `--save` and the failure checkpoint to
   `PathStore`; delete the local `write_session` calls.
5. **ACP.** Delete `store.rs`, migrate load/save, install the commit
   hook, keep the post-turn persist, and add the crash-mid-turn test.
6. **TUI.** Move `sessions.rs` onto `FileStore`/`PathStore`; keep
   `SavedSession`, the picker, `--save`, `--resume`, and the exit save
   behavior identical.
7. **SQLite crate.** `rhema-session-store-sqlite`: `SqliteStore`, its
   contract runs, the equality test against the file backend
   (dev-depend on it), and `in_memory`. Can land any time after step 1;
   last keeps the seam and the reference backend settled first. No
   frontend adopts it.
8. **Docs and publish.** Three READMEs, references in
   `crates/rhema-code/README.md` and the ACP docs, and a note in
   `SPEC.md` §6 naming the file backend as the reference store.
   Publish order: seam, file, sqlite.

After step 5, `rg 'private_write\(' crates/rhema-acp crates/rhema-code
crates/rhema-host-agent` must return no session writer. After step 6,
`rhema-code/src/sessions.rs` is an adapter, not an implementation.

---

## 12. Decisions

- **One backend per crate.** It matches the family and keeps the seam
  free of `rhema-config` and `rusqlite`; the TUI's dependency
  graph cannot grow a backend it does not use. Two extra crates for a
  public seam with two backends is the family's normal cost, not
  over-splitting. Splitting later would change every consumer's
  dependencies and imports.
- **No submissions or jobs crate.** Those stay in `driver_state` for
  now; a crate would exist to hold one module.
- **File first, SQLite last.** The compatibility format and the two
  consumers are file-based. SQLite lands after the seam and the
  reference backend are settled, so the trait is proven by two
  backends before any frontend adopts the new one.
- **No re-exports from the backends.** A fixed re-export list would
  put the seam's types into each backend's public API (removing one
  becomes a breaking change there, even if the backend never named it
  directly), give one type two import paths and two rustdoc entries,
  and break the family convention: `rhema-provider-*` and
  `rhema-websearch-*` export only their own names, and consumers import
  the seam types from the seam. The cost is one more `Cargo.toml` line
  and one more `use` per frontend, which is the dependency being
  visible.
- **`opening_prompt` lives in the seam.** `Meta` carries it, so it is
  contract-level, and both backends need it (the file backend at `list`,
  SQLite at `save`). The seam already depends on `rhema-core`; the
  helper costs nothing there.
- **No wrapper format.** A `{revision, session}` envelope would buy CAS
  and cost `--resume` compatibility and hand readability. The trait
  defers CAS instead.
- **`PathStore` is not a `SessionStore`.** Identity by path is a real
  shape (`--save`), not a degenerate id.
- **The listing parses bodies in v1.** It is what the TUI already does.
  A sidecar index is a measured follow-up, not a speculative format.
- **fsync in the shared writer, by default.** The family is named for
  durability; an atomic-only default would make the name a lie. The
  cost is measured on the audit's 1.98 MB and 15.16 MB sessions before
  merge; if the median at 2 MB exceeds 10 ms, the decision is revisited
  in review with numbers, not silently weakened.
- **`list` never fails on corruption.** A picker that disappears
  because one file is bad is worse than a row with no snippet.

---

## 13. Open questions for review

1. **fsync default.** Keep it on for the TUI, or expose
   `FileStore::without_fsync()`, whose docs say plainly that the store
   is atomic but not power-loss durable? My default is on (§12), but a
   measured 15.16 MB session may change it.
2. **Three crates for roughly 1.5k lines.** My answer: yes, now, for
   the dependency-isolation reason above. The smaller alternative, if
   the seam crate feels too thin to stand alone, is seam plus
   file in one crate with SQLite separate; the seam must not grow
   rusqlite either way. Is a SQLite backend with no frontend consumer
   acceptable in v1? Yes: it is the proof the trait is real, and cloud
   is the next consumer.
3. **Post-turn save duplication.** Both ACP and the TUI write once more
   after a successful turn for the meter. Should the store expose
   `save_if_changed` (hash/len compare) to make that write cheap, or is
   the second full write acceptable? My answer: acceptable in v1; it
   is once per turn, not once per step.
4. **`SavedSession` in `rhema-code`.** Keep the type and map from
   `Meta` (§7), or replace it outright? Keeping it minimizes churn in
   the picker and its tests.
5. **Corrupt files in `list`.** Include them with no snippet (today's
   behavior), or add `Meta.readable: bool`? My answer: no new field;
   the resume attempt reports the corruption.
6. **`next_id` race.** Two processes can allocate the same id in the
   same millisecond. Accept it (second save wins), or add a
   `create_new` reservation file at allocation? My answer: accept it;
   the TUI's ids are per-conversation and the race needs two processes
   starting in the same millisecond on the same workspace.

---

## 14. Acceptance

- `cargo test -p rhema-session-store --features store-contract`,
  `cargo test -p rhema-session-store-file`, and
  `cargo test -p rhema-session-store-sqlite` pass.
- `cargo tree -p rhema-code | rg rusqlite` exits 1 (no match): the
  TUI's graph never grows the SQLite backend.
- `cargo test -p rhema-code`, `-p rhema-acp`, and
  `-p rhema-host-agent` stay green.
- ACP crash test: kill the provider after a tool call; the file ends
  with the committed call, and a reopened agent closes it through
  `revive_dangling_calls`.
- A session file written by the pre-migration code loads through
  `FileStore`, `PathStore`, and appears in `list`.
- `--resume` and `--save` behave identically, including the
  "cannot resume" error on a missing path.
- No session write in the tree bypasses the crates; no `private_write`
  on session bytes remains.
- The 2 MB and 15.16 MB fsync measurements are in the PR body.
