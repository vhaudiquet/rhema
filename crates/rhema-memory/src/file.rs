//! The durable [`MemoryStore`]: one markdown file per record, in a flat
//! directory the consumer chooses.
//!
//! ```text
//! root/
//!   20261008T113456-00000000.md
//!   20261008T113457-00000001.md
//! ```
//!
//! Each file is YAML frontmatter followed by the body verbatim:
//!
//! ```yaml
//! ---
//! id: 20261008T113456-00000000
//! scope: Global
//! tags: [commits, convention]
//! created_at: 1759923296123
//! updated_at: 1759923296123
//! ---
//! Commit messages follow Conventional Commits with component scopes.
//! ```
//!
//! The id is the filename, so `ls` reads chronologically and a
//! model-supplied id can never name a path outside the root
//! ([`check_id`](crate::record) guards every entry point). Scope lives in
//! the frontmatter rather than the directory tree, so re-scoping is a
//! rewrite, not a move. A stray `---` line in a body cannot be confused
//! with the delimiter — the frontmatter ends at the *second* delimiter.
//!
//! Writes are atomic: a `put` writes to a `.tmp` sibling, `sync_all`s it,
//! and renames, so a `put` that returned `Ok` survives a crash. A crash
//! *during* a write can leave a `.tmp` behind; the `.md`-only scan ignores
//! it. Files a hand edit corrupted are skipped by `search` and reported
//! by `get`/`put` as [`MemoryError::Corrupt`] — never taken down — and
//! `forget` still removes them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::record::{IdCounter, MemoryDraft, MemoryRecord, Scope, check_id, normalize_tags};
use crate::search::rank;
use crate::store::{MemoryError, MemoryStore};

/// The durable [`MemoryStore`] over a directory of markdown records.
/// Shareable; every operation is serialized by an internal mutex, so all
/// of its mutations are atomic.
pub struct FileMemoryStore {
    root: PathBuf,
    ids: IdCounter,
    lock: Mutex<()>,
}

/// The frontmatter half of a record file; the body follows the second
/// delimiter.
#[derive(Serialize, Deserialize)]
struct RecordMeta {
    id: String,
    scope: Scope,
    tags: Vec<String>,
    created_at: u64,
    updated_at: u64,
}

impl FileMemoryStore {
    /// A store rooted at `root`, created if missing — fail fast on an
    /// unwritable root rather than at first write.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            ids: IdCounter::default(),
            lock: Mutex::new(()),
        })
    }

    /// A record file's path: the id is the filename.
    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.md"))
    }

    /// A fresh id whose file does not exist yet — the counter is
    /// process-local, so a second lifetime in the same wall-clock second
    /// retries through here.
    fn fresh_id(&self, now: SystemTime) -> String {
        loop {
            let candidate = self.ids.fresh(now);
            if !self.path(&candidate).exists() {
                return candidate;
            }
        }
    }
}

fn now_millis(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Renders a record to its file text: frontmatter, then the body
/// verbatim — round-trips are exact, so no newline is added or stripped.
fn serialize(record: &MemoryRecord) -> Result<String, MemoryError> {
    let meta = RecordMeta {
        id: record.id.clone(),
        scope: record.scope.clone(),
        tags: record.tags.clone(),
        created_at: record.created_at,
        updated_at: record.updated_at,
    };
    let yaml =
        serde_yaml::to_string(&meta).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let yaml = yaml.strip_prefix("---\n").unwrap_or(&yaml);
    Ok(format!("---\n{yaml}---\n{}", record.body))
}

/// Writes through a `.tmp` sibling, `sync_all`ed, then renamed — the
/// rename is the moment the write becomes durable.
fn write_file(path: &Path, record: &MemoryRecord) -> Result<(), MemoryError> {
    let text = serialize(record)?;
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, text)?;
    let file = fs::File::open(&tmp)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Parses a record file. `Corrupt` says what was wrong; `search` skips
/// these, `get` and `put` report them.
fn read_file(path: &Path) -> Result<MemoryRecord, MemoryError> {
    let text = fs::read_to_string(path)?;
    let corrupt = |reason: &str| MemoryError::Corrupt {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    };
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| corrupt("missing frontmatter"))?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| corrupt("unterminated frontmatter"))?;
    let meta: RecordMeta = serde_yaml::from_str(yaml).map_err(|e| corrupt(&e.to_string()))?;
    check_id(&meta.id).map_err(|e| corrupt(&e.to_string()))?;
    if body.trim().is_empty() {
        return Err(corrupt("empty body"));
    }
    Ok(MemoryRecord {
        id: meta.id,
        scope: meta.scope,
        tags: meta.tags,
        body: body.to_owned(),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    })
}

impl MemoryStore for FileMemoryStore {
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, MemoryError> {
        draft.validate()?;
        let tags = normalize_tags(&draft.tags);
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let now = SystemTime::now();
        let now_ms = now_millis(now);
        let (id, created_at) = match &draft.id {
            Some(id) => {
                let path = self.path(id);
                if !path.exists() {
                    return Err(MemoryError::UnknownId { id: id.clone() });
                }
                // `created_at` is the one field an update may not touch;
                // it lives only in the existing file.
                (id.clone(), read_file(&path)?.created_at)
            }
            None => (self.fresh_id(now), now_ms),
        };
        let record = MemoryRecord {
            id: id.clone(),
            scope: draft.scope,
            tags,
            body: draft.body,
            created_at,
            updated_at: now_ms,
        };
        write_file(&self.path(&id), &record)?;
        Ok(record)
    }

    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        check_id(id)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.path(id);
        if !path.exists() {
            return Ok(None);
        }
        read_file(&path).map(Some)
    }

    fn search(&self, query: crate::search::MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            match read_file(&path) {
                Ok(record) => records.push(record),
                // A hand-edited file must not take search down; it is
                // simply invisible until fixed.
                Err(MemoryError::Corrupt { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(rank(records, &query))
    }

    fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        check_id(id)?;
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
