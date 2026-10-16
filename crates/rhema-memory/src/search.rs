//! The v1 ranker: keyword match + recency, under the validity rules.
//!
//! Crate-internal on purpose — [`crate::MemoryStore::search`] promises
//! ranked-best-first and nothing about the method, so a semantic ranker
//! replaces the scoring without touching a signature. What must survive
//! that swap lives outside it, in [`select`]: the default visibility
//! rules (active, inside the validity window) and the guarantee that a
//! dead record never outranks a live one.

use crate::record::{MemoryRecord, Scope, normalize_tags};

/// `search` returns at most this many records; [`MemoryQuery::with_limit`]
/// clamps to it.
pub const SEARCH_LIMIT: u32 = 8;

/// What to look for. An empty `text` means "recent"; empty `scopes` means
/// every scope; every `tags` entry must match.
///
/// Visibility defaults to the rules a consumer almost always wants: only
/// records that are active and inside their validity window, at now or at
/// `as_of`. [`MemoryQuery::include_superseded`] widens the status rule;
/// [`MemoryQuery::fresh_only`] narrows it to unexpired decay intervals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryQuery {
    /// Keywords, matched case-insensitively against bodies and tags; every
    /// term must appear. Empty matches everything.
    pub text: String,
    /// The scopes to search; empty means all of them.
    pub scopes: Vec<Scope>,
    /// Tags the record must carry, all of them (normalized like stored
    /// tags).
    pub tags: Vec<String>,
    /// Result cap, clamped to [`SEARCH_LIMIT`].
    pub limit: u32,
    /// The instant to search *as of*, in millis — retrieval as a pure
    /// function of (query, t), the debugging/replay primitive. `None`
    /// means now.
    pub as_of: Option<u64>,
    /// Whether superseded and retracted records may answer. They rank
    /// below any active record regardless.
    pub include_superseded: bool,
    /// Whether records past their decay interval are dropped.
    pub fresh_only: bool,
}

impl MemoryQuery {
    /// A query for `text` over every scope, no tag filter, at the default
    /// limit.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            scopes: Vec::new(),
            tags: Vec::new(),
            limit: SEARCH_LIMIT,
            as_of: None,
            include_superseded: false,
            fresh_only: false,
        }
    }

    /// Restricts the search to `scopes`; empty restores every scope.
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = Scope>) -> Self {
        self.scopes.extend(scopes);
        self
    }

    /// Requires every tag.
    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let tags: Vec<String> = tags.into_iter().map(Into::into).collect();
        self.tags = normalize_tags(&tags);
        self
    }

    /// Caps the result count, clamped to [`SEARCH_LIMIT`].
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit.min(SEARCH_LIMIT);
        self
    }

    /// Searches as of `at` instead of now: what would this query have
    /// returned then?
    pub fn as_of(mut self, at: u64) -> Self {
        self.as_of = Some(at);
        self
    }

    /// Lets superseded and retracted records answer; they still rank
    /// below any active record.
    pub fn include_superseded(mut self) -> Self {
        self.include_superseded = true;
        self
    }

    /// Drops records whose decay interval has lapsed.
    pub fn fresh_only(mut self) -> Self {
        self.fresh_only = true;
        self
    }
}

/// Applies the visibility rules at the query's effective instant, then
/// ranks. The stores call this; a ranker swap replaces the scoring inside
/// [`rank`] and inherits the rules.
pub(crate) fn select(
    records: Vec<MemoryRecord>,
    query: &MemoryQuery,
    now: u64,
) -> Vec<MemoryRecord> {
    let t = query.as_of.unwrap_or(now);
    let visible = records
        .into_iter()
        // The window rule always holds; the status rule is the part
        // `include_superseded` widens.
        .filter(|record| {
            record.in_window(t) && (query.include_superseded || record.is_active_at(t))
        })
        .filter(|record| !query.fresh_only || !record.is_stale(t))
        .collect();
    rank(visible, query)
}

impl MemoryRecord {
    /// The validity window half of visibility, without the status rule:
    /// the record existed at `t`, had started to hold, and its horizon
    /// had not passed.
    fn in_window(&self, t: u64) -> bool {
        self.created_at <= t
            && self.valid_from <= t
            && self.valid_until.is_none_or(|until| until > t)
    }
}

/// Filters and ranks `records` for `query`: scope and tag filters first,
/// then score (a tag hit outweighs a body hit), ties broken by
/// `updated_at` descending and finally id — deterministic, and id order
/// is creation order. A dead record ranks below every active one: keyword
/// hits on dead facts are a known trap, and `include_superseded` must not
/// turn into a resurrection.
pub(crate) fn rank(records: Vec<MemoryRecord>, query: &MemoryQuery) -> Vec<MemoryRecord> {
    let terms: Vec<String> = query
        .text
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut scored: Vec<(u32, bool, MemoryRecord)> = records
        .into_iter()
        .filter(|record| query.scopes.is_empty() || query.scopes.contains(&record.scope))
        .filter(|record| query.tags.iter().all(|tag| record.tags.contains(tag)))
        .filter_map(|record| {
            score(&record, &terms).map(|points| (points, record.status.is_dead(), record))
        })
        .collect();
    scored.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| b.0.cmp(&a.0))
            .then_with(|| b.2.updated_at.cmp(&a.2.updated_at))
            .then_with(|| b.2.id.cmp(&a.2.id))
    });
    scored.truncate(query.limit as usize);
    scored.into_iter().map(|(_, _, record)| record).collect()
}

/// The match score, or `None` when some term misses entirely. An empty
/// term list matches everything at zero — "recent".
fn score(record: &MemoryRecord, terms: &[String]) -> Option<u32> {
    if terms.is_empty() {
        return Some(0);
    }
    let body = record.body.to_lowercase();
    let mut total = 0;
    for term in terms {
        let tag_hit = record.tags.iter().any(|tag| tag.contains(term));
        let body_hit = body.contains(term);
        if !tag_hit && !body_hit {
            return None;
        }
        total += u32::from(tag_hit) * 2 + u32::from(body_hit);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Decay, Source, Status};

    fn record(id: &str, updated_at: u64, body: &str, tags: &[&str]) -> MemoryRecord {
        MemoryRecord {
            id: id.to_owned(),
            scope: Scope::Global,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            body: body.to_owned(),
            created_at: 0,
            updated_at,
            valid_from: 0,
            valid_until: None,
            decay: Decay::Permanent,
            source: Source::default(),
            subject: None,
            status: Status::Active,
            verified_at: None,
            verify_hint: None,
        }
    }

    fn ids(records: Vec<MemoryRecord>) -> Vec<String> {
        records.into_iter().map(|record| record.id).collect()
    }

    #[test]
    fn an_empty_query_means_recent() {
        let records = vec![
            record("a", 1, "old", &[]),
            record("b", 9, "new", &[]),
            record("c", 5, "mid", &[]),
        ];
        assert_eq!(ids(rank(records, &MemoryQuery::new(""))), ["b", "c", "a"]);
    }

    #[test]
    fn equal_recency_breaks_ties_by_id() {
        let records = vec![record("a", 7, "x", &[]), record("b", 7, "y", &[])];
        assert_eq!(ids(rank(records, &MemoryQuery::new(""))), ["b", "a"]);
    }

    #[test]
    fn a_term_miss_excludes_the_record() {
        let records = vec![record("a", 1, "deploys", &[]), record("b", 2, "tests", &[])];
        assert_eq!(ids(rank(records, &MemoryQuery::new("deploy"))), ["a"]);
    }

    #[test]
    fn a_tag_hit_outranks_a_body_hit_even_when_older() {
        let records = vec![
            record("old-tagged", 1, "unrelated", &["deploy"]),
            record("new-body", 9, "deploy", &[]),
        ];
        assert_eq!(
            ids(rank(records, &MemoryQuery::new("deploy"))),
            ["old-tagged", "new-body"]
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_multiplicative() {
        let records = vec![
            record("both", 1, "Deploy the DEPLOYS", &["deploy"]),
            record("body", 2, "deploy", &[]),
        ];
        assert_eq!(
            ids(rank(records, &MemoryQuery::new("DEPLOY"))),
            ["both", "body"]
        );
    }

    #[test]
    fn scope_and_tag_filters_apply_before_ranking() {
        let mut tagged = record("tagged", 1, "content", &["wanted"]);
        tagged.scope = Scope::Workspace("/w".into());
        let global = record("global", 9, "content", &[]);
        let query = MemoryQuery::new("content")
            .with_scopes([Scope::Global])
            .with_tags(["wanted"]);
        assert!(rank(vec![tagged, global], &query).is_empty());
    }

    #[test]
    fn the_limit_truncates_after_ranking() {
        let records = vec![
            record("a", 1, "wasm", &[]),
            record("b", 3, "wasm", &[]),
            record("c", 2, "wasm", &[]),
        ];
        let query = MemoryQuery::new("wasm").with_limit(2);
        assert_eq!(ids(rank(records, &query)), ["b", "c"]);
    }

    #[test]
    fn every_query_term_must_appear() {
        let records = vec![
            record("one", 1, "rust and wasm", &[]),
            record("partial", 9, "rust only", &[]),
        ];
        assert_eq!(ids(rank(records, &MemoryQuery::new("rust wasm"))), ["one"]);
    }

    #[test]
    fn a_dead_record_never_outranks_a_live_one() {
        let mut old = record("old", 5, "deploy", &[]);
        old.status = Status::Superseded {
            by: "new".to_owned(),
            at: 9,
        };
        let fresh = record("new", 1, "deploy", &[]);
        let found = rank(
            vec![old, fresh],
            &MemoryQuery::new("deploy").include_superseded(),
        );
        assert_eq!(ids(found), ["new", "old"]);
    }

    #[test]
    fn select_applies_the_default_visibility_rules() {
        let day = crate::record::DAY_MILLIS;
        let mut expired = record("expired", 1, "quote", &[]);
        expired.valid_until = Some(day);
        let mut dead = record("dead", 1, "veto", &[]);
        dead.status = Status::Retracted {
            why: "wrong".to_owned(),
            at: day,
        };
        let mut future = record("future", 1, "plan", &[]);
        future.valid_from = 2 * day;
        let live = record("live", 1, "stable", &[]);

        let found = select(
            vec![expired.clone(), dead.clone(), future.clone(), live.clone()],
            &MemoryQuery::new(""),
            day,
        );
        assert_eq!(ids(found), ["live"]);

        let widened = select(
            vec![expired.clone(), dead.clone(), future.clone(), live.clone()],
            &MemoryQuery::new("").include_superseded(),
            day,
        );
        assert_eq!(
            ids(widened),
            ["live", "dead"],
            "the window rule holds even widened; the dead one ranks below"
        );

        let replayed = select(
            vec![expired, dead, future, live],
            &MemoryQuery::new("").as_of(day - 1),
            day,
        );
        // At day-1 all three existed, had started, and were still active
        // *at that instant* — the set is what as_of owes; the order among
        // them stays the ranker's business.
        assert_eq!(replayed.len(), 3);
        assert!(!ids(replayed).contains(&"future".to_owned()));
    }
}
