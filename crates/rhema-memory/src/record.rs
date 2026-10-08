//! The memory data: records, scopes, drafts — and the small shared pieces
//! every store composes (validation, tag normalization, id assignment,
//! UTC rendering).
//!
//! Timestamps are milliseconds since the Unix epoch, the framework's
//! convention (`rhema-rsd`'s `Clock`); UTC text exists only at display
//! edges, rendered by [`format_utc`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::MemoryError;

/// A record body is cut at this many bytes.
pub const BODY_LIMIT: usize = 4 * 1024;

/// Where a record applies. The layering mirrors skills — global plus
/// per-workspace — minus shadowing: search returns both, labeled, because
/// for recall the model should *see* that a workspace fact overrides a
/// global one, not have the global one silently hidden.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// True wherever this deployment runs: user tone, commit conventions.
    Global,
    /// Scoped to one checkout, named by its canonical path. Consumers
    /// canonicalize; the store takes the scope as opaque data.
    Workspace(PathBuf),
}

impl Scope {
    /// A short label for display: `global`, or the workspace path.
    pub fn label(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Scope::Global => std::borrow::Cow::Borrowed("global"),
            Scope::Workspace(path) => path.to_string_lossy(),
        }
    }
}

/// One thing worth remembering: a preference, a fact about an
/// environment, a decision and its why. Atomic by design — search ranks
/// records, and a document-sized record is either a needle lost in hay or
/// hay burying the needle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Store-assigned: `{UTC second}-{counter:08x}`, so directory listings
    /// and id order both read chronologically.
    pub id: String,
    /// Where the record applies.
    pub scope: Scope,
    /// Normalized tags: trimmed, lowercased, deduplicated — the model's
    /// case and spacing must not split one tag in two.
    pub tags: Vec<String>,
    /// The fact, as markdown, capped at [`BODY_LIMIT`].
    pub body: String,
    /// Millis since the Unix epoch; assigned once at creation.
    pub created_at: u64,
    /// Millis since the Unix epoch; refreshed on every `put`.
    pub updated_at: u64,
}

/// What a caller supplies to [`crate::MemoryStore::put`]. `id: None`
/// creates; `Some` replaces that record — `created_at` preserved, scope
/// re-aimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDraft {
    /// `None` to create; `Some` an existing record to replace.
    pub id: Option<String>,
    /// Where the record applies (on an update, the new scope).
    pub scope: Scope,
    /// Freeform keywords, stored normalized.
    pub tags: Vec<String>,
    /// The fact, as markdown.
    pub body: String,
}

impl MemoryDraft {
    /// A draft creating a record in `scope`.
    pub fn new(scope: Scope, body: impl Into<String>) -> Self {
        Self {
            id: None,
            scope,
            tags: Vec::new(),
            body: body.into(),
        }
    }

    /// Sets the tags.
    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Targets an existing record for replacement.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The checks every store applies before touching its backend.
    pub(crate) fn validate(&self) -> Result<(), MemoryError> {
        if self.body.trim().is_empty() {
            return Err(MemoryError::EmptyBody);
        }
        if self.body.len() > BODY_LIMIT {
            return Err(MemoryError::BodyTooLong {
                len: self.body.len(),
                limit: BODY_LIMIT,
            });
        }
        if let Some(id) = &self.id {
            check_id(id)?;
        }
        Ok(())
    }
}

/// Tags are stored trimmed, lowercased, deduplicated (first occurrence
/// kept), and empty entries dropped.
pub(crate) fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim().to_lowercase();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

/// Ids are `{UTC second}-{counter:08x}`; this shape check keeps a
/// model-supplied id from naming a path outside the store — in the file
/// store the id *is* the filename.
pub(crate) fn check_id(id: &str) -> Result<(), MemoryError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(MemoryError::InvalidId { id: id.to_owned() })
    }
}

/// UTC text of `unix_millis`, to the second (`1970-01-01T00:00:00Z`).
/// Display-edge only — records carry millis, and this is what renders
/// them for humans.
pub fn format_utc(unix_millis: u64) -> String {
    let secs = (unix_millis / 1000) as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// The id's date half: compact UTC of `unix_millis`, to the second.
fn stamp_compact(unix_millis: u64) -> String {
    let secs = (unix_millis / 1000) as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// Days since the Unix epoch to `(year, month, day)` — Howard Hinnant's
/// `civil_from_days`, proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

/// The counter half of store-assigned ids: process-global and monotonic,
/// so fresh ids differ even when the clock's granularity would not. A
/// collision (counter wrap, or a second process lifetime in the same
/// second) lands on the store, which retries — stores refuse duplicate
/// ids.
#[derive(Debug, Default)]
pub(crate) struct IdCounter(AtomicU32);

impl IdCounter {
    /// A fresh id for `now`.
    pub(crate) fn fresh(&self, now: SystemTime) -> String {
        let n = self.0.fetch_add(1, Ordering::Relaxed);
        let millis = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        format!("{}-{n:08x}", stamp_compact(millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_rendering_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_759_923_296_123), "2025-10-08T11:34:56Z");
        assert_eq!(format_utc(1_709_168_523_000), "2024-02-29T01:02:03Z");
        assert_eq!(format_utc(951_868_800_000), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn compact_stamps_match_the_utc_rendering() {
        assert_eq!(stamp_compact(0), "19700101T000000");
        assert_eq!(stamp_compact(1_759_923_296_123), "20251008T113456");
        assert_eq!(stamp_compact(1_709_168_523_000), "20240229T010203");
    }

    #[test]
    fn scope_labels_are_display_ready() {
        assert_eq!(Scope::Global.label(), "global");
        assert_eq!(Scope::Workspace(PathBuf::from("/tmp/w")).label(), "/tmp/w");
    }

    #[test]
    fn tags_are_trimmed_lowercased_deduplicated_and_emptied() {
        let tags: Vec<String> = ["Commits", " commits ", "", "CONVENTION", "commits"]
            .map(Into::into)
            .to_vec();
        assert_eq!(normalize_tags(&tags), vec!["commits", "convention"]);
        assert_eq!(normalize_tags(&[]), Vec::<String>::new());
    }

    #[test]
    fn ids_accept_only_the_generated_shape() {
        assert!(check_id("20261008T113456-0000002a").is_ok());
        assert!(check_id("19700101T000000-00000000").is_ok());
        assert!(check_id("").is_err(), "empty");
        assert!(check_id("../escape").is_err(), "path separators");
        assert!(check_id("has_underscore").is_err(), "underscore");
        assert!(check_id("has space").is_err(), "space");
        assert!(check_id(&"x".repeat(65)).is_err(), "too long");
    }

    #[test]
    fn drafts_validate_body_and_id() {
        assert!(matches!(
            MemoryDraft::new(Scope::Global, "   ").validate(),
            Err(MemoryError::EmptyBody)
        ));
        let long = "x".repeat(BODY_LIMIT + 1);
        assert!(matches!(
            MemoryDraft::new(Scope::Global, long).validate(),
            Err(MemoryError::BodyTooLong {
                limit: BODY_LIMIT,
                ..
            })
        ));
        assert!(matches!(
            MemoryDraft::new(Scope::Global, "ok")
                .with_id("../bad")
                .validate(),
            Err(MemoryError::InvalidId { .. })
        ));
        assert!(MemoryDraft::new(Scope::Global, "ok").validate().is_ok());
    }

    #[test]
    fn fresh_ids_fit_the_shape_and_differ() {
        let counter = IdCounter::default();
        let now = UNIX_EPOCH + std::time::Duration::from_millis(1_759_923_296_123);
        let first = counter.fresh(now);
        let second = counter.fresh(now);
        assert_eq!(first, "20251008T113456-00000000");
        assert_eq!(second, "20251008T113456-00000001");
        for id in [&first, &second] {
            assert_eq!(id.len(), 24, "{id}");
            assert!(check_id(id).is_ok(), "{id}");
        }
    }
}
