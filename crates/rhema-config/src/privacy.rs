//! Private-by-mode files and directories for state that holds
//! credentials or conversation content: directories `0700`, files
//! `0600`. The mode applies at creation, so nothing sensitive ever
//! touches disk world-readable, and is re-asserted on every write, so
//! state that predates the rule, or was widened outside these
//! helpers, tightens again. Beyond the create-time mode, changes are
//! best-effort: a filesystem that refuses mode changes (FAT, some
//! network mounts) must not turn a save that would otherwise succeed
//! into an error.

use std::io;
use std::path::Path;
use std::sync::atomic::AtomicU64;

/// Names one process's temp files; see [`private_write_atomic`].
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Creates `path` and its parents, then restricts the leaf directory
/// to `0700` (best-effort). The parents keep their modes, only the
/// directory this crate owns is tightened.
pub fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    set_mode(path, 0o700);
    Ok(())
}

/// Creates or truncates `path` and writes `contents` at `0600`: the
/// mode applies at create time, and a pre-existing wider file is
/// tightened (best-effort) before the bytes land.
pub fn private_write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let mut file = private_open(path)?;
    #[cfg(unix)]
    tighten(&file);
    use std::io::Write;
    file.write_all(contents.as_ref())
}

/// Writes `contents` to `path` atomically and `0600`: the bytes land
/// in a private sibling temp file first, then rename over `path`. A
/// crash mid-write leaves the previous file whole, and no reader sees
/// a half-written state, the shape session stores need, where the
/// file being replaced may be the only good copy. The temp file is
/// removed when the rename fails.
pub fn private_write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // The name carries a per-process sequence, and `private_create`
    // refuses a name that already exists: two threads writing the same
    // path used to open one temp, truncate each other's bytes, and
    // rename the mixture over the good file.
    let (temp, mut file) = loop {
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
        match private_create(&candidate) {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    };
    let written = {
        use std::io::Write;
        file.write_all(contents.as_ref())
            .and_then(|()| file.flush())
    };
    drop(file);
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Best-effort `0600` on an existing file this crate did not create,
/// a SQLite database another opener made, tightening whatever the
/// umask left wide. Silent by contract; see the module docs.
pub fn tighten_file(path: &Path) {
    let opened = std::fs::OpenOptions::new().write(true).open(path);
    if let Ok(file) = opened {
        #[cfg(unix)]
        tighten(&file);
        // No permission model here: the open is only the existence
        // check, and the handle closes unused.
        #[cfg(not(unix))]
        drop(file);
    }
}

/// Creates `path` as a new file, `0600` at creation, and fails if it
/// already exists. The session store's temp files use this: the mode
/// applies before the first byte lands, and `create_new` means two
/// writers can never open the same temp.
pub fn private_create(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Opens `path` for create-or-truncate writing, `0600` at creation.
fn private_open(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn tighten(file: &std::fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn a_fresh_write_lands_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // The umask could leave anything on the parent; write anyway.
        private_write(&path, "secret").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_widened_file_tightens_on_the_next_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        private_write(&path, "secret").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[cfg(unix)]
    fn tighten_file_covers_what_another_opener_created() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.db");
        std::fs::write(&path, "rows").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        tighten_file(&path);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A missing file is not an error: tightening is best-effort.
        tighten_file(&dir.path().join("absent.db"));
    }

    #[test]
    #[cfg(unix)]
    fn a_private_dir_is_created_0700_and_stays_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("leaf");
        private_dir(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A widened leaf tightens again on the next call.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    #[cfg(unix)]
    fn private_create_is_create_new_and_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temp");
        let file = private_create(&path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(file);
        // Two writers may never open the same temp.
        let err = private_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn an_atomic_write_replaces_whole_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"old").unwrap();
        private_write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Two threads writing the same path used to share one temp named
    /// by the pid alone: each truncated the other's bytes and the rename
    /// committed the mixture.
    #[test]
    fn concurrent_atomic_writes_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let payloads: Vec<String> = (0..8u8).map(|n| format!("{n}").repeat(64 * 1024)).collect();

        let handles: Vec<_> = payloads
            .iter()
            .cloned()
            .map(|payload| {
                let path = path.clone();
                std::thread::spawn(move || private_write_atomic(&path, payload.as_bytes()).unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let landed = std::fs::read(&path).unwrap();
        assert!(
            payloads.iter().any(|payload| payload.as_bytes() == landed),
            "the file holds one whole payload, {} bytes",
            landed.len()
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    #[cfg(unix)]
    fn an_atomic_write_lands_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        private_write_atomic(&path, b"payload").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
