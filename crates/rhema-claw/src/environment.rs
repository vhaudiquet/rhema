//! The boot-time environment report (`ENVIRONMENT.md`): what the machine
//! already knows about itself, written before the first model call so no
//! turn is spent rediscovering it (framework spec §1 — detection is
//! deterministic code; §9 — context is earned).
//!
//! Everything here is synchronous, offline, and best-effort. Host facts
//! come from `std::env::consts` plus a couple of tiny probes; every
//! external command runs on a fixed argv (no shell, nothing
//! interpolated) under a short budget whose expiry — like any other
//! probe failure — is a line in the report, never a failed boot. The
//! caller owns the path (the bootstrap store passes it in; the default
//! shape is `<state-dir>/bootstrap/ENVIRONMENT.md`) and rewrites the file
//! on every daemon start.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rhema_memory::record::format_utc;

/// The tools the report inventories, in report order. The list is what
/// v1's resident workflow reaches for; absence of any entry is ordinary.
const TOOLS: &[&str] = &["git", "cargo", "rustc", "docker", "node", "npm", "python3"];

/// The per-command budget: long enough for a cold `--version`, short
/// enough that a hung probe cannot stall a boot past it.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a running probe is polled for exit.
const POLL_STEP: Duration = Duration::from_millis(10);

/// The report line stands in for absent facts that are not tools.
const UNKNOWN: &str = "unknown";

/// Generates the report and writes it to `path`, creating parent
/// directories as needed. Returns the generated text so the caller can
/// log or inject it without re-reading the file. The only failures are
/// the write's own I/O errors: generation itself cannot fail, because a
/// boot that cannot probe its machine must still start.
pub fn write_report(path: impl AsRef<Path>) -> Result<String, std::io::Error> {
    let report = generate_report();
    if let Some(parent) = path.as_ref().parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &report)?;
    Ok(report)
}

/// The document: host, user, tools, and the generation stamp. Ran at
/// daemon startup only, so its cost is bounded by the probe budgets, not
/// by anything the model or the network does.
fn generate_report() -> String {
    let mut out = String::from(
        "# Environment\n\n\
         Generated at daemon boot; regenerated on every start; hand \
         edits are overwritten.\n\n\
         ## Host\n\n",
    );
    out.push_str(&format!("- OS: {}\n", std::env::consts::OS));
    out.push_str(&format!("- Arch: {}\n", std::env::consts::ARCH));
    out.push_str(&format!("- Hostname: {}\n\n", hostname()));
    out.push_str("## User\n\n");
    out.push_str(&format!("- User: {}\n", user_name()));
    out.push_str(&format!("- Shell: {}\n\n", shell()));
    out.push_str("## Tools\n\n");
    for line in inventory() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("\nGenerated at {}.\n", now_utc()));
    out
}

/// The tool inventory, one line per probed tool in [`TOOLS`] order.
/// Probes run concurrently — one thread per tool — so a slow probe costs
/// its own budget, not the sum of the budgets.
fn inventory() -> Vec<String> {
    let handles: Vec<_> = TOOLS
        .iter()
        .map(|name| {
            let name = *name;
            thread::spawn(move || tool_line(name))
        })
        .collect();
    TOOLS
        .iter()
        .zip(handles)
        .map(|(name, handle)| {
            handle
                .join()
                .unwrap_or_else(|_| format!("- {name}: present but unreadable (probe failed)"))
        })
        .collect()
}

/// One markdown inventory line for `name`: where it sits and what
/// version it reports, or the hard `absent` verdict when no probe can
/// place it.
fn tool_line(name: &str) -> String {
    // `which` only names the location; the direct `--version` spawn
    // below decides presence, so hosts without a `which` binary still
    // report their tools.
    let located = match run_within("which", &[name], COMMAND_TIMEOUT) {
        RunOutcome::Output(path) => {
            let path = bound(&path);
            (!path.is_empty()).then_some(path)
        }
        RunOutcome::NotFound | RunOutcome::Failed(_) => None,
    };
    match run_within(name, &["--version"], COMMAND_TIMEOUT) {
        RunOutcome::Output(version) => {
            let version = bound(&version);
            match (located.as_deref(), version.is_empty()) {
                (Some(path), false) => format!("- {name}: {path} — {version}"),
                (Some(path), true) => format!("- {name}: {path}"),
                (None, false) => format!("- {name}: {version}"),
                (None, true) => format!("- {name}: present but unreadable (no output)"),
            }
        }
        RunOutcome::NotFound => format!("- {name}: absent"),
        RunOutcome::Failed(reason) => match located {
            Some(path) => {
                let reason = bound(&reason);
                format!("- {name}: {path} — present but unreadable ({reason})")
            }
            None => format!("- {name}: absent"),
        },
    }
}

/// What one external command produced. Every variant renders as a report
/// line; none reaches the caller as an error.
enum RunOutcome {
    /// Exited within the budget; captured output, untrimmed.
    Output(String),
    /// The executable does not exist.
    NotFound,
    /// Exited non-zero or by signal, or outlived the budget.
    Failed(String),
}

/// Runs `program` with the fixed `args`, allowed `budget` to exit. The
/// argv is built only from literals and a tool name — never untrusted
/// input, never a shell. Stdin is null so a probe that wrongly reads it
/// sees EOF instead of hanging the boot. Pipes are read after the exit:
/// these targets are version printers whose output fits any pipe buffer,
/// so a post-exit read cannot block on them; anything still running at
/// the deadline is killed and reaped.
fn run_within(program: &str, args: &[&str], budget: Duration) -> RunOutcome {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return RunOutcome::NotFound;
        }
        Err(err) => return RunOutcome::Failed(format!("cannot spawn: {err}")),
    };
    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => thread::sleep(POLL_STEP),
            Err(err) => return RunOutcome::Failed(format!("cannot wait: {err}")),
        }
    };
    let Some(status) = status else {
        // Over budget: kill and reap so no child outlives the report.
        let _ = child.kill();
        let _ = child.wait();
        return RunOutcome::Failed(format!("no exit within {budget:?}"));
    };
    if !status.success() {
        return RunOutcome::Failed(format!("exit: {status}"));
    }
    let stdout = read_capped(child.stdout.take());
    let stderr = read_capped(child.stderr.take());
    // Version banners sometimes land on stderr (older python3), so the
    // empty-stdout case falls back to it.
    let raw = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    RunOutcome::Output(raw)
}

/// Reads a child pipe up to 16 KiB, lossily UTF-8. The cap bounds the
/// read, not the report — [`bound`] cuts again at render time.
fn read_capped(pipe: Option<impl Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = (&mut pipe).take(16 * 1024).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The report-side bound on captured output: first line only, cut to 200
/// characters — chars, not bytes, so the cut never splits a code point.
fn bound(raw: &str) -> String {
    let line = raw.trim().lines().next().unwrap_or("");
    line.chars().take(200).collect()
}

/// The hostname: `/etc/hostname`'s first line, else the `hostname`
/// command, else a stand-in — the file is not portable and the command
/// is not guaranteed, so absence is a report line.
fn hostname() -> String {
    if let Ok(raw) = std::fs::read_to_string("/etc/hostname") {
        let name = bound(&raw);
        if !name.is_empty() {
            return name;
        }
    }
    match run_within("hostname", &[], COMMAND_TIMEOUT) {
        RunOutcome::Output(name) => {
            let name = bound(&name);
            if name.is_empty() {
                UNKNOWN.to_owned()
            } else {
                name
            }
        }
        RunOutcome::NotFound | RunOutcome::Failed(_) => UNKNOWN.to_owned(),
    }
}

/// The account the daemon runs as: `whoami`, falling back to `$USER`,
/// else a stand-in — a daemon may run with neither available.
fn user_name() -> String {
    if let RunOutcome::Output(name) = run_within("whoami", &[], COMMAND_TIMEOUT) {
        let name = bound(&name);
        if !name.is_empty() {
            return name;
        }
    }
    std::env::var("USER")
        .ok()
        .map(|user| bound(&user))
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| UNKNOWN.to_owned())
}

/// The login shell from `$SHELL`; "unset" when the daemon runs without
/// one (a service unit, for instance).
fn shell() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|shell| bound(&shell))
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "unset".to_owned())
}

/// The wall-clock UTC stamp, to the second, rhema-memory's rendering —
/// one timestamp shape across the resident tier.
fn now_utc() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    format_utc(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report lands as UTF-8 markdown at the given path (parents
    /// created), with the fixed sections and one line per inventoried
    /// tool — whatever is or is not installed on this host — and closes
    /// on a to-the-second UTC stamp.
    #[test]
    fn the_report_writes_back_as_markdown_with_every_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bootstrap").join("ENVIRONMENT.md");
        let report = write_report(&path).expect("writes");
        let on_disk = String::from_utf8(std::fs::read(&path).expect("reads back"))
            .expect("the file is utf-8");
        assert_eq!(report, on_disk, "the returned text is what landed");
        assert!(report.starts_with("# Environment\n"), "{report}");
        assert!(report.contains("regenerated on every start"), "{report}");
        assert!(report.contains("hand edits are overwritten"), "{report}");
        for section in ["## Host", "## User", "## Tools"] {
            assert!(report.contains(section), "missing {section}: {report}");
        }
        for tool in TOOLS {
            assert!(
                report.contains(&format!("- {tool}: ")),
                "no line for {tool}: {report}"
            );
        }
        let footer = report.lines().last().expect("non-empty report");
        assert_eq!(
            footer.len(),
            "Generated at 1970-01-01T00:00:00Z.".len(),
            "a UTC stamp to the second: {footer}"
        );
        assert!(footer.ends_with("Z."), "{footer}");
    }

    /// A tool that exists nowhere still gets its line, through the same
    /// runner the real inventory uses: the hard `absent` verdict, never
    /// an error.
    #[test]
    fn a_missing_tool_gets_the_hard_absent_line() {
        assert_eq!(
            tool_line("rhema-claw-definitely-absent-probe"),
            "- rhema-claw-definitely-absent-probe: absent"
        );
    }

    /// Captured output cannot bloat the report: first line only, cut at
    /// the cap, multibyte-safe.
    #[test]
    fn probe_output_is_bounded() {
        let huge = format!("{}\n{}", "x".repeat(10 * 200), "second line");
        let cut = bound(&huge);
        assert_eq!(cut.chars().count(), 200);
        assert!(!cut.contains('\n'), "{cut}");
        let multibyte = "é".repeat(3 * 200);
        assert_eq!(bound(&multibyte).chars().count(), 200);
    }

    /// A probe that never exits is killed at its budget and reported as
    /// failed; the report never waits past a budget. Tolerates hosts
    /// without `sleep`.
    #[test]
    fn a_hanging_probe_is_cut_at_its_budget() {
        match run_within("sleep", &["30"], Duration::from_millis(150)) {
            RunOutcome::NotFound => {}
            RunOutcome::Failed(_) => {}
            RunOutcome::Output(_) => panic!("a 30s sleep outlived a 150ms budget"),
        }
    }
}
