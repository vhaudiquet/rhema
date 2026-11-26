//! Saved sessions: the conversations the TUI checkpoints as their turns
//! stream, writes back on exit, and the resume picker lists.
//!
//! This module is an adapter over the session store, not a writer:
//! [`FileStore`] owns the layout, the atomic write, the privacy modes,
//! the listing, and the id rules; [`PathStore`] owns the explicit
//! checkpoint path a turn commits into. What stays here is the TUI's own
//! shape: [`base_dir`], the [`SavedSession`] row the picker renders
//! (a path, which `--resume` needs), the usage accounting a save
//! carries, and the "an empty history leaves no file" rule, which is
//! product policy rather than storage.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_core::Session;
use rhema_session_store::{SessionStore, StoreError};
use rhema_session_store_file::{FileStore, PathStore};

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
    /// The saved file's byte length, the picker's size hint.
    pub bytes: u64,
}

/// The saved-session base: `$XDG_CACHE_HOME/rhema/sessions`, or under
/// `~/.cache` when the variable is unset. A cache, not data: losing it
/// loses history, never settings.
pub fn base_dir() -> io::Result<PathBuf> {
    let cache = match std::env::var("XDG_CACHE_HOME") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        // The home is rhema-config's resolution, HOME, then
        // USERPROFILE for windows, where HOME is unset outside git
        // bash, so the sessions land where the rest of the state
        // does.
        _ => match rhema_config::user_home() {
            Ok(home) => home.join(".cache"),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "neither XDG_CACHE_HOME, HOME, nor USERPROFILE is set",
                ));
            }
        },
    };
    Ok(cache.join("rhema").join("sessions"))
}

/// The usage accounting a saved session carries: what the Usage page
/// needs to keep reading as one run after a resume. It rides the
/// session as the opaque `usage_stats` JSON blob, the engine and the
/// other session readers never touch it, and this type owns the
/// blob's shape.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageStats {
    /// The transcript's benchmark counters: request counts, the
    /// prompt-cache totals, the categories' token split. `None` when
    /// the session ran with the transcript off or saved none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<rhema_transcript::TranscriptStats>,
    /// The transcript's committed sections (range, text, origin), so a
    /// resumed session adopts the compaction the last session already
    /// paid for instead of re-deriving and re-summarizing it. `None`
    /// when the session ran with the transcript off or saved none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_sections: Option<Vec<rhema_transcript::Section>>,
    /// The last finished turn's token accounting, so the page's "last
    /// turn" row and the context gauge pick up where the save left
    /// off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<rhema_core::Usage>,
}

impl UsageStats {
    /// Reads the accounting off a session; `None` when it carries
    /// none, a file from before the field existed, or a blob that no
    /// longer parses. Stats are decoration: a bad blob costs the page
    /// its numbers, never the resume.
    pub fn from_session(session: &Session) -> Option<Self> {
        if session.usage_stats.is_null() {
            return None;
        }
        serde_json::from_value(session.usage_stats.clone()).ok()
    }

    /// Stamps the accounting onto `session`, the save path, so the
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

/// Picks the file a workspace's next session lands in: the store
/// allocates an id and creates its directory (`0700`); naming a file
/// writes nothing, a run that never speaks leaves no husk behind, so a
/// checkpoint target can be allocated before the first turn and used by
/// whatever turn speaks first.
pub fn next_session_path(base: &Path, workspace: &Path) -> io::Result<PathBuf> {
    let store = FileStore::scoped(base, workspace);
    let id = store.next_id().map_err(to_io)?;
    Ok(store.path_for(&id))
}

/// Writes `session` under `base` for `workspace`, returning the file it
/// landed in. A session with no history is not written: there is nothing
/// to resume, and an empty conversation leaves no file behind.
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

/// Writes `session` to `path`, the in-place update a resumed session
/// gets on exit, so continuing it never grows a second entry. The write
/// is the store's: a private temp file, fsynced, renamed over the
/// target, so a kill mid-write leaves the previous checkpoint whole.
pub fn write_session(path: &Path, session: &Session) -> io::Result<()> {
    PathStore::new(path).save(session).map_err(to_io)
}

/// The saved sessions for `workspace`, newest first. A missing directory
/// is an empty list, nothing saved yet, not an error. A file whose JSON
/// no longer parses still lists (with no snippet): the resume attempt
/// surfaces the corruption, the listing does not hide it.
pub fn list_sessions(base: &Path, workspace: &Path) -> io::Result<Vec<SavedSession>> {
    let store = FileStore::scoped(base, workspace);
    let rows = store.list().map_err(to_io)?;
    Ok(rows
        .into_iter()
        // A subagent's session is a real file next to its parent, and a
        // conversation picker must not offer it: resuming it as a
        // conversation would make a second parent for work that has
        // one.
        .filter(|meta| !rhema_subagents::is_child(&meta.id))
        .map(|meta| SavedSession {
            path: store.path_for(&meta.id),
            saved_at: meta.saved_at,
            snippet: meta.opening_prompt,
            bytes: meta.bytes,
        })
        .collect())
}

/// The store's failures as the `io::Result` this module's callers have
/// always seen: a filesystem failure keeps its kind, anything else reads
/// as invalid data.
fn to_io(err: StoreError) -> io::Error {
    match err {
        StoreError::Io { source, .. } => source,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
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

/// A file size as the picker's metadata line shows it: whole bytes
/// under a kilobyte, whole kilobytes under a megabyte, one decimal
/// above, so a row reads as a length and not as a byte count.
pub fn size_label(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", (bytes + KB / 2) / KB)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
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
        // The directories do exist, so the first commit lands in place.
        assert!(path.parent().unwrap().is_dir());

        // A second pick never lands on a name that is taken on disk.
        write_session(&path, &session_with("hello")).unwrap();
        let second = next_session_path(&base, workspace).unwrap();
        assert_ne!(path, second);
    }

    #[test]
    fn save_then_list_reads_the_session_back_newest_first() {
        let (_dir, base) = temp_base();
        let workspace = Path::new("/home/u/proj");

        let first = save_session(&base, workspace, &session_with("first prompt"))
            .unwrap()
            .expect("a session with history saves");
        std::thread::sleep(std::time::Duration::from_millis(5));
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
        let loaded = rhema_session_store_file::PathStore::new(&first)
            .load()
            .unwrap()
            .unwrap();
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
            // The compaction rides with the counters, so the resumed
            // session adopts it instead of re-summarizing.
            transcript_sections: Some(vec![rhema_transcript::Section {
                range: rhema_transcript::tree::Range { id: 0, n: 12 },
                text: "## Goals Met\n- earlier work".into(),
                origin: rhema_transcript::SectionOrigin::Model,
                pinned: Vec::new(),
            }]),
        };
        stats.clone().write_into(&mut session);
        let path = save_session(&base, workspace, &session).unwrap().unwrap();

        // What the file holds, a load reads back: the same shape
        // --resume takes.
        let loaded = rhema_host_agent::HostAgent::load_session_from(&path).unwrap();
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
    fn sizes_read_in_buckets() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1024), "1 KB");
        assert_eq!(size_label(66_403), "65 KB");
        assert_eq!(size_label(1024 * 1024), "1.0 MB");
        assert_eq!(size_label(10_639_021), "10.1 MB");
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

#[cfg(test)]
mod picker_tests {
    use super::*;
    use rhema_core::{Message, Role};
    use rhema_session_store::{SessionId, SessionStore};

    #[test]
    fn a_subagent_child_is_not_offered_as_a_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        let workspace = Path::new("/home/u/proj");
        let store = FileStore::scoped(&base, workspace);
        let parent = SessionId::new("0000000000000001-00").unwrap();
        let child = SessionId::new("0000000000000001-00.sub-0000000000000000-0002").unwrap();
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "a prompt"));
        store.save(&parent, &session).unwrap();
        store.save(&child, &session).unwrap();

        let listed = list_sessions(&base, workspace).unwrap();
        assert_eq!(listed.len(), 1, "only the conversation lists: {listed:?}");
        assert_eq!(
            listed[0].path.file_stem().and_then(|stem| stem.to_str()),
            Some(parent.as_str())
        );
    }
}
