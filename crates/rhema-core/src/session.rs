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
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, session);
    }
}
