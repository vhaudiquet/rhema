//! The memory data: records, scopes, drafts — and the small shared pieces
//! every store composes (validation, tag normalization, id assignment,
//! UTC rendering).
//!
//! Timestamps are milliseconds since the Unix epoch, the framework's
//! convention (`rhema-rsd`'s `Clock`); UTC text exists only at display
//! edges, rendered by [`format_utc`].
//!
//! A record also carries its temporal facts: how long it claims to hold
//! ([`MemoryRecord::valid_from`] / [`MemoryRecord::valid_until`]), how
//! soon it should be doubted again ([`Decay`]), who or what produced it
//! ([`Source`]), what it is about ([`MemoryRecord::subject`]), whether it
//! still holds ([`Status`]) and how to re-check it ([`VerifyHint`]). None
//! of these is a truth value: age only ever says *unverified*, and only a
//! contradicting fact or a retraction retires a record.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::MemoryError;

/// A record body is cut at this many bytes.
pub const BODY_LIMIT: usize = 4 * 1024;

/// One day in milliseconds — the unit decay intervals are written in.
pub const DAY_MILLIS: u64 = 86_400_000;

/// The review interval of [`Decay::Long`], in days.
pub const LONG_DECAY_DAYS: u64 = 365;

/// The review interval of [`Decay::Volatile`], in days.
pub const VOLATILE_DECAY_DAYS: u64 = 30;

/// How long a record stays trusted before it is due for a re-check. Decay
/// never decides truth: a lapsed interval makes a record *due*, not dead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decay {
    /// Never due on its own: commitments, identity, rules.
    Permanent,
    /// Due once a year: projects, accounts, policies.
    Long,
    /// Due monthly: infrastructure, setup, environment, migration notes.
    Volatile,
}

impl Decay {
    /// The review interval in milliseconds, or `None` when the record is
    /// never due on its own.
    pub fn interval(self) -> Option<u64> {
        match self {
            Decay::Permanent => None,
            Decay::Long => Some(LONG_DECAY_DAYS * DAY_MILLIS),
            Decay::Volatile => Some(VOLATILE_DECAY_DAYS * DAY_MILLIS),
        }
    }
}

/// Tags that mark a record permanent: facts about people, preferences,
/// rules.
const PERMANENT_TAGS: [&str; 5] = ["family", "personal", "preferences", "rules", "identity"];
/// Tags that mark a record long-lived but checkable.
const LONG_TAGS: [&str; 3] = ["projects", "accounts", "policies"];
/// Tags that mark a record as describing a mutable environment.
const VOLATILE_TAGS: [&str; 4] = ["infrastructure", "setup", "environment", "migration"];

/// Derives the decay from a record's normalized tags, so records written
/// without an explicit decay still get a review interval. When tags from
/// several classes mix, the most volatile class wins — doubt over
/// silence; anything unclassified degrades to [`Decay::Long`].
pub fn decay_from_tags(tags: &[String]) -> Decay {
    let has = |names: &[&str]| tags.iter().any(|tag| names.contains(&tag.as_str()));
    if has(&VOLATILE_TAGS) {
        Decay::Volatile
    } else if has(&LONG_TAGS) {
        Decay::Long
    } else if has(&PERMANENT_TAGS) {
        Decay::Permanent
    } else {
        Decay::Long
    }
}

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

/// Who or what produced a fact. Provenance shapes doubt; it is not a
/// truth value — a told fact can be shaky, an observed one wrong.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A person or agent stated it.
    Told {
        /// Who stated it.
        by: String,
    },
    /// The agent saw it itself, through a command, a file, a fetch.
    Observed {
        /// How it was observed, e.g. `git ls-remote`.
        how: String,
    },
    /// The agent worked it out; `from` names what it was derived from,
    /// `"unknown"` when nobody can say.
    Inferred {
        /// What the fact was derived from.
        from: String,
    },
}

impl Default for Source {
    fn default() -> Self {
        Source::Inferred {
            from: "unknown".to_owned(),
        }
    }
}

impl Source {
    /// Whether this is the fallback provenance written when nobody
    /// declared where a fact came from.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Source::Inferred { from } if from == "unknown")
    }

    /// One-line description for display edges.
    pub fn describe(&self) -> String {
        match self {
            Source::Told { by } => format!("told by {by}"),
            Source::Observed { how } => format!("observed via {how}"),
            Source::Inferred { from } => format!("inferred from {from}"),
        }
    }
}

/// Whether a record still holds. Only supersession or retraction moves a
/// record out of [`Status::Active`]; time never does — a past horizon
/// makes a record due and hides it, but does not kill it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    /// Believed — not superseded, not retracted.
    Active,
    /// Replaced by a newer record about the same subject. The body stays;
    /// what was believed, and therefore done, stays readable.
    Superseded {
        /// The id of the record that replaced this one.
        by: String,
        /// Millis since the Unix epoch; when the replacement happened.
        at: u64,
    },
    /// Killed by a human judgement, with the reason kept.
    Retracted {
        /// Why it no longer holds.
        why: String,
        /// Millis since the Unix epoch; when the retraction happened.
        at: u64,
    },
}

impl Status {
    /// Whether the record is out of [`Status::Active`]. Dead records stay
    /// reachable (`include_superseded`, `as_of`) but never answer a
    /// plain query ahead of a live one.
    pub fn is_dead(&self) -> bool {
        !matches!(self, Status::Active)
    }
}

/// How to re-check a fact without asking a human. This is the difference
/// between a hygiene system and a nag: a sweep with hints re-checks most
/// records itself, and only [`VerifyHint::Manual`] reaches a person —
/// batched, not streamed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyHint {
    /// A command whose exit status says whether the fact still holds
    /// (exit 0: still true).
    Command {
        /// The command to run.
        run: String,
    },
    /// A page to fetch and compare against the body.
    Url {
        /// The URL to fetch.
        fetch: String,
    },
    /// A file to read and compare against the body.
    File {
        /// The path to read.
        path: PathBuf,
    },
    /// Only a human can confirm; this is what generates a question.
    Manual,
}

impl VerifyHint {
    /// Instruction text for display edges.
    pub fn describe(&self) -> String {
        match self {
            VerifyHint::Command { run } => format!("re-check by running `{run}`"),
            VerifyHint::Url { fetch } => format!("re-check by fetching {fetch}"),
            VerifyHint::File { path } => format!("re-check by reading {}", path.display()),
            VerifyHint::Manual => "only a human can confirm this".to_owned(),
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
    /// Millis since the Unix epoch; refreshed on every write.
    pub updated_at: u64,
    /// When the fact started to hold; defaults to `created_at`, and may
    /// name the past ("moved in last month") or the future.
    pub valid_from: u64,
    /// When the fact is *planned* to stop holding — a horizon, not a
    /// verdict. Passing it makes the record due and hides it from default
    /// search; only supersession or retraction kills it.
    pub valid_until: Option<u64>,
    /// How soon the fact should be re-checked.
    pub decay: Decay,
    /// Where the fact came from.
    pub source: Source,
    /// What the record is about, as a namespaced key
    /// (`home/accommodation`) — the overlap key contradiction detection
    /// matches on, exactly.
    pub subject: Option<String>,
    /// Whether the record still holds.
    pub status: Status,
    /// The last re-confirmation, by anyone; `None` when never re-checked.
    pub verified_at: Option<u64>,
    /// How to re-check without asking a human.
    pub verify_hint: Option<VerifyHint>,
}

impl MemoryRecord {
    /// The instant the current belief dates from: the last verification,
    /// or creation when the record was never re-checked. Age is measured
    /// from here, and it is never a truth value.
    pub fn believed_since(&self) -> u64 {
        self.verified_at.unwrap_or(self.created_at)
    }

    /// Days the current belief has gone unverified, at `t`.
    pub fn age_days(&self, t: u64) -> u64 {
        t.saturating_sub(self.believed_since()) / DAY_MILLIS
    }

    /// When the decay interval lapses, or `None` for
    /// [`Decay::Permanent`].
    pub fn stale_at(&self) -> Option<u64> {
        Some(self.believed_since().saturating_add(self.decay.interval()?))
    }

    /// Whether the decay interval has lapsed at `t`. The record is then
    /// *unverified*, never false.
    pub fn is_stale(&self, t: u64) -> bool {
        self.stale_at().is_some_and(|at| at < t)
    }

    /// Whether the record is due for review at `t`: still active, and
    /// either past its decay interval or past its `valid_until` horizon.
    pub fn is_due(&self, t: u64) -> bool {
        if self.status.is_dead() {
            return false;
        }
        self.is_stale(t) || self.valid_until.is_some_and(|until| until <= t)
    }

    /// Whether the record counted as active at `t`. Supersession and
    /// retraction carry the instant they happened, so an `as_of` query
    /// sees through them: a record marked superseded tomorrow was still
    /// believed today.
    pub fn is_active_at(&self, t: u64) -> bool {
        match &self.status {
            Status::Active => true,
            Status::Superseded { at, .. } | Status::Retracted { at, .. } => *at > t,
        }
    }

    /// Whether the default search rules surface the record at `t`: it
    /// existed and had started to hold, its horizon had not passed, and
    /// it was active. Time-travel-safe, so retrieval is a pure function
    /// of (query, t).
    pub fn visible_at(&self, t: u64) -> bool {
        self.created_at <= t
            && self.valid_from <= t
            && self.valid_until.is_none_or(|until| until > t)
            && self.is_active_at(t)
    }
}

/// What a caller supplies to [`crate::MemoryStore::put`]. `id: None`
/// creates; `Some` replaces that record — `created_at` preserved, scope
/// re-aimed.
///
/// On an update, every temporal field follows one rule: an explicit value
/// replaces, an absent one carries over — except `decay`, re-derived from
/// the tags unless declared, and `verified_at`, voided when the body
/// changes, because the new wording has not been checked by anyone. The
/// status never changes through `put`: an edit does not resurrect a dead
/// record.
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
    /// When the fact starts to hold; `None` means creation time on a
    /// create, and "when the body last changed" on an update.
    pub valid_from: Option<u64>,
    /// When the fact is planned to stop holding.
    pub valid_until: Option<u64>,
    /// The review interval; `None` derives it from the tags.
    pub decay: Option<Decay>,
    /// Where the fact came from; `None` keeps the existing provenance, or
    /// [`Source::default`] on a create.
    pub source: Option<Source>,
    /// The overlap key; must fit [`normalize_subject`].
    pub subject: Option<String>,
    /// How to re-check the fact.
    pub verify_hint: Option<VerifyHint>,
}

impl MemoryDraft {
    /// A draft creating a record in `scope`.
    pub fn new(scope: Scope, body: impl Into<String>) -> Self {
        Self {
            id: None,
            scope,
            tags: Vec::new(),
            body: body.into(),
            valid_from: None,
            valid_until: None,
            decay: None,
            source: None,
            subject: None,
            verify_hint: None,
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

    /// Sets when the fact starts to hold.
    pub fn with_valid_from(mut self, at: u64) -> Self {
        self.valid_from = Some(at);
        self
    }

    /// Sets the planned horizon.
    pub fn with_valid_until(mut self, at: u64) -> Self {
        self.valid_until = Some(at);
        self
    }

    /// Sets the review interval explicitly, instead of deriving it from
    /// the tags.
    pub fn with_decay(mut self, decay: Decay) -> Self {
        self.decay = Some(decay);
        self
    }

    /// Sets the provenance.
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    /// Sets the overlap key. It must fit [`normalize_subject`] or `put`
    /// rejects the draft.
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Sets how the fact gets re-checked.
    pub fn with_verify_hint(mut self, hint: VerifyHint) -> Self {
        self.verify_hint = Some(hint);
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
        if let Some(subject) = &self.subject {
            validate_subject(subject)?;
        }
        Ok(())
    }
}

/// Composes the record a `put` writes, applying the draft-over-existing
/// rule documented on [`MemoryDraft`]. Both bundled stores go through
/// here, so their write semantics cannot drift apart.
pub(crate) fn compose(
    draft: &MemoryDraft,
    tags: Vec<String>,
    existing: Option<MemoryRecord>,
    id: String,
    created_at: u64,
    now_ms: u64,
) -> MemoryRecord {
    match existing {
        None => {
            let (valid_from, decay, source, status) = with_defaults(
                draft.valid_from,
                created_at,
                draft.decay,
                &tags,
                draft.source.clone(),
                None,
            );
            MemoryRecord {
                id,
                scope: draft.scope.clone(),
                tags,
                body: draft.body.clone(),
                created_at,
                updated_at: now_ms,
                valid_from,
                valid_until: draft.valid_until,
                decay,
                source,
                subject: draft.subject.clone(),
                status,
                verified_at: None,
                verify_hint: draft.verify_hint.clone(),
            }
        }
        Some(existing) => {
            let body_changed = existing.body != draft.body;
            MemoryRecord {
                id,
                scope: draft.scope.clone(),
                tags: tags.clone(),
                body: draft.body.clone(),
                created_at: existing.created_at,
                updated_at: now_ms,
                valid_from: draft.valid_from.unwrap_or(if body_changed {
                    now_ms
                } else {
                    existing.valid_from
                }),
                valid_until: draft.valid_until.or(existing.valid_until),
                decay: draft.decay.unwrap_or_else(|| decay_from_tags(&tags)),
                source: draft
                    .source
                    .clone()
                    .unwrap_or_else(|| existing.source.clone()),
                subject: draft.subject.clone().or(existing.subject),
                status: existing.status,
                verified_at: if body_changed {
                    None
                } else {
                    existing.verified_at
                },
                verify_hint: draft.verify_hint.clone().or(existing.verify_hint),
            }
        }
    }
}

/// The reading defaults for fields an old record (or an old caller) never
/// wrote: validity starts at creation, decay follows the tag namespace,
/// provenance is unknown, status is active. An existing store therefore
/// reads unchanged, and each record enriches itself the next time it is
/// touched.
pub(crate) fn with_defaults(
    valid_from: Option<u64>,
    created_at: u64,
    decay: Option<Decay>,
    tags: &[String],
    source: Option<Source>,
    status: Option<Status>,
) -> (u64, Decay, Source, Status) {
    (
        valid_from.unwrap_or(created_at),
        decay.unwrap_or_else(|| decay_from_tags(tags)),
        source.unwrap_or_default(),
        status.unwrap_or(Status::Active),
    )
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

/// Normalizes a subject key: trimmed, lowercased. The convention is a
/// namespaced path (`home/accommodation`,
/// `account/github/kosmos-ai-bot`); namespacing keeps overlap detection
/// cheap and predictable, where free strings invite near-misses that
/// silently fail to supersede.
pub fn normalize_subject(raw: &str) -> Result<String, MemoryError> {
    let subject = raw.trim().to_lowercase();
    if validate_subject(&subject).is_ok() {
        Ok(subject)
    } else {
        Err(MemoryError::InvalidSubject {
            subject: raw.to_owned(),
        })
    }
}

/// The subject shape: non-empty, at most 128 chars, lowercase ASCII
/// letters, digits, and the `/`, `-`, `_` separators.
fn validate_subject(subject: &str) -> Result<(), MemoryError> {
    let ok = !subject.is_empty()
        && subject.len() <= 128
        && subject
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(MemoryError::InvalidSubject {
            subject: subject.to_owned(),
        })
    }
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

    fn record(decay: Decay) -> MemoryRecord {
        MemoryRecord {
            id: "20261013T000000-00000000".to_owned(),
            scope: Scope::Global,
            tags: vec![],
            body: "a fact".to_owned(),
            created_at: 0,
            updated_at: 0,
            valid_from: 0,
            valid_until: None,
            decay,
            source: Source::default(),
            subject: None,
            status: Status::Active,
            verified_at: None,
            verify_hint: None,
        }
    }

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

    #[test]
    fn decay_intervals_match_the_mapping() {
        assert_eq!(Decay::Permanent.interval(), None);
        assert_eq!(Decay::Long.interval(), Some(LONG_DECAY_DAYS * DAY_MILLIS));
        assert_eq!(
            Decay::Volatile.interval(),
            Some(VOLATILE_DECAY_DAYS * DAY_MILLIS)
        );
    }

    #[test]
    fn decay_follows_the_tag_namespace_with_volatile_winning() {
        let tags: Vec<String> = ["a", "b"].map(Into::into).to_vec();
        assert_eq!(decay_from_tags(&tags), Decay::Long, "unclassified");
        assert_eq!(decay_from_tags(&[]), Decay::Long, "untagged");
        let permanent: Vec<String> = ["identity"].map(Into::into).to_vec();
        assert_eq!(decay_from_tags(&permanent), Decay::Permanent);
        let long: Vec<String> = ["identity", "accounts"].map(Into::into).to_vec();
        assert_eq!(decay_from_tags(&long), Decay::Long, "long beats permanent");
        let volatile: Vec<String> = ["identity", "migration"].map(Into::into).to_vec();
        assert_eq!(
            decay_from_tags(&volatile),
            Decay::Volatile,
            "volatile beats everything"
        );
    }

    #[test]
    fn subjects_normalize_and_hold_their_shape() {
        assert_eq!(
            normalize_subject(" Home/Accommodation ").unwrap(),
            "home/accommodation"
        );
        assert_eq!(
            normalize_subject("account/github/kosmos-ai-bot").unwrap(),
            "account/github/kosmos-ai-bot"
        );
        for bad in [
            "",
            "   ",
            "House Buying",
            "a/b.c",
            "Ã©lectron",
            &"x".repeat(129),
        ] {
            assert!(normalize_subject(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            MemoryDraft::new(Scope::Global, "ok")
                .with_subject("Has Spaces")
                .validate(),
            Err(MemoryError::InvalidSubject { .. })
        ));
    }

    #[test]
    fn staleness_counts_from_verification_and_never_kills() {
        let day = DAY_MILLIS;
        let mut r = record(Decay::Volatile);
        assert!(!r.is_stale(day), "30-day decay has not lapsed at day 1");
        assert!(r.is_stale(VOLATILE_DECAY_DAYS * day + 1));
        assert!(r.is_due(VOLATILE_DECAY_DAYS * day + 1));
        assert_eq!(r.age_days(VOLATILE_DECAY_DAYS * day + 1), 30);

        r.verified_at = Some(day);
        assert!(!r.is_stale(VOLATILE_DECAY_DAYS * day), "age restarts");
        assert!(r.is_stale(day + VOLATILE_DECAY_DAYS * day + 1));
        assert_eq!(r.age_days(day + VOLATILE_DECAY_DAYS * day), 30);

        r.verified_at = None;
        r.decay = Decay::Permanent;
        assert!(!r.is_stale(u64::MAX / 2), "permanent is never stale");
        assert!(!r.is_due(u64::MAX / 2));
    }

    #[test]
    fn a_dead_or_unborn_record_is_never_due() {
        let day = DAY_MILLIS;
        let mut r = record(Decay::Permanent);
        r.valid_until = Some(day);
        assert!(r.is_due(day), "a passed horizon is due, not dead");
        assert!(!r.visible_at(day), "but hidden from default search");
        assert!(r.visible_at(day - 1), "still believed the day before");

        r.status = Status::Superseded {
            by: "other".to_owned(),
            at: 2 * day,
        };
        assert!(!r.is_due(3 * day), "superseded records leave the sweep");
        assert!(r.is_active_at(day), "still believed at day 1");
        assert!(!r.is_active_at(3 * day), "dead after the supersession");
    }

    #[test]
    fn compose_creates_with_defaults_and_updates_selectively() {
        let scope = Scope::Global;
        let hint = VerifyHint::Manual;
        let draft = MemoryDraft::new(scope.clone(), "v1")
            .with_tags(["Infrastructure"])
            .with_subject("home/accommodation")
            .with_verify_hint(hint.clone())
            .with_valid_until(10_000);
        let tags = normalize_tags(&draft.tags);
        let created = compose(&draft, tags.clone(), None, "id".to_owned(), 100, 100);
        assert_eq!(created.valid_from, 100, "valid_from defaults to creation");
        assert_eq!(created.decay, Decay::Volatile, "derived from the tags");
        assert_eq!(created.source, Source::default());
        assert_eq!(created.status, Status::Active);
        assert_eq!(created.verified_at, None);
        assert_eq!(created.verify_hint, Some(hint));
        assert_eq!(created.subject.as_deref(), Some("home/accommodation"));

        // A retag keeps the verification and the valid_from; a body change
        // voids both.
        let update = MemoryDraft::new(scope.clone(), "v1").with_tags(["setup"]);
        let retagged = compose(
            &update,
            normalize_tags(&update.tags),
            Some(created.clone()),
            created.id.clone(),
            created.created_at,
            200,
        );
        assert_eq!(retagged.verified_at, None);
        assert_eq!(retagged.valid_from, 100, "unchanged body keeps validity");
        assert_eq!(retagged.decay, Decay::Volatile, "re-derived from the tags");

        let mut verified = created.clone();
        verified.verified_at = Some(150);
        let rewrite = MemoryDraft::new(scope, "v2");
        let rewritten = compose(
            &rewrite,
            normalize_tags(&[]),
            Some(verified),
            created.id.clone(),
            created.created_at,
            300,
        );
        assert_eq!(rewritten.verified_at, None, "new wording, no verification");
        assert_eq!(rewritten.valid_from, 300, "new wording holds from now");
        assert_eq!(rewritten.status, Status::Active);
        assert_eq!(rewritten.subject.as_deref(), Some("home/accommodation"));
    }
}
