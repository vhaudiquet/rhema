//! The self-update seam: the version check against the distribution
//! channel's manifest, and the install script run behind the settings'
//! Update page. Everything shells out to `curl` (else `wget`), the same
//! tools and host the published installer uses; this crate carries no
//! HTTP client of its own.
//!
//! ON WINDOWS the published `code.ps1` copies the new `rhema.exe` over
//! the old one, and windows locks a running image against that copy
//! (a rename it allows, a write it does not). An update started from
//! the installed binary therefore renames the running image aside
//! first, runs the installer, and puts the image back when the install
//! did not land. The parked image is this process's own image, so it
//! only becomes deletable at the next start, which clears it.

use std::io::{ErrorKind, Write as _};
use std::process::{Command, Stdio};

/// The distribution host the manifest and the install script live on.
/// `RHEMA_DIST_BASE` overrides it (mirrors, tests, airgapped caches),
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

/// Drops the image a finished update left beside the new binary.
/// Windows cannot delete a running executable, so the install parks the
/// running image under `rhema.exe.old` and this clears it on the next
/// start, once the process that held it has exited. Best effort: a
/// leftover file is litter, not a startup failure.
pub fn clear_stale_install() {
    #[cfg(windows)]
    if let Some(dir) = install_dir() {
        let _ = std::fs::remove_file(dir.join(PARKED_IMAGE));
    }
}

/// The channel's latest release: the manifest's `stable` pointer, the
/// version the install script would put on disk.
pub async fn fetch_latest_version() -> Result<String, String> {
    let url = format!("{}/manifest.json", dist_base());
    let body = fetch_url(&url).await?;
    parse_stable(&body)
}

/// Runs the install script, the published `get.rhema.rs/code` one
/// through `sh` on unix, the `code.ps1` twin on windows, where `sh`
/// has no interpreter and every host ships powershell. Returns the
/// script's output tail (its last line names what landed where), or
/// the failure with the script's own error text. The new binary is on
/// disk when this answers `Ok`; the running process keeps the old one
/// until a restart.
pub async fn run_installer() -> Result<String, String> {
    // The script runs only after its published `<url>.sha256` sidecar
    // matches. That catches a corrupted download or a swapped mirror
    // response; it does not authenticate the distribution host, which
    // needs a signed manifest and a trusted public key in this binary.
    #[cfg(windows)]
    let url = format!("{}/code.ps1", dist_base());
    #[cfg(not(windows))]
    let url = format!("{}/code", dist_base());
    tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        {
            run_powershell_installer_blocking(&url)
        }
        #[cfg(not(windows))]
        {
            run_installer_blocking(&url)
        }
    })
    .await
    .map_err(|err| format!("the install task failed: {err}"))?
}

/// The windows twin: the published `code.ps1` fetched to a temp file
/// and run with `-File`, a param-carrying script cannot ride inside
/// `-Command`, and a file run keeps the fetch's nonzero exit from
/// reading as an empty-input success. The script also serves
/// `irm | iex`, which cannot exit nonzero, so its output is the
/// contract: the `installed rhema` line only prints once the binary
/// landed. stdout is piped, and both powershell 5.1 and 7 land
/// `Write-Host` text in redirected stdout.
///
/// The script copies the new image over `%LOCALAPPDATA%\Programs\rhema\rhema.exe`,
/// which windows refuses while that file is the running one, so the
/// running image steps aside first and comes back if the install did
/// not land.
#[cfg(windows)]
fn run_powershell_installer_blocking(url: &str) -> Result<String, String> {
    let script = fetch_verified_script(url)?;
    let mut file = tempfile::Builder::new()
        .prefix("rhema-install-")
        .suffix(".ps1")
        .tempfile()
        .map_err(|err| format!("cannot create the install script: {err}"))?;
    file.write_all(script.as_bytes())
        .map_err(|err| format!("cannot write the install script: {err}"))?;
    let parked = park_running_image()?;
    let out = match run_powershell(file.path()) {
        Ok(out) => out,
        Err(err) => {
            return Err(match unpark_image(parked) {
                Ok(()) => err,
                Err(restore) => format!("{err}; {restore}"),
            });
        }
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = tail(&text, OUTPUT_TAIL_BYTES);
    if reports_install(&text) {
        // The new image is in place. The parked one is this process's
        // own image, locked until the process exits, and the next
        // start clears it.
        return Ok(text);
    }
    let restored = unpark_image(parked);
    let mut message = format!(
        "the installer did not report an install{}{}",
        if text.is_empty() { "" } else { ":" },
        if text.is_empty() {
            String::new()
        } else {
            format!("\n{text}")
        }
    );
    if let Err(err) = restored {
        message.push_str(&format!("; {err}"));
    }
    Err(message)
}

/// Runs the fetched installer with windows powershell, falling back to
/// `pwsh.exe` on an image that carries only powershell 7. Both take the
/// same flags; `-ExecutionPolicy` is a no-op where the setting does not
/// exist.
#[cfg(windows)]
fn run_powershell(script: &std::path::Path) -> Result<std::process::Output, String> {
    let build = |program: &str| {
        let mut command = Command::new(program);
        command
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    match build("powershell.exe").output() {
        Ok(out) => Ok(out),
        Err(err) if err.kind() == ErrorKind::NotFound => build("pwsh.exe")
            .output()
            .map_err(|err| format!("cannot run powershell.exe or pwsh.exe: {err}")),
        Err(err) => Err(format!("cannot run powershell.exe: {err}")),
    }
}

/// The name the running image waits under while the installer writes
/// the new one.
#[cfg(windows)]
const PARKED_IMAGE: &str = "rhema.exe.old";

/// The installer's destination on windows: the directory `code.ps1`
/// copies `rhema.exe` into, `%LOCALAPPDATA%\Programs\rhema`.
#[cfg(windows)]
fn install_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("Programs")
            .join("rhema"),
    )
}

/// Moves the running image out of the installer's way, when this
/// process runs from the path the installer writes. Windows refuses to
/// write over a mapped executable but allows renaming it, so the
/// parked file is a working image and the destination is free. A build
/// started from anywhere else (a cargo target dir, a copy on PATH) has
/// no lock to clear and stays put.
#[cfg(windows)]
fn park_running_image() -> Result<Option<std::path::PathBuf>, String> {
    let Some(dir) = install_dir() else {
        return Ok(None);
    };
    let target = dir.join("rhema.exe");
    let me = std::env::current_exe()
        .map_err(|err| format!("cannot read this executable's path: {err}"))?;
    if !same_file(&me, &target) {
        return Ok(None);
    }
    let parked = dir.join(PARKED_IMAGE);
    // A leftover from an update this machine already restarted past.
    let _ = std::fs::remove_file(&parked);
    std::fs::rename(&target, &parked)
        .map_err(|err| format!("cannot move the running rhema.exe aside: {err}"))?;
    Ok(Some(parked))
}

/// Puts the parked image back at the installer's destination, after a
/// run that did not land. A partial copy the installer left behind
/// gives way to it.
#[cfg(windows)]
fn unpark_image(parked: Option<std::path::PathBuf>) -> Result<(), String> {
    let Some(parked) = parked else {
        return Ok(());
    };
    let Some(dir) = parked.parent() else {
        return Ok(());
    };
    std::fs::rename(&parked, dir.join("rhema.exe"))
        .map_err(|err| format!("cannot put the running rhema.exe back: {err}"))
}

/// Whether two paths name the same file: resolved, then compared
/// without the case difference windows path spellings carry.
#[cfg(windows)]
fn same_file(left: &std::path::Path, right: &std::path::Path) -> bool {
    let resolve = |path: &std::path::Path| {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    resolve(left) == resolve(right)
}

/// The script's success line: `installed rhema <version> to <path>`.
#[cfg(windows)]
fn reports_install(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim().starts_with("installed rhema"))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{reports_install, same_file};

    #[test]
    fn the_running_image_is_recognized_by_either_spelling() {
        // The shell's spelling and the exe's differ in case; a
        // canonicalize that cannot resolve (the file is absent on a
        // test box) falls back to the raw spelling.
        assert!(same_file(
            std::path::Path::new(r"C:\Users\u\Programs\rhema\rhema.exe"),
            std::path::Path::new(r"c:\users\u\programs\rhema\RHEMA.EXE"),
        ));
        assert!(!same_file(
            std::path::Path::new(r"C:\rhema.exe"),
            std::path::Path::new(r"C:\rhema\rhema.exe"),
        ));
    }

    #[test]
    fn the_success_line_names_the_install() {
        assert!(reports_install(
            "downloading rhema 0.2.6 ...\ninstalled rhema 0.2.6 to C:\\u\\rhema.exe\n"
        ));
        assert!(!reports_install(
            "rhema installer: cannot reach https://get.rhema.rs\n"
        ));
        assert!(!reports_install(""));
    }
}

/// One fetch through curl, else wget, the installer's own fallback
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

/// Fetches an install script and refuses it unless the published
/// `<url>.sha256` sidecar matches. The sidecar rides the same host as
/// the script, so this proves integrity, not authenticity.
pub(crate) fn fetch_verified_script(url: &str) -> Result<String, String> {
    let script = fetch_url_blocking(url)?;
    let sidecar = fetch_url_blocking(&format!("{url}.sha256"))?;
    verify_sha256(script.as_bytes(), &sidecar)?;
    Ok(script)
}

/// Checks one file against a sha256 sidecar (`<hex>`, optionally
/// followed by the file name). A missing or malformed sidecar is a
/// failure: an unverifiable installer must not run.
fn verify_sha256(bytes: &[u8], sidecar: &str) -> Result<(), String> {
    use sha2::Digest as _;
    let expected = sidecar
        .split_whitespace()
        .next()
        .ok_or_else(|| "the installer checksum is empty".to_owned())?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("the installer checksum is malformed".to_owned());
    }
    let actual: String = sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "the installer checksum does not match: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_installer_blocking(url: &str) -> Result<String, String> {
    // Fetch, verify, then run the local file. The URL never rides
    // inside shell text, and a `curl | sh` cannot swallow a failed
    // fetch behind an empty-input shell that exits 0.
    let script = fetch_verified_script(url)?;
    let mut file = tempfile::Builder::new()
        .prefix("rhema-install-")
        .suffix(".sh")
        .tempfile()
        .map_err(|err| format!("cannot create the install script: {err}"))?;
    file.write_all(script.as_bytes())
        .map_err(|err| format!("cannot write the install script: {err}"))?;
    let out = Command::new("sh")
        .arg(file.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("cannot run sh: {err}"))?;
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
/// lines and their last line, what landed where, not a whole log.
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
/// `-rc1`, an `+sha`) count for nothing, a prerelease of the same
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
    fn an_installer_checksum_must_match_its_sidecar() {
        // sha256("abc")
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", sum).is_ok());
        assert!(verify_sha256(b"abc", &format!("{sum}  code\n")).is_ok());
        assert!(verify_sha256(b"abc", &sum.to_ascii_uppercase()).is_ok());
        assert!(verify_sha256(b"abcd", sum).is_err());
        assert!(verify_sha256(b"abc", "not-a-checksum").is_err());
        assert!(verify_sha256(b"abc", "").is_err());
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
