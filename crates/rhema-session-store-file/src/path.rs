//! [`PathStore`]: one explicit file, the `--save`/`--resume` shape.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rhema_core::Session;
use rhema_session_store::StoreError;

use crate::write::write_atomic;

/// One session at one explicit path.
///
/// This is not a [`SessionStore`](rhema_session_store::SessionStore): its
/// identity is the path, which may sit outside any managed root. The
/// write protocol is the managed store's, so a `--save` file is atomic
/// and durable in exactly the same way.
#[derive(Debug, Clone)]
pub struct PathStore {
    path: PathBuf,
}

impl PathStore {
    /// A store over `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session at [`Self::path`]; `None` when the file is missing.
    /// A file that does not parse is an error: a resume surfaces it.
    pub fn load(&self) -> Result<Option<Session>, StoreError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|err| {
                StoreError::Backend(format!(
                    "session file {} does not parse: {err}",
                    self.path.display()
                ))
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Atomically replaces the file, creating its parent `0700` when the
    /// parent is missing.
    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        let tag = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_owned());
        write_atomic(&self.path, &tag, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Message, Role};

    fn session_with(prompt: &str) -> Session {
        Session {
            history: vec![Message::text(Role::User, prompt)],
            ..Session::default()
        }
    }

    #[test]
    fn a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = PathStore::new(dir.path().join("missing.json"));
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn a_save_then_load_round_trips_and_creates_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs").join("exit.json");
        let store = PathStore::new(&path);
        let session = session_with("saved by --save");
        store.save(&session).unwrap();
        assert_eq!(store.load().unwrap(), Some(session));
        assert!(path.is_file());
        assert_eq!(store.path(), path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    /// A directory the save did not create keeps its mode: `--save
    /// ~/notes/x.json` must not hide the user's own directory.
    #[test]
    #[cfg(unix)]
    fn an_existing_parent_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = notes.join("session.json");

        PathStore::new(&path)
            .save(&session_with("into notes"))
            .unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&notes), 0o755, "the user's directory stays theirs");
        assert_eq!(mode(&path), 0o600, "the file itself is private");
    }

    #[test]
    fn a_save_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        PathStore::new(&path).save(&session_with("one")).unwrap();
        PathStore::new(&path).save(&session_with("two")).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["session.json".to_owned()]);
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let store = PathStore::new(&path);
        store.save(&session_with("the good copy")).unwrap();
        let before = fs::read(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
            let result = store.save(&session_with("the new copy"));
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            // A privileged runner ignores the mode; the assertion below
            // is only meaningful when the write was actually refused.
            if result.is_err() {
                assert_eq!(fs::read(&path).unwrap(), before);
                let names: Vec<String> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, vec!["session.json".to_owned()]);
            }
        }
        #[cfg(not(unix))]
        let _ = before;
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(&path, "{not json").unwrap();
        let err = PathStore::new(&path).load().unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err}");
    }
}
