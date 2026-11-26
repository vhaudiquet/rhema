//! The host endpoint on a platform without Unix sockets: there is no
//! socket to bind, so the macros stay off. A sandbox on a non-Unix
//! host is a v2 concern; the `search` and `decide` prompt sections
//! simply do not render.

#![forbid(unsafe_code)]

use rhema_config::DecisionInstance;

use crate::sandbox::Sandboxed;

#[derive(Clone)]
pub(crate) struct DecisionSetup {
    pub instance: DecisionInstance,
    pub key: Option<String>,
}

pub(crate) enum Block {
    Search(std::sync::Arc<rhema_websearch::Chain>),
    Decision(DecisionSetup),
}

pub(crate) struct Endpoint;

impl Endpoint {
    pub(crate) fn env(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
    }
}

pub(crate) fn wire(
    _session_key: &str,
    _sandboxed: Option<&Sandboxed>,
    _block: Block,
) -> Option<std::sync::Arc<Endpoint>> {
    None
}
