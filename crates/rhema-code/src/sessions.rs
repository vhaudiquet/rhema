//! Saved sessions: the conversations the TUI checkpoints as their turns
//! stream, writes back on exit, and the resume picker lists.
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

/// The usage accounting a saved session carries: what the Usage page
/// needs to keep reading as one run after a resume. It rides the
/// session as the opaque `usage_stats` JSON blob — the engine and the
/// other session readers never touch it — and this type owns the
/// blob's shape.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageStats {
    /// The transcript's benchmark counters: request counts, the
    /// prompt-cache totals, the categories' token split. `None` when
    /// the session ran with the transcript off or saved none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<rhema_transcript::TranscriptStats>,
    /// The last finished turn's token accounting, so the page's "last
    /// turn" row and the context gauge pick up where the save left
    /// off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<rhema_core::Usage>,
}

impl UsageStats {
    /// Reads the accounting off a session; `None` when it carries
    /// none — a file from before the field existed, or a blob that no
    /// longer parses. Stats are decoration: a bad blob costs the page
    /// its numbers, never the resume.
    pub fn from_session(session: &Session) -> Option<Self> {
        if session.usage_stats.is_null() {
            return None;
        }
        serde_json::from_value(session.usage_stats.clone()).ok()
    }

    /// Stamps the accounting onto `session` — the save path, so the
    /// file carries what this run measured.
    pub fn write_into(self, session: &mut Session) {
        session.usage_stats = serde_json::to_value(&self).unwrap_or_default();
    }
}

/// Milliseconds since the epoch; 0 when the clock sits behind the epoch,
/// which only muddles ordering, never correctness.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// Picks the file a workspace's next session lands in: the base and the
/// workspace directory are created (`0700`), and the name carries the
/// current time with the same-millisecond suffix scheme the saves use.
/// Naming a file writes nothing — a run that never speaks leaves no husk
/// behind — so a checkpoint target can be allocated before the first
/// turn and used by whatever turn speaks first.
pub fn next_session_path(base: &Path, workspace: &Path) -> io::Result<PathBuf> {
    // Conversation content: the base and the workspace directory are
    // `0700`, the file `0600`, re-asserted on every save.
    rhema_config::privacy::private_dir(base)?;
    let dir = base.join(encode_workspace(workspace));
    rhema_config::privacy::private_dir(&dir)?;
    let millis = now_millis();
    // Same-millisecond saves pick the next free suffix; names sort by
    // time because the millis field is fixed-width.
    let mut n = 0u32;
    Ok(loop {
        let candidate = dir.join(format!("{millis:016}-{n:02}.json"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    })
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
    let path = next_session_path(base, workspace)?;
    write_session(&path, session)?;
    Ok(Some(path))
}

/// Writes `session` to `path` — the in-place update a resumed session
/// gets on exit, so continuing it never grows a second entry. The file
/// is written `0600` and an existing wider file tightens again. The
/// write lands through a sibling temporary file and a rename: the
/// checkpoint is the record a crash resume reads, and a kill mid-write
/// must leave the previous checkpoint whole, never a truncated
/// half-session no resume can parse.
pub fn write_session(path: &Path, session: &Session) -> io::Result<()> {
    let json = serde_json::to_string_pretty(session)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let temp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.{}.tmp", std::process::id()));
    let written = rhema_config::privacy::private_write(&temp, json.as_bytes());
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return written;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
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
        // Dot entries never list: a checkpoint write's temporary
        // half-file hides behind a leading dot, and a crash mid-write
        // may leave one behind.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name.starts_with('.'))
        {
            continue;
        }
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
    fn the_next_session_path_names_a_file_without_writing_it() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/someone/proj");

        let path = next_session_path(&base, workspace).unwrap();
        // Nothing exists yet: naming a checkpoint target is not a write.
        assert!(!path.exists());
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("json"));
        // The directories do exist, `0700`-private, so the first commit
        // lands in place.
        assert!(path.parent().unwrap().is_dir());

        // A second pick never lands on a name that is taken on disk:
        // once the first file is written (the run spoke), the next
        // allocation — a /clear'd conversation's fresh start — takes
        // the next suffix.
        write_session(&path, &session_with("hello")).unwrap();
        let second = next_session_path(&base, workspace).unwrap();
        assert_ne!(path, second);
    }

    #[test]
    fn the_checkpoint_write_leaves_no_temp_and_never_a_half_file() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/someone/proj");
        let path = next_session_path(&base, workspace).unwrap();

        write_session(&path, &session_with("first")).unwrap();
        // A second write over a live checkpoint still reads back whole
        // and leaves no sibling behind.
        write_session(&path, &session_with("second")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("second"));
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(siblings.is_empty(), "temp litter: {siblings:?}");
    }

    #[test]
    fn a_temp_left_by_a_crash_never_lists() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/someone/proj");
        let dir = base.join(encode_workspace(workspace));
        std::fs::create_dir_all(&dir).unwrap();

        let kept = next_session_path(&base, workspace).unwrap();
        write_session(&kept, &session_with("real")).unwrap();
        // What a kill mid-write could leave: a dot-prefixed half-file.
        std::fs::write(dir.join(".0001793121258253-00.json.4242.tmp"), b"{").unwrap();

        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, kept);
    }

    #[test]
    fn a_checkpointed_session_resumes_from_its_own_file() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/someone/proj");

        // The run allocates its target up front, the first stream
        // commits into it, and the file is resume-shaped from then on.
        let path = next_session_path(&base, workspace).unwrap();
        write_session(&path, &session_with("hello")).unwrap();
        assert!(path.is_file());

        // The exit save updates the same file: one conversation, one
        // entry, no second listing in the picker.
        let grown = session_with("hello");
        write_session(&path, &grown).unwrap();
        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, path);
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
    #[cfg(unix)]
    fn a_saved_session_lands_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");
        let path = save_session(&base, workspace, &session_with("prompt"))
            .unwrap()
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&base), 0o700, "the sessions base is 0700");
        assert_eq!(
            mode(&base.join(encode_workspace(workspace))),
            0o700,
            "the workspace directory is 0700"
        );
        assert_eq!(mode(&path), 0o600, "the session file is 0600");
        // The in-place update keeps the file private too.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_session(&path, &session_with("prompt again")).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn usage_stats_round_trip_through_the_saved_file() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");
        let mut session = session_with("prompt");
        let stats = UsageStats {
            transcript: Some(rhema_transcript::TranscriptStats {
                requests: 3,
                input_tokens: 900,
                cached_input_tokens: 700,
                output_tokens: 90,
                ..rhema_transcript::TranscriptStats::default()
            }),
            last_turn: Some(rhema_core::Usage {
                input_tokens: 300,
                output_tokens: 30,
                cached_input_tokens: 200,
            }),
        };
        stats.clone().write_into(&mut session);
        let path = save_session(&base, workspace, &session).unwrap().unwrap();

        // What the file holds, a load reads back: the same shape
        // --resume takes.
        let loaded = CodingAgent::load_session_from(&path).unwrap();
        assert_eq!(UsageStats::from_session(&loaded), Some(stats));
    }

    #[test]
    fn a_session_without_usage_stats_reads_as_none() {
        // Files from before the field existed load as null.
        let session = session_with("prompt");
        assert_eq!(UsageStats::from_session(&session), None);

        // A blob of a foreign object shape parses to an empty
        // accounting: the page loses its numbers, the resume does not
        // fail. Junk that is not an object fails the parse and reads
        // the same.
        let mut session = session_with("prompt");
        session.usage_stats = serde_json::json!({ "requests": 3 });
        assert_eq!(
            UsageStats::from_session(&session),
            Some(UsageStats::default())
        );
        session.usage_stats = serde_json::json!("three requests");
        assert_eq!(UsageStats::from_session(&session), None);
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
