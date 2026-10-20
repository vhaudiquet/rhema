//! The notification triage: what a notification earns, decided
//! deterministically before any model call.
//!
//! A [`Notification`] carries the forge's own reason for bothering the
//! account; [`classify`] reduces it to one of three verdicts. A
//! [`Triage::Agent`] dispatch becomes an RSD event whose payload names
//! the [`Role`] — the persona the handler's prompt opens with, so one
//! thread can move from review-requested to implement-feedback across
//! dispatches with the session carrying the continuity. A
//! [`Triage::Surface`] never dispatches an agent: a mention is not an
//! assignment, and the account's human hears about it some other way.
//! [`Triage::Ignore`] drops the noise (CI churn) on the floor.
//!
//! The mapping lives beside the vocabulary on purpose: a reason that
//! enters it (Gitea's collapsed shape, a new adapter's spelling) needs
//! a verdict in the same change, and this module's tests pin the
//! mapping so the co-edit cannot drift. A consumer may reclassify on
//! top — [`classify`] is the default, not a mandate.

use crate::gitea::COLLAPSED_REASON;
use crate::{Notification, NotificationReason};

/// The agent persona a dispatched notification runs as. The key stays
/// the thread — one session, one voice per thread, ever — and the role
/// rides the dispatch: it selects the prompt's opening framing and is
/// the seam where per-role tool policies land later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Asked to review: judge the pull request and post the review.
    Reviewer,
    /// Asked to do the work: implement the issue or answer the feedback.
    Implementer,
}

impl Role {
    /// The payload and prompt spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reviewer => "reviewer",
            Role::Implementer => "implementer",
        }
    }
}

/// What one notification earns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    /// Dispatch the thread's agent as this role.
    Agent(Role),
    /// No agent: surface the notification to the account's human through
    /// the device-notification queue.
    Surface,
    /// Drop it: noise the account should not hear about.
    Ignore,
}

/// Classifies one notification. A forge reason the vocabulary does not
/// know ([`NotificationReason::Other`]) surfaces rather than acts: an
/// unknown shape is the user's to judge, never the agent's to guess at.
/// The one exception is a forge whose feed names no per-row reason at
/// all (Gitea's): its adapter collapses every row under one fixed,
/// published spelling, so that spelling is a known shape — the account
/// was notified on a thread of its own, and the thread's agent acts.
/// The row cannot say *why* the account was notified (a comment, an
/// assignment, and a review request ride the same shape), so the
/// default is the do-the-work role; a reviewer framing can follow in
/// the thread's session once the text of the activity says so.
pub fn classify(notification: &Notification) -> Triage {
    match &notification.reason {
        NotificationReason::ReviewRequested => Triage::Agent(Role::Reviewer),
        NotificationReason::Assigned => Triage::Agent(Role::Implementer),
        NotificationReason::ReviewFeedback => Triage::Agent(Role::Implementer),
        NotificationReason::Author => Triage::Agent(Role::Implementer),
        NotificationReason::Mention => Triage::Surface,
        NotificationReason::CiActivity => Triage::Ignore,
        NotificationReason::Other(kind) if kind == COLLAPSED_REASON => {
            Triage::Agent(Role::Implementer)
        }
        NotificationReason::Other(_) => Triage::Surface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepoRef, Timestamp};

    fn notification(reason: NotificationReason) -> Notification {
        Notification {
            id: format!("github:notification:1@{}", reason.as_str()),
            reason,
            repo: RepoRef::new("acme", "widgets"),
            number: 7,
            is_pr: true,
            actor: None,
            title: None,
            url: None,
            timestamp: Timestamp::parse("2026-10-08T00:00:00Z").expect("parses"),
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn asks_become_agent_dispatches_of_the_matching_role() {
        assert_eq!(
            classify(&notification(NotificationReason::ReviewRequested)),
            Triage::Agent(Role::Reviewer)
        );
        for reason in [
            NotificationReason::Assigned,
            NotificationReason::ReviewFeedback,
            NotificationReason::Author,
        ] {
            let triage = classify(&notification(reason.clone()));
            assert_eq!(
                triage,
                Triage::Agent(Role::Implementer),
                "{} implements",
                reason.as_str()
            );
        }
    }

    #[test]
    fn mentions_surface_ci_ignores_and_unknowns_surface() {
        assert_eq!(
            classify(&notification(NotificationReason::Mention)),
            Triage::Surface
        );
        assert_eq!(
            classify(&notification(NotificationReason::CiActivity)),
            Triage::Ignore
        );
        assert_eq!(
            classify(&notification(NotificationReason::Other(
                "subscription".to_owned()
            ))),
            Triage::Surface
        );
    }

    #[test]
    fn the_collapsed_reasonless_forge_dispatches_other_spellings_do_not() {
        // Gitea's adapter collapses every notifications row under one
        // fixed spelling; that spelling is the known shape the triage
        // acts on — this is the path a comment on a thread the account
        // authored takes on a reason-less forge.
        assert_eq!(
            classify(&notification(NotificationReason::Other("gitea".to_owned()))),
            Triage::Agent(Role::Implementer)
        );
        // A spelling no adapter owns is still the user's to judge.
        assert_eq!(
            classify(&notification(NotificationReason::Other(
                "something-new".to_owned()
            ))),
            Triage::Surface
        );
    }

    #[test]
    fn roles_round_trip_through_serde() {
        for role in [Role::Reviewer, Role::Implementer] {
            let encoded = serde_json::to_value(role).expect("serializes");
            assert_eq!(
                serde_json::from_value::<Role>(encoded).expect("deserializes"),
                role
            );
        }
    }
}
