//! The startup environment report: machine facts for the system prompt,
//! probed once before the first model call so no turn is spent
//! rediscovering them. The claw's daemon writes the same report at boot
//! (`ENVIRONMENT.md`); here the report renders straight into the
//! prompt's environment section.
//!
//! The probe is synchronous on purpose — agent construction is sync —
//! and best-effort: one `bash -c` under a short budget on a fixed
//! script, a failure degrading to the line that says so, never a failed
//! startup. It describes the host, which is where commands run unless a
//! sandbox re-points them; a sandboxed session takes
//! [`remote_notice`] instead, because host facts would be lies about a
//! container. The report carries no timestamp: it rides the system
//! prompt, which must stay byte-stable for a session's life.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The tools the report inventories, in report order: what a coding
/// turn reaches for. Absence of any entry is ordinary.
const TOOLS: &[&str] = &[
    "git", "rg", "fd", "cargo", "rustc", "node", "npm", "python3", "docker", "gh",
];

/// The probe's whole budget: long enough for a cold tool list, short
/// enough that a hung shell cannot stall a startup past it.
const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// How often a running probe is polled for exit.
const POLL_STEP: Duration = Duration::from_millis(10);

/// The report-side bound on the probe's stdout: the inventory is
/// fixed-size, so this is a guard against a pathological `--version`,
/// not an expectation.
const OUTPUT_CAP: usize = 4 * 1024;

/// The probe script: OS, shell, one line per inventoried tool
/// ([`TOOLS`], interpolated so the list cannot drift), and the
/// workspace's git branch when it is a checkout. POSIX sh throughout —
/// every executor in the tree runs `bash -c`.
fn probe_script() -> String {
    format!(
        r#"
uname -sm
printf 'shell: %s\n' "${{SHELL:-unset}}"
for t in {tools}; do
  if p=$(command -v "$t" 2>/dev/null); then
    v=$("$t" --version 2>&1 | head -n 1)
    printf '%s: %s — %s\n' "$t" "$p" "$v"
  else
    printf '%s: absent\n' "$t"
  fi
done
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf 'git branch: %s\n' "$(git branch --show-current 2>/dev/null || printf detached)"
fi
"#,
        tools = TOOLS.join(" ")
    )
}

/// The host report: what `bash` on this machine, rooted at `workspace`,
/// would answer at startup. Never fails — a probe that cannot run
/// renders as the line that says so.
pub fn host_report(workspace: &Path) -> String {
    match run_probe(workspace) {
        Ok(output) => render_report(&output),
        Err(reason) => format!(
            "## Environment\n\n\
             The startup probe did not answer ({reason}); ask the shell\n\
             directly (`uname -sm`, `command -v <tool>`) when a fact\n\
             matters.\n"
        ),
    }
}

/// The section a sandboxed session renders instead of host facts: the
/// report's honesty rule — commands run somewhere a host probe cannot
/// see, so it claims nothing and names the kind.
pub fn remote_notice(kind: &str) -> String {
    format!(
        "## Environment\n\n\
         Commands run in the {kind} sandbox, not on the host that\n\
         launched rhema, so no host facts are claimed here. Probe the\n\
         sandbox itself (`uname -sm`, `command -v <tool>`) when a fact\n\
         matters.\n"
    )
}

/// Shapes the probe's bounded stdout into the report's markdown: the
/// first two lines are the OS and the shell, the rest is the tool
/// inventory verbatim, and a present ripgrep earns the one search
/// note — environment-conditional advice lives exactly where it is
/// true.
fn render_report(output: &str) -> String {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let os = lines.next().unwrap_or("unknown");
    let shell = lines.next().unwrap_or("shell: unknown");
    let mut out = String::from(
        "## Environment\n\n\
         The machine `bash` runs on, probed at startup.\n\n\
         - OS: ",
    );
    out.push_str(os);
    out.push_str("\n- ");
    out.push_str(shell);
    out.push('\n');
    let rest: Vec<&str> = lines.collect();
    if let Some(branch) = rest.iter().find(|l| l.starts_with("git branch: ")) {
        out.push_str("- ");
        out.push_str(branch);
        out.push('\n');
    }
    out.push_str("\nTools:\n");
    let tools: Vec<&str> = rest
        .iter()
        .filter(|l| !l.starts_with("git branch: "))
        .copied()
        .collect();
    for line in &tools {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    if tools
        .iter()
        .any(|l| l.starts_with("rg: ") && !l.starts_with("rg: absent"))
    {
        out.push_str(
            "\nSearch with `rg` (it respects .gitignore; `-uu` also\n\
             searches ignored files) rather than recursive grep.\n",
        );
    }
    out
}

/// Runs the probe script with `workspace` as working directory and
/// returns its bounded stdout. Stdin is null so a shell that wrongly
/// reads it sees EOF; anything still running at the deadline is killed
/// and reaped.
fn run_probe(workspace: &Path) -> Result<String, String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(probe_script())
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot spawn the probe: {err}"))?;
    let deadline = Instant::now() + PROBE_BUDGET;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(POLL_STEP),
            Err(err) => return Err(format!("cannot wait on the probe: {err}")),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("no exit within {:?}", PROBE_BUDGET));
    };
    if !status.success() {
        return Err(format!("probe exited: {status}"));
    }
    let mut bytes = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = (&mut stdout)
            .take(OUTPUT_CAP as u64)
            .read_to_end(&mut bytes);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host report carries its sections and one line per inventoried
    /// tool — whatever is or is not installed here — and the OS line
    /// leads. Runs real bash, like the claw's environment tests.
    #[test]
    fn the_host_report_lists_every_tool_and_leads_with_the_os() {
        let report = host_report(Path::new("."));
        assert!(report.starts_with("## Environment\n"), "{report}");
        assert!(report.contains("- OS: "), "{report}");
        assert!(report.contains("- shell: "), "{report}");
        assert!(report.contains("\nTools:\n"), "{report}");
        for tool in TOOLS {
            assert!(
                report.contains(&format!("- {tool}: ")),
                "no line for {tool}: {report}"
            );
        }
    }

    /// A git checkout names its branch; the branch line rides with the
    /// host facts, not the tool inventory.
    #[test]
    fn a_git_workspace_names_its_branch_among_the_facts() {
        // The crate's own tree is a checkout; the probe runs from it.
        let report = host_report(Path::new("."));
        assert!(report.contains("- git branch: "), "{report}");
        assert_eq!(
            report.matches("- git branch: ").count(),
            1,
            "the branch is a fact, not a tool line: {report}"
        );
    }

    /// The ripgrep note rides only when rg answered: absent rg must not
    /// advise rg.
    #[test]
    fn the_search_note_follows_ripgreps_presence() {
        let with = render_report("Linux x86_64\nshell: /bin/bash\nrg: /usr/bin/rg — ripgrep 14\n");
        assert!(with.contains("Search with `rg`"), "{with}");
        let without = render_report("Linux x86_64\nshell: /bin/bash\nrg: absent\n");
        assert!(!without.contains("Search with `rg`"), "{without}");
    }

    /// The remote notice names the kind and claims no host facts.
    #[test]
    fn the_remote_notice_names_the_kind_and_claims_nothing() {
        let notice = remote_notice("docker");
        assert!(notice.contains("docker sandbox"), "{notice}");
        assert!(!notice.contains("- OS: "), "{notice}");
    }

    /// A probe that cannot run degrades to the line that says so.
    #[test]
    fn a_directory_workspace_still_renders_a_report() {
        // `/` is readable but not writable and not a probe target; a
        // missing directory makes spawn's cwd fail — the shape under
        // test — while a present one answers normally.
        let report = host_report(Path::new("/definitely/not/here"));
        assert!(report.starts_with("## Environment\n"), "{report}");
        assert!(
            report.contains("did not answer") || report.contains("Tools:"),
            "either shape is honest: {report}"
        );
    }
}
