//! Private-by-mode files and directories for state that holds
//! credentials or conversation content: directories `0700`, files
//! `0600`. The mode applies at creation, so nothing sensitive ever
//! touches disk world-readable, and is re-asserted on every write, so
//! state that predates the rule — or was widened outside these
//! helpers — tightens again. Beyond the create-time mode, changes are
//! best-effort: a filesystem that refuses mode changes (FAT, some
//! network mounts) must not turn a save that would otherwise succeed
//! into an error.

use std::io;
use std::path::Path;

/// Creates `path` and its parents, then restricts the leaf directory
/// to `0700` (best-effort). The parents keep their modes — only the
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
    tighten(&_file);
    use std::io::Write;
    file.write_all(contents.as_ref())
}

/// Best-effort `0600` on an existing file this crate did not create —
/// a SQLite database another opener made, tightening whatever the
/// umask left wide. Silent by contract; see the module docs.
pub fn tighten_file(path: &Path) {
    let opened = std::fs::OpenOptions::new().write(true).open(path);
    if let Ok(_file) = opened {
        #[cfg(unix)]
        tighten(&_file);
    }
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
}
