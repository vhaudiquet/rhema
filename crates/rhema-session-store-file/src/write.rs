//! The one atomic write both file stores use.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rhema_core::Session;
use rhema_session_store::StoreError;

/// Names the temp files of one process; two threads saving the same id
/// never collide on a name.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `session` to `path` atomically and durably.
///
/// `tag` only names the temp file; it comes from the session id or the
/// path's file name and never from untrusted bytes on its own.
///
/// The session streams into a private temp file in the target's
/// directory, the file is flushed and fsynced, renamed over the target,
/// and the directory is fsynced best effort. On any failure the temp is
/// removed and the previous file is untouched.
pub(crate) fn write_atomic(path: &Path, tag: &str, session: &Session) -> Result<(), StoreError> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // A directory this save creates is ours to tighten to 0700. An
    // existing one is the caller's: `--save ~/notes/x.json` names a
    // directory the user made, and hiding their own files from their
    // group is not this store's call. The temp and the target land 0600
    // either way, which is what the content needs.
    let io_error = |source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    };
    if dir.exists() {
        fs::create_dir_all(dir).map_err(io_error)?;
    } else {
        rhema_config::privacy::private_dir(dir).map_err(io_error)?;
    }

    let (temp, mut file) = create_temp(dir, tag)?;
    if let Err(err) = write_stream(&mut file, session, &temp) {
        remove(&temp);
        return Err(err);
    }
    drop(file);
    if let Err(err) = fs::rename(&temp, path) {
        remove(&temp);
        return Err(StoreError::Io {
            path: path.to_path_buf(),
            source: err,
        });
    }
    sync_dir(dir)
}

/// Creates the temp file, retrying on the only collision `create_new`
/// can report (a leftover from a reused pid and sequence).
fn create_temp(dir: &Path, tag: &str) -> Result<(PathBuf, File), StoreError> {
    loop {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".{tag}.{}.{seq}.tmp", std::process::id()));
        match rhema_config::privacy::private_create(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(StoreError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
}

/// Serializes straight into the file, no intermediate string, then
/// reaches the durability point before the caller renames.
fn write_stream(file: &mut File, session: &Session, temp: &Path) -> Result<(), StoreError> {
    serde_json::to_writer_pretty(&mut *file, session).map_err(|err| serialize_error(temp, err))?;
    file.flush().map_err(|source| StoreError::Io {
        path: temp.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        path: temp.to_path_buf(),
        source,
    })
}

/// A serialization failure is the writer's IO error when serde_json
/// carried one, and a backend failure (a value that cannot be
/// represented) otherwise.
fn serialize_error(temp: &Path, err: serde_json::Error) -> StoreError {
    match err.io_error_kind() {
        Some(kind) => StoreError::Io {
            path: temp.to_path_buf(),
            source: io::Error::new(kind, err.to_string()),
        },
        None => StoreError::Backend(format!("cannot serialize session: {err}")),
    }
}

/// Fsyncs the directory that now names the target. Platforms that
/// refuse a directory handle (and filesystems that refuse its fsync)
/// are ignored: there the rename itself is all the durability there
/// is. Windows takes the non-unix arm because it refuses to open a
/// directory as a file at all.
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        let handle = match File::open(dir) {
            Ok(handle) => handle,
            Err(err) if unsupported(&err) => return Ok(()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        };
        match handle.sync_all() {
            Ok(()) => Ok(()),
            Err(err) if unsupported(&err) => Ok(()),
            Err(source) => Err(StoreError::Io {
                path: dir.to_path_buf(),
                source,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Whether the platform declined directory fsync rather than failing.
#[cfg(unix)]
fn unsupported(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    )
}

/// Best-effort temp removal; the error is already being returned.
fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::Message;

    #[test]
    fn a_write_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let session = Session {
            history: vec![Message::text(rhema_core::Role::User, "hello")],
            ..Session::default()
        };
        write_atomic(&path, "session", &session).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["session.json".to_owned()]);
    }

    #[test]
    fn a_failed_write_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        // The target directory cannot be created: a regular file holds
        // the name.
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let path = blocked.join("session.json");
        let err = write_atomic(&path, "session", &Session::default()).unwrap_err();
        assert!(matches!(err, StoreError::Io { .. }), "{err}");
    }
}
