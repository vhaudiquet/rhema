//! `rhema-session-store-file`, the reference [`rhema_session_store::SessionStore`] backend.
//!
//! Two stores over one writer:
//!
//! - [`FileStore`] is the managed layout, `<base>/<id>.json` or
//!   `<base>/<encoded-workspace>/<id>.json`. The TUI and ACP use exactly
//!   this shape today, and the file is a bare [`rhema_core::Session`] in
//!   the same JSON `--resume` takes.
//! - [`PathStore`] is one explicit path, the file `--save` and `--resume`
//!   name. Its identity is the path, so it does not implement the trait:
//!   an id parameter would be a lie.
//!
//! Both write the way the store contract requires: the session streams
//! into a private sibling temp file (`create_new`, `0600`), the file is
//! fsynced, renamed over the target, and the parent directory is fsynced
//! best effort. A reader sees the old value or the new one; a crash
//! leaves the previous checkpoint whole and at most an inert dot-prefixed
//! temp behind, which `list` ignores.
//!
//! Two layout rules stay public because consumers need them:
//! [`encode_workspace`] builds the workspace directory name, and
//! [`FileStore::next_id`] allocates the TUI's `<millis:016>-<n:02>`
//! name.

#![forbid(unsafe_code)]

mod file;
mod layout;
mod path;
mod write;

pub use file::FileStore;
pub use layout::{decode_workspace, encode_workspace};
pub use path::PathStore;
