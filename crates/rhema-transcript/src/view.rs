//! The request view: the provider decorator that rewrites
//! `req.messages` under a byte budget, and the mirror `zoom` reads.
//!
//! The decorator sits outside everything else on the request path, as
//! paste expansion does: the engine clones the session into the request
//! untouched, the decorator projects it, the session keeps every
//! message (SPEC.md §6). Requests carry the full history, so the
//! decorator's mirror is exactly the log as of the last request — a
//! resumed session rebuilds it wholesale on the first call.
//!
//! Past the budget the aged prefix is covered by the stepped
//! compaction block ([`crate::compact`]): sections tiling `[0, cut)`,
//! appended only at flushes so the request prefix stays cache-stable
//! between them. This module owns the cut and the sections; the
//! summarizer seam and the section writers live in
//! [`crate::compact`].

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use rhema_core::{
    Content, EventSink, Message, ModelOutput, ModelProvider, ProviderError, Role, ToolSpec, Usage,
};

use crate::compact::{self, PendingSummary, Section, SectionOrigin, Summarizer};
use crate::summary;
use crate::tree::Range;

/// How many section fills the background worker runs at once. A flush
/// can chunk a whole allowance into a dozen sections; the cap keeps
/// one worker provider from seeing a stampede.
const WORKER_CONCURRENCY: usize = 4;

/// The view's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSettings {
    /// The byte budget a request's messages aim for. The transcript
    /// engages only past it — below, requests pass through
    /// byte-identical — and the current turn may exceed it: the tail is
    /// never cut.
    pub max_view_bytes: usize,
    /// The bytes one section's messages aim for: the summarizer
    /// worker's input bound, and the granularity compaction works at.
    /// A section carries at least one message whatever this says.
    pub step_bytes: usize,
    /// The messages one section covers at most. Bounds the extractive
    /// stand-in's lossiness: each section's digest is a few hundred
    /// bytes, so this caps how much history one line may paper over
    /// when no model fill is configured.
    pub max_section_messages: usize,
    /// The bytes the committed block may reach before the oldest
    /// sections consolidate into one. Consolidation rewrites the
    /// block — a full cache break — so it fires only past this cap,
    /// and merges down to half of it before stopping.
    pub max_block_bytes: usize,
}

impl Default for TranscriptSettings {
    fn default() -> Self {
        Self {
            // Tool-heavy sessions (build logs, file dumps in results)
            // run oversized tails constantly; 256 KB keeps the
            // allowance — and with it the stretch between flushes —
            // worth the name at that workload.
            max_view_bytes: 256_000,
            step_bytes: 24_000,
            max_section_messages: 128,
            max_block_bytes: 32_000,
        }
    }
}

/// The session's token usage, split by what it bought. The five input
/// categories share each request's reported input tokens in proportion
/// to their wire bytes (tool *definitions* ride with the calls that
/// use them); the model's generated output tokens all land on
/// `model_output_tokens`, which also carries the assistant text the
/// history re-sends. The compaction block travels as a user message
/// and counts there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageBreakdown {
    /// The system prompt minus its skills section.
    pub system_prompt_tokens: u64,
    /// The skills section of the system prompt, when the agent could
    /// isolate it at render time; zero otherwise.
    pub skills_tokens: u64,
    /// Tool definitions plus the tool-call arguments re-sent in
    /// assistant messages.
    pub tool_calls_input_tokens: u64,
    /// Tool results — the tools' output, re-sent as input.
    pub tool_calls_output_tokens: u64,
    /// User text, the compaction block included.
    pub user_messages_tokens: u64,
    /// The model's words: assistant text in the history plus every
    /// generated output token.
    pub model_output_tokens: u64,
}

impl UsageBreakdown {
    /// The six categories, labeled as the Usage page spells them.
    pub fn labeled(&self) -> [(&'static str, u64); 6] {
        [
            ("system prompt", self.system_prompt_tokens),
            ("skills", self.skills_tokens),
            ("tool calls input", self.tool_calls_input_tokens),
            ("tool calls output", self.tool_calls_output_tokens),
            ("user messages", self.user_messages_tokens),
            ("model output", self.model_output_tokens),
        ]
    }

    /// The grand total the percentages read against.
    pub fn total(&self) -> u64 {
        self.system_prompt_tokens
            + self.skills_tokens
            + self.tool_calls_input_tokens
            + self.tool_calls_output_tokens
            + self.user_messages_tokens
            + self.model_output_tokens
    }
}

/// The log's own wire bytes by category — each message counted once,
/// the content the billed split re-sends once per request. The system
/// prompt, the skills section, and the tool definitions ride every
/// request but live in no message, so they have no figure here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogContent {
    /// Tool-call arguments in assistant messages.
    pub tool_calls: usize,
    /// Tool results in tool messages.
    pub tool_results: usize,
    /// User text, the compaction block included.
    pub user_text: usize,
    /// Assistant text.
    pub assistant_text: usize,
}

/// The compaction's benchmark snapshot: what the view has been doing,
/// and what the providers' own usage says about the prefix cache. All
/// counters run from the transcript's construction; a resumed session
/// keeps its run continuous by folding the saved counters in with
/// [`TranscriptStats::carry`]. The struct is the saved shape: the
/// frontend serializes it into the session file verbatim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptStats {
    /// Requests the decorator forwarded.
    pub requests: u64,
    /// Requests below the budget, sent byte-identical: the whole
    /// prefix a cache holds.
    pub pass_through: u64,
    /// Requests carrying the compaction block plus the whole band.
    pub viewed: u64,
    /// Flushes: cut movements, the only moments the view's prefix
    /// breaks.
    pub flushes: u64,
    /// Consolidations: block rewrites past the block cap, each one a
    /// full cache break.
    pub consolidations: u64,
    /// Summarizer calls issued, successes and failures together.
    pub summarizer_calls: u64,
    /// Summarizer calls that failed; each left the extractive
    /// stand-in in place.
    pub summarizer_failures: u64,
    /// The mirror's message count at the last request.
    pub log_messages: usize,
    /// Messages under the block at the last request: the cut.
    pub sectioned_messages: usize,
    /// The block's section count at the last request.
    pub sections: usize,
    /// The block's bytes at the last request.
    pub block_bytes: usize,
    /// The bytes carried whole from the cut to the log's end, at the
    /// last request.
    pub band_bytes: usize,
    /// The current turn's bytes at the last request.
    pub tail_bytes: usize,
    /// The log's wire bytes at the last request — what the
    /// conversation actually holds, each message counted once. The
    /// usage split below sums the same content once per request;
    /// this is the once.
    #[serde(default)]
    pub log_bytes: usize,
    /// The log's bytes by category at the last request, counted once.
    #[serde(default)]
    pub log_content: LogContent,
    /// Input tokens the providers reported, summed over requests —
    /// cached tokens included.
    pub input_tokens: u64,
    /// Of those, the tokens the provider served from its prompt cache.
    pub cached_input_tokens: u64,
    /// Output tokens the providers reported, summed over requests.
    pub output_tokens: u64,
    /// The session's usage split by category.
    pub usage: UsageBreakdown,
}

impl TranscriptStats {
    /// The session's prompt-cache hit rate: cached input over all
    /// input. `None` before any usage was reported. The providers'
    /// own numbers — the ground truth the stepped design optimizes.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        if self.input_tokens == 0 {
            None
        } else {
            Some(self.cached_input_tokens as f64 / self.input_tokens as f64)
        }
    }

    /// Folds a saved session's counters in, so a resumed session's
    /// stats read as one run with the pre-resume requests included:
    /// the run counters and the usage split sum, the cache rate keeps
    /// covering everything. The at-last-request snapshots describe
    /// the mirror that measured them, so they stay this side's —
    /// except while this mirror has not looked at a log yet
    /// (`log_messages == 0`), where the snapshot is what a Usage page
    /// opened before the first request would otherwise render as
    /// zeros, and the saved one is the honest figure.
    pub fn carry(&mut self, saved: &Self) {
        self.requests += saved.requests;
        self.pass_through += saved.pass_through;
        self.viewed += saved.viewed;
        self.flushes += saved.flushes;
        self.consolidations += saved.consolidations;
        self.summarizer_calls += saved.summarizer_calls;
        self.summarizer_failures += saved.summarizer_failures;
        self.input_tokens += saved.input_tokens;
        self.cached_input_tokens += saved.cached_input_tokens;
        self.output_tokens += saved.output_tokens;
        let usage = &mut self.usage;
        let saved_usage = &saved.usage;
        usage.system_prompt_tokens += saved_usage.system_prompt_tokens;
        usage.skills_tokens += saved_usage.skills_tokens;
        usage.tool_calls_input_tokens += saved_usage.tool_calls_input_tokens;
        usage.tool_calls_output_tokens += saved_usage.tool_calls_output_tokens;
        usage.user_messages_tokens += saved_usage.user_messages_tokens;
        usage.model_output_tokens += saved_usage.model_output_tokens;
        if self.log_messages == 0 {
            self.log_messages = saved.log_messages;
            self.sectioned_messages = saved.sectioned_messages;
            self.sections = saved.sections;
            self.block_bytes = saved.block_bytes;
            self.band_bytes = saved.band_bytes;
            self.tail_bytes = saved.tail_bytes;
            self.log_bytes = saved.log_bytes;
            self.log_content = saved.log_content;
        }
    }
}

/// How one summarizer call ended, as [`Transcript::record_worker_call`]
/// counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    /// The worker answered; the section's stand-in was upgraded.
    Upgraded,
    /// The worker failed; the extractive stand-in stays in place.
    Failed,
}

/// The mirror: the log as the last request carried it, the stepped
/// compaction state over it, and the benchmark counters. Shared
/// between the decorator (which refreshes it) and the zoom tool (which
/// reads the log).
#[derive(Debug)]
pub struct Transcript {
    settings: TranscriptSettings,
    log: Vec<Message>,
    /// Per-message wire bytes, parallel to `log`: the budget's unit,
    /// counted once per message instead of per request.
    sizes: Vec<usize>,
    /// [`Vec`]`sizes` summed once, maintained by `observe` and read by
    /// the planner and the counters. The whole-log walk this replaces
    /// re-serialized every tool call's arguments on every request.
    total_bytes: usize,
    /// The log's bytes by category, maintained by `observe` the same
    /// way; `render` snapshots it into `stats.log_content`.
    content: LogContent,
    /// First message not under the block: `[0, cut)` is sectioned,
    /// `[cut, len)` travels whole. Moves only at flushes.
    cut: usize,
    /// The committed block: sections tiling `[0, cut)`, oldest first,
    /// append-only between flushes.
    sections: Vec<Section>,
    /// The skills section's byte share of the system prompt, measured
    /// at render time by the agent that assembled it. Zero until set.
    skills_bytes: usize,
    /// The wire bytes the pending response's input tokens spread over:
    /// staged from the request before it goes out, consumed by
    /// [`Transcript::record_usage`].
    pending_shares: Shares,
    stats: TranscriptStats,
    /// A previous session's sections, handed over by the session save
    /// and waiting for the resumed log. `observe` adopts them on the
    /// resume's replace when they tile a prefix of the rebuilt log,
    /// and drops them for anything else.
    saved: Option<Vec<Section>>,
}

/// One request's wire bytes by category — the proportions the
/// reported input tokens are spread over.
#[derive(Debug, Clone, Copy, Default)]
struct Shares {
    system_prompt: usize,
    skills: usize,
    tool_defs: usize,
    tool_calls: usize,
    tool_results: usize,
    user_text: usize,
    assistant_text: usize,
}

/// Adds one message's bytes by category into `content` — the
/// per-message form of the split the counters and the usage shares
/// both read. The same taxonomy the request shares spread the billed
/// input over, minus the pieces that ride no message.
fn add_message_content(content: &mut LogContent, message: &Message) {
    for part in &message.content {
        match part {
            Content::Text(text) => match message.role {
                Role::User => content.user_text += text.len(),
                Role::Assistant => content.assistant_text += text.len(),
                Role::System | Role::Tool => {}
            },
            Content::ToolCall(call) => {
                content.tool_calls += call.name.len() + call.args.to_string().len();
            }
            Content::ToolResult(result) => {
                content.tool_results += result.content.len();
            }
            Content::Image(_) => {}
        }
    }
}

impl Shares {
    fn total(&self) -> usize {
        self.system_prompt
            + self.skills
            + self.tool_defs
            + self.tool_calls
            + self.tool_results
            + self.user_text
            + self.assistant_text
    }
}

impl Transcript {
    /// A mirror with no history yet; the first request fills it.
    pub fn new(settings: TranscriptSettings) -> Self {
        Self {
            settings,
            log: Vec::new(),
            sizes: Vec::new(),
            total_bytes: 0,
            content: LogContent::default(),
            cut: 0,
            sections: Vec::new(),
            skills_bytes: 0,
            pending_shares: Shares::default(),
            stats: TranscriptStats::default(),
            saved: None,
        }
    }

    /// Folds a request's messages into the log: an extension of what is
    /// already here extends it; anything else — a fresh session, a
    /// resume — replaces it, and the compaction state rebuilds from
    /// nothing (the first request re-derives it; nothing derived is
    /// ever persisted).
    pub fn observe(&mut self, messages: &[Message]) {
        if messages.len() >= self.log.len() && messages[..self.log.len()] == self.log[..] {
            for message in &messages[self.log.len()..] {
                let bytes = summary::message_bytes(message);
                self.sizes.push(bytes);
                self.total_bytes += bytes;
                add_message_content(&mut self.content, message);
            }
            self.log.extend_from_slice(&messages[self.log.len()..]);
        } else {
            self.log = messages.to_vec();
            self.sizes = Vec::with_capacity(self.log.len());
            self.total_bytes = 0;
            self.content = LogContent::default();
            for message in &self.log {
                let bytes = summary::message_bytes(message);
                self.sizes.push(bytes);
                self.total_bytes += bytes;
                add_message_content(&mut self.content, message);
            }
            self.cut = 0;
            self.sections.clear();
        }
        // The saved sections are adopted only when they tile a prefix
        // of the log this mirror now holds: the resume then rides the
        // compaction the previous session already paid for, dense
        // from the first request. Anything else — a truncated log, an
        // edited session — falls back to the rebuild. A fresh
        // mirror's first observe lands here too: its log was empty,
        // so the compare above took the extension branch.
        if let Some(saved) = self.saved.take() {
            let mut end = 0usize;
            let tiles = !saved.is_empty()
                && saved.iter().all(|section| {
                    let contiguous = section.range.id == end;
                    end = section.range.id + section.range.n;
                    contiguous
                })
                && end <= self.log.len();
            if tiles {
                self.sections = saved;
                self.cut = end;
            }
        }
    }

    /// How many messages the log holds.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Message `id`, as `zoom(id, 1)` reads it.
    pub fn message(&self, id: usize) -> Option<&Message> {
        self.log.get(id)
    }

    /// The messages of `range`, as `zoom` reads them; `None` past the
    /// log's end.
    pub fn range(&self, range: Range) -> Option<&[Message]> {
        self.log.get(range.id..range.id + range.n)
    }

    /// The compaction's benchmark snapshot.
    pub fn stats(&self) -> TranscriptStats {
        self.stats
    }

    /// Folds a saved session's benchmark counters in: a resumed
    /// session's stats keep reading as one run. See
    /// [`TranscriptStats::carry`] for what sums and what does not.
    pub fn carry(&mut self, saved: &TranscriptStats) {
        self.stats.carry(saved);
    }

    /// The committed sections, oldest first: what a session save
    /// carries so the next session skips the re-fill.
    pub fn saved_sections(&self) -> Vec<Section> {
        self.sections.clone()
    }

    /// Hands a previous session's sections over. They are not adopted
    /// on the spot: the resumed log is unknown until the first
    /// request, and [`Transcript::observe`] validates them against it.
    pub fn adopt_sections(&mut self, sections: Vec<Section>) {
        self.saved = Some(sections);
    }

    /// Zeroes the benchmark counters; the log and the compaction state
    /// stay — the next request rebuilds both anyway. A conversation
    /// cleared mid-agent starts its accounting over.
    pub fn reset_stats(&mut self) {
        self.stats = TranscriptStats::default();
    }

    /// Sets the skills section's byte share of the system prompt —
    /// measured by the agent that rendered it, since the section's
    /// shape is the template's to decide. Zero (the default) folds the
    /// whole system prompt into `system_prompt_tokens`.
    pub fn set_skills_bytes(&mut self, bytes: usize) {
        self.skills_bytes = bytes;
    }

    /// Stages the byte shares of the request about to go out: what the
    /// next [`Transcript::record_usage`] spreads the reported input
    /// tokens over. The decorator calls it after the view is applied,
    /// so the shares read what is actually sent.
    pub fn stage_usage_shares(
        &mut self,
        system: Option<&str>,
        tools: &[ToolSpec],
        messages: &[Message],
    ) {
        let mut shares = Shares::default();
        if let Some(system) = system {
            let skills = self.skills_bytes.min(system.len());
            shares.skills = skills;
            shares.system_prompt = system.len() - skills;
        }
        // The tool definitions ride the request's tools array; they
        // are input every step, and they share the calls' row.
        for spec in tools {
            shares.tool_defs +=
                spec.name.len() + spec.description.len() + spec.parameters.to_string().len();
        }
        for message in messages {
            for content in &message.content {
                match content {
                    Content::Text(text) => match message.role {
                        Role::User => shares.user_text += text.len(),
                        Role::Assistant => shares.assistant_text += text.len(),
                        Role::System | Role::Tool => {}
                    },
                    Content::ToolCall(call) => {
                        shares.tool_calls += call.name.len() + call.args.to_string().len();
                    }
                    Content::ToolResult(result) => {
                        shares.tool_results += result.content.len();
                    }
                    // Shares spreads tokens over the categories it names;
                    // image bytes are the message budgets' to count.
                    Content::Image(_) => {}
                }
            }
        }
        self.pending_shares = shares;
    }

    /// Records one provider response's usage: the ground truth the
    /// cache-hit rate reports from. The decorator calls this once per
    /// forwarded request; the reported input tokens spread over the
    /// staged shares by byte proportion.
    pub fn record_usage(&mut self, usage: &Usage) {
        self.stats.input_tokens += u64::from(usage.input_tokens);
        self.stats.cached_input_tokens += u64::from(usage.cached_input_tokens);
        self.stats.output_tokens += u64::from(usage.output_tokens);
        let shares = std::mem::take(&mut self.pending_shares);
        let total = shares.total();
        let input = u64::from(usage.input_tokens);
        if total > 0 && input > 0 {
            let spread = |bytes: usize| input * bytes as u64 / total as u64;
            let breakdown = &mut self.stats.usage;
            breakdown.system_prompt_tokens += spread(shares.system_prompt);
            breakdown.skills_tokens += spread(shares.skills);
            breakdown.tool_calls_input_tokens += spread(shares.tool_defs + shares.tool_calls);
            breakdown.tool_calls_output_tokens += spread(shares.tool_results);
            breakdown.user_messages_tokens += spread(shares.user_text);
            breakdown.model_output_tokens += spread(shares.assistant_text);
        }
        self.stats.usage.model_output_tokens += u64::from(usage.output_tokens);
    }

    /// Records one summarizer call the decorator issued outside the
    /// mirror's lock; a [`WorkerOutcome::Failed`] counts it against the
    /// worker.
    pub fn record_worker_call(&mut self, outcome: WorkerOutcome) {
        self.stats.summarizer_calls += 1;
        if outcome == WorkerOutcome::Failed {
            self.stats.summarizer_failures += 1;
        }
    }

    /// Commits an upgraded section text from a [`PendingSummary`] a
    /// worker task filled outside the request. `false` when the section no
    /// longer matches — a concurrent plan rebuilt it — and the text is
    /// dropped: every intermediate state stays a valid view.
    pub fn commit_summary(&mut self, pending: &PendingSummary, text: String) -> bool {
        let Some(section) = self.sections.get(pending.section) else {
            return false;
        };
        if section.range != pending.range {
            return false;
        }
        let section = &mut self.sections[pending.section];
        section.text = compact::cap_summary(text);
        section.origin = SectionOrigin::Model;
        true
    }

    /// The flush planner: decides whether the budget is breached, and
    /// if so moves the cut and commits the new sections — extractive
    /// stand-ins first — returning the summaries a [`Summarizer`]
    /// should upgrade. Between flushes this touches nothing: the block
    /// is append-only by construction.
    ///
    /// Called under the mirror's lock with `observe` already applied;
    /// never blocks and never calls a model.
    pub fn plan_flush(&mut self) -> Vec<PendingSummary> {
        self.sync_sizes();
        let len = self.log.len();
        if self.total_bytes <= self.settings.max_view_bytes {
            return Vec::new();
        }
        let tail_start = self.tail_index();
        let tail_bytes = self.bytes(tail_start, len);
        let allowance = self.settings.max_view_bytes.saturating_sub(tail_bytes);

        // The minimum band: walk back from the tail over whole
        // messages while they fit. The walk stops at the cut — the
        // block never un-commits.
        let mut c_min = tail_start;
        let mut budget = allowance;
        while c_min > self.cut {
            let cost = self.sizes[c_min - 1];
            if cost > budget {
                break;
            }
            budget -= cost;
            c_min -= 1;
        }
        let mut pending = Vec::new();
        if c_min > self.cut {
            // Breach. Overshoot to half the allowance: the band after
            // the flush leaves a whole hysteresis of growth before the
            // next one, so flushes — the only prefix breaks — stay
            // far apart.
            let half = allowance / 2;
            let mut c_new = c_min;
            let mut band = self.bytes(c_new, tail_start);
            // The smallest position whose remaining band fits half:
            // walk forward while the band still exceeds it. The break
            // on `next > half` was the inverse and left the band at
            // the full allowance — flushes every request.
            while c_new < tail_start && band > half {
                band -= self.sizes[c_new];
                c_new += 1;
            }
            // The band must not open on a tool result: its call would
            // stay behind in the last section. Trailing results come
            // along into the block.
            while c_new < tail_start && self.log[c_new].role == Role::Tool {
                c_new += 1;
            }
            if c_new > self.cut {
                pending = self.extend_sections(self.cut, c_new);
                self.cut = c_new;
                self.stats.flushes += 1;
                pending.extend(self.consolidate());
            }
        }
        pending
    }

    /// The view of the log: `None` passes it through unchanged — below
    /// the budget, or nothing sectioned yet. Past it, the block
    /// followed by the whole band from the cut.
    ///
    /// The one-stop form for callers without a summarizer:
    /// [`Transcript::plan_flush`] plus this, with the extractive
    /// stand-ins final. The decorator plans and renders under the
    /// lock and hands the fill a background task.
    pub fn view(&mut self, messages: &[Message]) -> Option<Vec<Message>> {
        self.observe(messages);
        self.plan_flush();
        self.render()
    }

    /// Builds the request view from the committed state. Called once
    /// per request, after any summarizer commits; updates the
    /// benchmark counters.
    pub fn render(&mut self) -> Option<Vec<Message>> {
        self.stats.requests += 1;
        self.stats.log_bytes = self.total_bytes;
        self.stats.log_content = self.content;
        if self.cut == 0 {
            self.stats.pass_through += 1;
            return None;
        }
        let len = self.log.len();
        let tail_start = self.tail_index();
        self.stats.log_messages = len;
        self.stats.sectioned_messages = self.cut;
        self.stats.sections = self.sections.len();
        self.stats.block_bytes = self.block_bytes();
        self.stats.band_bytes = self.bytes(self.cut, len);
        self.stats.tail_bytes = self.bytes(tail_start, len);
        self.stats.viewed += 1;

        let mut text = String::from(
            "<transcript>\n\
             The blocks below cover this conversation's older messages,\n\
             oldest first. Each block is headed `id+n|`: the n messages\n\
             from index id on, condensed. zoom(id, n) returns those n\n\
             messages whole; long output is cut and the note names the\n\
             `at` to resume at. The messages after the blocks are\n\
             recent; they are whole.\n\n",
        );
        for section in &self.sections {
            text.push_str(&format!(
                "{}+{}|\n{}\n\n",
                section.range.id, section.range.n, section.text
            ));
        }
        text.push_str("</transcript>");
        let mut out = Vec::with_capacity(len - self.cut + 1);
        out.push(Message::text(Role::User, text));
        out.extend(self.log[self.cut..].iter().cloned());
        Some(out)
    }

    /// The last user message's index, clamped to the cut: the tail is
    /// never under the block. No user message at all — an
    /// assistant-only history — and everything from the cut on is the
    /// tail.
    fn tail_index(&self) -> usize {
        let user = self
            .log
            .iter()
            .rposition(|message| message.role == Role::User)
            .unwrap_or(self.log.len());
        user.max(self.cut)
    }

    /// The wire bytes of `log[from..to]`, from the size cache.
    fn bytes(&self, from: usize, to: usize) -> usize {
        self.sizes[from..to].iter().sum()
    }

    /// The block's bytes: every section's header plus text.
    fn block_bytes(&self) -> usize {
        self.sections.iter().map(Section::view_bytes).sum()
    }

    /// Realigns the size cache with the log; `observe` maintains it,
    /// so this is a defensive no-op in normal flow. When it does patch,
    /// the running totals rebuild from scratch.
    fn sync_sizes(&mut self) {
        self.sizes.truncate(self.log.len());
        let mut patched = false;
        while self.sizes.len() < self.log.len() {
            let id = self.sizes.len();
            self.sizes.push(summary::message_bytes(&self.log[id]));
            patched = true;
        }
        if patched {
            self.total_bytes = self.sizes.iter().sum();
            self.content = LogContent::default();
            for message in &self.log {
                add_message_content(&mut self.content, message);
            }
        }
    }

    /// Sections `[start, end)` into the block: chunks at message
    /// boundaries within the step and message-count bounds, each
    /// ending so the next never opens on a tool result whose call the
    /// chunk holds. Returns one pending summary per chunk.
    fn extend_sections(&mut self, start: usize, end: usize) -> Vec<PendingSummary> {
        let mut pending = Vec::new();
        let mut at = start;
        while at < end {
            let mut stop = at;
            let mut bytes = 0usize;
            let mut count = 0usize;
            while stop < end {
                let cost = self.sizes[stop];
                if count > 0
                    && (bytes + cost > self.settings.step_bytes
                        || count >= self.settings.max_section_messages)
                {
                    break;
                }
                bytes += cost;
                count += 1;
                stop += 1;
            }
            while stop < end && self.log[stop].role == Role::Tool {
                stop += 1;
            }
            let range = Range {
                id: at,
                n: stop - at,
            };
            let messages = self.log[at..stop].to_vec();
            let text = compact::section_digest(&messages);
            // The digest of whatever sits directly before this chunk:
            // the stand-in committed for the previous chunk, or the
            // block's last section for the first chunk of a flush.
            let context = self.sections.last().map(|s| s.text.clone());
            self.sections.push(Section {
                range,
                text,
                origin: SectionOrigin::Extractive,
            });
            pending.push(PendingSummary {
                section: self.sections.len() - 1,
                range,
                messages,
                context,
            });
            at = stop;
        }
        pending
    }

    /// Consolidates the block past its cap: the two oldest sections
    /// merge into one — the union range re-derived from the log, not
    /// from the summaries — until the block is at half the cap. Each
    /// merge returns a pending summary for the model fill. Rare and
    /// stepped: this is the one append-only exception, and it is a
    /// full cache break when it fires.
    fn consolidate(&mut self) -> Vec<PendingSummary> {
        let mut pending = Vec::new();
        while self.sections.len() >= 2 && self.block_bytes() > self.settings.max_block_bytes {
            let range = Range {
                id: self.sections[0].range.id,
                n: self.sections[0].range.n + self.sections[1].range.n,
            };
            let messages = self.log[range.id..range.id + range.n].to_vec();
            let text = compact::section_digest(&messages);
            self.sections.drain(..2);
            self.sections.insert(
                0,
                Section {
                    range,
                    text,
                    origin: SectionOrigin::Extractive,
                },
            );
            // The union is the block's first section: nothing sits
            // before it to resolve references against.
            pending.push(PendingSummary {
                section: 0,
                range,
                messages,
                context: None,
            });
            self.stats.consolidations += 1;
        }
        pending
    }
}

/// The transcript as a [`ModelProvider`]: wraps `inner`, rewrites every
/// request's messages through the stepped view, and refreshes the
/// shared mirror first. A configured [`Summarizer`] upgrades the new
/// sections' stand-ins in a background task after the request went
/// out — never inside it, never under the mirror's lock, so a slow or
/// failing worker delays nothing.
pub struct TranscriptView {
    inner: Arc<dyn ModelProvider>,
    transcript: Arc<Mutex<Transcript>>,
    summarizer: Option<Arc<dyn Summarizer>>,
}

impl TranscriptView {
    /// The wrapper over `inner`, mirroring into `transcript` — the same
    /// mirror a [`crate::zoom::Zoom`] resolver reads. Build both from
    /// one mirror, or let [`crate::attach`] do it. No summarizer: new
    /// sections keep their extractive stand-ins.
    pub fn new(inner: Arc<dyn ModelProvider>, transcript: Arc<Mutex<Transcript>>) -> Self {
        Self {
            inner,
            transcript,
            summarizer: None,
        }
    }

    /// The wrapper with a summarizer for the model fill of new
    /// sections. The fill runs in tokio tasks: calling [`Self::complete`]
    /// with a summarizer requires a running runtime.
    pub fn with_summarizer(
        inner: Arc<dyn ModelProvider>,
        transcript: Arc<Mutex<Transcript>>,
        summarizer: Arc<dyn Summarizer>,
    ) -> Self {
        Self {
            inner,
            transcript,
            summarizer: Some(summarizer),
        }
    }
}

impl std::fmt::Debug for TranscriptView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner provider is a trait object and stays opaque.
        f.debug_struct("TranscriptView").finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for TranscriptView {
    async fn complete(
        &self,
        mut req: rhema_core::ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        // Plan under the lock: move the cut, commit stand-ins. Never
        // a model call, never an await here.
        let pending = {
            let mut transcript = self
                .transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            transcript.observe(&req.messages);
            transcript.plan_flush()
        };
        // The fill runs after this request, outside it and outside the
        // lock: the request goes out now with the extractive
        // stand-ins, and the upgrades commit under the mirror's lock
        // as they land. A slow or failing worker delays nothing and
        // leaves the stand-in standing.
        if let Some(worker) = &self.summarizer
            && !pending.is_empty()
        {
            Self::spawn_worker_fill(Arc::clone(worker), Arc::clone(&self.transcript), pending);
        }
        let view = {
            let mut transcript = self
                .transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            transcript.render()
        };
        if let Some(messages) = view {
            req.messages = messages;
        }
        {
            let mut transcript = self
                .transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            transcript.stage_usage_shares(req.system.as_deref(), &req.tools, &req.messages);
        }
        let result = self.inner.complete(req, sink).await;
        if let Ok(output) = &result {
            let mut transcript = self
                .transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            transcript.record_usage(&output.usage);
        }
        result
    }
}

impl TranscriptView {
    /// Dispatches one background fill over `pending`: chunks of
    /// [`WORKER_CONCURRENCY`] summarize concurrently, and each chunk's
    /// answers commit under the mirror's lock as they resolve. The
    /// range check in [`Transcript::commit_summary`] rejects anything a
    /// concurrent plan rebuilt in the meantime, so a commit that races
    /// a flush or a consolidation drops its text and the stand-in
    /// stands.
    fn spawn_worker_fill(
        worker: Arc<dyn Summarizer>,
        mirror: Arc<Mutex<Transcript>>,
        pending: Vec<PendingSummary>,
    ) {
        tokio::spawn(async move {
            for chunk in pending.chunks(WORKER_CONCURRENCY) {
                let mut fills = Vec::with_capacity(chunk.len());
                for item in chunk {
                    let worker = Arc::clone(&worker);
                    let messages = item.messages.clone();
                    let context = item.context.clone();
                    let section = item.section;
                    let range = item.range;
                    fills.push(tokio::spawn(async move {
                        (
                            worker.summarize(&messages, context.as_deref()).await,
                            section,
                            range,
                        )
                    }));
                }
                for fill in fills {
                    // A task that panicked or was cancelled counts as a
                    // failure, like a provider error: the stand-in
                    // stands either way.
                    let outcome = match fill.await {
                        Ok((Ok(text), section, range)) => Some((text, section, range)),
                        _ => None,
                    };
                    let mut transcript = mirror.lock().unwrap_or_else(PoisonError::into_inner);
                    match outcome {
                        Some((text, section, range)) => {
                            // `commit_summary` reads the address, not
                            // the messages; the range check decides.
                            let pending = PendingSummary {
                                section,
                                range,
                                messages: Vec::new(),
                                context: None,
                            };
                            transcript.commit_summary(&pending, text);
                            transcript.record_worker_call(WorkerOutcome::Upgraded);
                        }
                        None => transcript.record_worker_call(WorkerOutcome::Failed),
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Content, ToolCall, ToolResult};
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The test runtime is current-thread: spawned tasks run while the
    /// test yields. Wait until `done` holds, then stop yielding.
    async fn wait_for(done: impl Fn() -> bool) {
        for _ in 0..10_000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the background fill never finished");
    }

    /// A big tool result, in kilobytes.
    fn result_bytes(kb: usize) -> String {
        "x".repeat(kb * 1024)
    }

    fn call_round(id: &str, result_kb: usize) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("checking".into()),
                    Content::ToolCall(ToolCall {
                        id: id.into(),
                        name: "bash".into(),
                        args: serde_json::json!({ "command": "make test" }),
                        provider_data: serde_json::Value::Null,
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::success(
                    id,
                    result_bytes(result_kb),
                ))],
            },
        ]
    }

    /// A history over the default budget: many 8 KiB tool rounds, then
    /// a final user message.
    fn long_history(rounds: usize) -> Vec<Message> {
        let mut messages = vec![Message::text(Role::User, "fix the build")];
        for i in 0..rounds {
            messages.extend(call_round(&format!("c{i}"), 8));
        }
        messages.push(Message::text(Role::User, "and now summarize"));
        messages
    }

    fn settings(max_view_bytes: usize) -> TranscriptSettings {
        TranscriptSettings {
            max_view_bytes,
            ..TranscriptSettings::default()
        }
    }

    #[test]
    fn below_the_budget_the_view_is_none() {
        let mut transcript = Transcript::new(settings(128_000));
        let messages = long_history(4); // ~33 KiB
        assert!(transcript.view(&messages).is_none());
        let stats = transcript.stats();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.pass_through, 1);
        assert_eq!(stats.viewed, 0);
    }

    #[test]
    fn past_the_budget_the_aged_prefix_is_block_and_the_tail_whole() {
        let mut transcript = Transcript::new(settings(128_000));
        let messages = long_history(30); // ~245 KiB
        let view = transcript.view(&messages).unwrap();

        // One synthetic user message first, tagged, carrying the
        // block's sections.
        assert_eq!(view[0].role, Role::User);
        let rhema_core::Content::Text(text) = &view[0].content[0] else {
            panic!("text view message");
        };
        assert!(text.starts_with("<transcript>"), "{text}");
        assert!(text.contains("zoom(id, n)"), "{text}");
        // Sections are addressed `id+n|`, tiling from the log's start.
        assert!(text.contains("0+"), "{text}");

        // The tail — the last user message — is present whole.
        let last = view.last().unwrap();
        assert_eq!(*last, Message::text(Role::User, "and now summarize"));
        assert!(view.len() < messages.len(), "the view is shorter");

        let stats = transcript.stats();
        assert_eq!(stats.flushes, 1);
        assert_eq!(stats.sectioned_messages, transcript.cut);
        assert!(stats.block_bytes > 0);
        assert!(stats.band_bytes + stats.block_bytes < 245 * 1024);
    }

    #[test]
    fn between_flushes_the_block_is_append_only() {
        let mut transcript = Transcript::new(settings(40_000));
        let first = long_history(5); // ~40 KiB, over budget
        let view = transcript.view(&first).unwrap();
        let first_block = match &view[0].content[..] {
            [rhema_core::Content::Text(text)] => text.clone(),
            _ => panic!("text view message"),
        };
        let sections_after_first = transcript.sections.clone();

        // Growth well under the hysteresis: no new flush, the
        // committed sections byte-identical, only the band grows.
        let mut second = first.clone();
        second.extend(call_round("c5", 1));
        second.push(Message::text(Role::User, "next"));
        let view = transcript.view(&second).unwrap();
        let second_block = match &view[0].content[..] {
            [rhema_core::Content::Text(text)] => text.clone(),
            _ => panic!("text view message"),
        };
        assert_eq!(transcript.stats.flushes, 1, "no second flush");
        assert_eq!(transcript.sections, sections_after_first);
        assert_eq!(first_block, second_block, "the block is byte-identical");
        assert!(view.len() > 1);
    }

    #[test]
    fn a_flush_overshoots_to_half_the_allowance_and_the_hysteresis_holds() {
        let mut transcript = Transcript::new(settings(128_000));
        let mut messages = vec![Message::text(Role::User, "fix the build")];
        for i in 0..20 {
            messages.extend(call_round(&format!("c{i}"), 8)); // ~164 KiB
        }
        messages.push(Message::text(Role::User, "go on"));
        assert!(transcript.view(&messages).is_some());
        let stats = transcript.stats();
        assert_eq!(stats.flushes, 1);
        // The band the flush leaves behind sits at or below half the
        // allowance — the overshoot — not at the full allowance.
        let allowance = 128_000 - stats.tail_bytes;
        let band = stats.band_bytes - stats.tail_bytes;
        assert!(
            band <= allowance / 2,
            "band {band} over half the allowance {}",
            allowance / 2
        );
        // The hysteresis is the half-allowance the overshoot bought:
        // growth inside it must not flush again.
        let mut grown = messages.clone();
        for i in 20..26 {
            grown.extend(call_round(&format!("c{i}"), 8)); // ~49 KiB < allowance/2
        }
        grown.push(Message::text(Role::User, "still going"));
        transcript.view(&grown);
        assert_eq!(transcript.stats.flushes, 1, "flushed inside the hysteresis");
    }

    #[test]
    fn a_flush_never_splits_a_tool_round_at_a_section_boundary() {
        let mut transcript = Transcript::new(settings(20_000));
        // user, round c0 (16 KiB), round c1 (16 KiB), user.
        let messages = vec![Message::text(Role::User, "go")]
            .into_iter()
            .chain(call_round("c0", 16))
            .chain(call_round("c1", 16))
            .chain(std::iter::once(Message::text(Role::User, "done")))
            .collect::<Vec<_>>();
        let view = transcript.view(&messages).unwrap();
        // Every tool result carried whole has its call carried too:
        // the pairs survive the cut together.
        let carried_calls: HashSet<String> = view
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                rhema_core::Content::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        for message in &view {
            for content in &message.content {
                if let rhema_core::Content::ToolResult(result) = content {
                    assert!(
                        carried_calls.contains(&result.id),
                        "orphan result {} in the view",
                        result.id
                    );
                }
            }
        }
        // And no section's range opens on a result: the same pairing
        // holds inside the block, so a section never stands in for a
        // round it splits.
        for section in &transcript.sections {
            let first = transcript
                .range(section.range)
                .expect("section in the log")
                .first()
                .unwrap();
            assert_ne!(first.role, Role::Tool, "section {:?}", section.range);
        }
    }

    #[test]
    fn sections_respect_the_step_and_message_bounds() {
        let mut transcript = Transcript::new(TranscriptSettings {
            step_bytes: 20_000,
            max_section_messages: 3,
            ..settings(20_000)
        });
        let messages = long_history(8);
        transcript.observe(&messages);
        transcript.plan_flush();
        assert!(transcript.sections.len() > 1, "bounds force a split");
        for section in &transcript.sections {
            assert!(section.range.n <= 4, "{:?}", section.range);
        }
        // The sections tile the cut without gaps.
        let mut at = 0;
        for section in &transcript.sections {
            assert_eq!(section.range.id, at);
            at += section.range.n;
        }
        assert_eq!(at, transcript.cut);
    }

    #[test]
    fn a_worker_upgrade_lands_and_a_wrong_one_is_rejected() {
        let mut transcript = Transcript::new(settings(20_000));
        let messages = long_history(30);
        transcript.observe(&messages);
        let pending = transcript.plan_flush();
        assert!(!pending.is_empty());

        let good = pending[0].clone();
        assert!(transcript.commit_summary(&good, "## Goals Met\n- all".into()));
        assert_eq!(transcript.sections[0].text, "## Goals Met\n- all");
        assert_eq!(transcript.sections[0].origin, SectionOrigin::Model);

        // A stale pending — the range moved on — is dropped.
        // The context is irrelevant to the commit's range check.
        let stale = PendingSummary {
            section: pending[0].section,
            range: Range { id: 999, n: 1 },
            messages: Vec::new(),
            context: None,
        };
        assert!(!transcript.commit_summary(&stale, "ghost".into()));
        assert_eq!(transcript.sections[0].text, "## Goals Met\n- all");
    }

    #[test]
    fn a_resume_adopts_the_saved_sections_and_skips_the_refill() {
        let log = long_history(30);
        // The first session sections and upgrades the block.
        let mut first = Transcript::new(settings(20_000));
        first.observe(&log);
        let pending = first.plan_flush();
        assert!(!pending.is_empty());
        for item in &pending {
            assert!(first.commit_summary(item, format!("## Goals Met\n- model {}", item.section)));
        }
        let saved = first.saved_sections();
        assert!(saved.iter().all(|s| s.origin == SectionOrigin::Model));

        // The resumed session adopts them: the compaction the last
        // session paid for rides as-is, dense from the first request.
        let mut second = Transcript::new(settings(20_000));
        second.adopt_sections(saved.clone());
        second.observe(&log);
        assert_eq!(second.cut, first.cut);
        assert_eq!(second.sections, saved);
        // No re-sectioning, no pending fill: nothing to re-pay.
        assert!(second.plan_flush().is_empty());

        // The band keeps growing from the adopted cut.
        let mut extended = log.clone();
        extended.push(Message::text(Role::User, "one more"));
        second.observe(&extended);
        let view = second.render().unwrap();
        assert_eq!(view.last(), Some(&Message::text(Role::User, "one more")));
    }

    #[test]
    fn tampered_saved_sections_fall_back_to_the_rebuild() {
        let mut transcript = Transcript::new(settings(20_000));
        transcript.adopt_sections(vec![Section {
            range: Range { id: 5, n: 9 },
            text: "## Goals Met\n- orphan".into(),
            origin: SectionOrigin::Model,
        }]);
        transcript.observe(&long_history(30));
        // The first id 5, not 0: the tiling rejects the adoption.
        assert_eq!(transcript.cut, 0);
        assert!(transcript.sections.is_empty());
        // The next request rebuilds from nothing.
        assert!(!transcript.plan_flush().is_empty());
    }

    #[test]
    fn a_pending_summary_carries_the_adjacent_digest_as_context() {
        let mut transcript = Transcript::new(settings(20_000));
        transcript.observe(&long_history(30));
        let pending = transcript.plan_flush();
        assert!(pending.len() >= 2);
        // The first section has no predecessor; every later one points
        // at the stand-in committed for the section before it.
        assert_eq!(pending[0].context, None);
        for (prev, item) in pending.iter().zip(pending.iter().skip(1)) {
            let expected = transcript.sections[prev.section].text.clone();
            assert_eq!(item.context.as_deref(), Some(expected.as_str()));
        }

        // A later flush's first chunk inherits the block's last
        // section as its context.
        let first = pending[0].clone();
        assert!(transcript.commit_summary(&first, "## Goals Met\n- model".into()));
        let mut extended = long_history(30);
        for i in 0..12 {
            extended.push(Message::text(
                Role::User,
                format!("extra filler {i}: {}", "y".repeat(1024)),
            ));
        }
        transcript.observe(&extended);
        // Read the predecessor before planning: plan_flush itself
        // pushes the new sections.
        let expected = transcript.sections.last().unwrap().text.clone();
        let pending = transcript.plan_flush();
        assert!(!pending.is_empty());
        assert_eq!(pending[0].context.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn the_block_consolidates_past_its_cap() {
        let mut transcript = Transcript::new(TranscriptSettings {
            max_block_bytes: 500,
            ..settings(20_000)
        });
        let messages = long_history(30);
        transcript.observe(&messages);
        transcript.plan_flush();
        assert!(transcript.block_bytes() <= 500);
        assert!(transcript.stats.consolidations > 0);
        // The surviving sections still tile the prefix.
        let mut at = 0;
        for section in &transcript.sections {
            assert_eq!(section.range.id, at);
            at += section.range.n;
        }
        assert_eq!(at, transcript.cut);
    }

    #[test]
    fn a_resumed_session_rebuilds_the_compaction_from_nothing() {
        let mut transcript = Transcript::new(settings(20_000));
        transcript.observe(&long_history(30));
        transcript.plan_flush();
        assert!(transcript.cut > 0);
        // A different history wholesale: the derived state resets.
        transcript.observe(&long_history(2));
        assert_eq!(transcript.cut, 0);
        assert!(transcript.sections.is_empty());
    }

    #[test]
    fn the_breakdown_spreads_reported_input_by_wire_share() {
        let mut transcript = Transcript::new(TranscriptSettings::default());
        transcript.set_skills_bytes(500);
        // System: 1500 base bytes + 500 skills bytes. Messages: one
        // user text, one assistant text with a call, one result.
        let system = format!("{}{}", "b".repeat(1_500), "s".repeat(500));
        let call_args = serde_json::json!({ "c": "x".repeat(100) });
        let messages = vec![
            Message::text(Role::User, "u".repeat(1_000)),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("a".repeat(400)),
                    Content::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        args: call_args.clone(),
                        provider_data: serde_json::Value::Null,
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::success(
                    "c1",
                    "r".repeat(1_000),
                ))],
            },
        ];
        transcript.stage_usage_shares(Some(&system), &[], &messages);
        transcript.record_usage(&Usage {
            input_tokens: 10_000,
            output_tokens: 100,
            cached_input_tokens: 0,
        });

        let total = 2_000 + 1_000 + 400 + ("bash".len() + call_args.to_string().len()) + 1_000;
        let spread = |bytes: usize| 10_000 * bytes as u64 / total as u64;
        let b = transcript.stats().usage;
        assert_eq!(b.system_prompt_tokens, spread(1_500));
        assert_eq!(b.skills_tokens, spread(500));
        assert_eq!(
            b.tool_calls_input_tokens,
            spread("bash".len() + call_args.to_string().len())
        );
        assert_eq!(b.tool_calls_output_tokens, spread(1_000));
        assert_eq!(b.user_messages_tokens, spread(1_000));
        // The model's words: the history's assistant text plus the
        // generated output.
        assert_eq!(b.model_output_tokens, spread(400) + 100);
        // The shares consumed: a usage record without a staged request
        // spreads nothing.
        transcript.record_usage(&Usage {
            input_tokens: 1_000,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
        assert_eq!(transcript.stats().usage.user_messages_tokens, spread(1_000));
    }

    #[test]
    fn a_skills_share_past_the_system_clamps() {
        let mut transcript = Transcript::new(TranscriptSettings::default());
        transcript.set_skills_bytes(9_999);
        transcript.stage_usage_shares(Some("short"), &[], &[]);
        transcript.record_usage(&Usage {
            input_tokens: 100,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
        let b = transcript.stats().usage;
        assert_eq!(b.skills_tokens, 100);
        assert_eq!(b.system_prompt_tokens, 0);
    }

    #[test]
    fn a_carry_continues_the_saved_run() {
        let saved = TranscriptStats {
            requests: 10,
            pass_through: 6,
            viewed: 4,
            flushes: 1,
            consolidations: 1,
            summarizer_calls: 2,
            summarizer_failures: 1,
            input_tokens: 90_000,
            cached_input_tokens: 70_000,
            output_tokens: 10_000,
            usage: UsageBreakdown {
                system_prompt_tokens: 20_000,
                skills_tokens: 2_000,
                tool_calls_input_tokens: 30_000,
                tool_calls_output_tokens: 30_000,
                user_messages_tokens: 8_000,
                model_output_tokens: 10_000,
            },
            // The saved snapshot describes the log the resume replaced.
            log_messages: 12,
            sectioned_messages: 4,
            sections: 2,
            block_bytes: 900,
            band_bytes: 3_000,
            tail_bytes: 1_200,
            log_bytes: 5_400,
            log_content: LogContent {
                tool_calls: 100,
                tool_results: 4_000,
                user_text: 300,
                assistant_text: 1_000,
            },
        };

        // A mirror that has not looked at a log yet shows the saved
        // run whole — the counters and, until its first request, the
        // snapshot too.
        let mut fresh = TranscriptStats::default();
        fresh.carry(&saved);
        assert_eq!(fresh, saved);

        // The mirror then counts requests of its own: each read folds
        // the same saved run in once, so the totals are the run's,
        // never doubled, and the snapshot is the live mirror's.
        let mut live = TranscriptStats {
            requests: 2,
            input_tokens: 5_000,
            output_tokens: 300,
            usage: UsageBreakdown {
                user_messages_tokens: 5_000,
                model_output_tokens: 300,
                ..UsageBreakdown::default()
            },
            log_messages: 4,
            block_bytes: 100,
            ..TranscriptStats::default()
        };
        live.carry(&saved);
        assert_eq!(live.requests, 12);
        assert_eq!(live.input_tokens, 95_000);
        assert_eq!(live.output_tokens, 10_300);
        assert_eq!(live.usage.user_messages_tokens, 13_000);
        assert_eq!(live.usage.model_output_tokens, 10_300);
        assert_eq!(live.usage.system_prompt_tokens, 20_000);
        assert_eq!(live.block_bytes, 100, "the live mirror's snapshot");
        assert_eq!(live.log_messages, 4);

        // A later read: the mirror grew, the baseline still folds in
        // once.
        let mut later = TranscriptStats {
            requests: 3,
            input_tokens: 8_000,
            ..TranscriptStats::default()
        };
        later.carry(&saved);
        assert_eq!(later.requests, 13);
        assert_eq!(later.input_tokens, 98_000);
    }

    /// The log-size snapshot: what the conversation holds, each
    /// message counted once, refreshed on every request — pass-through
    /// ones included. The usage split's once-counted counterpart.
    #[test]
    fn the_log_bytes_snapshot_counts_the_log_once() {
        let mut transcript = Transcript::new(settings(128_000));
        let messages = long_history(4); // ~33 KiB, below the budget
        transcript.observe(&messages);
        transcript.render();
        let expected: usize = messages.iter().map(summary::message_bytes).sum();
        assert_eq!(transcript.stats().log_bytes, expected);

        // An extension rides in; the next request recounts the log.
        let grown = long_history(5);
        transcript.observe(&grown);
        transcript.render();
        let expected: usize = grown.iter().map(summary::message_bytes).sum();
        assert_eq!(transcript.stats().log_bytes, expected);
    }

    /// The content split: the log's bytes by category, each message
    /// counted once. For this fixture the categories tile the log —
    /// no images, no system or tool-role text — so they sum to the
    /// log's whole size.
    #[test]
    fn the_content_split_counts_each_message_once() {
        let mut transcript = Transcript::new(settings(128_000));
        let messages = long_history(4);
        transcript.observe(&messages);
        transcript.render();
        let content = transcript.stats().log_content;
        assert_eq!(content.tool_results, 4 * result_bytes(8).len());
        assert_eq!(content.assistant_text, 4 * "checking".len());
        assert_eq!(
            content.user_text,
            "fix the build".len() + "and now summarize".len()
        );
        let args = serde_json::json!({ "command": "make test" }).to_string();
        assert_eq!(content.tool_calls, 4 * ("bash".len() + args.len()));
        assert_eq!(
            content.tool_calls + content.tool_results + content.user_text + content.assistant_text,
            transcript.stats().log_bytes
        );
    }

    #[test]
    fn reset_stats_zeroes_the_accounting_and_keeps_the_log() {
        let mut transcript = Transcript::new(TranscriptSettings::default());
        transcript.observe(&[Message::text(Role::User, "kept")]);
        transcript.record_usage(&Usage {
            input_tokens: 1_000,
            output_tokens: 100,
            cached_input_tokens: 0,
        });
        assert_eq!(transcript.len(), 1);
        transcript.reset_stats();
        assert_eq!(transcript.stats(), TranscriptStats::default());
        assert_eq!(transcript.len(), 1);
    }

    #[test]
    fn usage_records_and_the_hit_rate_reads() {
        let mut transcript = Transcript::new(TranscriptSettings::default());
        assert!(transcript.stats().cache_hit_rate().is_none());
        transcript.record_usage(&Usage {
            input_tokens: 1_000,
            output_tokens: 100,
            cached_input_tokens: 800,
        });
        let stats = transcript.stats();
        assert_eq!(stats.input_tokens, 1_000);
        assert_eq!(stats.cached_input_tokens, 800);
        let rate = stats.cache_hit_rate().unwrap();
        assert!((rate - 0.8).abs() < 1e-9);
    }

    /// Records what it was handed.
    struct Recording(StdMutex<Vec<Vec<Message>>>);
    #[async_trait]
    impl ModelProvider for Recording {
        async fn complete(
            &self,
            req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            self.0.lock().unwrap().push(req.messages);
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: rhema_core::StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 500,
                    output_tokens: 10,
                    cached_input_tokens: 400,
                },
            })
        }
    }

    #[tokio::test]
    async fn the_decorator_rewrites_the_request_and_the_mirror_stays_whole() {
        let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
        let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
        let view = TranscriptView::new(recording.clone(), transcript.clone());
        let messages = long_history(30);
        let req = rhema_core::ModelRequest {
            system: None,
            messages: messages.clone(),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let mut sink = NoopSink;
        view.complete(req, &mut sink).await.unwrap();

        let seen = recording.0.lock().unwrap();
        let sent = &seen[0];
        assert!(sent.len() < messages.len());
        assert_eq!(sent.last().unwrap().role, Role::User);
        // The mirror kept every message, none dropped.
        let mirror = transcript.lock().unwrap();
        assert_eq!(mirror.len(), messages.len());
        assert_eq!(mirror.message(0), Some(&messages[0]));
        // The usage landed: the hit rate is reportable.
        assert_eq!(mirror.stats().cache_hit_rate(), Some(0.8));
    }

    #[tokio::test]
    async fn the_decorator_upgrades_sections_with_its_summarizer() {
        /// Answers every call with the same dense block.
        struct Worker;
        #[async_trait]
        impl ModelProvider for Worker {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<ModelOutput, ProviderError> {
                Ok(ModelOutput {
                    message: Message::text(Role::Assistant, "## Goals Met\n- worker wrote this"),
                    stop_reason: rhema_core::StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }

        let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
        let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
        let view = TranscriptView::with_summarizer(
            recording.clone(),
            transcript.clone(),
            Arc::new(compact::ModelSummarizer::new(Arc::new(Worker), "cheap")),
        );
        let req = rhema_core::ModelRequest {
            system: None,
            messages: long_history(30),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let mut sink = NoopSink;
        view.complete(req, &mut sink).await.unwrap();

        // The request went out with the extractive stand-ins: the fill
        // had not landed when it rendered.
        {
            let sent = recording.0.lock().unwrap();
            let rhema_core::Content::Text(text) = &sent[0][0].content[0] else {
                panic!("text view message");
            };
            assert!(!text.contains("worker wrote this"), "{text}");
        }

        // The background fill upgrades every section.
        wait_for(|| {
            transcript
                .lock()
                .unwrap()
                .sections
                .iter()
                .all(|s| s.origin == SectionOrigin::Model)
        })
        .await;

        // The guard drops at the brace: the runtime is current-thread,
        // so the next request cannot take the lock while it is held.
        {
            let mirror = transcript.lock().unwrap();
            let stats = mirror.stats();
            assert!(stats.summarizer_calls > 0);
            assert_eq!(stats.summarizer_failures, 0);
            assert!(
                mirror
                    .sections
                    .iter()
                    .all(|s| s.text.contains("worker wrote this"))
            );
        }

        // The next request carries the dense text.
        let req = rhema_core::ModelRequest {
            system: None,
            messages: long_history(30),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let mut sink = NoopSink;
        view.complete(req, &mut sink).await.unwrap();
        let sent = recording.0.lock().unwrap();
        let rhema_core::Content::Text(text) = &sent[1][0].content[0] else {
            panic!("text view message");
        };
        assert!(text.contains("worker wrote this"), "{text}");
    }

    #[tokio::test]
    async fn a_failing_summarizer_never_blocks_the_request() {
        struct Failing;
        #[async_trait]
        impl ModelProvider for Failing {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<ModelOutput, ProviderError> {
                Err(ProviderError::Timeout)
            }
        }

        let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
        let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
        let view = TranscriptView::with_summarizer(
            recording.clone(),
            transcript.clone(),
            Arc::new(compact::ModelSummarizer::new(Arc::new(Failing), "cheap")),
        );
        let req = rhema_core::ModelRequest {
            system: None,
            messages: long_history(30),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let mut sink = NoopSink;
        // The main call went out with the extractive stand-ins, and
        // never waited on the worker: the failure is counted by the
        // background fill, after the response.
        view.complete(req, &mut sink).await.unwrap();
        {
            // The guard drops before the wait: no lock is held across
            // an await.
            let sent = recording.0.lock().unwrap();
            assert_eq!(sent.len(), 1);
        }
        wait_for(|| transcript.lock().unwrap().stats().summarizer_calls > 0).await;
        let mirror = transcript.lock().unwrap();
        assert_eq!(
            mirror.stats().summarizer_failures,
            mirror.stats().summarizer_calls
        );
        assert!(
            mirror
                .sections
                .iter()
                .all(|s| s.origin == SectionOrigin::Extractive)
        );
    }

    /// A summarizer worker that answers only once its gate opens; it
    /// spins on yields, so an inline await of it would hang the
    /// current-thread runtime.
    struct Gated(Arc<AtomicBool>);

    #[async_trait]
    impl ModelProvider for Gated {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            while !self.0.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "## Goals Met\n- late but dense"),
                stop_reason: rhema_core::StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn a_slow_summarizer_never_delays_the_request() {
        let gate = Arc::new(AtomicBool::new(false));
        let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
        let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
        let view = TranscriptView::with_summarizer(
            recording.clone(),
            transcript.clone(),
            Arc::new(compact::ModelSummarizer::new(
                Arc::new(Gated(Arc::clone(&gate))),
                "cheap",
            )),
        );
        let req = rhema_core::ModelRequest {
            system: None,
            messages: long_history(30),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let turn = tokio::spawn(async move {
            let mut sink = NoopSink;
            view.complete(req, &mut sink).await.unwrap();
        });

        // The request goes out, and the turn finishes, while the worker
        // is still gated: the fill never sat in the request path.
        wait_for(|| !recording.0.lock().unwrap().is_empty()).await;
        wait_for(|| turn.is_finished()).await;
        assert!(
            !gate.load(Ordering::SeqCst),
            "the request waited on the gated worker"
        );

        // Then the fill lands, and the section upgrades.
        gate.store(true, Ordering::SeqCst);
        wait_for(|| {
            transcript
                .lock()
                .unwrap()
                .sections
                .iter()
                .all(|s| s.origin == SectionOrigin::Model)
        })
        .await;
        let mirror = transcript.lock().unwrap();
        assert!(
            mirror
                .sections
                .iter()
                .all(|s| s.text.contains("late but dense")),
            "{:?}",
            mirror.sections
        );
    }

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: rhema_core::Event<'_>) {}
    }
}
