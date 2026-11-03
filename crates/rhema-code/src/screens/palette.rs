//! The chat command palette: command and provider rows, and the filesystem path rows.

use super::chat::CompletionCache;
use super::*;

/// The commands whose names start with the slash token in `input`, in
/// table order. `/` alone matches everything; an input that is not a
/// command at all matches nothing. Reads the command table — the single
/// source of truth — so a row exists for the palette the moment it
/// dispatches.
pub fn filtered_commands(input: &str) -> Vec<&'static crate::commands::CommandSpec> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    let needle = input.to_lowercase();
    crate::commands::COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&needle))
        .collect()
}

/// The command a slash input names together with the fixed words its
/// argument completes from, when the command's argument is one:
/// `/thinking h` names `/thinking` and its levels. None when the input
/// names no command, or one whose argument completes from live rows
/// (the models, the profiles, the sandboxes).
pub(crate) fn choice_argument(
    input: &str,
) -> Option<(&'static str, &'static [crate::commands::Choice])> {
    let body = input.strip_prefix('/')?;
    let (name, _) = body.split_once(' ')?;
    let spec = crate::commands::COMMANDS
        .iter()
        .find(|spec| &spec.name[1..] == name)?;
    match spec.argument {
        crate::commands::Argument::Choice(choices) => Some((spec.name, choices)),
        _ => None,
    }
}

/// One completable profile row, built from the config registry: the
/// palette reads it, `/profile <name>` switches onto it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteProfile {
    /// The profile's name, the value completion inserts.
    pub name: String,
    /// The provider the profile runs on, when configured.
    pub provider: Option<String>,
    /// The profile's stored model, when configured.
    pub model: Option<String>,
    /// Whether the session currently runs on this profile.
    pub active: bool,
}

/// One completable sandbox row, built from the session profile's
/// configured instances: the palette reads it, `/sandbox <name>` runs
/// `bash` in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteSandbox {
    /// The instance's name, the value completion inserts.
    pub name: String,
    /// The kind the instance runs: `docker`, `lxd`, `ssh`, `http`,
    /// `ecrou` — stored as configured.
    pub kind: String,
    /// What the kind names its instance by, when configured.
    pub target: Option<String>,
    /// Whether the session currently runs `bash` in this instance.
    pub active: bool,
}

/// One row of the input palette: a command while the typed token matches
/// the command table, a model once `/model ` is naming its argument, a
/// profile under `/profile`, or a sandbox under `/sandbox`. The
/// palette's rendering and its keys (arrows, tab) read the same rows.
#[derive(Debug, Clone)]
pub enum PaletteRow {
    /// A slash command; completing replaces the whole input with its name.
    Command(&'static crate::commands::CommandSpec),
    /// A model under `/model`; completing fills the argument. The
    /// provider's config name prefixes the row and rides on the switch.
    Model {
        /// The provider's config name.
        provider: &'static str,
        /// The model id, the value completion inserts.
        id: String,
        /// What the row says about the model, after the id.
        description: String,
    },
    /// A profile under `/profile`; completing fills the argument.
    Profile {
        /// The profile's name, the value completion inserts.
        name: String,
        /// What the row says about the profile, after the name.
        description: String,
    },
    /// A fixed word under a command that names one (`/thinking`);
    /// completing fills the argument.
    Choice {
        /// The command the word belongs to, with the slash.
        command: &'static str,
        /// The word completion inserts.
        value: &'static str,
        /// What the row says about the word, after it.
        description: &'static str,
    },
    /// A sandbox under `/sandbox` — a configured instance or a kind;
    /// completing fills the argument.
    Sandbox {
        /// The word completion inserts: an instance's name or a kind's.
        value: String,
        /// What the row says about it, after the word.
        description: String,
    },
    /// A filesystem path under the cursor; completing splices the
    /// entry over the token it came from.
    Path {
        /// Start of the token (char offset) the completion replaces.
        token_start: usize,
        /// End of the token (char offset), exclusive.
        token_end: usize,
        /// The entry's name, with a trailing `/` on directories — the
        /// row's label.
        name: String,
        /// Whether the entry is a directory.
        is_dir: bool,
        /// The text to splice over the token: the directory part as
        /// typed, the entry's name, and a trailing `/` on directories.
        completion: String,
    },
}

/// The palette's rows for an input: command names while the input
/// matches one, the models matching the `/model` argument, the profiles
/// matching the `/profile` argument, the sandboxes matching the
/// `/sandbox` argument, or — when none of those fire — the filesystem
/// entries under the cursor's path token. The argument filters follow
/// the picker's rules — the rendered row text, case-insensitive.
pub fn palette_rows(
    input: &str,
    cursor: usize,
    models: &[CatalogModel],
    profiles: &[PaletteProfile],
    sandboxes: &[PaletteSandbox],
    icons: &crate::icons::Icons,
) -> Vec<PaletteRow> {
    if !input.starts_with('/') {
        return path_completion_rows(input, cursor);
    }
    if let Some(needle) = input.strip_prefix("/model ") {
        let needle = needle.to_lowercase();
        let rows: Vec<PaletteRow> = models
            .iter()
            .filter(|entry| {
                let mut haystack = format!("{}/{}", entry.provider, entry.model.id);
                if let Some(instance) = &entry.instance {
                    haystack.push_str(&format!(" {instance}"));
                }
                needle.is_empty()
                    || haystack.to_lowercase().contains(&needle)
                    || entry.model.display_name.to_lowercase().contains(&needle)
            })
            .map(|entry| PaletteRow::Model {
                provider: entry.provider,
                id: entry.model.id.clone(),
                description: model_description(&entry.model, icons),
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    if let Some(needle) = input.strip_prefix("/profile ") {
        let needle = needle.to_lowercase();
        let rows: Vec<PaletteRow> = profiles
            .iter()
            .filter(|profile| {
                needle.is_empty() || {
                    let row = format!(
                        "{} {} {}",
                        profile.name,
                        profile.provider.as_deref().unwrap_or(""),
                        profile.model.as_deref().unwrap_or("")
                    );
                    row.to_lowercase().contains(&needle)
                }
            })
            .map(|profile| PaletteRow::Profile {
                name: profile.name.clone(),
                description: profile_description(profile),
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    // `/sandbox` completes from the profile's configured instances
    // alone. A bare kind still runs (`/sandbox <name>` falls back to
    // parsing the word as a kind), but the palette names nothing the
    // profile does not.
    if let Some(needle) = input.strip_prefix("/sandbox ") {
        let needle = needle.to_lowercase();
        let rows: Vec<PaletteRow> = sandboxes
            .iter()
            .filter(|instance| {
                let mut haystack = format!("{} {}", instance.name, instance.kind);
                if let Some(target) = &instance.target {
                    haystack.push_str(&format!(" {target}"));
                }
                needle.is_empty() || haystack.to_lowercase().contains(&needle)
            })
            .map(|instance| PaletteRow::Sandbox {
                value: instance.name.clone(),
                description: sandbox_description(instance),
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    // A choice argument lists its command's fixed words, filtered by
    // the half-typed one: `/thinking h` matches `high`.
    if let Some((command, choices)) = choice_argument(input) {
        let needle = input
            .split_once(' ')
            .map(|(_, rest)| rest.trim().to_lowercase())
            .unwrap_or_default();
        let rows: Vec<PaletteRow> = choices
            .iter()
            .filter(|choice| needle.is_empty() || choice.name.starts_with(&needle))
            .map(|choice| PaletteRow::Choice {
                command,
                value: choice.name,
                description: choice.description,
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    let commands: Vec<PaletteRow> = filtered_commands(input)
        .into_iter()
        .map(PaletteRow::Command)
        .collect();
    if !commands.is_empty() {
        return commands;
    }
    // A slash input naming no command may still be a path: `/etc/hos`
    // completes like any other absolute path.
    path_completion_rows(input, cursor)
}

/// The most path rows one token lists: big directories still complete,
/// without the palette carrying every entry.
const MAX_PATH_ROWS: usize = 256;

/// The rows the popup shows, cached against `(input, cursor)` on the
/// chat state. The render path runs every frame; the directory walk
/// behind the path rows runs only when the query changes.
pub fn palette_rows_cached(
    chat: &mut ChatState,
    models: &[CatalogModel],
    profiles: &[PaletteProfile],
    sandboxes: &[PaletteSandbox],
    icons: &crate::icons::Icons,
) -> Vec<PaletteRow> {
    let fresh = match &chat.completion {
        Some(cache) => cache.input == chat.input && cache.cursor == chat.cursor,
        None => false,
    };
    if !fresh {
        let rows = palette_rows(&chat.input, chat.cursor, models, profiles, sandboxes, icons);
        chat.completion = Some(CompletionCache {
            input: chat.input.clone(),
            cursor: chat.cursor,
            rows,
        });
    }
    chat.completion
        .as_ref()
        .map_or_else(Vec::new, |c| c.rows.clone())
}

/// The path rows for the token under the cursor: the listed directory's
/// entries whose name starts with the typed stem, case-insensitive,
/// directories first. Hidden entries (a leading dot) stay hidden until
/// the stem leads with one. A token completes nothing unless it carries
/// a separator or opens with `~` or `.`; a directory that cannot be
/// read simply lists nothing.
pub(crate) fn path_completion_rows(input: &str, cursor: usize) -> Vec<PaletteRow> {
    let Some((token_start, token_end)) = path_token(input, cursor) else {
        return Vec::new();
    };
    let token: String = input
        .chars()
        .skip(token_start)
        .take(token_end - token_start)
        .collect();
    let Some((dir_part, stem, dir)) = split_path_token(&token) else {
        return Vec::new();
    };
    let needle = stem.to_lowercase();
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|listing| {
            listing
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    if name.starts_with('.') && !stem.starts_with('.') {
                        return None;
                    }
                    let is_dir = entry.path().is_dir();
                    (needle.is_empty() || name.to_lowercase().starts_with(&needle))
                        .then_some((name, is_dir))
                })
                .collect()
        })
        .unwrap_or_default();
    // Directories first, then names case-insensitively.
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    entries.truncate(MAX_PATH_ROWS);
    entries
        .into_iter()
        .map(|(name, is_dir)| PaletteRow::Path {
            token_start,
            token_end,
            name: if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            },
            is_dir,
            completion: format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }),
        })
        .collect()
}

/// The word under the cursor: the run of characters around it that are
/// neither whitespace nor an attachment anchor, as `(start, end)` char
/// offsets. None when the run is empty.
pub(crate) fn path_token(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let breaks = |c: char| c.is_whitespace() || c == ChatState::MARK;
    let chars: Vec<char> = input.chars().collect();
    let pos = cursor.min(chars.len());
    let start = (0..pos)
        .rev()
        .find(|index| breaks(chars[*index]))
        .map_or(0, |index| index + 1);
    let end = chars[pos..]
        .iter()
        .position(|c| breaks(*c))
        .map_or(chars.len(), |offset| pos + offset);
    (start < end).then_some((start, end))
}

/// The token split for completion: the directory part as typed (through
/// the final `/`), the stem the entries must match, and the directory
/// to list — `~` expands to the home directory, an empty part to the
/// working directory. None when the token names no path.
pub(crate) fn split_path_token(token: &str) -> Option<(String, String, PathBuf)> {
    if token == "~" {
        let home = std::env::home_dir()?;
        return Some(("~".to_owned(), String::new(), home));
    }
    let path_like = token.starts_with('~') || token.starts_with('.') || token.contains('/');
    if !path_like {
        return None;
    }
    let (dir_part, stem) = match token.rfind('/') {
        Some(index) => (&token[..=index], &token[index + 1..]),
        None => ("", token),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        std::env::home_dir()?.join(rest)
    } else if dir_part == "~/" {
        std::env::home_dir()?
    } else if dir_part.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(dir_part)
    };
    Some((dir_part.to_owned(), stem.to_owned(), dir))
}

/// A model's palette description: the display name with the picker's
/// facts — pricing, the vision verdict, and the context window — when
/// the catalog knows them. The verdict is tri-state like the catalog's:
/// `None` says nothing, because unknown is not a verdict.
pub(crate) fn model_description(model: &ModelCard, icons: &crate::icons::Icons) -> String {
    let mut detail = model.display_name.clone();
    if let Some(pricing) = model.pricing {
        detail.push_str(&format!(
            " · ${:.2}/M in · ${:.2}/M out",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    match model.supports_vision {
        Some(true) => detail.push_str(&format!(" · {} vision", icons.vision)),
        Some(false) => detail.push_str(" · no vision"),
        None => {}
    }
    if let Some(limit) = model.input_token_limit {
        detail.push_str(&format!(" · {} ctx", format_tokens(Some(limit))));
    }
    detail
}

/// A profile's palette description: its provider and model, the way the
/// profile picker says them, with the active marker last.
pub(crate) fn profile_description(profile: &PaletteProfile) -> String {
    let mut detail = profile
        .provider
        .clone()
        .unwrap_or_else(|| "no provider".into());
    if let Some(model) = &profile.model {
        detail.push_str(&format!(" · {model}"));
    }
    if profile.active {
        detail.push_str(" · active");
    }
    detail
}

/// A sandbox instance's palette description: the kind with its target,
/// the way the settings Sandbox page says them, with the active marker
/// last.
pub(crate) fn sandbox_description(instance: &PaletteSandbox) -> String {
    let mut detail = instance.kind.clone();
    if let Some(target) = &instance.target {
        detail.push_str(&format!(" · {target}"));
    }
    if instance.active {
        detail.push_str(" · active");
    }
    detail
}

/// Humanizes a token count: 1_048_576 → "1.0M", 131_072 → "131k", none →
/// "?".
pub(crate) fn format_tokens(limit: Option<u64>) -> String {
    match limit {
        None => "?".to_owned(),
        Some(limit) if limit >= 1_000_000 => {
            format!("{:.1}M", limit as f64 / 1_000_000.0)
        }
        Some(limit) if limit >= 10_000 => format!("{}k", limit / 1_000),
        Some(limit) => format!("{limit}"),
    }
}

// --- Model picker ---
