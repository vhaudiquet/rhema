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
    // The label describes git state, so git activity must invalidate
    // the cached build script — without these, a label outlives the
    // tree it named (a commit touching nothing in the crate would
    // leave the previous sha standing). HEAD covers checkouts; the
    // index covers commits, adds, and status's stat refreshes. The
    // sources and manifest keep the default freshness. In packaged
    // builds the .git paths do not exist and the queries below answer
    // None anyway.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    // Refs move on tag create/delete too — the stable label keys off
    // an exact tag at HEAD, so a tag operation is a label event.
    // packed-refs covers repositories whose refs have been packed.
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    // The logo asset bakes into the binary; a changed image is a build
    // event the same way a changed source is. Absent in packaged
    // builds, where the path simply never fires.
    println!("cargo:rerun-if-changed=../../.github/img/a1-breath-dark.png");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
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
            } else if dirty {
                // Before the channel check: a dirty tree is never an
                // edge build — the CI legs are always clean — and a
                // stray channel variable in a developer's shell must
                // not rename local changes.
                format!("{pkg}+{sha}.dirty")
            } else if std::env::var("RHEMA_BUILD_CHANNEL").as_deref() == Ok("edge") {
                format!("{pkg}-edge.{sha}")
            } else {
                format!("{pkg}+{sha}")
            }
        }
        None => pkg,
    };
    println!("cargo:rustc-env=RHEMA_VERSION_LABEL=v{label}");

    // The onboarding logo, copied into OUT_DIR as one canonical asset:
    // the source lives with the repository's other images, and the
    // include stays inside the crate either way. The file is always
    // written (empty when the asset is absent — a packaged crate
    // without the docs images) because an include in a dead arm still
    // names a path that must exist; `RHEMA_LOGO_EMBED` says which case
    // landed, and the runtime falls back to the text wordmark on None.
    let logo = std::path::Path::new(&manifest).join("../../.github/img/a1-breath-dark.png");
    let bytes = std::fs::read(&logo).unwrap_or_default();
    let out = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let dest = std::path::Path::new(&out).join("rhema-logo.png");
    let embedded = !bytes.is_empty() && std::fs::write(&dest, bytes).is_ok();
    if embedded {
        println!("cargo:rustc-env=RHEMA_LOGO_EMBED=1");
    }
}
