//! The machine's git: the host's effective config flattened into
//! the workspace, staged where the share carries it, the settings
//! that break the machine guarded against.

use std::path::Path;

use crate::EcrouSandbox;

/// The staged git identity: where the file landed on the host, and
/// the path it answers to inside the machine, the same absolute path,
/// the share being 1:1.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GitEnv {
    pub(crate) guest_config_path: String,
}

impl EcrouSandbox {
    /// Stage the host's effective git config into the workspace, for
    /// the machine's commands to read through `GIT_CONFIG_GLOBAL`.
    ///
    /// The shape is deliberate. A mount of `~/.config` would drag every
    /// application's secrets into a machine that exists for one
    /// command, and the strict snap cannot see dot directories anyway;
    /// a verbatim copy of `~/.gitconfig` breaks on `includeIf`, the
    /// common per-directory identity points at host-only files, and
    /// carries host-bound settings that actively break the machine
    /// (credential helpers that are not installed there, commit
    /// signing whose keys are not there). So the effective config is
    /// flattened here, by git, with the workspace as the cwd: the
    /// `gitdir:` conditions resolve to this tree, the same tree the
    /// share gives the machine, so the identity that lands is the one
    /// this workspace already gets. The flattened pairs re-emit as a
    /// plain config file under three guard overrides, `safe.directory`
    /// (ownership across the share), `commit.gpgsign`, an empty
    /// `credential.helper` (both host-only), and repo-local
    /// `.git/config` crosses with the tree on its own, under the usual
    /// system < global < local precedence.
    ///
    /// Anything that cannot be done, no git here, no config, a
    /// workspace that is not a repo, stages nothing, and the payload's
    /// git behaves as it always has. Never an error: a config is
    /// provisioning, not the payload.
    pub(crate) fn stage_git_config(&self) -> Option<GitEnv> {
        let root = self.root.map("/").ok()?;
        let out_dir = self.root.map("/.rhema/git").ok()?;
        std::fs::create_dir_all(&out_dir).ok()?;

        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "--list", "-z", "--includes"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let flat = String::from_utf8_lossy(&output.stdout);
        let mut body = render_pairs(&flat);
        if body.is_empty() {
            return None;
        }
        body.push_str(GIT_GUARDS);

        let path = out_dir.join("config");
        let unchanged = std::fs::read(&path)
            .map(|bytes| bytes == body.as_bytes())
            .unwrap_or(false);
        if !unchanged {
            std::fs::write(&path, body).ok()?;
        }

        // The staging lives inside the tree the user commits; keep it
        // out of `git status` through the local-only exclude, never
        // through their `.gitignore`.
        self.exclude_staging(&root);

        Some(GitEnv {
            guest_config_path: format!("{}/.rhema/git/config", self.workspace),
        })
    }

    /// Adds `.rhema/` to the workspace's `.git/info/exclude`, the
    /// local ignore that never leaves the machine, when the tree is a
    /// repository and the line is not already there.
    fn exclude_staging(&self, root: &Path) {
        let Some(git_dir) = std::fs::canonicalize(root.join(".git")).ok() else {
            return;
        };
        if !git_dir.is_dir() {
            return; // a worktree's .git is a file; its exclude is not ours to edit
        }
        let exclude = git_dir.join("info/exclude");
        let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
        if existing.lines().any(|line| line.trim() == ".rhema/") {
            return;
        }
        let _ = std::fs::create_dir_all(git_dir.join("info"));
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(".rhema/\n");
        let _ = std::fs::write(&exclude, next);
    }
}

/// The settings that would break the machine, overridden after the
/// flattened host config: git's dubious-ownership refusal (the tree
/// crosses the share), commit signing (the keys stayed home), and the
/// credential helper stack (the helpers stayed home too, an empty
/// value resets the list git accumulated).
pub(crate) const GIT_GUARDS: &str = "\n[safe]\n\tdirectory = *\n[commit]\n\tgpgsign = false\n[credential]\n\thelper =\n[core]\n\tfsmonitor = false\n";

/// Re-emits `git config --list -z` records (`key\nvalue\0`) as config
/// file syntax: everything before the last dot is the section path, the
/// rest is the key. Values that survive bare stay bare; anything with
/// newlines, quotes, backslashes, or padding goes out double-quoted
/// with C escapes, the one form git re-reads losslessly.
pub(crate) fn render_pairs(flat: &str) -> String {
    let mut out = String::new();
    for record in flat.split('\0') {
        let record = record.strip_prefix('\n').unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let Some((key, value)) = record.split_once('\n') else {
            continue;
        };
        let Some((section, name)) = key.rsplit_once('.') else {
            continue;
        };
        out.push('[');
        out.push_str(section);
        out.push_str("]\n\t");
        out.push_str(name);
        out.push_str(" = ");
        out.push_str(&config_value(value));
        out.push('\n');
    }
    out
}

/// One value, file-safe: bare when plain, quoted and C-escaped when
/// not, a newline in a value is a real thing (custom diff drivers
/// carry them) and bare it would end the entry early.
fn config_value(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with(' ')
        && !value.ends_with(' ')
        && !value
            .chars()
            .any(|c| matches!(c, '\n' | '"' | '\\' | ';' | '#'));
    if plain {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '\n' => quoted.push_str("\\n"),
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}
