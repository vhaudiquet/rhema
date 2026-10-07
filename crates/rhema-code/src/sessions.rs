//! Saved sessions: the conversations the TUI writes on exit and the
//! resume picker lists.
//!
//! LAYOUT: `<base>/<encoded-workspace>/<millis>-<n>.json`, where `base`
//! is [`base_dir`] and the workspace directory name is the absolute
//! workspace path with every byte outside `[-._0-9A-Za-z]` percent-encoded.
//! The encoding is reversible, so distinct workspaces can never collide
//! the way a truncated hash could. The file itself is a bare `Session`
//! in the same JSON `--resume` and `--save` take — one format, every
//! reader.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_coding_agent::CodingAgent;
use rhema_core::{Content, Role, Session};

use crate::transcript::truncate_chars;

/// Width at which a session's opening prompt is cut for a picker row.
const SNIPPET_WIDTH: usize = 48;

/// One saved session, as the resume picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSession {
    /// The file to load the session from.
    pub path: PathBuf,
    /// When the session was saved, in milliseconds since the epoch.
    pub saved_at: u64,
    /// The session's opening prompt, cut for a picker row; `None` when
    /// the file predates a prompt or could not be read.
    pub snippet: Option<String>,
}

/// The saved-session base: `$XDG_CACHE_HOME/rhema/sessions`, or under
/// `~/.cache` when the variable is unset. A cache, not data: losing it
/// loses history, never settings.
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
    Ok(cache.join("rhema").join("sessions"))
}

/// Milliseconds since the epoch; 0 when the clock sits behind the epoch,
/// which only muddles ordering, never correctness.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes `session` under `base` for `workspace`, returning the file it
/// landed in. A session with no history is not written: there is nothing
/// to resume, and /clear should not leave a husk behind.
pub fn save_session(
    base: &Path,
    workspace: &Path,
    session: &Session,
) -> io::Result<Option<PathBuf>> {
    if session.history.is_empty() {
        return Ok(None);
    }
    let dir = base.join(encode_workspace(workspace));
    std::fs::create_dir_all(&dir)?;
    let millis = now_millis();
    // Same-millisecond saves pick the next free suffix; names sort by
    // time because the millis field is fixed-width.
    let mut n = 0u32;
    let path = loop {
        let candidate = dir.join(format!("{millis:016}-{n:02}.json"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    write_session(&path, session)?;
    Ok(Some(path))
}

/// Writes `session` to `path` — the in-place update a resumed session
/// gets on exit, so continuing it never grows a second entry.
pub fn write_session(path: &Path, session: &Session) -> io::Result<()> {
    let json = serde_json::to_string_pretty(session)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    std::fs::write(path, json)
}

/// The saved sessions for `workspace`, newest first. A missing directory
/// is an empty list — nothing saved yet, not an error. A file whose JSON
/// no longer parses still lists (with no snippet): the resume attempt
/// surfaces the corruption, the listing does not hide it. The timestamp
/// is the file's modification time, so an updated session's row moves to
/// the top instead of wearing its original save date.
pub fn list_sessions(base: &Path, workspace: &Path) -> io::Result<Vec<SavedSession>> {
    let dir = base.join(encode_workspace(workspace));
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let path = entry?.path();
        // The name stays the list filter (stray files never show); the
        // mtime is the truth about when the session was last live.
        let name_at = name_millis(&path);
        let saved_at = file_millis(&path)?.or(name_at);
        let Some(saved_at) = saved_at else {
            continue;
        };
        let snippet = CodingAgent::load_session_from(&path)
            .ok()
            .as_ref()
            .and_then(opening_prompt)
            .map(|text| truncate_chars(&text, SNIPPET_WIDTH));
        out.push(SavedSession {
            path,
            saved_at,
            snippet,
        });
    }
    out.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then(b.path.cmp(&a.path)));
    Ok(out)
}

/// The file's modification time in millis; `None` when it cannot be read.
fn file_millis(path: &Path) -> io::Result<Option<u64>> {
    Ok(std::fs::File::open(path)?
        .metadata()?
        .modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64))
}

/// The millis field of a session file name; `None` for anything else,
/// so stray files in a workspace directory never list.
fn name_millis(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_string_lossy();
    let (millis, _) = name.split_once('-')?;
    millis.parse().ok()
}

/// The session's opening prompt: the first user text, for the picker row.
fn opening_prompt(session: &Session) -> Option<String> {
    session
        .history
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| {
            message.content.iter().find_map(|content| match content {
                Content::Text(text) if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
        })
}

/// The workspace directory name: the path with every byte outside
/// `[-._0-9A-Za-z]` percent-encoded.
fn encode_workspace(workspace: &Path) -> String {
    let mut out = String::new();
    for byte in workspace.to_string_lossy().bytes() {
        match byte {
            b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The age of `saved_at` against `now` (both millis), as the picker row
/// shows it. `now` is a parameter so tests pin the wording.
pub fn ago(saved_at: u64, now: u64) -> String {
    let Some(delta) = now.checked_sub(saved_at) else {
        return "just now".to_owned();
    };
    let secs = delta / 1000;
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Message, Role};

    fn temp_base() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        (dir, base)
    }

    fn session_with(prompt: &str) -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        session
    }

    #[test]
    fn save_then_list_reads_the_session_back_newest_first() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");

        let first = save_session(&base, workspace, &session_with("first prompt"))
            .unwrap()
            .expect("a session with history saves");
        let second = save_session(&base, workspace, &session_with("second prompt"))
            .unwrap()
            .expect("a session with history saves");

        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, second, "newest first");
        assert_eq!(listed[1].path, first);
        assert_eq!(listed[1].snippet.as_deref(), Some("first prompt"));

        // What the picker lists, --resume can load: the file is a bare
        // session in the flag's format.
        let loaded = CodingAgent::load_session_from(&first).unwrap();
        assert_eq!(loaded.history.len(), 1);
    }

    #[test]
    fn an_empty_session_is_not_written() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");
        assert_eq!(
            save_session(&base, workspace, &Session::default()).unwrap(),
            None
        );
        assert!(list_sessions(&base, workspace).unwrap().is_empty());
    }

    #[test]
    fn workspaces_never_share_a_directory() {
        // The separator between `a` and `b` encodes differently on each
        // side, so these paths cannot collapse into one directory.
        let (_dir, base) = temp_base();
        save_session(&base, Path::new("/a/b"), &session_with("one")).unwrap();
        save_session(&base, Path::new("/a_b"), &session_with("two")).unwrap();
        assert_eq!(list_sessions(&base, Path::new("/a/b")).unwrap().len(), 1);
        assert_eq!(list_sessions(&base, Path::new("/a_b")).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_directory_lists_as_empty() {
        let (_dir, base) = temp_base();
        assert!(
            list_sessions(&base, Path::new("/nowhere"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_corrupt_file_lists_without_a_snippet() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");
        let path = save_session(&base, workspace, &session_with("prompt"))
            .unwrap()
            .unwrap();
        std::fs::write(&path, "{not json").unwrap();

        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].snippet, None);
    }

    #[test]
    fn an_update_in_place_keeps_one_entry_and_refreshes_its_age() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");
        let path = save_session(&base, workspace, &session_with("prompt"))
            .unwrap()
            .unwrap();
        // The file was last live an hour ago...
        let old = SystemTime::now() - std::time::Duration::from_secs(3_600);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        // ...then the resumed session continued and exits: the same file
        // is rewritten, and the list still shows exactly one entry whose
        // age reads from the rewrite, not the creation.
        write_session(&path, &session_with("prompt and more")).unwrap();
        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, path);
        assert!(
            listed[0].saved_at >= now_millis() - 60_000,
            "the age follows the update: {}",
            listed[0].saved_at
        );
        assert_eq!(
            listed[0].snippet.as_deref(),
            Some("prompt and more"),
            "the row previews the continued session"
        );
    }

    #[test]
    fn ages_read_in_buckets() {
        let now = 1_000_000_000_000;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 59_000, now), "just now");
        assert_eq!(ago(now - 60_000, now), "1m ago");
        assert_eq!(ago(now - 90 * 60_000, now), "1h ago");
        assert_eq!(ago(now - 26 * 3_600_000, now), "1d ago");
        // A timestamp from the future (clock skew) reads as now.
        assert_eq!(ago(now + 5_000, now), "just now");
    }
}
