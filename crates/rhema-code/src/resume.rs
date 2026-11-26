//! Program resume over OSC 7577: the escape sequence that names the
//! command which brings this session back, so a terminal that restarted,
//! reattached, or reopened its window starts the conversation again
//! instead of an empty prompt. The protocol is
//! [Superlogical's program resume](https://www.superlogical.com/rex/docs/build/program-resume);
//! its sibling in this tree is [`crate::status`], OSC 7501.
//!
//! Wire shape: `ESC ] 7577 ; pid=…:time=…:argv=<b64>,…:cwd=<b64>:mac=<sha256> ESC \`.
//! The `mac` is the SHA-256 of the body, a colon, and the terminal's
//! token; the terminal compares it before it accepts anything.
//!
//! Two properties decide everything here:
//!
//! - The token is the only way to be heard. A terminal that does not set
//!   `OSC7577_TOKEN` does not implement the protocol, so its absence is
//!   the feature gate: no token, no sequence, nothing for terminals that
//!   would ignore it to parse.
//! - A report is a command the terminal may run later without the user
//!   typing it. Each element stays one argument, absolute and free of
//!   control characters, `argv` is never cut, and a command that cannot
//!   be spelled within the spec's 2048 bytes removes the record instead
//!   of leaving a truncated one behind.
//!
//! Lifetime: the record follows the conversation. `/clear`, a `/resume`
//! onto another file, and the first checkpoint of a fresh session all
//! change what would have to come back, and each change re-reports. A
//! clean quit removes the record, because the user ended that session. A
//! crash, a panic, a SIGHUP, and SIGTERM leave it, which is exactly when
//! a terminal is expected to bring the session back; `rhema --resume`
//! points the restored command at a file that may be gone, and
//! `--resume-or-new` starts a fresh conversation there instead of dying
//! on it.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::App;
use crate::status::base64_of;

// The environment variable a terminal sets to advertise program resume,
// carrying the token every report is authenticated with; absent on
// terminals without the protocol. The name is defined beside the code
// that strips it from children, so the two never drift apart.
use rhema_bash::exec::OSC7577_TOKEN as TOKEN_VAR;

/// The spec's minimum report size, the OSC prefix and the terminating
/// `ESC \` included. A terminal must accept a report within it, and may
/// drop one above: rhema stays inside, and removes its record rather
/// than send a command no terminal is obliged to keep whole.
const MAX_REPORT: usize = 2048;

/// What the terminal should bring back, or nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// The session file to resume, absolute.
    Resume(PathBuf),
    /// No conversation to come back to: drop the record.
    Clear,
}

/// The target the app currently names: the file its turns checkpoint
/// into, which is `--save`'s explicit path when the run has one and the
/// conversation's own file otherwise.
fn target(app: &App) -> Target {
    target_of(app.checkpoint_target().as_deref())
}

/// [`target`] without the app, so the rule stays testable.
fn target_of(checkpoint: Option<&Path>) -> Target {
    match checkpoint.and_then(absolute) {
        Some(session) => Target::Resume(session),
        None => Target::Clear,
    }
}

/// The report body: `pid`, `time`, the command's `argv`, and its `cwd`,
/// in the order the spec's examples use, paths base64-encoded as
/// elements. `None` when a piece cannot be spelled: a filename carrying
/// a control character (a newline in a path) makes a terminal discard the
/// whole report, and there is no truncated form of a command that would
/// bring back the right conversation.
fn body(pid: u32, time: u64, argv: &[String], cwd: &Path) -> Option<String> {
    let cwd = cwd.to_str()?;
    if cwd.chars().any(char::is_control) {
        return None;
    }
    let mut encoded = Vec::with_capacity(argv.len());
    for arg in argv {
        if arg.chars().any(char::is_control) {
            return None;
        }
        encoded.push(base64_of(arg));
    }
    Some(format!(
        "pid={pid}:time={time}:argv={}:cwd={}",
        encoded.join(","),
        base64_of(cwd)
    ))
}

/// The authentication tag the terminal recomputes: hex SHA-256 over the
/// body, a colon, and the token. The token itself never goes on the
/// wire, so a recording of the terminal's output cannot mint a record.
fn mac(body: &str, token: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(format!("{body}:{token}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One full report, or `None` when it is over the spec's size: a terminal
/// is free to ignore anything longer, so an oversized command would leave
/// the previous record standing, and resuming it would bring back the
/// wrong conversation.
fn sequence(body: &str, token: &str) -> Option<String> {
    let sequence = format!("\x1b]7577;{body}:mac={}\x1b\\", mac(body, token));
    (sequence.len() <= MAX_REPORT).then_some(sequence)
}

/// The sequence that removes this process's record.
fn clear_sequence(pid: u32, time: u64, token: &str) -> Option<String> {
    sequence(&format!("pid={pid}:time={time}:clear=1"), token)
}

/// Seconds since the epoch, the spec's `time`: a report older than a
/// minute is stale, and the terminal drops it. Every emission here is
/// current, so the window never has to be reasoned about.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// `path` as an absolute path: canonical when it exists, joined onto the
/// process's directory when it does not. A fresh session's file is
/// allocated before it is ever written, and `--save` may name a relative
/// path the user has not created yet.
fn absolute(path: &Path) -> Option<PathBuf> {
    let resolved = crate::args::canonical(path);
    if resolved.is_absolute() {
        return Some(resolved);
    }
    std::env::current_dir()
        .ok()
        .map(|dir| dir.join(path))
        .map(|joined| crate::args::canonical(&joined))
}

/// The command that comes back: this executable, the workspace it runs
/// tools in, and the session file. The executable is the one running,
/// not a bare `rhema`, so a restore lands on the build that reported; the
/// workspace rides in `argv` as well as `cwd`, because a terminal may
/// fall back to its own directory and tools must not.
fn resume_sequence(app: &App, session: &Path, token: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let workspace = absolute(&app.options.workspace)?;
    report(&exe, &workspace, session, token)
}

/// The report for one command line: the executable as it runs now, the
/// absolute workspace, and the session file, started in that workspace.
/// Split from [`resume_sequence`] so the command is testable without a
/// process.
fn report(exe: &Path, workspace: &Path, session: &Path, token: &str) -> Option<String> {
    let argv = [
        exe.to_str()?.to_owned(),
        "--workspace".to_owned(),
        workspace.to_str()?.to_owned(),
        "--resume-or-new".to_owned(),
        session.to_str()?.to_owned(),
    ];
    let body = body(std::process::id(), now(), &argv, workspace)?;
    sequence(&body, token)
}

/// Writes one sequence to the terminal, when one is attached there: a
/// piped stdout captures the reply byte-exact, and no terminal would see
/// it anyway. Errors are ignored, a failed report never fails a session.
fn emit(sequence: &str) {
    let mut out = std::io::stdout().lock();
    if !out.is_terminal() {
        return;
    }
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

/// Removes the record: this conversation is over. The TUI calls this on a
/// clean quit only, never from the panic hook or the signal restore.
pub(crate) fn clear() {
    let Ok(token) = std::env::var(TOKEN_VAR) else {
        return;
    };
    if token.is_empty() {
        return;
    }
    if let Some(sequence) = clear_sequence(std::process::id(), now(), &token) {
        emit(&sequence);
    }
}

/// The UI thread's report state: the target last announced, so a change
/// emits once and the frames after it write nothing. Lives beside
/// [`crate::status::Tracker`] and syncs the same way, once per loop
/// iteration.
#[derive(Debug)]
pub(crate) struct Tracker {
    /// The terminal's token; `None` on a terminal without the protocol,
    /// which turns every sync into a no-op.
    token: Option<String>,
    /// What was announced last, if anything.
    last: Option<Target>,
}

impl Tracker {
    /// A tracker for the terminal this process runs on.
    pub(crate) fn new() -> Self {
        let token = std::env::var(TOKEN_VAR)
            .ok()
            .filter(|token| !token.is_empty());
        Self { token, last: None }
    }

    /// Announces the app's current target when it changed. A command that
    /// cannot be spelled does not leave the previous record standing: the
    /// record it belonged to is stale by definition, and the spec asks
    /// for `clear` rather than a resume of the wrong conversation.
    pub(crate) fn sync(&mut self, app: &App) {
        let Some(token) = self.token.as_deref() else {
            return;
        };
        let target = target(app);
        if self.last.as_ref() == Some(&target) {
            return;
        }
        let sequence = match &target {
            Target::Resume(session) => resume_sequence(app, session, token)
                .or_else(|| clear_sequence(std::process::id(), now(), token)),
            Target::Clear => clear_sequence(std::process::id(), now(), token),
        };
        self.last = Some(target);
        if let Some(sequence) = sequence {
            emit(&sequence);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token the spec's published examples are computed with: the
    /// `mac` values below are the protocol document's own, so they check
    /// this implementation against another one.
    const TOKEN: &str = "dummy-token-replace-me";

    #[test]
    fn the_spec_vectors_authenticate() {
        // Neovim's report, pid 74877, a session file in its state dir.
        let nvim = "pid=74877:time=1795543200:argv=bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt";
        assert_eq!(
            mac(nvim, TOKEN),
            "9dbb99bfb856749a87563839d7ce9062818d78cf25909db93f309e0cd3b1909b"
        );
        // Its `clear` on quit.
        assert_eq!(
            mac("pid=74877:time=1795543200:clear=1", TOKEN),
            "c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93"
        );
        // The wrapper example: three elements, the first reused, so a
        // joined argv and a picked one hash to different tags.
        let wrapper = "pid=90412:time=1795543200:argv=cmVzdW1lLXdyYXA=,c3No,ZGV2Ym94";
        assert_eq!(
            mac(wrapper, TOKEN),
            "e76149d0ba54a83570ffab0583867dacdfa23928ba9d18604f0badea88b62b65"
        );
        // The document's lazygit example is not a vector: its published
        // `mac` does not authenticate its own body (this token, any
        // sha256); the three above do, and they pin the same scheme.
    }

    #[test]
    fn a_report_carries_one_base64_element_per_argument() {
        let argv = vec![
            "/usr/local/bin/rhema".to_owned(),
            "--workspace".to_owned(),
            "/home/user/src".to_owned(),
        ];
        let body = body(42, 1_795_543_200, &argv, Path::new("/home/user/src")).unwrap();
        assert_eq!(
            body,
            "pid=42:time=1795543200:argv=L3Vzci9sb2NhbC9iaW4vcmhlbWE=,LS13b3Jrc3BhY2U=,L2hvbWUvdXNlci9zcmM=:cwd=L2hvbWUvdXNlci9zcmM="
        );
        // The wire form puts the mac last, where the terminal reads it.
        assert_eq!(
            sequence(&body, TOKEN).unwrap(),
            format!("\x1b]7577;{body}:mac={}\x1b\\", mac(&body, TOKEN))
        );
    }

    #[test]
    fn the_command_names_the_workspace_and_the_file() {
        let sequence = report(
            Path::new("/usr/local/bin/rhema"),
            Path::new("/home/user/src"),
            Path::new("/home/user/.cache/rhema/sessions/ab/1.json"),
            TOKEN,
        )
        .unwrap();
        // The literal base64 spellings keep the check independent of
        // `base64_of` for everything but the shape.
        let body = sequence
            .strip_prefix("\x1b]7577;")
            .and_then(|rest| rest.split(":mac=").next())
            .unwrap();
        let mut halves = body.split(":cwd=");
        let head = halves.next().unwrap();
        let cwd = halves.next().unwrap();
        let argv = head.split_once(":argv=").unwrap().1;
        assert_eq!(
            argv,
            "L3Vzci9sb2NhbC9iaW4vcmhlbWE=,LS13b3Jrc3BhY2U=,L2hvbWUvdXNlci9zcmM=,\
             LS1yZXN1bWUtb3ItbmV3,L2hvbWUvdXNlci8uY2FjaGUvcmhlbWEvc2Vzc2lvbnMvYWIvMS5qc29u"
        );
        // The working directory is the workspace, not wherever the
        // terminal happened to start.
        assert_eq!(cwd, "L2hvbWUvdXNlci9zcmM=");
    }

    #[test]
    fn a_clear_names_no_command() {
        let clear = clear_sequence(7, 1_795_543_200, TOKEN).unwrap();
        assert_eq!(
            clear,
            format!(
                "\x1b]7577;pid=7:time=1795543200:clear=1:mac={}\x1b\\",
                mac("pid=7:time=1795543200:clear=1", TOKEN)
            )
        );
        assert!(!clear.contains("argv="));
    }

    #[test]
    fn a_path_that_cannot_be_spelled_has_no_report() {
        // A newline in a path would have the terminal discard the report
        // whole, so the command is refused outright.
        let argv = vec!["/bin/rhema".to_owned(), "a\nb".to_owned()];
        assert!(body(1, 1, &argv, Path::new("/tmp")).is_none());
        assert!(body(1, 1, &argv[..1], Path::new("/tmp/a\nb")).is_none());
        // Non-UTF-8 is just as unsendable: the base64 is over the bytes
        // the path holds, and `to_str` is the boundary the report takes.
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let raw = OsStr::from_bytes(b"/tmp/a\xff");
            assert!(raw.to_str().is_none());
        }
    }

    #[test]
    fn a_report_over_the_spec_limit_is_refused() {
        let long = format!("/{}", "x".repeat(MAX_REPORT));
        let argv = vec!["/bin/rhema".to_owned(), long];
        let over = body(1, 1, &argv, Path::new("/tmp")).unwrap();
        assert!(sequence(&over, TOKEN).is_none());
        // Just inside: a report the spec requires every terminal to take.
        let fits = "x".repeat(MAX_REPORT / 2);
        let under = body(1, 1, &[fits], Path::new("/tmp")).unwrap();
        let sequence = sequence(&under, TOKEN).unwrap();
        assert!(sequence.len() <= MAX_REPORT);
        assert!(sequence.starts_with("\x1b]7577;pid=1:"));
        assert!(sequence.ends_with("\x1b\\"));
    }

    #[test]
    fn the_target_follows_the_checkpoint_file() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.json");
        std::fs::write(&session, "{}").unwrap();
        assert_eq!(
            target_of(Some(&session)),
            Target::Resume(std::fs::canonicalize(&session).unwrap())
        );
        // No checkpoint (a clear before the next turn allocates one): the
        // record goes away rather than resuming the backed-up file.
        assert_eq!(target_of(None), Target::Clear);
    }

    #[test]
    fn a_path_that_does_not_exist_yet_still_resolves_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-yet.json");
        let resolved = absolute(&missing).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, missing);
        // A relative name resolves against the process directory, so the
        // argv element never depends on where the terminal starts.
        let relative = absolute(Path::new("relative.json")).unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("relative.json"));
    }
}
