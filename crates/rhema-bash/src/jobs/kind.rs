//! The `job` submission kind: a background shell as a durable record.
//!
//! `job run` is a shell function, so there is no Rust point at which the
//! parent session can be written. The work's state is the daemon's, and
//! the record is the index that points at it. Two things use that:
//!
//! - the frontend's poller submits a record the first time it sees a
//!   running row for its tag, and the record settles when the row ends;
//! - [`Kind::adopt`] rebuilds what the ledger missed from one listing at
//!   load, and [`Kind::recover`] asks the listing again for each stale
//!   record.
//!
//! A recovered `Done` never starts a second copy: the daemon already
//! ran it. A running row is `Interrupted`, which tells the driver to
//! re-attach its poller, not to start the job again.

use std::path::PathBuf;

use rhema_submissions::{AdoptCtx, Kind, RecoverCtx, Recovery};
use serde::{Deserialize, Serialize};

/// The kind tag of a background job.
pub const NAME: &str = "job";

/// A submission of kind `job`: where the work runs, and what it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    /// The session identity the job runs under.
    pub tag: String,
    /// The backend's short id.
    pub id: String,
    /// The shell line as the model wrote it.
    pub command: String,
}

/// The `job` kind, registered by a frontend that knows the session tag
/// its shell starts jobs under.
pub struct JobKind {
    /// The tag this session's `job run` carries.
    tag: String,
    /// The backend the listing asks; `gob` on `PATH` in production.
    backend: PathBuf,
    /// The directory the listing runs in. The session scope lists every
    /// directory, so this only names a working directory for the child.
    cwd: PathBuf,
}

impl JobKind {
    /// The kind for `tag`, reading the installed backend.
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            backend: PathBuf::from(super::BACKEND),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }

    /// [`Self::new`] over an explicit backend and directory: the test
    /// seam, and the shape a peer process reads the listing with.
    pub fn at(tag: impl Into<String>, backend: PathBuf, cwd: PathBuf) -> Self {
        Self {
            tag: tag.into(),
            backend,
            cwd,
        }
    }

    /// The rows the daemon lists for one tag.
    fn listing(&self, tag: &str) -> Result<Vec<super::JobRow>, super::ListingError> {
        super::rows_for(&self.backend, &self.cwd, tag)
    }
}

impl Kind for JobKind {
    fn name(&self) -> &'static str {
        NAME
    }

    fn recover(&self, ctx: &RecoverCtx<'_>) -> Recovery {
        let Ok(payload) = ctx.payload::<Payload>() else {
            return Recovery::Interrupted;
        };
        let rows = match self.listing(&payload.tag) {
            Ok(rows) => rows,
            Err(super::ListingError::NoBackend) => {
                return Recovery::Failed {
                    error: "no job backend".to_owned(),
                };
            }
            // A listing that failed for any other reason is no proof
            // the job died: leave it for the next load to ask again.
            Err(super::ListingError::Failed(_)) => return Recovery::Interrupted,
        };
        match rows.iter().find(|row| row.id == payload.id) {
            Some(row) if row.status.starts_with("running") => Recovery::Interrupted,
            Some(_) => Recovery::Done,
            // Reaped or lost: the listing keeps no row, and today's
            // live feed reads the same absence as a finish.
            None => Recovery::Done,
        }
    }

    /// Adopts only the tag's `running` rows nobody tracks: a running
    /// row is live work that mints and re-attaches the poller, while
    /// a `stopped` row nobody tracked is history and mints nothing,
    /// the live subscription's own rule. A fresh `Running` record of
    /// an already-stopped row would settle the moment it recovered,
    /// a phantom finish announced at resume for a job that ended
    /// while nobody watched. A saved `Running` record is different:
    /// it keeps its own settle at load, the down-time announce, which
    /// is the intended one.
    fn adopt(&self, ctx: &AdoptCtx<'_>) -> Vec<serde_json::Value> {
        let Ok(rows) = self.listing(&self.tag) else {
            return Vec::new();
        };
        let known: Vec<String> = ctx
            .snapshot
            .iter()
            .filter(|record| record.kind.as_str() == NAME)
            .filter_map(|record| {
                serde_json::from_value::<Payload>(record.payload.clone())
                    .ok()
                    .map(|payload| payload.id)
            })
            .collect();
        rows.into_iter()
            .filter(|row| !known.contains(&row.id))
            .filter(|row| row.session.as_deref() == Some(self.tag.as_str()))
            .filter(|row| row.status.starts_with("running"))
            .map(|row| {
                serde_json::to_value(Payload {
                    tag: self.tag.clone(),
                    id: row.id,
                    command: row.command,
                })
                .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }
}
