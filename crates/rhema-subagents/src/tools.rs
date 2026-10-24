//! The `subagent` tool: one resolver, one tool, one inner turn per call.
//!
//! [`SubagentTools`] advertises `subagent(name, prompt)`. A call builds a
//! fresh [`Agent`](rhema_core::Agent) over the named
//! [`SubagentDef`]'s body as system prompt, runs a bounded step loop on a
//! fresh [`Session`](rhema_core::Session) whose first message is the
//! `prompt`, and returns the final assistant text as the result. The
//! inner turn inherits four things from the parent turn and nothing
//! else: the provider, the approval hook (read off the parent's
//! [`ShellTools`] per call, so the policy travels with the turn), the
//! cancel flag, and the thinking effort (a subagent reasons the way the
//! agent that delegated it reasons — a provider default can think the
//! whole reply budget away). Its history lives and dies with the call.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;

use rhema_agent_tools::{Overlay, ShellTools, render_template};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, ModelProvider, Role, Session, ToolCall,
    ToolContext, ToolResolver, ToolResult, ToolSpec,
};

use crate::def::SubagentDef;

/// The report is cut at this size — in bytes and in char boundaries,
/// whichever is hit first — keeping the tail: the conclusion of a report
/// sits at its end, as an error sits at the end of a command's output.
const REPORT_LIMIT: usize = 50 * 1024;

/// Runs subagents behind the `subagent` tool.
pub struct SubagentTools {
    /// The provider the inner agents run on; the parent's.
    provider: Arc<dyn ModelProvider>,
    /// The discovered definitions; the tool's catalog.
    defs: Vec<SubagentDef>,
    /// The parent's shell. Two jobs: the workspace the subagent's tools
    /// run in, and the current turn's approval hook the inner shell
    /// inherits.
    shell: Arc<ShellTools>,
    /// The model identifier a definition without its own `model` runs
    /// on: the parent's.
    default_model: String,
    /// The thinking effort the inner requests carry: the parent turn's,
    /// set by [`Self::with_thinking`]. The default,
    /// [`ThinkingEffort::Default`], lets the provider decide — and on a
    /// thinking-by-default backend with a bounded reply budget, that
    /// decision can spend the whole budget on reasoning and leave the
    /// report empty. The parent's posture is the safe inheritance.
    thinking: ThinkingEffort,
    /// How many nesting levels remain. One is the default: a subagent
    /// gets no `subagent` tool of its own. Zero advertises nothing.
    max_depth: u32,
}

impl SubagentTools {
    /// A resolver over `defs`, running on the parent's `provider` and
    /// `shell`, defaulting to one level of nesting (none).
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        defs: Vec<SubagentDef>,
        shell: Arc<ShellTools>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            defs,
            shell,
            default_model: default_model.into(),
            thinking: ThinkingEffort::Default,
            max_depth: 1,
        }
    }

    /// Sets the thinking effort the inner requests carry; the caller
    /// passes the parent turn's, so a subagent samples the way the
    /// agent that delegated it samples.
    pub fn with_thinking(mut self, thinking: ThinkingEffort) -> Self {
        self.thinking = thinking;
        self
    }

    /// Sets the remaining nesting budget; each nested level gets one
    /// less, and a subagent at zero is advertised no `subagent` tool.
    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// The `subagent` tool spec over `defs`: the catalog rides the
    /// description, so the prompt's tool list teaches the model when to
    /// delegate with no renderer change.
    pub fn spec(defs: &[SubagentDef]) -> ToolSpec {
        let mut description = String::from(
            "Run a named subagent: an independent agent with its own \
             conversation, system prompt, and workspace tools, which works \
             the task and returns its final report as the tool result. The \
             subagent does not see this conversation and its report is all \
             you get back, so the prompt must carry everything it needs. \
             Available subagents:",
        );
        for def in defs {
            description.push_str(&format!("\n- {}: {}", def.name, def.description));
        }
        ToolSpec::new(
            "subagent",
            description,
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The subagent to run, from the catalog in this tool's description."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The full task for the subagent: what to do, and what to report back."
                    }
                },
                "required": ["name", "prompt"]
            }),
        )
    }

    /// The inner shell for one call: fresh, with the parent turn's
    /// approval hook installed. No hook installed means the inner tools
    /// reject — the fail-safe default, never the case mid-turn.
    fn inner_shell(&self) -> Arc<ShellTools> {
        let shell = Arc::new(ShellTools::with_executor(
            self.shell.workspace().to_path_buf(),
            self.shell.executor(),
        ));
        if let Some(hook) = self.shell.current_approval() {
            shell.install_approval(hook);
        }
        shell
    }

    /// The inner resolver over the fresh shell: the shell alone, plus a
    /// nested `SubagentTools` when the nesting budget allows one.
    fn inner_resolver(&self, inner_shell: &Arc<ShellTools>) -> Arc<dyn ToolResolver> {
        if self.max_depth <= 1 {
            return inner_shell.clone();
        }
        let nested = Self::new(
            self.provider.clone(),
            self.defs.clone(),
            inner_shell.clone(),
            self.default_model.clone(),
        )
        .with_max_depth(self.max_depth - 1);
        Arc::new(Overlay::new(inner_shell.clone(), Arc::new(nested)))
    }

    /// Runs one definition to its report, or to the reason it failed.
    async fn run(
        &self,
        def: &SubagentDef,
        prompt: &str,
        ctx: &ToolContext,
        inner_shell: &Arc<ShellTools>,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String, String> {
        let system = render_template(
            &def.body,
            &json!({
                "workspace": inner_shell.workspace().display().to_string(),
                "name": def.name,
            }),
        )
        .map_err(|err| format!("its system prompt template failed: {err}"))?;
        let resolver = self.inner_resolver(inner_shell);
        let mut agent = Agent::builder()
            .provider(self.provider.clone())
            .resolver(resolver)
            .system(system)
            .params(ChatParams {
                thinking: self.thinking,
                ..ChatParams::new(
                    def.model
                        .clone()
                        .unwrap_or_else(|| self.default_model.clone()),
                )
            })
            .build()
            .map_err(|err| format!("its engine failed to build: {err}"))?;

        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        let inner_ctx = ToolContext {
            cwd: inner_shell.workspace().to_path_buf(),
            run_id: format!("{}/subagent:{}", ctx.run_id, def.name),
            cancelled: ctx.cancelled.clone(),
        };
        let mut sink = LineForwarder {
            buf: String::new(),
            progress,
        };

        for _ in 0..def.max_steps() {
            if ctx.cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".to_owned());
            }
            let step = agent
                .step(&mut session, &mut sink, &inner_ctx)
                .await
                .map_err(|err| err.to_string())?;
            if step.output.stop_reason != rhema_core::StopReason::ToolUse {
                let report: String = step
                    .output
                    .message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if report.trim().is_empty() {
                    return Err("finished without a report".to_owned());
                }
                return Ok(capped(report));
            }
        }
        Err(format!("hit its step budget of {}", def.max_steps()))
    }

    /// Validates the call's arguments into the definition and prompt to
    /// run, or the failure result to return instead.
    fn parse_call<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> Result<(&'a SubagentDef, &'a str), ToolResult> {
        let invalid = |why: String| ToolResult::failure(&call.id, why);
        let name = call
            .args
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("the subagent tool needs a string `name`".to_owned()))?;
        let prompt = call
            .args
            .get("prompt")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("the subagent tool needs a string `prompt`".to_owned()))?;
        let def = self
            .defs
            .iter()
            .find(|def| def.name == name)
            .ok_or_else(|| {
                invalid(format!(
                    "no subagent named {name}; available: {}",
                    self.defs
                        .iter()
                        .map(|def| def.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        if prompt.trim().is_empty() {
            return Err(invalid("the `prompt` argument is empty".to_owned()));
        }
        Ok((def, prompt))
    }
}

#[async_trait]
impl ToolResolver for SubagentTools {
    fn specs(&self) -> Vec<ToolSpec> {
        if self.max_depth == 0 || self.defs.is_empty() {
            return Vec::new();
        }
        vec![Self::spec(&self.defs)]
    }

    fn replay_safety(&self, name: &str) -> rhema_core::ReplaySafety {
        let _ = name;
        // A subagent can side-effect through its shell: never re-run one
        // that lost its result to a crash.
        rhema_core::ReplaySafety::Unsafe
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        self.resolve_streaming(call, ctx, &mut |_| {}).await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        let id = call.id.clone();
        let (def, prompt) = match self.parse_call(&call) {
            Ok(parsed) => parsed,
            Err(result) => return result,
        };
        let inner_shell = self.inner_shell();
        let outcome = self.run(def, prompt, ctx, &inner_shell, progress).await;
        inner_shell.clear_approval();
        match outcome {
            Ok(report) => ToolResult::success(id, report),
            Err(why) => ToolResult::failure(id, format!("subagent {}: {why}", def.name)),
        }
    }
}

/// Forwards the inner turn's text and tool output to the parent's
/// progress callback, whole lines at a time — the same shape the shell
/// streams in. Lifecycle events are dropped: the parent's sink already
/// renders its own call's lifecycle, and the subagent's is private.
struct LineForwarder<'a> {
    /// Fragments accumulated since the last newline.
    buf: String,
    progress: &'a mut (dyn for<'b> FnMut(&'b str) + Send),
}

impl EventSink for LineForwarder<'_> {
    fn emit(&mut self, event: Event<'_>) {
        let chunk = match event {
            Event::TextDelta(text) | Event::ToolOutputDelta { chunk: text, .. } => text,
            _ => return,
        };
        self.buf.push_str(chunk);
        while let Some(end) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=end).collect();
            let line = line.trim_end_matches('\n');
            if line.is_empty() {
                continue;
            }
            (self.progress)(&format!("{line}\n"));
        }
    }
}

/// Caps a report to [`REPORT_LIMIT`], keeping the tail.
fn capped(report: String) -> String {
    if report.len() <= REPORT_LIMIT {
        return report;
    }
    let mut start = report.len() - REPORT_LIMIT;
    while !report.is_char_boundary(start) {
        start += 1;
    }
    format!("[...truncated...]\n{}", &report[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_oversized_report_keeps_its_tail() {
        let head = "h".repeat(200);
        let tail = "t".repeat(REPORT_LIMIT);
        let out = capped(format!("{head}{tail}"));
        assert!(out.starts_with("[...truncated...]\n"));
        assert!(out.ends_with(&tail));
        assert!(out.len() < head.len() + tail.len());
    }

    #[test]
    fn a_within_limit_report_passes_through() {
        assert_eq!(capped("short".to_owned()), "short");
    }

    #[test]
    fn the_spec_carries_the_catalog() {
        let defs = vec![
            SubagentDef {
                name: "reviewer".into(),
                description: "reviews diffs".into(),
                model: None,
                max_steps: None,
                body: "Review.".into(),
            },
            SubagentDef {
                name: "scout".into(),
                description: "explores the tree".into(),
                model: None,
                max_steps: None,
                body: "Explore.".into(),
            },
        ];
        let spec = SubagentTools::spec(&defs);
        assert_eq!(spec.name, "subagent");
        assert!(spec.description.contains("reviewer: reviews diffs"));
        assert!(spec.description.contains("scout: explores the tree"));
    }
}
