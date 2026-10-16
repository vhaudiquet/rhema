//! The persistent per-key session: RSD state wrapped around crate 1's
//! caller-owned session.
//!
//! One [`Session`] per [`ResourceKey`](crate::ResourceKey), forever. It wraps
//! [`rhema_core::Session`] — the history the step engine appends to plus the
//! driver's opaque blob — and adds the RSD fields from spec §5. Fully
//! serializable: checkpointing round-trips the whole struct through the
//! store, which is why the wrap adds only plain data.

use rhema_core::Session as CoreSession;
use serde::{Deserialize, Serialize};

use crate::key::ResourceKey;

/// Persistent state tied 1:1 to a resource key: conversation history, plan,
/// and pointers to physical artifacts. Survives across lease acquisitions
/// and process restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The key this session belongs to; restamped by
    /// [`crate::Dispatcher::checkpoint`] so a session can never migrate
    /// between keys.
    pub key: ResourceKey,
    /// The crate-1 session the step engine consumes: history plus opaque
    /// driver state. The dispatcher never interprets it.
    pub core: CoreSession,
    /// The handler's current working plan, when it keeps one. Opaque to the
    /// dispatcher — plain JSON so it round-trips.
    pub plan: Option<serde_json::Value>,
    /// Pointer to physical state a handler wants to find again on resume:
    /// a git worktree path, a branch name, a file list.
    pub artifact_ref: Option<String>,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// Last checkpoint or acquire, in milliseconds since the Unix epoch.
    pub last_active: u64,
}

impl Session {
    /// A fresh, empty session for `key`, stamped at `now_millis`.
    pub fn fresh(key: ResourceKey, now_millis: u64) -> Self {
        Self {
            key,
            core: CoreSession::default(),
            plan: None,
            artifact_ref: None,
            created_at: now_millis,
            last_active: now_millis,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Content, Message, Role, ToolCall};

    #[test]
    fn a_session_round_trips_with_its_wrapped_core_state() {
        let session = Session {
            key: ResourceKey::from("github:acme/widgets#142"),
            core: CoreSession {
                history: vec![
                    Message::text(Role::User, "fix the flaky test"),
                    Message {
                        role: Role::Assistant,
                        content: vec![
                            Content::Text("on it".into()),
                            Content::ToolCall(ToolCall {
                                id: "call_1".into(),
                                name: "cargo_test".into(),
                                args: serde_json::json!({ "package": "rhema-core" }),
                                provider_data: serde_json::Value::Null,
                            }),
                        ],
                    },
                ],
                driver_state: serde_json::json!({ "attempt": 2 }),
                cost_usd: 0.0,
            },
            plan: Some(serde_json::json!(["reproduce", "fix", "verify"])),
            artifact_ref: Some("worktrees/issue-142".into()),
            created_at: 1_000,
            last_active: 2_000,
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, session);
    }

    #[test]
    fn fresh_sessions_start_empty_and_stamped() {
        let session = Session::fresh(ResourceKey::from("k"), 42);
        assert!(session.core.history.is_empty());
        assert_eq!(session.core.driver_state, serde_json::Value::Null);
        assert_eq!(session.created_at, 42);
        assert_eq!(session.last_active, 42);
    }
}
