//! rhema-identity — the agent accounts layer.
//!
//! The accounts a deployment holds: one [`Account`] per identity on a
//! service (`"github"`, `"gitlab"`, an SMTP host, a CLI tool's host), with
//! a username and exactly one [`Secret`] — an API token or a stored
//! password. Neutral by design: no forge vocabulary, no model seam, no
//! tools. Consumers compose it — the claw daemon mounts the store under
//! its own toolset and routes, and points its forge clients at
//! [`IdentityStore::secret`].
//!
//! The contract is **use-never-read**: [`IdentityStore::secret`] exists
//! for *code* that acts with a credential — a client authenticating, a
//! login being performed — never for anything that answers a model. This
//! crate ships no tool surface at all, so the guarantee is structural:
//! the model can only ever see [`Account`], which has no secret field.
//! Consumers must keep it that way — a tool that echoes a secret into a
//! transcript breaks the contract the store cannot enforce.
//!
//! Secrets live where the backend puts them: the SQLite store keeps them
//! in its file, and file privacy is the consumer's job (a 0600 database
//! in a 0700 directory, the pattern `rhema-config`'s `privacy` helpers
//! exist for). An OS keychain is a future backend behind the same trait.
//!
//! The store trait is sync, like `rhema-rsd`'s `Store` and
//! `rhema-memory`'s `MemoryStore`: operations are short local reads and
//! writes, serialized by the implementation, durable on return.
//!
//! Layout:
//! - [`record`] — `Account`, `AccountDraft`, `Secret`, `SecretKind`, ids
//! - [`store`] — the `IdentityStore` contract and `IdentityError`
//! - [`volatile`] — the in-process `VolatileIdentityStore`
//! - [`sqlite`] — the durable `SqliteIdentityStore`
//! - [`store_contract`] — the replayable behavioral suite

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod record;
pub mod sqlite;
pub mod store;
pub mod store_contract;
pub mod volatile;

pub use record::{Account, AccountDraft, SECRET_LIMIT, Secret, SecretKind};
pub use sqlite::SqliteIdentityStore;
pub use store::{IdentityError, IdentityStore};
pub use volatile::VolatileIdentityStore;
