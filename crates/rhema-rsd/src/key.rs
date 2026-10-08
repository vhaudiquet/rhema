//! Resource keys: the external identity every dispatch state hangs from.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A string identifying the external resource an event is about, e.g.
/// `github:acme/widgets#142`. One key owns exactly one [`crate::Session`],
/// one lease at a time, and one inbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceKey(String);

impl ResourceKey {
    /// Wraps `key`.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ResourceKey {
    fn from(key: String) -> Self {
        Self(key)
    }
}

impl From<&str> for ResourceKey {
    fn from(key: &str) -> Self {
        Self(key.to_owned())
    }
}

impl fmt::Display for ResourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_round_trips_as_a_bare_json_string() {
        let key = ResourceKey::from("github:acme/widgets#142");
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, r#""github:acme/widgets#142""#);
        let back: ResourceKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, key);
    }
}
