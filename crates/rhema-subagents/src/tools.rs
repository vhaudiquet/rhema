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
//! whole reply budget away). Its history lives and dies with the call,
//! and its requests read that history through the bounded transcript
//! view (`rhema_transcript::attach`), the same bound the main session
//! runs under — below the transcript's budget a history passes through
//! unchanged, past it the old tail folds into the compaction block.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;

use rhema_agent_tools::{Overlay, ShellTools, render_template};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, ModelProvider, Role, Session, ToolCall,
    ToolContext, ToolProgress, ToolResolver, ToolResult, ToolSpec,
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
        progress: &mut dyn ToolProgress,
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
        // Each call wraps the provider in its own fresh transcript: the
        // inner history lives and dies with the call, so there is no
        // mirror to share and no per-definition settings to plumb. The
        // `zoom` half of `attach` is dropped on purpose — the
        // subagent's tool surface stays the shell alone.
        let (provider, _zoom) = rhema_transcript::attach(
            self.provider.clone(),
            rhema_transcript::TranscriptSettings::default(),
        );
        let mut agent = Agent::builder()
            .provider(provider)
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
        let mut sink = Forwarder {
            progress,
            model: def
                .model
                .clone()
                .unwrap_or_else(|| self.default_model.clone()),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
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
        self.resolve_streaming(call, ctx, &mut rhema_core::OutputOnly(|_| {}))
            .await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut dyn ToolProgress,
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

/// Renders the inner turn's lifecycle into the call's own status: the
/// phase the subagent is in, the context its current model call
/// carries, and what its model calls have spent so far. The inner
/// turn's text and tool output go nowhere — the subagent is
/// its own line on the parent's screen, and its report is the result;
/// the lifecycle events themselves stay private, the status is
/// screen-facing, never history. A nested subagent's status is dropped:
/// to this call the nested subagent is just one running tool.
struct Forwarder<'a> {
    progress: &'a mut dyn ToolProgress,
    /// The model id the inner turn runs on, stamped into every status.
    model: String,
    /// The phase last reported; only transitions are sent.
    phase: Option<&'static str>,
    /// The current model call's usage: the latest running tally, held
    /// past [`Event::StepEnd`](rhema_core::Event::StepEnd) until the
    /// next call's first frame replaces it — the reply rides into the
    /// next request, so input + output stays the right current-context
    /// figure between steps.
    context: Option<rhema_core::Usage>,
    /// Cumulative spend across the call's steps. Only a `StepEnd` adds
    /// into it: the running frames already cover their step's final
    /// tally, and summing them would double-count.
    spent: rhema_core::Usage,
}

/// The phase words a status line reads, in the order the inner turn
/// moves through them.
const THINKING: &str = "thinking";
const RUNNING: &str = "running";
const WRITING: &str = "writing output";

impl Forwarder<'_> {
    /// Reports a phase transition with the spend so far; a repeated
    /// phase sends nothing.
    fn report(&mut self, phase: &'static str) {
        if self.phase == Some(phase) {
            return;
        }
        self.phase = Some(phase);
        self.send();
    }

    /// Sends the current phase, context, and spend.
    fn send(&mut self) {
        self.progress.status(rhema_core::ToolStatus {
            phase: self.phase.unwrap_or(THINKING).to_owned(),
            context: self.context,
            spent: Some(self.spent),
            model: Some(self.model.clone()),
        });
    }
}

impl ToolProgress for Forwarder<'_> {
    fn output(&mut self, _chunk: &str) {}

    fn status(&mut self, _status: rhema_core::ToolStatus) {}
}

impl EventSink for Forwarder<'_> {
    fn emit(&mut self, event: Event<'_>) {
        match event {
            // The model is streaming: reasoning is thinking, text is the
            // report being written.
            Event::StepStart { .. } | Event::ThinkingDelta(_) => self.report(THINKING),
            Event::TextDelta(_) => self.report(WRITING),
            // A tool of the subagent's is executing.
            Event::ToolCallStart { .. } => self.report(RUNNING),
            Event::ToolResult { .. } => self.report(THINKING),
            Event::StepEnd { usage, .. } => {
                self.spent.input_tokens =
                    self.spent.input_tokens.saturating_add(usage.input_tokens);
                self.spent.output_tokens =
                    self.spent.output_tokens.saturating_add(usage.output_tokens);
                self.spent.cached_input_tokens = self
                    .spent
                    .cached_input_tokens
                    .saturating_add(usage.cached_input_tokens);
                self.context = Some(usage);
                self.send();
            }
            // A nested subagent's status stays private to it, argument
            // streaming is not a phase of its own, and the inner
            // output's lines ride no channel. A running tally replaces
            // the context — it is the current call's figure, never a
            // spend to accumulate; the accounting still rides `StepEnd`.
            Event::ToolStatus { .. }
            | Event::ToolCallArgsDelta { .. }
            | Event::ToolOutputDelta { .. } => {}
            Event::Usage(usage) => {
                if self.context.as_ref() != Some(&usage) {
                    self.context = Some(usage);
                    self.send();
                }
            }
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

    /// A progress channel that keeps everything it is handed.
    struct Collector {
        lines: Vec<String>,
        statuses: Vec<rhema_core::ToolStatus>,
    }

    impl ToolProgress for Collector {
        fn output(&mut self, chunk: &str) {
            self.lines.push(chunk.to_owned());
        }
        fn status(&mut self, status: rhema_core::ToolStatus) {
            self.statuses.push(status);
        }
    }

    #[test]
    fn the_inner_lifecycle_becomes_phase_transitions_and_a_spend() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "parent-model".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        // Reasoning first, then a tool, its output, the step's spend,
        // then the report being written.
        sink.emit(Event::ThinkingDelta("hmm"));
        sink.emit(Event::ToolCallStart {
            id: "c1",
            name: "bash",
        });
        sink.emit(Event::StepEnd {
            usage: rhema_core::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cached_input_tokens: 80,
            },
            stop_reason: &rhema_core::StopReason::ToolUse,
        });
        sink.emit(Event::TextDelta("the report"));
        drop(sink);

        // The phases arrived as transitions; the step's spend re-sent
        // the running phase with the figures: thinking → running (the
        // tool) → running again (the spend) → writing output (the
        // report).
        let phases: Vec<&str> = collected
            .statuses
            .iter()
            .map(|status| status.phase.as_str())
            .collect();
        assert_eq!(phases, vec![THINKING, RUNNING, RUNNING, WRITING]);
        let last = collected.statuses.last().unwrap();
        assert_eq!(last.model.as_deref(), Some("parent-model"));
        // The step's call is the current context...
        let context = last.context.unwrap();
        assert_eq!(context.input_tokens, 100);
        assert_eq!(context.output_tokens, 20);
        assert_eq!(context.cached_input_tokens, 80);
        // ...and the only spend so far.
        let spent = last.spent.unwrap();
        assert_eq!(spent.input_tokens, 100);
        assert_eq!(spent.output_tokens, 20);
        assert_eq!(spent.cached_input_tokens, 80);

        // The inner output rides no channel: the subagent is its own
        // line, its report the result.
        assert!(collected.lines.is_empty());
    }

    #[test]
    fn repeated_phases_send_nothing() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "m".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        sink.emit(Event::ThinkingDelta("a"));
        sink.emit(Event::ThinkingDelta("b"));
        assert_eq!(collected.statuses.len(), 1);
    }

    /// A running tally is the current call's context, never a spend: a
    /// mid-call frame replaces the context without touching the spend,
    /// and only the step's end bills into the spend. The context holds
    /// past a `StepEnd` — the reply rides into the next request — until
    /// the next call's first frame replaces it.
    #[test]
    fn a_running_tally_is_context_and_only_a_step_end_is_spend() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "m".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        sink.emit(Event::ThinkingDelta("hmm"));
        sink.emit(Event::Usage(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 10,
            cached_input_tokens: 0,
        }));
        sink.emit(Event::StepEnd {
            usage: rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            },
            stop_reason: &rhema_core::StopReason::ToolUse,
        });
        sink.emit(Event::ToolCallStart {
            id: "c2",
            name: "bash",
        });
        sink.emit(Event::Usage(rhema_core::Usage {
            input_tokens: 41_000,
            output_tokens: 20,
            cached_input_tokens: 40_000,
        }));
        drop(sink);

        // The thinking status carried the running frame as its context
        // and nothing billed yet.
        let thinking = &collected.statuses[1];
        assert_eq!(thinking.phase, THINKING);
        assert_eq!(
            thinking.context,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 10,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(
            thinking.spent,
            Some(rhema_core::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
            })
        );

        // The step's end billed the spend and held the context; the
        // running status kept both...
        let running = &collected.statuses[3];
        assert_eq!(running.phase, RUNNING);
        assert_eq!(
            running.context,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(
            running.spent,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );

        // ...and the next call's frame replaced the context, the spend
        // still counting every step.
        let next = collected.statuses.last().unwrap();
        assert_eq!(
            next.context,
            Some(rhema_core::Usage {
                input_tokens: 41_000,
                output_tokens: 20,
                cached_input_tokens: 40_000,
            })
        );
        assert_eq!(
            next.spent,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );
    }
}
