//! The coding-agent driver: the first consumer of rhema-core's step engine.
//!
//! Wires a [`ModelProvider`](rhema_core::ModelProvider) and a built-in tool
//! set (file tools scoped to a workspace, plus an approval-gated command
//! tool) into a `CodingAgent` whose turns drive `run_turn`, and whose
//! `Session` can be persisted and resumed.
