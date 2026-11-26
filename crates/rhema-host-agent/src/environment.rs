//! The startup environment report: machine facts for the system prompt,
//! probed once before the first model call so no turn is spent
//! rediscovering them. The claw's daemon writes the same report at boot
//! (`ENVIRONMENT.md`); here the report renders straight into the
//! prompt's environment section.
//!
//! Every text the report carries is data: [`environment.yaml`] at the
//! crate root, beside SYSTEM.md, holds the OS name mapping, the probe
//! inventory, and the report, notice, and unavailable texts as Jinja
//! templates over the facts this module gathers. The Rust here is
//! mechanics alone: run the probe, shape the facts, render the
//! template. The file is baked in (`include_str!`), not read at
//! runtime: the report must not turn machine-dependent, and the cloud
//! frontends have no filesystem to read it from anyway.
//!
//! The probe is synchronous on purpose, agent construction is sync,
//! and best-effort: one `bash -c` under a short budget on a fixed
//! script, a failure degrading to the `unavailable` text, never a
//! failed startup. The same parse answers twice over: the report for
//! the prompt, and, through [`host_environment`], the structured
//! absence list a frontend may surface to its user. Which package
//! holds a missing tool stays out of the report on purpose: that is
//! the package manager's question, answered live when the user asks
//! for the install, not a machine-dependent claim to memorize up
//! front. It describes the host, which is where commands run
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

use rhema_prompt::render_template;
use serde_json::json;

/// One inventoried tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    /// The executable the probe looks for and the report names, the
    /// command the model would type, not the project's name for it.
    pub executable: String,
    /// Guidance that joins the report's notes only when the family
    /// answered. Advice about a missing tool would be the stale kind.
    /// The advice is data like the manifest's other texts: a Jinja
    /// template over the `executable` map, so it names a family by its
    /// manifest name (`{{ executable.chromium }}`) and renders the
    /// command that answered for it.
    #[serde(default)]
    pub advice: Option<String>,
    /// Fallback executables tried after `executable`, first answer
    /// winning. One entry stands for a family whose binary name
    /// varies by install, chromium versus google-chrome versus a
    /// fork like helium, and the report names the candidate that
    /// answered: still the command the model would type. The same
    /// shell-safety filter applies.
    #[serde(default)]
    pub alternatives: Vec<String>,
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
    /// The session facts every shape places by reference, written
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
/// once. A manifest that does not parse falls to `fallback_report`:
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
/// digits, dots, dashes, underscores, nothing that could close the
/// `"$t"` quoting and become shell of its own.
fn shell_safe(executable: &str) -> bool {
    !executable.is_empty()
        && executable
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The executables one entry probes, in order: the entry's own name,
/// then its alternatives, the shell-unsafe names dropped. Empty when
/// nothing safe remains, and the entry never reaches the script.
fn candidates(entry: &Probe) -> Vec<&str> {
    std::iter::once(&entry.executable)
        .chain(entry.alternatives.iter())
        .filter(|name| shell_safe(name))
        .map(String::as_str)
        .collect()
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

/// The probe script: OS, shell, the distribution the host names in
/// /etc/os-release, the sudo password gate, one probe call per
/// manifest entry, an entry with alternatives tries its executables
/// in order, first present wins, the entry's own name answers when
/// nothing did. Versions stay out; presence is the fact that changes
/// how the model should work, and the workspace's git branch when it
/// is a checkout, `detached` when HEAD is detached: the shape a PR
/// checkout lands in. POSIX sh throughout, every executor in the tree
/// runs `bash -c`.
fn probe_script(manifest: &Manifest) -> String {
    // One probe call per manifest entry: the family's executables in
    // order, first present wins, the entry's own name answers when
    // nothing did.
    let calls: String = manifest
        .probe
        .iter()
        .map(candidates)
        .filter(|cands| !cands.is_empty())
        .map(|cands| format!("probe {}", cands.join(" ")))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"
uname -sm
printf 'shell: %s\n' "${{SHELL:-unset}}"
if [ -r /etc/os-release ]; then
  # The distribution is the file's PRETTY_NAME, its NAME as the
  # fallback; the file is read, not sourced: it is data, not script.
  d=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release | head -n 1)
  [ -n "$d" ] || d=$(sed -n 's/^NAME=//p' /etc/os-release | head -n 1)
  d=$(printf '%s' "$d" | sed -e 's/^"//' -e 's/"$//')
  [ -n "$d" ] && printf 'distro: %s\n' "$d"
fi
# `sudo -n true` answers without a prompt: a prompt would hang past
# the budget, and a startup cannot ask for a password.
if command -v sudo >/dev/null 2>&1; then
  if sudo -n true 2>/dev/null; then
    printf 'sudo: passwordless\n'
  else
    printf 'sudo: requires a password\n'
  fi
else
  printf 'sudo: not installed\n'
fi
probe() {{
  for t in "$@"; do
    if command -v "$t" >/dev/null 2>&1; then
      printf '%s: present\n' "$t"
      return 0
    fi
  done
  printf '%s: absent\n' "$1"
}}
{calls}
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # `git branch --show-current` answers empty, and exits 0, on a
  # detached HEAD (a PR checkout, a tag, a bisect), so the name is
  # captured first: a blank one means detached, not "no branch line".
  b=$(git branch --show-current 2>/dev/null)
  [ -n "$b" ] || b=detached
  printf 'git branch: %s\n' "$b"
fi
"#,
    )
}

/// One host probe, both shapes it feeds: the report the prompt
/// carries and the absence list a frontend surfaces itself. The probe
/// runs once and parses once, a startup notice that wanted a second
/// run would pay the probe budget twice for the same facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnvironment {
    /// The rendered report: what the prompt's environment section
    /// carries, byte for byte.
    pub report: String,
    /// The probed entries whose whole family is absent, in report
    /// order, named as the report names them, the executable the
    /// model would type. Facts for the user's eyes (a startup notice,
    /// an installer's tail), never the prompt's: which package holds
    /// `fd` is the package manager's question, answered when the user
    /// asks for the install, not a fact to memorize up front. A probe
    /// that cannot run leaves it empty, an unprobed host claims no
    /// absences either.
    pub missing: Vec<String>,
}

/// The host environment: what `bash` on this machine, rooted at
/// `workspace`, would answer at startup, carrying `model`, the model
/// the session runs, and `harness`, the frontend assembling it. Never
/// fails: a probe that cannot run renders the unavailable text and
/// names nothing missing.
pub fn host_environment(workspace: &Path, model: &str, harness: &str) -> HostEnvironment {
    let manifest = manifest();
    let facts = facts(model, harness);
    match run_probe(manifest, workspace) {
        Ok(output) => {
            let parsed = parse_probe(manifest, &output);
            HostEnvironment {
                report: render_report(manifest, workspace, &facts, &parsed),
                missing: missing_tools(manifest, &parsed),
            }
        }
        Err(reason) => HostEnvironment {
            report: unavailable(&facts, &reason),
            missing: Vec::new(),
        },
    }
}

/// The host report alone, the prompt's view of
/// [`host_environment`], for callers that carry no user-facing
/// surface beside it.
pub fn host_report(workspace: &Path, model: &str, harness: &str) -> String {
    host_environment(workspace, model, harness).report
}

/// The section a sandboxed session renders instead of host facts: the
/// report's honesty rule, commands run somewhere a host probe cannot
/// see, so it claims none and names the kind. The sandbox-side
/// `workspace` is carried, where the model's commands run is not a
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
    /// `facts` block, the yaml's one copy of the session facts,
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

/// One probe's parsed stdout: the host lines and, per manifest entry,
/// the candidate that answered. The parse is shared between the
/// report and the absence list, so both read one probe run.
struct ParsedProbe<'a> {
    /// The raw `uname -sm` line; the report labels it.
    os: String,
    /// The login shell, `unknown` when the line did not answer.
    shell: String,
    /// The distribution the host names in /etc/os-release, `None`
    /// where there is no such file (macOS, the BSDs) or no usable
    /// name in it.
    distro: Option<String>,
    /// The sudo password gate's answer: `passwordless` when root
    /// commands run without interaction, `requires a password` when
    /// the gate would prompt, `not installed` without the binary,
    /// `unknown` when the line never answered.
    sudo: String,
    /// The workspace's git branch, when the probe ran in a checkout.
    branch: Option<String>,
    /// Per entry, in manifest order, the candidate whose presence
    /// line answered, `None` when the whole family is absent. The
    /// report names the answerer (the command the model would type),
    /// falling to the entry's own name when nothing did.
    answered: Vec<Option<&'a str>>,
}

/// Parses the probe's bounded stdout: the first two lines are the OS
/// and the shell, the distribution and sudo lines are matched by
/// prefix from there, the presence lines follow, and every answered
/// executable earns its report line and note.
fn parse_probe<'a>(manifest: &'a Manifest, output: &str) -> ParsedProbe<'a> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let os = lines.next().unwrap_or("unknown").to_owned();
    let shell = lines
        .next()
        .and_then(|line| line.strip_prefix("shell: "))
        .unwrap_or("unknown")
        .to_owned();
    let rest: Vec<&str> = lines.collect();
    let distro = rest
        .iter()
        .find_map(|l| l.strip_prefix("distro: "))
        .map(str::to_owned);
    let sudo = rest
        .iter()
        .find_map(|l| l.strip_prefix("sudo: "))
        .unwrap_or("unknown")
        .to_owned();
    let branch = rest
        .iter()
        .find_map(|l| l.strip_prefix("git branch: "))
        .map(str::to_owned);
    let answered = manifest
        .probe
        .iter()
        .map(|entry| {
            candidates(entry)
                .into_iter()
                .find(|cand| rest.iter().any(|line| *line == format!("{cand}: present")))
        })
        .collect();
    ParsedProbe {
        os,
        shell,
        distro,
        sudo,
        branch,
        answered,
    }
}

/// The entries whose whole family is absent, in report order, named as
/// the report names them, the entry's own executable. The
/// frontend's startup notice carries these; the prompt carries the
/// same fact as its available-tools list and needs no parallel list.
fn missing_tools(manifest: &Manifest, parsed: &ParsedProbe<'_>) -> Vec<String> {
    manifest
        .probe
        .iter()
        .zip(&parsed.answered)
        .filter(|(_, answer)| answer.is_none())
        .map(|(entry, _)| entry.executable.clone())
        .collect()
}

/// The `executable` map the manifest's texts read: every entry's name
/// bound to the command that answered for it, `null` when the whole
/// family is absent. An advice template names the family it means
/// (`{{ executable.chromium }}` renders `helium` where helium
/// answered), so one map serves every text and no template has to
/// know which entry is in scope. A name carrying a dash or a dot is
/// not a valid attribute path, so the template subscripts it instead
/// (`executable["google-chrome-stable"]`).
fn executable_map(manifest: &Manifest, parsed: &ParsedProbe<'_>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (entry, answer) in manifest.probe.iter().zip(&parsed.answered) {
        map.insert(
            entry.executable.clone(),
            match answer {
                Some(name) => json!(name),
                None => serde_json::Value::Null,
            },
        );
    }
    serde_json::Value::Object(map)
}

/// Shapes a parsed probe into the report over the manifest: the OS
/// line with the distribution beside it when the host names one, the
/// sudo line, the names of the tools that answered, and a note for
/// every answered tool that carries advice.
fn render_report(
    manifest: &Manifest,
    workspace: &Path,
    facts: &Facts,
    parsed: &ParsedProbe<'_>,
) -> String {
    let os = os_label(manifest, &parsed.os);
    let shell = parsed.shell.clone();
    let distro = parsed.distro.clone();
    let sudo = parsed.sudo.clone();
    let branch = parsed.branch.clone();
    let answered = &parsed.answered;
    let executable = executable_map(manifest, parsed);
    let tools: Vec<serde_json::Value> = manifest
        .probe
        .iter()
        .zip(answered)
        .map(|(entry, answer)| {
            // The note renders the entry's advice over the answered
            // name, the family's alternative, so
            // `{{ executable.<entry> }}` is always a command this
            // machine has. Absent tools earn none: guidance about a
            // missing tool is the stale kind.
            let note = answer
                .as_deref()
                .zip(entry.advice.as_ref())
                .map(|(_name, advice)| {
                    // The advice renders through the section joiner,
                    // which appends a newline; a note rides inside a
                    // line, so its terminator is the template's.
                    let context =
                        facts.context(manifest, json!({ "executable": executable.clone() }));
                    render_text(advice, &context).trim_end().to_owned()
                })
                .filter(|note| !note.is_empty())
                .unwrap_or_default();
            json!({
                "name": answer.unwrap_or(entry.executable.as_str()),
                "state": answer.is_some(),
                "note": note,
            })
        })
        .collect();
    let context = facts.context(
        manifest,
        json!({
            "os": os,
            "distro": distro,
            "sudo": sudo,
            "shell": shell,
            "workspace": workspace.display().to_string(),
            "branch": branch,
            "tools": tools,
            "executable": executable,
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
/// beside it, `macOS (Darwin arm64)`, or the raw spelling alone when
/// the manifest maps no friendly name.
fn os_label(manifest: &Manifest, raw: &str) -> String {
    let Some((system, _rest)) = raw.split_once(' ') else {
        return raw.to_owned();
    };
    let system = system.to_lowercase();
    // The git-bash family reports itself as MINGW64_NT-10.0-19045 and
    // kin, a windows host by any prefix spelling, so the family gets
    // the friendly name without a table row per environment.
    if system.starts_with("mingw") || system.starts_with("msys") || system.starts_with("cygwin") {
        return format!("Windows ({raw})");
    }
    match manifest.os_names.get(&system) {
        Some(friendly) => format!("{friendly} ({raw})"),
        None => raw.to_owned(),
    }
}

/// Runs the probe script with `workspace` as working directory and
/// returns its bounded stdout. Stdin is null so a shell that wrongly
/// reads it sees EOF; anything still running at the deadline is killed
/// and reaped.
fn run_probe(manifest: &Manifest, workspace: &Path) -> Result<String, String> {
    let mut child = Command::new(rhema_bash::exec::bash_executable())
        .arg("-c")
        .arg(probe_script(manifest))
        .current_dir(workspace)
        // The terminal's program-resume token stops with the programs
        // rhema starts: this probe runs commands of its own, and none of
        // them may register a record for the terminal.
        .env_remove(rhema_bash::exec::OSC7577_TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => rhema_bash::exec::bash_missing_message(),
            _ => format!("cannot spawn the probe: {err}"),
        })?;
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

/// The wall-clock UTC stamp, to the minute, the one timestamp shape
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

/// Days since the Unix epoch to (year, month, day), UTC: `datealgo`'s
/// one copy of the civil calendar, without a clock dependency, and
/// saturated to its ±1.5 million-year range, never a panic.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days.clamp(datealgo::RD_MIN as i64, datealgo::RD_MAX as i64) as i32;
    let (year, month, day) = datealgo::rd_to_date(days);
    (i64::from(year), u32::from(month), u32::from(day))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names on one of the report's tool lines, trimmed, so an
    /// assertion names the tools and not the line's alignment padding.
    fn tool_line<'a>(report: &'a str, label: &str) -> Vec<&'a str> {
        report
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_else(|| panic!("no {label} line: {report}"))
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The baked manifest parses and every text renders over sample
    /// facts, the drift test a hand edit fails: a manifest that stops
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
                    "distro": "Ubuntu 24.04 LTS", "sudo": "passwordless",
                    "tools": [{"name": "rg", "state": true, "note": "n"}],
                    "kind": "k", "reason": "r",
                }),
            );
            let rendered = render_template(text, &context).expect("the text renders");
            assert!(!rendered.trim().is_empty());
        }
    }

    /// The host report carries its facts, date, model, harness, the OS
    /// line, and the two tool lists, with no versions anywhere. Both
    /// lists name only probed executables, and no name lands in both:
    /// a tool is present or missing, never the pair. The branch line
    /// follows the probe's own rule, present on a checkout and absent
    /// off one, so both sides get tested: the CWD decides which, and a
    /// throwaway repo pins the positive side wherever the suite happens
    /// to run. Runs real bash, like the claw's environment tests.
    #[test]
    fn the_host_report_carries_facts_and_the_tool_lists_without_versions() {
        let manifest = manifest();
        let report = host_report(Path::new("."), "glm-5.3", "rhema test");
        assert!(report.starts_with("## Environment\n"), "{report}");
        for fact in [
            "- Date: ",
            "- Model: glm-5.3",
            "- Harness: rhema test",
            "### Machine",
            "- OS: ",
            "- Sudo: ",
            "### Workspace",
            "- CWD: ",
            "### Command-line tools",
        ] {
            assert!(report.contains(fact), "missing {fact}: {report}");
        }
        let available = tool_line(&report, "Available tools:");
        let missing = tool_line(&report, "Missing tools:");
        let probed: Vec<&str> = manifest
            .probe
            .iter()
            .flat_map(|entry| {
                std::iter::once(entry.executable.as_str())
                    .chain(entry.alternatives.iter().map(String::as_str))
            })
            .collect();
        for name in &available {
            assert!(probed.contains(name), "{name} was never probed: {report}");
            assert!(
                !missing.contains(name),
                "{name} is present and missing: {report}"
            );
        }
        for name in &missing {
            assert!(probed.contains(name), "{name} was never probed: {report}");
        }
        // On Linux the distribution rides the OS line, from
        // /etc/os-release; elsewhere the line keeps whatever spelling
        // the host answers with.
        if std::path::Path::new("/etc/os-release").is_file() {
            let os_line = report
                .lines()
                .find(|line| line.starts_with("- OS: "))
                .expect("the OS line renders");
            assert!(
                os_line.contains('(') && os_line.ends_with(')'),
                "the distribution rides the Linux OS line: {os_line}"
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

        // Detached, the branch line still stands: `git branch
        // --show-current` answers empty there, and a PR checkout is
        // exactly that shape - the line used to vanish and take the
        // suite with it.
        run(&["checkout", "--detach"]);
        let report = host_report(&repo, "glm-5.3", "rhema test");
        assert!(
            report.contains("- git branch: detached"),
            "a detached HEAD says so: {report}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The `executable` map binds every entry by its manifest name to
    /// the command that answered, `null` when the whole family is
    /// absent: a text names the family it means and an absent family
    /// renders empty instead of naming a command the host lacks.
    #[test]
    fn the_executable_map_binds_each_entry_to_the_name_that_answered() {
        let manifest = manifest();
        let parsed = parse_probe(
            manifest,
            "Linux x86_64\nshell: /bin/bash\nrg: present\nchromium: absent\nhelium: present\n",
        );
        let executable = executable_map(manifest, &parsed);
        assert_eq!(executable["rg"], json!("rg"));
        assert_eq!(executable["chromium"], json!("helium"));
        assert_eq!(executable["fd"], serde_json::Value::Null);
        assert_eq!(
            executable.as_object().map(serde_json::Map::len),
            Some(manifest.probe.len()),
            "every entry is bound, answered or not"
        );
    }

    /// The machine lines over synthetic feeds, both shapes pinned:
    /// the distribution rides the OS line in parentheses when the
    /// host names one and stays off when it does not, and the sudo
    /// line always speaks, however the password gate stands.
    #[test]
    fn the_machine_lines_carry_the_distribution_and_sudo() {
        let manifest = manifest();
        let facts = facts("m", "h");
        let linux = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            &parse_probe(
                manifest,
                "Linux x86_64\nshell: /bin/bash\ndistro: Ubuntu 24.04 LTS\n\
                 sudo: passwordless\nrg: present\nfd: absent\njq: absent\n\
                 yq: absent\nsd: absent\ncurl: absent\nchromium: absent\n",
            ),
        );
        assert!(
            linux.contains("- OS: Linux x86_64 (Ubuntu 24.04 LTS)"),
            "{linux}"
        );
        assert!(linux.contains("- Sudo: passwordless"), "{linux}");
        let mac = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            &parse_probe(
                manifest,
                "Darwin arm64\nshell: /bin/zsh\nsudo: requires a password\nrg: absent\n",
            ),
        );
        assert!(
            mac.contains("- OS: macOS (Darwin arm64)\n- Sudo: "),
            "{mac}"
        );
        assert!(mac.contains("- Sudo: requires a password"), "{mac}");
    }

    /// The OS line maps the friendly name beside the raw spelling and
    /// keeps unmapped systems raw.
    #[test]
    fn the_os_line_maps_friendly_names_beside_the_raw() {
        let manifest = manifest();
        assert_eq!(os_label(manifest, "Darwin arm64"), "macOS (Darwin arm64)");
        assert_eq!(os_label(manifest, "SunOS sun4u"), "SunOS sun4u");
    }

    /// The notes name only the tools that answered, the ripgrep
    /// guidance is data like any other entry's, and a host without the
    /// tool gets neither its name nor its advice.
    #[test]
    fn advice_follows_presence_not_the_tool_name() {
        let manifest = manifest();
        let facts = facts("m", "h");
        let with = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            &parse_probe(
                manifest,
                "Darwin arm64\nshell: /bin/zsh\nrg: present\nfd: absent\ngit branch: main\n",
            ),
        );
        assert_eq!(tool_line(&with, "Available tools:"), ["rg"], "{with}");
        // Every family the probe left unanswered is missing, not just
        // the one whose absent line the fixture spelled out.
        assert_eq!(
            tool_line(&with, "Missing tools:"),
            ["fd", "jq", "yq", "sd", "curl", "lightpanda", "chromium"],
            "{with}"
        );
        assert!(with.contains("Notes:\n- Use `rg`"), "{with}");
        assert!(!with.contains("Use `fd`"), "{with}");
        let without = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            &parse_probe(
                manifest,
                "Darwin arm64\nshell: /bin/zsh\nrg: absent\nfd: absent\n",
            ),
        );
        assert!(
            tool_line(&without, "Available tools:").is_empty(),
            "{without}"
        );
        assert_eq!(
            tool_line(&without, "Missing tools:"),
            [
                "rg",
                "fd",
                "jq",
                "yq",
                "sd",
                "curl",
                "lightpanda",
                "chromium"
            ],
            "{without}"
        );
        assert!(!without.contains("Use `rg`"), "{without}");
        assert!(!without.contains("Notes:"), "{without}");
    }

    /// An alternatives family answers under the candidate that
    /// answered: the tools line and the advice's `executable` entry
    /// both carry the command the model would type, not the family's
    /// canonical name.
    #[test]
    fn alternatives_answer_under_the_candidate_that_answered() {
        let manifest = manifest();
        let facts = facts("m", "h");
        let report = render_report(
            manifest,
            Path::new("/ws"),
            &facts,
            &parse_probe(
                manifest,
                "Linux x86_64\nshell: /bin/bash\nrg: absent\nfd: absent\njq: absent\n\
                 yq: absent\nsd: absent\ncurl: absent\nlightpanda: absent\nchromium: absent\nhelium: present\n",
            ),
        );
        assert_eq!(
            tool_line(&report, "Available tools:"),
            ["helium"],
            "{report}"
        );
        assert_eq!(
            tool_line(&report, "Missing tools:"),
            ["rg", "fd", "jq", "yq", "sd", "curl", "lightpanda"],
            "{report}"
        );
        assert!(report.contains("helium --headless"), "{report}");
        assert!(!report.contains("chromium --headless"), "{report}");
    }

    /// The absence list names the entries whose whole family is
    /// absent, as the report spells them, the family's own name, not
    /// the alternative that does not exist here, and nothing else. A
    /// bare host lines answers names everything: every `command -v`
    /// spoke, and each said no. A probe that cannot run is the other
    /// extreme, [`host_environment`] leaves the list empty there,
    /// because an unprobed host asserts nothing.
    #[test]
    fn the_missing_list_names_absent_families_as_the_report_does() {
        let manifest = manifest();
        let present = parse_probe(
            manifest,
            "Linux x86_64\nshell: /bin/bash\nrg: present\nfd: absent\njq: absent\n\
             yq: absent\nsd: absent\ncurl: present\nlightpanda: absent\
             \nchromium: absent\nchrome: present\n",
        );
        assert_eq!(
            missing_tools(manifest, &present),
            vec!["fd", "jq", "yq", "sd", "lightpanda"],
            "the answered chromium family and the present rg and curl stay out"
        );
        let bare = parse_probe(manifest, "Linux x86_64\nshell: /bin/bash\n");
        assert_eq!(
            missing_tools(manifest, &bare).len(),
            manifest.probe.len(),
            "a host where every probe said absent misses everything"
        );
        let failed = host_environment(Path::new("/definitely/not/here"), "m", "h");
        assert!(
            failed.missing.is_empty(),
            "a failed probe claims no absences: {:?}",
            failed.missing
        );
    }

    /// The script reads the distribution without executing the file
    /// and checks sudo non-interactively: a prompt would hang a
    /// startup past its budget.
    #[test]
    fn the_script_reads_os_release_and_checks_sudo_without_a_prompt() {
        let script = probe_script(manifest());
        assert!(script.contains("/etc/os-release"), "{script}");
        assert!(!script.contains(". /etc/os-release"), "{script}");
        assert!(script.contains("sudo -n true"), "{script}");
    }

    /// The script probes a family in one call, order preserved, and
    /// drops the names the shell-safety filter refuses.
    #[test]
    fn the_script_probes_alternatives_in_order_and_drops_unsafe_names() {
        let manifest = Manifest {
            os_names: BTreeMap::new(),
            probe: vec![Probe {
                executable: "chromium".into(),
                advice: None,
                alternatives: vec!["google-chrome".into(), "bad;name".into()],
            }],
            facts: String::new(),
            report: String::new(),
            notice: String::new(),
            unavailable: String::new(),
        };
        let script = probe_script(&manifest);
        assert!(
            script.contains("probe chromium google-chrome\n"),
            "{script}"
        );
        assert!(!script.contains("bad;name"), "{script}");
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
            alternatives: Vec::new(),
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
