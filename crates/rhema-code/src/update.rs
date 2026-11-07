//! The self-update seam: the version check against the distribution
//! channel's manifest, and the install script run behind the settings'
//! Update page. Everything shells out to `curl` (else `wget`) — the same
//! tools and host the published installer uses; this crate carries no
//! HTTP client of its own.

use std::io::ErrorKind;
use std::process::{Command, Stdio};

/// The distribution host the manifest and the install script live on.
/// `RHEMA_DIST_BASE` overrides it (mirrors, tests, airgapped caches) —
/// the same override the installer script honors, and the value the
/// spawned script inherits on its own fetches.
const DIST_BASE: &str = "https://get.rhema.rs";

/// The version this binary was built at.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The distribution base URL: `RHEMA_DIST_BASE` over the default.
pub fn dist_base() -> String {
    std::env::var("RHEMA_DIST_BASE").unwrap_or_else(|_| DIST_BASE.to_owned())
}

/// The channel's latest release: the manifest's `stable` pointer, the
/// version the install script would put on disk.
pub async fn fetch_latest_version() -> Result<String, String> {
    let url = format!("{}/manifest.json", dist_base());
    let body = fetch_url(&url).await?;
    parse_stable(&body)
}

/// Runs the install script — the published `get.rhema.rs/code` one —
/// through `sh`. Returns the script's output tail (its last line names
/// what landed where), or the failure with the script's own error text.
/// The new binary is on disk when this answers `Ok`; the running process
/// keeps the old one until a restart.
pub async fn run_installer() -> Result<String, String> {
    let url = format!("{}/code", dist_base());
    tokio::task::spawn_blocking(move || run_installer_blocking(&url))
        .await
        .map_err(|err| format!("the install task failed: {err}"))?
}

/// One fetch through curl, else wget — the installer's own fallback
/// order. The tools carry their own time bounds so a dead link cannot
/// park a blocking-pool thread forever.
async fn fetch_url(url: &str) -> Result<String, String> {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || fetch_url_blocking(&url))
        .await
        .map_err(|err| format!("the fetch task failed: {err}"))?
}

fn fetch_url_blocking(url: &str) -> Result<String, String> {
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", "--max-time", "20"])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match curl.output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(format!(
            "curl exited with {}{}",
            out.status,
            stderr_tail(&out.stderr)
        )),
        // No curl on the machine: the installer's wget fallback.
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut wget = Command::new("wget");
            wget.args(["-qO-", "-T", "20"])
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match wget.output() {
                Ok(out) if out.status.success() => {
                    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                }
                Ok(out) => Err(format!(
                    "wget exited with {}{}",
                    out.status,
                    stderr_tail(&out.stderr)
                )),
                Err(err) => Err(format!("neither curl nor wget runs: {err}")),
            }
        }
        Err(err) => Err(format!("cannot run curl: {err}")),
    }
}

fn run_installer_blocking(url: &str) -> Result<String, String> {
    // The URL rides as a positional parameter, never spliced into the
    // shell text. The script lands in a temp file rather than piping
    // straight into `sh` so curl's exit code reaches us — a `curl | sh`
    // swallows a failed fetch behind an empty-input shell that exits 0.
    let child = Command::new("sh")
        .arg("-c")
        .arg(
            "tmp=$(mktemp) || exit 1\n\
             if command -v curl >/dev/null 2>&1; then\n\
               curl -fsSL \"$1\" -o \"$tmp\"\n\
             else\n\
               wget -qO \"$tmp\" \"$1\"\n\
             fi || { rm -f \"$tmp\"; exit 1; }\n\
             sh \"$tmp\"; rc=$?; rm -f \"$tmp\"; exit $rc",
        )
        .arg("sh")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot run sh: {err}"))?;
    let out = child
        .wait_with_output()
        .map_err(|err| format!("cannot wait for the installer: {err}"))?;
    let text = tail(&String::from_utf8_lossy(&out.stdout), OUTPUT_TAIL_BYTES);
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "the installer exited with {}{}{}",
            out.status,
            stderr_tail(&out.stderr),
            if text.is_empty() {
                String::new()
            } else {
                format!("\n{text}")
            }
        ))
    }
}

/// How much of the installer's output a result carries: the progress
/// lines and their last line — what landed where — not a whole log.
const OUTPUT_TAIL_BYTES: usize = 2048;

/// The manifest's `stable` field: the channel's version pointer. The
/// binaries' map beside it concerns the installer, not a check.
fn parse_stable(body: &str) -> Result<String, String> {
    let manifest: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("the manifest is not JSON: {err}"))?;
    match manifest.get("stable").and_then(|value| value.as_str()) {
        Some(version) if !version.trim().is_empty() => Ok(version.trim().to_owned()),
        _ => Err("the manifest carries no stable version".to_owned()),
    }
}

/// A failed fetch's stderr, trimmed to a sentence's worth: the tools'
/// own error text is the useful part of the failure.
fn stderr_tail(bytes: &[u8]) -> String {
    let text = tail(&String::from_utf8_lossy(bytes), 200);
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

/// The last `max` bytes of `text`, cut at a char boundary and its
/// trailing whitespace dropped.
fn tail(text: &str, max: usize) -> String {
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text[cut..].to_owned()
}

/// Whether `latest` is a release above `current`: dotted numeric
/// segments compared part by part. Non-numeric tails of a segment (a
/// `-rc1`, an `+sha`) count for nothing — a prerelease of the same
/// numbers reads equal, and equal is not newer; neither is a current
/// build already ahead of the channel.
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        version
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (latest, current) = (parts(latest), parts(current));
    for index in 0..latest.len().max(current.len()) {
        let left = latest.get(index).copied().unwrap_or(0);
        let right = current.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifests_stable_pointer_is_the_latest_version() {
        let manifest = r#"{
          "stable": "0.2.1",
          "binaries": {
            "rhema-linux-amd64": {"url": "https://get.rhema.rs/0.2.1/rhema-linux-amd64.tar.gz", "sha256": "ab"}
          }
        }"#;
        assert_eq!(
            parse_stable(manifest),
            Ok("0.2.1".to_owned()),
            "the pointer survives the binaries beside it"
        );
        assert_eq!(
            parse_stable(r#"{"binaries": {}}"#),
            Err("the manifest carries no stable version".to_owned()),
        );
        assert!(
            parse_stable("not json").is_err(),
            "a non-JSON body is a failure, not a version"
        );
    }

    #[test]
    fn newer_releases_compare_numerically_part_by_part() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"), "numeric, not lexicographic");
        assert!(is_newer("0.3", "0.2.9"), "a shorter prefix above");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"), "equal is not newer");
        assert!(
            !is_newer("0.2.0", "0.2.1"),
            "the channel may trail a dev build"
        );
        assert!(!is_newer("0.2", "0.2.0"), "missing parts read as zero");
        assert!(
            !is_newer("0.2.0-rc1", "0.2.0"),
            "a prerelease of the same numbers is not newer"
        );
        assert!(
            !is_newer("0.2.0+sha", "0.2.0"),
            "a build suffix is not newer either"
        );
    }

    #[test]
    fn the_tail_keeps_the_last_bytes_at_a_char_boundary() {
        assert_eq!(tail("installed rhema 0.2.1", 5), "0.2.1");
        assert_eq!(tail("short", 100), "short");
        assert_eq!(tail("trailing  \n", 100), "trailing");
        // A multibyte character straddling the cut moves whole.
        let text = "h\u{e9}llo world";
        assert_eq!(tail(text, 9), "llo world");
    }
}
