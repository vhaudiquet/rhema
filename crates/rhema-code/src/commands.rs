//! The slash-command table: the single source of truth for what commands
//! exist, what they say, and what they do.
//!
//! ARCHITECTURE: every command is one row of [`COMMANDS`] — name,
//! description, argument kind, and handler function, in one place. The
//! palette, `/help`, the parser, and the dispatcher all read this table,
//! so a command cannot exist outside it: a handler without a row is dead
//! code (fails the `-D warnings` build), and a row without a handler does
//! not compile. Adding a command means adding exactly one row.

use crate::app::{App, Effect};

/// How the text after a command's name is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Argument {
    /// No argument; one present is an error.
    None,
    /// An optional free-form word (e.g. a profile name).
    Optional,
}

/// One row of the command table.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// The name as typed, with the slash.
    pub name: &'static str,
    /// What it does, shown in the palette and `/help`.
    pub description: &'static str,
    /// How the argument after the name is handled.
    pub argument: Argument,
    /// The implementation.
    pub handler: fn(&mut App, Option<&str>) -> Vec<Effect>,
}

/// Every command, in palette order. Aliases are separate rows sharing a
/// handler (`/exit`, `/quit`).
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        description: "commands and keys",
        argument: Argument::None,
        handler: help,
    },
    CommandSpec {
        name: "/model",
        description: "pick the model (type for suggestions)",
        argument: Argument::Optional,
        handler: model,
    },
    CommandSpec {
        name: "/profile",
        description: "switch profile (work, personal)",
        argument: Argument::Optional,
        handler: profile,
    },
    CommandSpec {
        name: "/thinking",
        description: "reasoning effort (low, medium, high)",
        argument: Argument::Optional,
        handler: thinking,
    },
    CommandSpec {
        name: "/login",
        description: "re-login: provider, key, or server",
        argument: Argument::None,
        handler: login,
    },
    CommandSpec {
        name: "/settings",
        description: "providers, colors, skills, usage, about",
        argument: Argument::None,
        handler: settings,
    },
    CommandSpec {
        name: "/clear",
        description: "fresh session",
        argument: Argument::None,
        handler: clear,
    },
    CommandSpec {
        name: "/new",
        description: "fresh session",
        argument: Argument::None,
        handler: clear,
    },
    CommandSpec {
        name: "/resume",
        description: "continue a saved session",
        argument: Argument::None,
        handler: resume,
    },
    CommandSpec {
        name: "/skills",
        description: "list discovered skills",
        argument: Argument::None,
        handler: skills,
    },
    CommandSpec {
        name: "/system",
        description: "the system prompt in use",
        argument: Argument::None,
        handler: system,
    },
    CommandSpec {
        name: "/exit",
        description: "quit",
        argument: Argument::None,
        handler: exit,
    },
    CommandSpec {
        name: "/quit",
        description: "quit",
        argument: Argument::None,
        handler: exit,
    },
];

/// The outcome of parsing one chat input. Deliberately not `Eq`: the
/// command variant carries the row, and function pointers compare by
/// identity, not meaning.
#[derive(Debug, Clone)]
pub enum Parsed {
    /// Not a slash input — a prompt for the model.
    Prompt(String),
    /// A table command, with its argument when one was typed.
    Command {
        /// The row this input resolved to.
        spec: &'static CommandSpec,
        /// The argument after the name, when present.
        argument: Option<String>,
    },
    /// A slash input no row matches; the caller declines it in-band.
    Unknown(String),
}

/// Splits a slash input into the command name and the argument after it.
fn split(input: &str) -> (&str, Option<&str>) {
    let body = &input[1..];
    match body.split_once(' ') {
        Some((name, argument)) => (name, Some(argument.trim())),
        None => (body, None),
    }
}

/// Parses one chat input through the table.
pub fn parse(input: &str) -> Parsed {
    if !input.starts_with('/') {
        return Parsed::Prompt(input.to_owned());
    }
    let (name, argument) = split(input);
    match COMMANDS.iter().find(|spec| &spec.name[1..] == name) {
        Some(spec) => match (spec.argument, argument) {
            (Argument::None, Some(_)) => Parsed::Unknown(input.to_owned()),
            (Argument::Optional, argument) => Parsed::Command {
                spec,
                argument: argument.map(|a| a.to_owned()).filter(|a| !a.is_empty()),
            },
            (Argument::None, None) => Parsed::Command {
                spec,
                argument: None,
            },
        },
        None => Parsed::Unknown(input.to_owned()),
    }
}

/// The palette's candidates for an input: the table rows whose name starts
/// with the typed token.
pub fn candidates(input: &str) -> Vec<&'static CommandSpec> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    let needle = input.to_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&needle))
        .collect()
}

/// Runs the parsed command through its row's handler. Unknown inputs are
/// declined in-band; prompts never reach here (the chat submit path keeps
/// them).
pub fn dispatch(app: &mut App, input: &str) -> Vec<Effect> {
    match parse(input) {
        Parsed::Command { spec, argument } => (spec.handler)(app, argument.as_deref()),
        Parsed::Unknown(name) => {
            app.unknown_command(&name);
            Vec::new()
        }
        Parsed::Prompt(_) => Vec::new(),
    }
}

// --- Handlers ---
// Private on purpose: an unregistered handler is dead code and fails the
// `-D warnings` build, which is what keeps the table complete.

fn help(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    let commands = COMMANDS
        .iter()
        .map(|spec| format!("{} ({})", spec.name, spec.description))
        .collect::<Vec<_>>()
        .join(" · ");
    app.push_notice(&format!(
        "{commands}; keys: ctrl-p model · pgup/pgdn or wheel history · \
         shift+drag to copy text · ctrl-c or ctrl-d quit"
    ));
    Vec::new()
}

fn model(app: &mut App, argument: Option<&str>) -> Vec<Effect> {
    if app.turn.running {
        app.push_notice("a turn is running; /model waits for it to finish");
        return Vec::new();
    }
    match argument {
        None => app.open_picker(),
        Some(needle) => app.choose_model_by_id(needle),
    }
}

fn resume(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    if app.turn.running {
        app.push_notice("a turn is running; /resume waits for it to finish");
        return Vec::new();
    }
    app.open_resume()
}

fn skills(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    let global = rhema_config::Config::skills_dir().ok();
    let workspace = std::fs::canonicalize(&app.options.workspace)
        .unwrap_or_else(|_| app.options.workspace.clone());
    app.show_skills(global.as_deref(), &workspace);
    Vec::new()
}

fn system(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    // The owned copy ends the borrow before the transcript grows.
    let prompt = app.agent.as_ref().map(|agent| agent.system().to_owned());
    match prompt {
        Some(prompt) => app.show_system_prompt(&prompt),
        None => app.push_notice("no agent yet; /system shows the prompt once one exists"),
    }
    Vec::new()
}

fn profile(app: &mut App, argument: Option<&str>) -> Vec<Effect> {
    match argument {
        Some(name) => {
            app.switch_profile(name);
            Vec::new()
        }
        None => app.open_profile_picker(),
    }
}

fn thinking(app: &mut App, argument: Option<&str>) -> Vec<Effect> {
    use rhema_core::request::ThinkingEffort;

    let Some(text) = argument else {
        app.push_notice("usage: /thinking low, medium, high, or default");
        return Vec::new();
    };
    match ThinkingEffort::parse(text) {
        Some(effort) => {
            app.set_thinking(effort);
            Vec::new()
        }
        None => {
            app.push_notice(&format!(
                "unknown effort {text:?}; use low, medium, high, or default"
            ));
            Vec::new()
        }
    }
}

fn login(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    // The login flow lives on the settings' Providers page now.
    app.open_settings_at(
        crate::screens::SettingsSection::Providers,
        crate::screens::SettingsPurpose::FromChat,
    );
    Vec::new()
}

fn settings(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    app.open_settings_at(
        crate::screens::SettingsSection::Providers,
        crate::screens::SettingsPurpose::FromChat,
    );
    Vec::new()
}

fn clear(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    app.clear_session();
    Vec::new()
}

fn exit(app: &mut App, _argument: Option<&str>) -> Vec<Effect> {
    app.should_quit = true;
    Vec::new()
}
