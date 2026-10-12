//! The identity contract: six operations, any backend.
//!
//! Sync, like `rhema-rsd`'s `Store` and `rhema-memory`'s `MemoryStore`:
//! the operations are short local reads and writes, serialized by the
//! implementation, durable on return.

use crate::record::{Account, AccountDraft, Secret};

/// The identity contract: put, get, list, forget, rotate, and the one
/// method that breaks the redaction — sealed behind the use-never-read
/// rule. Implementations are `Send + Sync` and serialize their own
/// mutations; a write that returned `Ok` is durable.
pub trait IdentityStore: Send + Sync {
    /// Insert, or replace the account with the draft's id (`created_at`
    /// preserved; metadata *and* secret replaced). An update naming an
    /// absent account is [`IdentityError::UnknownId`].
    fn put(&self, draft: AccountDraft) -> Result<Account, IdentityError>;

    /// The account with this id, if any — redacted: no path from the
    /// returned [`Account`] to the secret exists.
    fn get(&self, id: &str) -> Result<Option<Account>, IdentityError>;

    /// Every account, creation order.
    fn list(&self) -> Result<Vec<Account>, IdentityError>;

    /// Remove by id; `Ok(false)` when absent — not an error. The
    /// account's secret goes with it.
    fn forget(&self, id: &str) -> Result<bool, IdentityError>;

    /// Replaces the account's secret, leaving metadata and timestamps'
    /// history intact (`updated_at` advances). Naming an absent account
    /// is [`IdentityError::UnknownId`].
    fn set_secret(&self, id: &str, secret: Secret) -> Result<Account, IdentityError>;

    /// The account's credential, for *code that acts with it*: a forge
    /// client authenticating, a login being performed. This is the
    /// crate's one unredacted read, and the use-never-read contract is
    /// what guards it — the value must reach destinations, never
    /// transcripts, and this crate ships no model-facing tool that calls
    /// it. `None` when the account does not exist.
    fn secret(&self, id: &str) -> Result<Option<Secret>, IdentityError>;
}

/// Why an identity operation failed. Tool callers turn every variant into
/// an in-band result; nothing aborts a turn.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A draft's label was empty or whitespace.
    #[error("account label is empty")]
    EmptyLabel,
    /// A draft's service was empty or whitespace.
    #[error("account service is empty")]
    EmptyService,
    /// A draft's secret was empty or whitespace.
    #[error("account secret is empty")]
    EmptySecret,
    /// A secret exceeded [`crate::SECRET_LIMIT`].
    #[error("secret of {len} bytes exceeds the {limit}-byte limit")]
    SecretTooLong {
        /// The offending secret's size.
        len: usize,
        /// The cap that rejected it.
        limit: usize,
    },
    /// A field exceeded its cap (label [`crate::record::LABEL_LIMIT`],
    /// the rest [`crate::record::FIELD_LIMIT`]).
    #[error("{field} of {len} bytes exceeds the {limit}-byte limit")]
    FieldTooLong {
        /// The field that was too long.
        field: &'static str,
        /// Its size.
        len: usize,
        /// The cap that rejected it.
        limit: usize,
    },
    /// An id outside the store's shape — non-empty, at most 64 chars,
    /// `[0-9A-Za-z-]` — was rejected before any lookup.
    #[error("invalid id {id:?}")]
    InvalidId {
        /// The rejected id.
        id: String,
    },
    /// A write named an id the store does not have.
    #[error("no account with id {id:?}")]
    UnknownId {
        /// The absent id.
        id: String,
    },
    /// The SQLite backend failed.
    #[error("identity store backend: {0}")]
    Backend(#[from] rusqlite::Error),
}
