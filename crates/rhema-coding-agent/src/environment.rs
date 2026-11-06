//! The startup environment report: machine facts for the system prompt,
//! probed once before the first model call so no turn is spent
//! rediscovering them. The claw's daemon writes the same report at boot
//! (`ENVIRONMENT.md`); here the report renders straight into the
//! prompt's environment section.
//!
//! Every text the report carries is data: [`environment.yaml`] at the
//! crate root — beside SYSTEM.md — holds the OS name mapping, the probe
//! inventory, and the report, notice, and unavailable texts as Jinja
//! templates over the facts this module gathers. The Rust here is
//! mechanics alone: run the probe, shape the facts, render the
//! template. The file is baked in (`include_str!`), not read at
//! runtime: the report must not turn machine-dependent, and the cloud
//! frontends have no filesystem to read it from anyway.
//!
//! The probe is synchronous on purpose — agent construction is sync —
//! and best-effort: one `bash -c` under a short budget on a fixed
//! script, a failure degrading to the `unavailable` text, never a
//! failed startup. It describes the host, which is where commands run
//! unless a sandbox re-points them; a sandboxed session takes
//! [`remote_notice`] instead, because host facts would be lies about a
//! container. The report's date is stamped once at construction: it
//! rides the system prompt, which stays byte-stable for a session's
//! life.
//!
//! [`environment.yaml`]: crate::environment::manifest

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rhema_agent_tools::render_template;
use serde_json::json;

/// One inventoried tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    /// The executable the probe looks for and the report names — the
    /// command the model would type, not the project's name for it.
    pub executable: String,
    /// Guidance that joins the report's notes only when `executable`
    /// answered. Advice about a missing tool would be the stale kind.
    #[serde(default)]
    pub advice: Option<String>,
}

/// The report's data: the inventory, the OS name mapping, and the
/// texts (Jinja templates) the report renders.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Manifest {
    /// The friendly name for each `uname -s` system value; a system
    /// without a row keeps its raw uname spelling.
    #[serde(default)]
    pub os_names: BTreeMap<String, String>,
    /// The probed executables, in report order.
    pub probe: Vec<Probe>,
    /// The session facts every shape places by reference — written
    /// once, never duplicated in the texts.
    pub facts: String,
    /// The report a local session renders.
    pub report: String,
    /// The message a sandboxed session shows instead.
    pub notice: String,
    /// The message a failed probe degrades to.
    pub unavailable: String,
}

/// The baked-in manifest (`environment.yaml` at the crate root), parsed
/// once. A manifest that does not parse falls to [`fallback_report`]:
/// the drift test is where that break belongs, never a startup.
pub fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_yaml::from_str(include_str!("../environment.yaml")).unwrap_or_else(|_| Manifest {
            os_names: BTreeMap::new(),
            probe: Vec::new(),
            facts: String::new(),
            report: String::new(),
            notice: String::new(),
            unavailable: String::new(),
        })
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
/// fixed-size, so this is a guard against a pathological shell, not an
/// expectation.
const OUTPUT_CAP: usize = 4 * 1024;

/// The probe script: OS, shell, one presence line per manifest entry —
/// versions stay out; presence is the fact that changes how the model
/// should work — and the workspace's git branch when it is a checkout.
/// POSIX sh throughout — every executor in the tree runs `bash -c`.
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
  if command -v "$t" >/dev/null 2>&1; then
    printf '%s: present\n' "$t"
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
/// would answer at startup — carrying `model`, the model the session
/// runs, and `harness`, the frontend assembling it. Never fails: a
/// probe that cannot run renders the unavailable text.
pub fn host_report(workspace: &Path, model: &str, harness: &str) -> String {
    let facts = facts(model, harness);
    match run_probe(manifest(), workspace) {
        Ok(output) => render_report(manifest(), workspace, &facts, &output),
        Err(reason) => unavailable(&facts, &reason),
    }
}

/// The section a sandboxed session renders instead of host facts: the
/// report's honesty rule — commands run somewhere a host probe cannot
/// see, so it claims none and names the kind. The sandbox-side
/// `workspace` is carried — where the model's commands run is not a
/// host fact.
pub fn remote_notice(kind: &str, workspace: &str, model: &str, harness: &str) -> String {
    let manifest = manifest();
    let facts = facts(model, harness);
    let rendered = render_text(
        &manifest.notice,
        &facts.context(manifest, json!({ "kind": kind, "workspace": workspace })),
    );
    if rendered.is_empty() {
        fallback_report(&facts)
    } else {
        rendered
    }
}

/// The facts every report shape carries, however the probe went.
struct Facts {
    /// The construction-time UTC stamp, to the minute.
    date: String,
    /// The model the session runs.
    model: String,
    /// The frontend assembling the agent.
    harness: String,
}

impl Facts {
    /// The template context every text extends, carrying the composed
    /// `facts` block — the yaml's one copy of the session facts,
    /// placed by reference, never re-typed in a text.
    fn context(&self, manifest: &Manifest, extra: serde_json::Value) -> serde_json::Value {
        let serde_json::Value::Object(mut map) = extra else {
            return serde_json::Value::Null;
        };
        map.insert("date".into(), json!(self.date));
        map.insert("model".into(), json!(self.model));
        map.insert("harness".into(), json!(self.harness));
        let facts = render_template(
            &manifest.facts,
            &json!({ "date": self.date, "model": self.model, "harness": self.harness }),
        )
        .unwrap_or_default();
        map.insert("facts".into(), json!(facts));
        serde_json::Value::Object(map)
    }
}

fn facts(model: &str, harness: &str) -> Facts {
    Facts {
        date: utc_now(),
        model: model.to_owned(),
        harness: harness.to_owned(),
    }
}

/// Renders one manifest text over its context; an empty text (the
/// unparsed-manifest case) falls to [`fallback_report`].
fn render_text(text: &str, context: &serde_json::Value) -> String {
    if text.is_empty() {
        return String::new();
    }
    render_template(text, context).unwrap_or_default()
}

/// Shapes the probe's bounded stdout into the report over the manifest:
/// the first two lines are the OS and the shell, the presence lines
/// follow, and every answered executable with advice earns its note.
fn render_report(manifest: &Manifest, workspace: &Path, facts: &Facts, output: &str) -> String {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let os = os_label(manifest, lines.next().unwrap_or("unknown"));
    let shell = lines
        .next()
        .and_then(|line| line.strip_prefix("shell: "))
        .unwrap_or("unknown")
        .to_owned();
    let rest: Vec<&str> = lines.collect();
    let branch = rest
        .iter()
        .find_map(|l| l.strip_prefix("git branch: "))
        .map(str::to_owned);
    let tools: Vec<serde_json::Value> = manifest
        .probe
        .iter()
        .map(|entry| {
            let present = rest
                .iter()
                .any(|l| *l == format!("{}: present", entry.executable));
            json!({ "name": entry.executable, "state": if present { "present" } else { "absent" } })
        })
        .collect();
    let notes: Vec<String> = manifest
        .probe
        .iter()
        .filter(|entry| {
            entry.advice.is_some()
                && rest
                    .iter()
                    .any(|l| *l == format!("{}: present", entry.executable))
        })
        .filter_map(|entry| entry.advice.clone())
        .collect();
    let context = facts.context(
        manifest,
        json!({
            "os": os,
            "shell": shell,
            "workspace": workspace.display().to_string(),
            "branch": branch,
            "tools": tools,
            "notes": notes,
        }),
    );
    let rendered = render_text(&manifest.report, &context);
    if rendered.is_empty() {
        fallback_report(facts)
    } else {
        rendered
    }
}

/// The unavailable text over the failure reason.
fn unavailable(facts: &Facts, reason: &str) -> String {
    let manifest = manifest();
    let rendered = render_text(
        &manifest.unavailable,
        &facts.context(manifest, json!({ "reason": reason })),
    );
    if rendered.is_empty() {
        fallback_report(facts)
    } else {
        rendered
    }
}

/// The mechanism's last resort, when the manifest itself did not parse:
/// the facts the shell-independent clock still names, no template.
fn fallback_report(facts: &Facts) -> String {
    format!(
        "## Environment\n\n\
         The report's data did not parse; ask the shell (`uname -sm`,\n\
         `command -v <tool>`) when a fact matters.\n\n\
         - Date: {}\n- Model: {}\n- Harness: {}\n",
        facts.date, facts.model, facts.harness
    )
}

/// The OS line: the mapping's friendly name with the raw uname spelling
/// beside it — `macOS (Darwin arm64)` — or the raw spelling alone when
/// the manifest maps no friendly name.
fn os_label(manifest: &Manifest, raw: &str) -> String {
    let Some((system, _rest)) = raw.split_once(' ') else {
        return raw.to_owned();
    };
    match manifest.os_names.get(&system.to_lowercase()) {
        Some(friendly) => format!("{friendly} ({raw})"),
        None => raw.to_owned(),
    }
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
        return Err(format!("no exit within {PROBE_BUDGET:?}"));
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

/// The wall-clock UTC stamp, to the minute — the one timestamp shape
/// the reports carry, stamped once at construction.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let secs_of_day = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs_of_day / 3_600,
        secs_of_day % 3_600 / 60
    )
}

/// Days since the Unix epoch to (year, month, day), UTC — the civil
/// calendar without a clock dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baked manifest parses and every text renders over sample
    /// facts — the drift test a hand edit fails: a manifest that stops
    /// parsing falls to the fallback at runtime, so the break belongs
    /// in a test, not a startup.
    #[test]
    fn the_baked_manifest_parses_and_every_text_renders() {
        let manifest = manifest();
        assert!(!manifest.probe.is_empty(), "an empty inventory is a bug");
        assert!(!manifest.report.is_empty());
        assert!(
            !manifest.facts.is_empty(),
            "the facts block is written once"
        );
        let facts = facts("m", "h");
        for text in [
            &manifest.facts,
            &manifest.report,
            &manifest.notice,
            &manifest.unavailable,
        ] {
            let context = facts.context(
                manifest,
                json!({
                    "os": "macOS (Darwin arm64)", "shell": "/bin/zsh",
                    "workspace": "/ws", "branch": "main",
                    "tools": [{"name": "rg", "state": "present"}], "notes": ["n"],
                    "kind": "k", "reason": "r",
                }),
            );
            let rendered = render_template(text, &context).expect("the text renders");
            assert!(!rendered.trim().is_empty());
        }
    }

    /// The host report carries its facts — date, model, harness, the OS
    /// line, one presence line per manifest entry — with no versions
    /// anywhere. The branch line follows the probe's own rule, present
    /// on a checkout and absent off one, so both sides get tested: the
    /// CWD decides which, and a throwaway repo pins the positive side
    /// wherever the suite happens to run. Runs real bash, like the
    /// claw's environment tests.
    #[test]
    fn the_host_report_carries_facts_and_presence_without_versions() {
        let manifest = manifest();
        let report = host_report(Path::new("."), "glm-5.3", "rhema test");
        assert!(report.starts_with("## Environment\n"), "{report}");
        for fact in [
            "- Date: ",
            "- Model: glm-5.3",
            "- Harness: rhema test",
            "### Machine",
            "- OS: ",
            "### Workspace",
            "- CWD: ",
            "### Command-line tools",
        ] {
            assert!(report.contains(fact), "missing {fact}: {report}");
        }
        for entry in &manifest.probe {
            assert!(
                report.contains(&format!("- {}: ", entry.executable)),
                "no line for {}: {report}",
                entry.executable
            );
        }
        let on_checkout = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .is_ok_and(|out| out.status.success());
        assert_eq!(
            report.contains("- git branch: "),
            on_checkout,
            "branch line must match the checkout: {report}"
        );
        assert!(
            !report.contains("version"),
            "presence, not versions: {report}"
        );

        // A throwaway checkout proves the positive side on any runner:
        // the report names whatever branch the fixture lands on.
        let repo = std::env::temp_dir().join(format!("rhema-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).expect("the fixture directory creates");
        let run = |args: &[&str]| {
            Command::new("git")
                .args(["-c", "user.email=test@localhost", "-c", "user.name=test"])
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git runs")
        };
        run(&["init"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let branch = String::from_utf8(run(&["branch", "--show-current"]).stdout)
            .expect("the branch name is utf-8")
            .trim()
            .to_owned();
        let report = host_report(&repo, "glm-5.3", "rhema test");
        assert!(
            report.contains(&format!("- git branch: {branch}")),
            "the checkout's branch reaches the report: {report}"
        );
        assert!(
            !report.contains("version"),
            "presence, not versions: {report}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The OS line maps the friendly name beside the raw spelling and
    /// keeps unmapped systems raw.
    #[test]
    fn the_os_line_maps_friendly_names_beside_the_raw() {
        let manifest = manifest();
        assert_eq!(os_label(manifest, "Darwin arm64"), "macOS (Darwin arm64)");
        assert_eq!(os_label(manifest, "SunOS sun4u"), "SunOS sun4u");
    }

    /// Advice joins the notes only when its executable answered — the
    /// ripgrep note is data like any other entry's.
    #[test]
    fn advice_follows_presence_not_the_tool_name() {
        let manifest = manifest();
        let facts = facts("m", "h");
        let with = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            "Darwin arm64\nshell: /bin/zsh\nrg: present\nfd: absent\ngit branch: main\n",
        );
        assert!(with.contains("Notes:"), "{with}");
        assert!(with.contains("Use `rg`"), "{with}");
        let without = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            "Darwin arm64\nshell: /bin/zsh\nrg: absent\nfd: absent\n",
        );
        assert!(!without.contains("Notes:"), "{without}");
        assert!(without.contains("- rg: absent"), "{without}");
    }

    /// The notice names the kind, the model, and the harness, and
    /// claims no host facts.
    #[test]
    fn the_notice_names_the_kind_workspace_and_no_host_facts() {
        let notice = remote_notice("docker", "/sandbox/ws", "glm-5.3", "rhema test");
        assert!(notice.contains("docker sandbox"), "{notice}");
        assert!(notice.contains("- Workspace: /sandbox/ws"), "{notice}");
        assert!(notice.contains("- Model: glm-5.3"), "{notice}");
        assert!(notice.contains("- Harness: rhema test"), "{notice}");
        assert!(!notice.contains("- OS: "), "{notice}");
    }

    /// A manifest entry the shell-safety filter refuses never reaches
    /// the script: quoting in a probe is not executable data.
    #[test]
    fn unsafe_executables_never_reach_the_script() {
        let mut manifest = manifest().clone();
        manifest.probe.push(Probe {
            executable: "x; rm -rf /".into(),
            advice: None,
        });
        let script = probe_script(&manifest);
        assert!(!script.contains("rm -rf"), "{script}");
    }

    /// A probe that cannot run degrades to the unavailable text, facts
    /// and all.
    #[test]
    fn a_missing_workspace_renders_the_unavailable_text() {
        let report = host_report(Path::new("/definitely/not/here"), "m", "h");
        assert!(report.contains("did not answer"), "{report}");
        assert!(report.contains("- Model: m"), "{report}");
    }

    /// The stamp is the civil UTC calendar: two anchors pin the shape.
    #[test]
    fn the_stamp_is_civil_utc() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
