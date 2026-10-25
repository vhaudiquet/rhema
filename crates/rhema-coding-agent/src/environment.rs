//! The startup environment report: machine facts for the system prompt,
//! probed once before the first model call so no turn is spent
//! rediscovering them. The claw's daemon writes the same report at boot
//! (`ENVIRONMENT.md`); here the report renders straight into the
//! prompt's environment section.
//!
//! The inventory is data, not code: `environment.yaml` beside this
//! module names what the probe looks for and the advice a present tool
//! earns — the TOOL.yaml shape, applied to probes. The file is baked in
//! (`include_str!`), not read at runtime: the report must not turn
//! machine-dependent, and the cloud frontends have no filesystem to
//! read it from anyway.
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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One inventoried tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    /// The executable the probe runs and the report names — the command
    /// the model would type, not the project's name for it.
    pub executable: String,
    /// Guidance rendered into the report only when `executable`
    /// answered. Advice about a missing tool would be the stale kind.
    #[serde(default)]
    pub advice: Option<String>,
}

/// The report's inventory: what to probe, in report order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Manifest {
    /// The probed executables, in report order.
    pub probe: Vec<Probe>,
}

/// The baked-in manifest (`environment.yaml`), parsed once. A manifest
/// that does not parse, or names an executable the shell-safety filter
/// refuses, loses that entry — reported by the drift test, never a
/// failed startup.
pub fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_yaml::from_str(include_str!("environment.yaml"))
            .unwrap_or_else(|_| Manifest { probe: Vec::new() })
    })
}

/// An executable name the probe script can carry verbatim: letters,
/// digits, dots, dashes, underscores — nothing that could close the
/// `"$t"` quoting and become shell of its own.
fn shell_safe(executable: &str) -> bool {
    !executable.is_empty()
        && executable
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The probe's whole budget: long enough for a cold tool list, short
/// enough that a hung shell cannot stall a startup past it.
const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// How often a running probe is polled for exit.
const POLL_STEP: Duration = Duration::from_millis(10);

/// The report-side bound on the probe's stdout: the inventory is
/// fixed-size, so this is a guard against a pathological `--version`,
/// not an expectation.
const OUTPUT_CAP: usize = 4 * 1024;

/// The probe script: OS, shell, one line per manifest entry, and the
/// workspace's git branch when it is a checkout. POSIX sh throughout —
/// every executor in the tree runs `bash -c`.
fn probe_script(manifest: &Manifest) -> String {
    let tools: Vec<&str> = manifest
        .probe
        .iter()
        .filter(|entry| shell_safe(&entry.executable))
        .map(|entry| entry.executable.as_str())
        .collect();
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
        tools = tools.join(" ")
    )
}

/// The host report: what `bash` on this machine, rooted at `workspace`,
/// would answer at startup. Never fails — a probe that cannot run
/// renders as the line that says so.
pub fn host_report(workspace: &Path) -> String {
    match run_probe(manifest(), workspace) {
        Ok(output) => render_report(manifest(), &output),
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

/// Shapes the probe's bounded stdout into the report's markdown over
/// the manifest: the first two lines are the OS and the shell, the
/// tool lines follow verbatim, and every present executable with
/// advice earns its line — environment-conditional guidance rendered
/// exactly where it is true.
fn render_report(manifest: &Manifest, output: &str) -> String {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let os = lines.next().unwrap_or("unknown");
    let shell = lines.next().unwrap_or("shell: unknown");
    let mut out = String::from(
        "## Environment\n\n\
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
    for line in rest.iter().filter(|l| !l.starts_with("git branch: ")) {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    let mut notes = String::new();
    for entry in &manifest.probe {
        let present = rest.iter().any(|l| {
            l.starts_with(&format!("{}: ", entry.executable))
                && !l.starts_with(&format!("{}: absent", entry.executable))
        });
        if present {
            if let Some(advice) = &entry.advice {
                notes.push_str("- ");
                notes.push_str(advice);
                notes.push('\n');
            }
        }
    }
    if !notes.is_empty() {
        out.push_str("\nNotes:\n");
        out.push_str(&notes);
    }
    out
}

/// Runs the probe script with `workspace` as working directory and
/// returns its bounded stdout. Stdin is null so a shell that wrongly
/// reads it sees EOF; anything still running at the deadline is killed
/// and reaped.
fn run_probe(manifest: &Manifest, workspace: &Path) -> Result<String, String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(probe_script(manifest))
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

    /// The baked manifest parses, and every executable is shell-safe
    /// and unique — the drift test a hand edit fails, in the
    /// TOOL.yaml spirit: a manifest that stops parsing inventories
    /// nothing at runtime, so the break belongs in a test, not a
    /// startup.
    #[test]
    fn the_baked_manifest_parses_and_names_safe_unique_executables() {
        let manifest = manifest();
        assert!(!manifest.probe.is_empty(), "an empty manifest is a bug");
        let mut seen = std::collections::BTreeSet::new();
        for entry in &manifest.probe {
            assert!(
                shell_safe(&entry.executable),
                "not shell-safe: {}",
                entry.executable
            );
            assert!(
                seen.insert(entry.executable.as_str()),
                "duplicate: {entry:?}"
            );
        }
    }

    /// The host report carries its sections and one line per manifest
    /// entry — whatever is or is not installed here — and the OS line
    /// leads. Runs real bash, like the claw's environment tests.
    #[test]
    fn the_host_report_lists_every_manifest_entry_and_leads_with_the_os() {
        let manifest = manifest();
        let report = host_report(Path::new("."));
        assert!(report.starts_with("## Environment\n"), "{report}");
        assert!(report.contains("- OS: "), "{report}");
        assert!(report.contains("- shell: "), "{report}");
        assert!(report.contains("\nTools:\n"), "{report}");
        for entry in &manifest.probe {
            assert!(
                report.contains(&format!("- {}: ", entry.executable)),
                "no line for {}: {report}",
                entry.executable
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

    /// Advice renders only for a tool that answered, whatever the
    /// advice says — the ripgrep note is data like any other entry's.
    #[test]
    fn advice_follows_presence_not_the_tool_name() {
        let manifest = Manifest {
            probe: vec![Probe {
                executable: "rg".into(),
                advice: Some("search with rg".into()),
            }],
        };
        let present = render_report(
            &manifest,
            "Linux x86_64\nshell: /bin/bash\nrg: /usr/bin/rg — ripgrep 14\n",
        );
        assert!(present.contains("- search with rg"), "{present}");
        let absent = render_report(&manifest, "Linux x86_64\nshell: /bin/bash\nrg: absent\n");
        assert!(!absent.contains("search with rg"), "{absent}");
        assert!(!absent.contains("Notes:"), "{absent}");
    }

    /// A manifest entry the shell-safety filter refuses never reaches
    /// the script: quoting in a probe is not executable data.
    #[test]
    fn unsafe_executables_never_reach_the_script() {
        let manifest = Manifest {
            probe: vec![
                Probe {
                    executable: "git".into(),
                    advice: None,
                },
                Probe {
                    executable: "x; rm -rf /".into(),
                    advice: None,
                },
            ],
        };
        let script = probe_script(&manifest);
        assert!(script.contains(" git "), "{script}");
        assert!(!script.contains("rm -rf"), "{script}");
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
    fn a_missing_workspace_still_renders_a_report() {
        let report = host_report(Path::new("/definitely/not/here"));
        assert!(report.starts_with("## Environment\n"), "{report}");
        assert!(
            report.contains("did not answer") || report.contains("Tools:"),
            "either shape is honest: {report}"
        );
    }
}
