//! ACP session persistence: the session id the client holds maps to a
//! saved [`Session`] file, so `session/load` can restore a conversation
//! across agent restarts.
//!
//! LAYOUT: `<base>/<session-id>.json`, where base is
//! [`base_dir`] — `$XDG_CACHE_HOME/rhema/acp-sessions`, or under
//! `~/.cache` when unset. A cache, like the TUI's saved sessions:
//! losing it loses history, never settings. The file is a bare
//! `Session` in the same JSON `--resume` takes — one format, every
//! reader. Client-supplied ids are validated before they ever touch a
//! path: only `[-._0-9A-Za-z]` passes, so an id is a file name, never a
//! traversal.

use std::io;
use std::path::{Path, PathBuf};

use rhema_core::Session;

/// The saved-session base for ACP sessions.
pub fn base_dir() -> io::Result<PathBuf> {
    let cache = match std::env::var("XDG_CACHE_HOME") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => match std::env::var("HOME") {
            Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join(".cache"),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "neither XDG_CACHE_HOME nor HOME is set",
                ));
            }
        },
    };
    Ok(cache.join("rhema").join("acp-sessions"))
}

/// Whether `id` is a safe file-name component: every byte inside
/// `[-._0-9A-Za-z]` and neither empty nor overly long. ACP session ids
/// are agent-assigned, but `session/load` hands one back — treat it as
/// input.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(
            |byte| matches!(byte, b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'),
        )
}

/// Writes `session` as the file for `id`, replacing any prior save.
/// The base is `0700` and the file `0600`: conversation content.
pub fn save_session(base: &Path, id: &str, session: &Session) -> io::Result<()> {
    debug_assert!(valid_id(id), "session ids are validated at the boundary");
    rhema_config::privacy::private_dir(base)?;
    let json = serde_json::to_string_pretty(session)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    rhema_config::privacy::private_write(&base.join(format!("{id}.json")), json.as_bytes())
}

/// Reads the session saved for `id`.
pub fn load_session(base: &Path, id: &str) -> io::Result<Session> {
    if !valid_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id",
        ));
    }
    let raw = std::fs::read_to_string(base.join(format!("{id}.json")))?;
    serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Message, Role};

    fn session_with(prompt: &str) -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        session
    }

    #[test]
    fn a_session_round_trips_through_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("acp-sessions");

        save_session(&base, "rhema-abc-001", &session_with("hello")).unwrap();
        let loaded = load_session(&base, "rhema-abc-001").unwrap();
        assert_eq!(loaded.history.len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn a_saved_session_lands_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("acp-sessions");
        let path = base.join("rhema-abc-001.json");
        save_session(&base, "rhema-abc-001", &session_with("hello")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&base), 0o700, "the session base is 0700");
        assert_eq!(mode(&path), 0o600, "the session file is 0600");
    }

    #[test]
    fn an_id_never_leaves_the_base_directory() {
        assert!(valid_id("rhema-1730000000000000-000"));
        assert!(!valid_id("../escape"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
        assert!(!valid_id("a\\b"));
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session(dir.path(), "../escape").is_err());
        // A valid-looking id with no save is simply missing.
        assert!(load_session(dir.path(), "missing").is_err());
    }
}
