//! The review sweep: what is due for a re-check, and nothing else.
//!
//! The engine computes *what is due*; it schedules nothing and decides
//! nothing. A caller — a scheduled agent turn, not a daemon — asks
//! [`due_for_review`], re-checks what it can through each
//! [`VerifyHint`], and only the `Manual` hints reach a human, batched.

use crate::record::{MemoryRecord, VerifyHint};

/// One record the sweep hands back: the fact, how long its belief has
/// gone unverified, and how to re-check it without asking a human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewItem {
    /// The record itself, with its current status and horizon.
    pub record: MemoryRecord,
    /// Days the current belief has gone unverified.
    pub age_days: u64,
    /// How to re-check; `None` when the record carries no hint.
    pub verify_hint: Option<VerifyHint>,
}

/// The instant `record` became due, used to order the sweep — most
/// overdue first.
fn due_since(record: &MemoryRecord) -> u64 {
    let candidates = [record.stale_at(), record.valid_until];
    candidates
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(record.created_at)
}

/// The active records in `records` that are due for review at `now` —
/// past their decay interval or past their horizon — most overdue first,
/// ties by id. Records already superseded or retracted never appear: the
/// sweep reviews beliefs, not corpses.
pub fn due_for_review(records: Vec<MemoryRecord>, now: u64) -> Vec<ReviewItem> {
    let mut items: Vec<ReviewItem> = records
        .into_iter()
        .filter(|record| record.is_due(now))
        .map(|record| ReviewItem {
            age_days: record.age_days(now),
            verify_hint: record.verify_hint.clone(),
            record,
        })
        .collect();
    items.sort_by(|a, b| {
        due_since(&a.record)
            .cmp(&due_since(&b.record))
            .then_with(|| a.record.id.cmp(&b.record.id))
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{DAY_MILLIS, Decay, Scope, Source, Status};

    fn record(id: &str, decay: Decay) -> MemoryRecord {
        MemoryRecord {
            id: id.to_owned(),
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
    fn the_sweep_reports_stale_records_most_overdue_first() {
        let day = DAY_MILLIS;
        let mut never = record("permanent", Decay::Permanent);
        never.valid_until = Some(400 * day);
        let stale_late = record("long", Decay::Long);
        let mut stale_early = record("volatile", Decay::Volatile);
        stale_early.verify_hint = Some(VerifyHint::Manual);
        let mut dead = record("dead", Decay::Volatile);
        dead.status = Status::Superseded {
            by: "new".to_owned(),
            at: day,
        };

        assert!(
            due_for_review(vec![never.clone(), stale_late.clone(), dead.clone()], day).is_empty()
        );

        let items = due_for_review(
            vec![never, stale_late, dead.clone(), stale_early],
            366 * day,
        );
        let ids: Vec<&str> = items.iter().map(|item| item.record.id.as_str()).collect();
        assert_eq!(
            ids,
            ["volatile", "long"],
            "most overdue first, dead skipped"
        );
        assert_eq!(items[0].age_days, 366);
        assert_eq!(items[0].verify_hint, Some(VerifyHint::Manual));
        assert!(!items[1].record.status.is_dead());
    }

    #[test]
    fn a_passed_horizon_makes_a_record_due() {
        let day = DAY_MILLIS;
        let mut planned = record("quote", Decay::Permanent);
        planned.valid_until = Some(day);
        let items = due_for_review(vec![planned], day + 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].record.id, "quote");
    }
}
