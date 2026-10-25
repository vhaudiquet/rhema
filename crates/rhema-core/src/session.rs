//! The caller-owned session: plain, serializable conversation state.
//!
//! The engine mutates only [`Session::history`]. [`Session::driver_state`] is
//! opaque to crate 1 — the driver stores whatever it needs there (or keeps
//! its own table). A `Session` holds no hidden engine state and no open
//! stream, so a driver can checkpoint it at any step boundary (SPEC.md §6).

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// History plus an opaque driver blob; fully serializable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The conversation so far; the engine appends assistant and tool
    /// messages in place during [`crate::Agent::step`].
    pub history: Vec<Message>,
    /// Driver-owned. Crate 1 never reads this. The coding driver may leave
    /// it empty; the RSD driver stores key/plan/artifact_ref here (or keeps
    /// its own table). Opaque on purpose.
    pub driver_state: serde_json::Value,
    /// The USD the session has cost so far, accumulated by the frontend
    /// from per-turn usage and its model pricing; the engine never reads
    /// it. Serde-defaulted, so files written before the field existed
    /// load as free.
    #[serde(default)]
    pub cost_usd: f64,
    /// The frontend's usage accounting, carried across resumes so a
    /// resumed session's stats keep reading as one run: an opaque JSON
    /// blob the engine never reads. Serde-defaulted to null, so files
    /// written before the field existed load without stats.
    #[serde(default)]
    pub usage_stats: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Content, Role};

    #[test]
    fn session_round_trips_with_driver_state() {
        let session = Session {
            history: vec![Message {
                role: Role::User,
                content: vec![Content::Text("fix the test".into())],
            }],
            driver_state: serde_json::json!({ "key": "interactive:main" }),
            cost_usd: 0.0,
            usage_stats: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, session);
    }

    #[test]
    fn a_session_written_before_cost_existed_loads_as_free() {
        // A resumed conversation must not wear a phantom cost: the same
        // shape minus the field is what the first format wrote.
        let session = Session {
            history: vec![Message::text(Role::User, "hi")],
            ..Session::default()
        };
        let mut value = serde_json::to_value(&session).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("cost_usd")
            .expect("the field was written");
        let session: Session = serde_json::from_value(value).unwrap();
        assert_eq!(session.cost_usd, 0.0);
    }

    #[test]
    fn a_session_written_before_usage_stats_existed_loads_without() {
        // The same shape minus the field is what the first format
        // wrote; a resumed conversation starts with no stats blob, not
        // a corrupt one.
        let session = Session {
            history: vec![Message::text(Role::User, "hi")],
            ..Session::default()
        };
        let mut value = serde_json::to_value(&session).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("usage_stats")
            .expect("the field was written");
        let session: Session = serde_json::from_value(value).unwrap();
        assert_eq!(session.usage_stats, serde_json::Value::Null);
    }

    #[test]
    fn the_usage_stats_blob_round_trips_through_the_saved_format() {
        let session = Session {
            usage_stats: serde_json::json!({ "transcript": { "requests": 3 } }),
            ..Session::default()
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.usage_stats["transcript"]["requests"], 3);
    }

    #[test]
    fn the_cost_round_trips_through_the_saved_format() {
        let session = Session {
            cost_usd: 4.275,
            ..Session::default()
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cost_usd, 4.275);
    }
}
