//! One host directory standing in as the sandbox filesystem.
//!
//! [`FsRoot`] is the fs-sharing family's shared piece (spec §4): the
//! sandbox's POSIX tree maps onto one host directory, and every file
//! route — and every exec working directory — goes through
//! [`FsRoot::map`]. The paths are model-influenced; the guard is the
//! point. It refuses `..` in both directions: a sandbox path that
//! climbs out of the root textually, and a host path that resolves
//! outside the root — a symlink planted in the workspace is refused
//! exactly like a `..`.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use crate::SandboxError;

/// One host directory standing in as the sandbox filesystem.
#[derive(Debug, Clone)]
pub struct FsRoot {
    /// The canonical host directory every sandbox path maps into; the
    /// canonical form is what makes the containment checks below
    /// meaningful when the root itself sits behind a link.
    root: PathBuf,
}

impl FsRoot {
    /// Creates (when missing) and canonicalizes the host root. A root
    /// that cannot be stood up is an unavailable sandbox, not a failed
    /// command.
    pub fn new(host: PathBuf) -> Result<Self, SandboxError> {
        std::fs::create_dir_all(&host).map_err(|err| {
            SandboxError::Unavailable(format!("sandbox root {}: {err}", host.display()))
        })?;
        let root = std::fs::canonicalize(&host).map_err(|err| {
            SandboxError::Unavailable(format!("sandbox root {}: {err}", host.display()))
        })?;
        Ok(Self { root })
    }

    /// Maps a sandbox-absolute POSIX path (`/workspace/x`) onto the
    /// host root (`<root>/workspace/x`). The empty string is the root
    /// itself — the shim's absent `cwd` — and nothing else may be
    /// relative.
    pub fn map(&self, sandbox_path: &str) -> Result<PathBuf, SandboxError> {
        if sandbox_path.is_empty() {
            return Ok(self.root.clone());
        }
        if !sandbox_path.starts_with('/') {
            return Err(SandboxError::Failed(format!(
                "sandbox path must be absolute: {sandbox_path:?}"
            )));
        }
        // The sandbox direction: `..` never maps, whatever it would
        // land on; `//` and `/a/./b` spell plain nesting.
        let mut names: Vec<&str> = Vec::new();
        for name in sandbox_path.split('/') {
            match name {
                "" | "." => {}
                ".." => {
                    return Err(SandboxError::Failed(format!(
                        "sandbox path escapes the workspace: {sandbox_path:?}"
                    )));
                }
                name => names.push(name),
            }
        }
        self.resolve_inside(&names)
    }

    /// The host direction: walks the vetted names component by
    /// component, so every link that exists along the way is resolved
    /// and re-checked against the root. Only something that exists can
    /// redirect the walk — a link is resolved (a dangling one fails),
    /// a directory the walk continues through, anything else is a
    /// plain leaf name for the caller's fs call to judge.
    fn resolve_inside(&self, names: &[&str]) -> Result<PathBuf, SandboxError> {
        let mut resolved = self.root.clone();
        for name in names {
            resolved.push(name);
            if let Ok(meta) = std::fs::symlink_metadata(&resolved)
                && (meta.is_symlink() || meta.is_dir())
            {
                resolved = std::fs::canonicalize(&resolved).map_err(|err| {
                    SandboxError::Failed(format!("sandbox path does not resolve: {err}"))
                })?;
                if !resolved.starts_with(&self.root) {
                    return Err(SandboxError::Failed(
                        "sandbox path leaves the workspace through a link".into(),
                    ));
                }
            }
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical form of a tempdir's path — the form [`FsRoot`]
    /// stores, and the form the mapping's results compare against (a
    /// tempdir may sit behind a link, `/tmp` on macOS among them).
    fn canonical(dir: &std::path::Path) -> PathBuf {
        std::fs::canonicalize(dir).expect("the tempdir resolves")
    }

    #[test]
    fn a_nested_sandbox_path_maps_under_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        let host = canonical(dir.path());
        assert_eq!(
            root.map("/workspace/x").expect("the path maps"),
            host.join("workspace").join("x")
        );
        assert_eq!(
            root.map("/a/b/c").expect("the path maps"),
            host.join("a").join("b").join("c")
        );
    }

    #[test]
    fn the_empty_path_and_the_root_slash_are_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        assert_eq!(
            root.map("").expect("the empty path maps"),
            canonical(dir.path())
        );
        assert_eq!(
            root.map("/").expect("the slash maps"),
            canonical(dir.path())
        );
    }

    #[test]
    fn a_relative_sandbox_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        assert!(root.map("workspace").is_err());
        assert!(root.map("workspace/x").is_err());
    }

    #[test]
    fn a_sandbox_path_may_not_climb_out_of_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        for path in ["/..", "/../etc", "/workspace/../..", "/a/../../b"] {
            let error = root
                .map(path)
                .expect_err("the climb is refused in the sandbox spelling");
            assert!(
                matches!(error, SandboxError::Failed(_)),
                "the refusal is an in-band failure, not a crash: {error:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_host_path_that_resolves_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        let host = canonical(dir.path());
        std::fs::create_dir_all(host.join("workspace")).expect("the workspace dir is made");
        // A link pointing inside the root is fine: the mapping follows
        // it and stays put.
        std::os::unix::fs::symlink("workspace", host.join("in")).expect("the link is made");
        assert_eq!(
            root.map("/in/deep.txt").expect("the inside link maps"),
            host.join("workspace").join("deep.txt")
        );
        // A link pointing out of the root is refused, with or without a
        // tail behind it.
        std::os::unix::fs::symlink("/etc", host.join("out")).expect("the link is made");
        assert!(root.map("/out").is_err());
        assert!(root.map("/out/passwd").is_err());
        // A dangling link resolves to nothing and is refused too.
        std::os::unix::fs::symlink("/nonexistent-rhema-fsroot", host.join("dangling"))
            .expect("the link is made");
        assert!(root.map("/dangling/x").is_err());
    }
}
