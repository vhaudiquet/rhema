//! Tool permission scoping (spec §8): what a handler run may call.
//!
//! The allowlist is fixed before the handler's first reasoning step; the
//! model never decides its own permissions mid-run. Policy — which event
//! earns which allowlist — is per-deployment, resolved by the consumer;
//! this type only carries the decision.

use std::collections::BTreeSet;

/// The tool-permission set one handler run is granted. `None` means
/// permissive — every registered tool — which is the spec's v1 default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    allowed: Option<BTreeSet<String>>,
}

impl Allowlist {
    /// Every registered tool is permitted.
    pub fn permissive() -> Self {
        Self { allowed: None }
    }

    /// Only the named tools are permitted.
    pub fn from_tools(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed: Some(names.into_iter().map(Into::into).collect()),
        }
    }

    /// Whether `tool` may be invoked under this allowlist.
    pub fn allows(&self, tool: &str) -> bool {
        match &self.allowed {
            None => true,
            Some(allowed) => allowed.contains(tool),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_allows_everything() {
        let allowlist = Allowlist::permissive();
        assert!(allowlist.allows("bash"));
        assert!(allowlist.allows("anything_at_all"));
    }

    #[test]
    fn a_tool_set_allows_exactly_its_members() {
        let allowlist = Allowlist::from_tools(["bash", "comment"]);
        assert!(allowlist.allows("bash"));
        assert!(!allowlist.allows("merge_pr"));
    }

    #[test]
    fn an_empty_allowlist_permits_nothing() {
        let allowlist = Allowlist::from_tools(Vec::<String>::new());
        assert!(!allowlist.allows("bash"));
    }

    #[test]
    fn the_default_allowlist_is_the_permissive_one() {
        assert_eq!(Allowlist::default(), Allowlist::permissive());
        assert!(Allowlist::default().allows("bash"));
    }
}
