//! The slot algebra: how ranges of the log are named and split.
//!
//! A range `(id, n)` is the `n` messages of the log from index `id` on.
//! Larger ranges split into halves that always cover them — the tree a
//! later compactor will grow is binary, but the algebra does not need
//! it. This module is the contract every fill policy keeps: the lines
//! change, the addresses never.

/// A range of the log: the `n` messages from `id` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Range {
    /// Index of the first message.
    pub id: usize,
    /// How many messages the range covers; at least 1.
    pub n: usize,
}

impl Range {
    /// The range's single message: `(id, 1)`.
    pub fn message(id: usize) -> Self {
        Self { id, n: 1 }
    }
}
