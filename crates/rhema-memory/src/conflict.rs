//! Subject-overlap detection on the write path.
//!
//! The engine finds *what may conflict*; it never decides *what
//! replaces what*. A fuzzy match (tag overlap) alone must not
//! auto-supersede — a false positive there destroys history silently,
//! which is the one outcome worse than the bug being fixed. The caller
//! declares the relation for each conflict; only a declared supersession
//! rewrites the older record.

use crate::Scope;
use crate::record::MemoryRecord;
use crate::search::MemoryQuery;
use crate::store::{MemoryError, MemoryStore};

/// A saved record may collide with this existing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The existing active record.
    pub record: MemoryRecord,
    /// Whether the subject keys match exactly. `false` means the match is
    /// fuzzy: tag overlap over the same scope, no shared subject.
    pub exact: bool,
}

/// The share of the smaller tag set that must overlap for a fuzzy
/// match. A tuning knob, not a contract.
pub const TAG_OVERLAP_THRESHOLD: f32 = 0.5;

/// Finds the active records `subject` / `tags` may collide with, in
/// `scopes`. A conflict is a shared subject key (exact) or a tag overlap
/// of at least [`TAG_OVERLAP_THRESHOLD`] of the smaller set (fuzzy).
/// Records hidden by the default rules — expired horizons, dead status —
/// are not conflicts: they no longer answer queries, and `exclude` (the
/// id being updated, if any) never conflicts with itself. Exact subject
/// matches first, then most recently touched — the likeliest true
/// conflict.
pub fn find_conflicts(
    store: &dyn MemoryStore,
    scopes: Vec<Scope>,
    subject: Option<&str>,
    tags: &[String],
    exclude: Option<&str>,
) -> Result<Vec<Conflict>, MemoryError> {
    // No text, no cap: the sweep must see every active record in scope,
    // not the search page.
    let query = MemoryQuery {
        text: String::new(),
        scopes,
        tags: Vec::new(),
        limit: u32::MAX,
        as_of: None,
        include_superseded: false,
        fresh_only: false,
    };
    let mut conflicts: Vec<Conflict> = store
        .search(query)?
        .into_iter()
        .filter(|record| Some(record.id.as_str()) != exclude)
        .filter_map(|record| {
            let exact = subject.is_some() && record.subject.as_deref() == subject;
            let fuzzy = tags_overlap(tags, &record.tags);
            (exact || fuzzy).then_some(Conflict { record, exact })
        })
        .collect();
    conflicts.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then_with(|| b.record.updated_at.cmp(&a.record.updated_at))
            .then_with(|| b.record.id.cmp(&a.record.id))
    });
    Ok(conflicts)
}

/// Whether `a` and `b` overlap enough to flag: at least one shared tag,
/// covering at least [`TAG_OVERLAP_THRESHOLD`] of the smaller set. Tag
/// sets are already normalized, so the comparison is plain equality.
fn tags_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let shared = a.iter().filter(|tag| b.contains(tag)).count();
    let smaller = a.len().min(b.len());
    shared > 0 && shared as f32 / smaller as f32 >= TAG_OVERLAP_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volatile::VolatileStore;

    #[test]
    fn an_exact_subject_beats_a_fuzzy_tag_match() {
        let store = VolatileStore::new();
        let same_subject = store
            .put(
                crate::MemoryDraft::new(Scope::Global, "the house hunt")
                    .with_subject("home/accommodation"),
            )
            .unwrap();
        store
            .put(crate::MemoryDraft::new(Scope::Global, "the move").with_tags(["house", "move"]))
            .unwrap();
        store
            .put(
                crate::MemoryDraft::new(Scope::Global, "the work machine")
                    .with_subject("work/machine"),
            )
            .unwrap();

        let found = find_conflicts(
            &store,
            vec![Scope::Global],
            Some("home/accommodation"),
            &["house".to_owned()],
            None,
        )
        .unwrap();
        let bodies: Vec<&str> = found.iter().map(|c| c.record.body.as_str()).collect();
        assert_eq!(bodies, ["the house hunt", "the move"]);
        assert_eq!(found[0].record.id, same_subject.id, "exact match first");
        assert!(found[0].exact);
        assert!(!found[1].exact);
    }

    #[test]
    fn fuzzy_needs_enough_of_the_smaller_set() {
        let two: Vec<String> = ["a", "b"].map(Into::into).to_vec();
        let half: Vec<String> = ["a", "c"].map(Into::into).to_vec();
        let one: Vec<String> = ["a"].map(Into::into).to_vec();
        let four: Vec<String> = ["a", "c", "d", "e"].map(Into::into).to_vec();
        let four_touching: Vec<String> = ["a", "f", "g", "h"].map(Into::into).to_vec();
        let none: Vec<String> = ["z"].map(Into::into).to_vec();
        assert!(
            tags_overlap(&two, &half),
            "one shared tag of two-tag sets is half"
        );
        assert!(tags_overlap(&one, &half), "a one-tag set matches fully");
        assert!(
            !tags_overlap(&four, &four_touching),
            "a quarter of the set is not enough"
        );
        assert!(!tags_overlap(&two, &none));
        assert!(!tags_overlap(&two, &[]), "no tags, no fuzzy match");
    }

    #[test]
    fn the_updated_record_never_conflicts_with_itself() {
        let store = VolatileStore::new();
        let draft = crate::MemoryDraft::new(Scope::Global, "a fact");
        let created = store.put(draft).unwrap();
        let found = find_conflicts(
            &store,
            vec![Scope::Global],
            None,
            &created.tags,
            Some(&created.id),
        )
        .unwrap();
        assert!(found.is_empty());
    }
}
