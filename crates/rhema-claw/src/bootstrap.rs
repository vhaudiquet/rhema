//! The bootstrap layer: the resident agent's user-editable identity
//! files, the system prompt rendered from them per dispatch, and the
//! one tool the model writes them with.
//!
//! Context is earned (framework spec §9): one small always-injected
//! bootstrap doc, and nothing else force-injected — memory stays in
//! `rhema-memory`, pulled via tools. SOUL.md and USER.md are the earned
//! exceptions: tiny, stable, high-signal, and written only through
//! [`BootstrapSave`], so the model never free-writes config paths.
//! ENVIRONMENT.md lives in the same store but is generated at boot by
//! the environment module; the save path refuses it. The store's root
//! is handed over through [`Bootstrap::root`] for exactly that writer.
//!
//! The template ([`DEFAULT_SYSTEM_MD`]) is data next to the code, the
//! coding agent's house pattern, and it renders through the agent
//! toolkit's Jinja renderer — one template language for both agents:
//! `{{ workspace }}`, the three file sections from the live files, a
//! `{% for %}` over `tools` from the dispatch's own resolver chain, so
//! the roster the prompt shows is the roster the model gets. A present
//! file replaces its placeholder verbatim — never appended to, never
//! re-parsed; a missing one renders its stub. The `{% if onboarding %}`
//! section renders only while either file is unwritten, so the offer
//! self-terminates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rhema_agent_tools::render_template;
use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde_json::{Value, json};

/// The built-in system prompt, kept as data next to the code and read
/// in whole — placeholders filled per dispatch by [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// The cap on one bootstrap file: these documents enter every request,
/// so a bigger write is a misuse of the channel, refused loudly.
pub const MAX_CONTENT_BYTES: usize = 16 * 1024;

/// The save tool's name — allowlists reference it.
pub const BOOTSTRAP_SAVE: &str = "bootstrap_save";

/// One of the three bootstrap files. SOUL.md and USER.md are the
/// model-written pair; ENVIRONMENT.md is the boot-generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootstrapKind {
    /// SOUL.md — who the claw is: name, tone, purpose.
    Soul,
    /// USER.md — who the user is.
    User,
    /// ENVIRONMENT.md — the machine, written by the environment
    /// module's boot pass; never through [`Bootstrap::save`].
    Environment,
}

impl BootstrapKind {
    /// The file name under the store's root — uppercase, the house
    /// spelling.
    pub fn file_name(self) -> &'static str {
        match self {
            BootstrapKind::Soul => "SOUL.md",
            BootstrapKind::User => "USER.md",
            BootstrapKind::Environment => "ENVIRONMENT.md",
        }
    }

    /// Parses the spelling the tool schema uses. `None` for anything
    /// else.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "soul" => Some(BootstrapKind::Soul),
            "user" => Some(BootstrapKind::User),
            "environment" => Some(BootstrapKind::Environment),
            _ => None,
        }
    }
}

/// Why a bootstrap operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// The store's directory could not be created, or a file could not
    /// be read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// ENVIRONMENT.md is boot-generated; the save path refuses it.
    #[error("ENVIRONMENT.md is generated at boot; it is not saveable through bootstrap_save")]
    EnvironmentNotSaveable,
    /// The content is over [`MAX_CONTENT_BYTES`]: a bootstrap file
    /// enters every request, so a bigger one is refused, not trimmed.
    #[error("bootstrap content is {size} bytes; the cap is {MAX_CONTENT_BYTES} bytes")]
    TooLarge {
        /// The rejected content's size, in bytes.
        size: usize,
    },
    /// The system-prompt template failed to render — bad syntax, unknown
    /// filter — with the template position when known. The dispatch
    /// refuses to run on a prompt it cannot fully render.
    #[error("system prompt template: {0}")]
    Template(String),
}

/// Which of the onboarding files exist — the badge the HTTP surface
/// serves and the onboarding section's off switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BootstrapStatus {
    /// SOUL.md exists.
    pub soul: bool,
    /// USER.md exists.
    pub user: bool,
}

/// The identity-file store: three markdown files under one root —
/// SOUL.md, USER.md, ENVIRONMENT.md. Its only writers are
/// [`BootstrapSave`] (the model's path, SOUL/USER only) and the
/// onboarding flow through it; the environment module writes
/// ENVIRONMENT.md directly at [`Bootstrap::root`].
pub struct Bootstrap {
    root: PathBuf,
}

impl Bootstrap {
    /// Opens the store at `root`, creating the directory: a fresh
    /// install has no files, which is the pending-onboarding state, not
    /// an error.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, BootstrapError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The store's root — the documented seam the daemon hands to the
    /// environment module, whose boot pass writes ENVIRONMENT.md here.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file's text, `None` while it is unwritten.
    pub fn load(&self, kind: BootstrapKind) -> Result<Option<String>, BootstrapError> {
        match std::fs::read_to_string(self.root.join(kind.file_name())) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Overwrites the file with `content`, verbatim. ENVIRONMENT.md is
    /// refused — it is generated at boot, never model-written.
    pub fn save(&self, kind: BootstrapKind, content: &str) -> Result<(), BootstrapError> {
        if kind == BootstrapKind::Environment {
            return Err(BootstrapError::EnvironmentNotSaveable);
        }
        let size = content.len();
        if size > MAX_CONTENT_BYTES {
            return Err(BootstrapError::TooLarge { size });
        }
        std::fs::write(self.root.join(kind.file_name()), content)?;
        Ok(())
    }

    /// Which of the onboarding files exist, read live.
    pub fn status(&self) -> Result<BootstrapStatus, BootstrapError> {
        Ok(BootstrapStatus {
            soul: self.load(BootstrapKind::Soul)?.is_some(),
            user: self.load(BootstrapKind::User)?.is_some(),
        })
    }

    /// Whether the onboarding flow still applies: either file is
    /// unwritten. An unreadable store counts as pending — the worst
    /// case is an offered setup, never a silently skipped one.
    pub fn needs_onboarding(&self) -> bool {
        !self.status().is_ok_and(|status| status.soul && status.user)
    }
}

/// The stub a missing SOUL.md or USER.md renders.
const PENDING: &str = "(pending onboarding)";

/// The stub a missing ENVIRONMENT.md renders; the environment module's
/// boot pass writes the real file.
const GENERATED_AT_BOOT: &str = "(generated at boot)";

/// The offered onboarding, rendered while either file is unwritten. It
/// self-terminates: the per-dispatch status check stops rendering it
/// the moment both files exist. The offer is made in chat and may be
/// declined — never an unprompted interrogation. Compiled in like the
/// base template (`../ONBOARDING.md`), for the same reason: the prompt
/// is code-reviewed text, not runtime state.
const ONBOARDING_MD: &str = include_str!("../ONBOARDING.md");

/// One rendered markdown section: header, blank line, the file's text
/// — or the stub while it is unwritten.
fn section(header: &str, content: Option<&str>, stub: &str) -> String {
    format!("{header}\n\n{}", content.unwrap_or(stub))
}

/// Renders the system prompt for one dispatch through the agent
/// toolkit's Jinja renderer: `{{ workspace }}` and the three file
/// sections from the live store, `tools` from the dispatch's own
/// resolver specs, `{{ session }}` — the dispatching key, the address
/// the model schedules its own wakes to — and `onboarding`, the offer
/// the template's `{% if %}` shows only while a file is unwritten.
/// Fallible on purpose — a store that cannot be read surfaces loudly
/// instead of rendering a prompt without it, and a template that
/// cannot render fails the dispatch before any model call.
pub fn render_system(
    template: &str,
    session: &str,
    workspace: &Path,
    bootstrap: &Bootstrap,
    tools: &[ToolSpec],
) -> Result<String, BootstrapError> {
    let soul = bootstrap.load(BootstrapKind::Soul)?;
    let user = bootstrap.load(BootstrapKind::User)?;
    let environment = bootstrap.load(BootstrapKind::Environment)?;
    let onboarding = if soul.is_none() || user.is_none() {
        ONBOARDING_MD
    } else {
        ""
    };
    let context = json!({
        "session": session,
        "workspace": workspace.display().to_string(),
        "soul": section("## SOUL", soul.as_deref(), PENDING),
        "user": section("## USER", user.as_deref(), PENDING),
        "environment": section("## ENVIRONMENT", environment.as_deref(), GENERATED_AT_BOOT),
        "onboarding": onboarding,
        "tools": tools,
    });
    render_template(template, &context).map_err(|err| {
        let line = err
            .line()
            .map(|line| format!(", line {line}"))
            .unwrap_or_default();
        BootstrapError::Template(format!("{err}{line}"))
    })
}

#[derive(serde::Deserialize)]
struct SaveArgs {
    kind: String,
    content: String,
}

/// Writes SOUL.md or USER.md through the [`Bootstrap`] store — the only
/// writer the model gets, deliberately allowlistable and deterministic.
pub struct BootstrapSave {
    bootstrap: Arc<Bootstrap>,
}

#[async_trait]
impl Tool for BootstrapSave {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            BOOTSTRAP_SAVE,
            "Write one of your identity files. `kind` is `soul` (who you \
             are: name, tone, purpose) or `user` (who the user is). \
             Overwrites the file, so send the whole document every time. \
             Both files enter every request: keep each one small, \
             stable, and high-signal.",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["soul", "user"],
                        "description": "Which file to write: soul or user." },
                    "content": { "type": "string",
                        "description": "The file's full markdown contents." }
                },
                "required": ["kind", "content"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: SaveArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = BootstrapKind::parse(&args.kind)
            .ok_or_else(|| format!("unknown kind {:?}: soul or user", args.kind))?;
        self.bootstrap
            .save(kind, &args.content)
            .map_err(|err| err.to_string())?;
        Ok(format!("saved {}", kind.file_name()))
    }
}

/// The bootstrap tool set over `store`: `bootstrap_save`, the shape the
/// interactive resolver chain composes.
pub fn registry(bootstrap: Arc<Bootstrap>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BootstrapSave { bootstrap }));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Bootstrap) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bootstrap = Bootstrap::new(dir.path().join("bootstrap")).expect("store opens");
        (dir, bootstrap)
    }

    /// ENVIRONMENT.md is written directly — the environment module's
    /// boot pass is its only writer, never [`Bootstrap::save`].
    fn write_environment(bootstrap: &Bootstrap, content: &str) {
        std::fs::write(bootstrap.root().join("ENVIRONMENT.md"), content).expect("write");
    }

    #[test]
    fn new_creates_the_store_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("bootstrap");
        let bootstrap = Bootstrap::new(&nested).expect("store opens");
        assert!(nested.is_dir());
        assert_eq!(bootstrap.root(), nested.as_path());
        assert!(bootstrap.needs_onboarding(), "a fresh store is pending");
    }

    #[test]
    fn kinds_parse_and_name_their_files() {
        assert_eq!(BootstrapKind::parse("soul"), Some(BootstrapKind::Soul));
        assert_eq!(BootstrapKind::parse(" user "), Some(BootstrapKind::User));
        assert_eq!(
            BootstrapKind::parse("environment"),
            Some(BootstrapKind::Environment)
        );
        assert_eq!(BootstrapKind::parse("soul.md"), None);
        assert_eq!(BootstrapKind::parse(""), None);
        for kind in [
            BootstrapKind::Soul,
            BootstrapKind::User,
            BootstrapKind::Environment,
        ] {
            let name = kind.file_name().to_lowercase();
            assert_eq!(
                BootstrapKind::parse(name.trim_end_matches(".md")),
                Some(kind),
                "{}",
                kind.file_name()
            );
        }
    }

    #[test]
    fn save_then_load_round_trips_and_overwrites() {
        let (_dir, bootstrap) = store();
        assert_eq!(bootstrap.load(BootstrapKind::Soul).expect("reads"), None);
        bootstrap
            .save(BootstrapKind::Soul, "I am claw.\n")
            .expect("saves");
        assert_eq!(
            bootstrap.load(BootstrapKind::Soul).expect("reads"),
            Some("I am claw.\n".to_owned()),
            "saved verbatim"
        );
        bootstrap
            .save(BootstrapKind::Soul, "I am claw, v2.")
            .expect("overwrites");
        assert_eq!(
            bootstrap.load(BootstrapKind::Soul).expect("reads"),
            Some("I am claw, v2.".to_owned())
        );
        let status = bootstrap.status().expect("status reads");
        assert!(status.soul);
        assert!(!status.user);
        assert!(bootstrap.needs_onboarding(), "user is still unwritten");
        bootstrap
            .save(BootstrapKind::User, "the user is Ana.")
            .expect("saves");
        assert!(!bootstrap.needs_onboarding(), "both files exist");
    }

    #[test]
    fn the_size_cap_is_loud_not_trimmed() {
        let (_dir, bootstrap) = store();
        let too_big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let err = bootstrap
            .save(BootstrapKind::Soul, &too_big)
            .expect_err("over the cap is loud");
        assert!(err.to_string().contains("bytes"), "{err}");
        assert!(
            bootstrap
                .load(BootstrapKind::Soul)
                .expect("reads")
                .is_none(),
            "nothing was written"
        );
        let at_cap = "x".repeat(MAX_CONTENT_BYTES);
        bootstrap
            .save(BootstrapKind::User, &at_cap)
            .expect("exactly the cap saves");
    }

    #[test]
    fn environment_refuses_save_everywhere() {
        let (_dir, bootstrap) = store();
        let err = bootstrap
            .save(BootstrapKind::Environment, "machine facts")
            .expect_err("environment is not saveable");
        assert!(
            err.to_string().contains("generated at boot"),
            "the refusal names the reason: {err}"
        );
    }

    #[test]
    fn a_fresh_house_renders_stubs_and_the_onboarding_section() {
        let (_dir, bootstrap) = store();
        let tools = registry(Arc::new(Bootstrap {
            root: bootstrap.root().to_path_buf(),
        }))
        .resolver_for(&rhema_autonomous_agent::Allowlist::permissive())
        .specs();
        let prompt = render_system(
            DEFAULT_SYSTEM_MD,
            "interactive:web",
            Path::new("/ws"),
            &bootstrap,
            &tools,
        )
        .expect("renders");
        assert!(prompt.contains("## SOUL"), "{prompt}");
        assert!(prompt.contains("## USER"), "{prompt}");
        assert!(prompt.contains("## ENVIRONMENT"), "{prompt}");
        assert_eq!(prompt.matches(PENDING).count(), 2, "{prompt}");
        assert!(prompt.contains(GENERATED_AT_BOOT), "{prompt}");
        assert!(prompt.contains("## Onboarding"), "{prompt}");
        assert!(prompt.contains("/ws"), "the workspace named: {prompt}");
        assert!(prompt.contains("bootstrap_save"), "{prompt}");
    }

    #[test]
    fn the_full_house_renders_each_file_and_drops_the_onboarding_section() {
        let (_dir, bootstrap) = store();
        bootstrap
            .save(BootstrapKind::Soul, "I am claw: terse, warm, relentless.")
            .expect("saves");
        bootstrap
            .save(BootstrapKind::User, "The user is Ana; she ships compilers.")
            .expect("saves");
        write_environment(&bootstrap, "Linux, 32 cores, no GPU.");
        let prompt = render_system(
            DEFAULT_SYSTEM_MD,
            "interactive:web",
            Path::new("/ws"),
            &bootstrap,
            &[],
        )
        .expect("renders");
        assert!(
            prompt.contains("I am claw: terse, warm, relentless."),
            "{prompt}"
        );
        assert!(
            prompt.contains("The user is Ana; she ships compilers."),
            "{prompt}"
        );
        assert!(prompt.contains("Linux, 32 cores, no GPU."), "{prompt}");
        assert!(!prompt.contains(PENDING), "{prompt}");
        assert!(!prompt.contains(GENERATED_AT_BOOT), "{prompt}");
        assert!(!prompt.contains("## Onboarding"), "{prompt}");
    }

    #[test]
    fn a_user_file_replaces_its_placeholder_verbatim() {
        let (_dir, bootstrap) = store();
        let content = "the user is Ana.\n\n- braces stay: {{ keep }} and {soul}\n";
        bootstrap.save(BootstrapKind::User, content).expect("saves");
        let prompt = render_system(
            DEFAULT_SYSTEM_MD,
            "interactive:web",
            Path::new("/ws"),
            &bootstrap,
            &[],
        )
        .expect("renders");
        assert!(prompt.contains(content), "{prompt}");
        assert!(
            prompt.contains("{soul}"),
            "a file's own placeholder-looking text is never substituted: {prompt}"
        );
        assert!(
            !prompt.contains("## USER\n\n\n"),
            "the file rides under its header, not appended: {prompt}"
        );
    }

    #[test]
    fn the_tool_specs_render_with_their_schemas() {
        let (_dir, bootstrap) = store();
        let tools = vec![ToolSpec::new(
            "my_tool",
            "its purpose",
            json!({
                "type": "object",
                "properties": { "kind": { "type": "string" } },
                "required": ["kind"]
            }),
        )];
        let prompt = render_system(
            DEFAULT_SYSTEM_MD,
            "interactive:web",
            Path::new("/ws"),
            &bootstrap,
            &tools,
        )
        .expect("renders");
        assert!(prompt.contains("- my_tool | its purpose | "), "{prompt}");
        assert!(
            prompt.contains(r#""required": ["kind"]"#),
            "the JSON schema rendered: {prompt}"
        );
    }

    #[tokio::test]
    async fn the_tool_writes_through_the_store_and_answers_one_line() {
        use rhema_autonomous_agent::Allowlist;
        use rhema_core::{ToolCall, ToolContext};

        let (_dir, bootstrap) = store();
        let resolver = registry(Arc::new(Bootstrap {
            root: bootstrap.root().to_path_buf(),
        }))
        .resolver_for(&Allowlist::permissive());
        let ctx = ToolContext::default();
        let call = |id: &str, args: Value| {
            resolver.resolve(
                ToolCall {
                    id: id.into(),
                    name: BOOTSTRAP_SAVE.into(),
                    args,
                    provider_data: Value::Null,
                },
                &ctx,
            )
        };

        let saved = call(
            "c1",
            json!({"kind": "soul", "content": "I am claw: terse."}),
        )
        .await;
        assert!(!saved.is_error, "{}", saved.content);
        assert_eq!(saved.content, "saved SOUL.md");
        assert_eq!(
            bootstrap.load(BootstrapKind::Soul).expect("reads"),
            Some("I am claw: terse.".to_owned())
        );

        // A kind the schema does not offer still cannot reach the file:
        // the refusal comes back in band, with the reason.
        let environment = call(
            "c2",
            json!({"kind": "environment", "content": "machine facts"}),
        )
        .await;
        assert!(environment.is_error);
        assert!(
            environment.content.contains("generated at boot"),
            "{}",
            environment.content
        );

        let unknown = call("c3", json!({"kind": "persona", "content": "x"})).await;
        assert!(unknown.is_error);
        assert!(
            unknown.content.contains("unknown kind"),
            "{}",
            unknown.content
        );

        let malformed = call("c4", json!({"kind": "soul"})).await;
        assert!(malformed.is_error);
        assert!(
            malformed.content.contains("bad arguments"),
            "{}",
            malformed.content
        );
    }
}
