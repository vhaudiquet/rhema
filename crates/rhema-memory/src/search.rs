//! The v1 ranker: keyword match + recency.
//!
//! Crate-internal on purpose — [`crate::MemoryStore::search`] promises
//! ranked-best-first and nothing about the method, so a semantic ranker
//! replaces this file without touching a signature.

use crate::record::{MemoryRecord, Scope, normalize_tags};

/// `search` returns at most this many records; [`MemoryQuery::with_limit`]
/// clamps to it.
pub const SEARCH_LIMIT: u32 = 8;

/// What to look for. An empty `text` means "recent"; empty `scopes` means
/// every scope; every `tags` entry must match.
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
}

/// Filters and ranks `records` for `query`: scope and tag filters first,
/// then score (a tag hit outweighs a body hit), ties broken by
/// `updated_at` descending and finally id — deterministic, and id order
/// is creation order.
pub(crate) fn rank(records: Vec<MemoryRecord>, query: &MemoryQuery) -> Vec<MemoryRecord> {
    let terms: Vec<String> = query
        .text
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut scored: Vec<(u32, MemoryRecord)> = records
        .into_iter()
        .filter(|record| query.scopes.is_empty() || query.scopes.contains(&record.scope))
        .filter(|record| query.tags.iter().all(|tag| record.tags.contains(tag)))
        .filter_map(|record| score(&record, &terms).map(|points| (points, record)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
            .then_with(|| b.1.id.cmp(&a.1.id))
    });
    scored.truncate(query.limit as usize);
    scored.into_iter().map(|(_, record)| record).collect()
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

    fn record(id: &str, updated_at: u64, body: &str, tags: &[&str]) -> MemoryRecord {
        MemoryRecord {
            id: id.to_owned(),
            scope: Scope::Global,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            body: body.to_owned(),
            created_at: 0,
            updated_at,
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
}
