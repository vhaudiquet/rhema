//! Bakes the version label the transcript banner shows, from the tree
//! the binary was built from. Every label anchors on the workspace
//! version — a bare `v` against hex reads as one opaque token — and
//! the states separate by semver's own marks:
//!
//! - `v0.1.0` — a clean checkout exactly at the matching `v*` tag, or
//!   a build outside any git tree (a crates.io checkout)
//! - `v0.1.0-edge.<sha>` — an edge build (`RHEMA_BUILD_CHANNEL=edge`);
//!   a named pre-release of the channel
//! - `v0.1.0+<sha>` — a clean build off any other commit; `+` is
//!   semver's build metadata, which is what a devel build is
//! - `v0.1.0+<sha>.dirty` — a build from a tree with local changes
//!
//! The label rides `env!("RHEMA_VERSION_LABEL")` (see
//! [`crate::VERSION_LABEL`]).

use std::process::Command;

/// One git query, run in the crate dir. None: no answer (no git, no
/// repo, or the query legitimately fails — e.g. exact-match off a tag).
fn git(args: &[&str]) -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    // Only the rhema checkout counts. A packaged crate (crates.io, a
    // `cargo install`) has no .git above it; a verify build inside this
    // workspace's target/ must not adopt the workspace's tree either —
    // both would silently label a foreign build.
    if !std::path::Path::new(&manifest).join("../../.git").is_dir() {
        return None;
    }
    let out = Command::new("git")
        .args(args)
        .current_dir(&manifest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if line.is_empty() { None } else { Some(line) }
}

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    let label = match git(&["rev-parse", "--short=8", "HEAD"]) {
        Some(sha) => {
            let dirty = git(&["status", "--porcelain"])
                .map(|out| !out.is_empty())
                .unwrap_or(false);
            let tag = git(&["describe", "--exact-match", "--tags", "HEAD"]);
            if tag.as_deref() == Some(format!("v{pkg}").as_str()) && !dirty {
                // The release: a clean checkout at this version's tag.
                pkg
            } else if std::env::var("RHEMA_BUILD_CHANNEL").as_deref() == Ok("edge") {
                format!("{pkg}-edge.{sha}")
            } else if dirty {
                format!("{pkg}+{sha}.dirty")
            } else {
                format!("{pkg}+{sha}")
            }
        }
        None => pkg,
    };
    println!("cargo:rustc-env=RHEMA_VERSION_LABEL=v{label}");
}
