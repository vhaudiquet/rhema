//! The request view: the provider decorator that rewrites
//! `req.messages` under a byte budget, and the mirror `zoom` reads.
//!
//! The decorator sits outside everything else on the request path, as
//! paste expansion does: the engine clones the session into the request
//! untouched, the decorator projects it, the session keeps every
//! message (SPEC.md §6). Requests carry the full history, so the
//! decorator's mirror is exactly the log as of the last request, a
//! resumed session rebuilds it wholesale on the first call.
//!
//! Past the budget the aged prefix is covered by the stepped
//! compaction block ([`crate::compact`]): sections tiling `[0, cut)`,
//! appended only at flushes so the request prefix stays cache-stable
//! between them. This module owns the cut and the sections; the
//! summarizer seam and the section writers live in
//! [`crate::compact`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use rhema_core::{
    Content, EventSink, Message, ModelOutput, ModelProvider, ProviderError, Role, ToolResult,
    ToolSpec, Usage,
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
    /// engages only past it, below, requests pass through
    /// byte-identical, and the current turn may exceed it: the tail is
    /// never cut.
    pub max_view_bytes: usize,
    /// The bytes one section's messages aim for: the summarizer
    /// worker's input bound, and the granularity compaction works at.
    /// A section carries at least one message whatever this says.
    pub step_bytes: usize,
    /// The messages one section covers at most. Bounds the extractive
    /// stand-in's lossiness: a digest keeps the user's words whole and
    /// counts the rest, so this caps how much tool traffic one line
    /// may paper over when no model fill is configured.
    pub max_section_messages: usize,
    /// The bytes the committed block may reach before the oldest
    /// sections consolidate into one. Consolidation rewrites the
    /// block, a full cache break, so it fires only past this cap,
    /// and merges down to half of it before stopping.
    pub max_block_bytes: usize,
    /// The most whole images one request's band may carry. Providers
    /// reject a request past a hard per-request image count, and an
    /// image's flat budget charge never bounds the count, so the cap
    /// is its own bound: a flush fires on whichever of bytes or
    /// images binds first, sectioning the oldest image-bearing
    /// messages. The tail keeps its exception: one turn may carry
    /// more than this.
    pub max_view_images: usize,
}

impl Default for TranscriptSettings {
    fn default() -> Self {
        Self {
            // Tool-heavy sessions (build logs, file dumps in results)
            // run oversized tails constantly; 256 KB keeps the
            // allowance, and with it the stretch between flushes,
            // worth the name at that workload.
            max_view_bytes: 256_000,
            step_bytes: 24_000,
            max_section_messages: 128,
            max_block_bytes: 32_000,
            // Generous against the strictest vision adapters, tight
            // against the per-request rejection thresholds.
            max_view_images: 10,
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
    /// Tool results, the tools' output, re-sent as input.
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
            ("System Prompt", self.system_prompt_tokens),
            ("Skills", self.skills_tokens),
            ("Tool Calls Input", self.tool_calls_input_tokens),
            ("Tool Calls Output", self.tool_calls_output_tokens),
            ("User Messages", self.user_messages_tokens),
            ("Model Output", self.model_output_tokens),
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

/// The log's own content by category, in estimated tokens, each
/// message counted once, the content the billed split re-sends once
/// per request. Tool content has its own buckets, text and images go
/// to the message's role, and a system message counts nowhere: the
/// system prompt, the skills section, and the tool definitions ride
/// every request but live in no message, so they have no figure here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogContent {
    /// Tool-call arguments in assistant messages.
    pub tool_calls: u64,
    /// Tool results in tool messages.
    pub tool_results: u64,
    /// User text, the compaction block included.
    pub user_text: u64,
    /// Assistant text.
    pub assistant_text: u64,
}

impl LogContent {
    /// The four categories summed: the log's estimated tokens, each
    /// message counted once.
    pub fn total(&self) -> u64 {
        self.tool_calls + self.tool_results + self.user_text + self.assistant_text
    }
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
    /// The block's bytes at the last request: the budget's unit.
    pub block_bytes: usize,
    /// The block's estimated tokens at the last request.
    #[serde(default)]
    pub block_tokens: u64,
    /// The bytes carried whole from the cut to the log's end, at the
    /// last request.
    pub band_bytes: usize,
    /// The estimated tokens carried whole from the cut to the log's
    /// end, at the last request.
    #[serde(default)]
    pub band_tokens: u64,
    /// Whole images the band carried at the last request. The image
    /// cap's gauge: past [`TranscriptSettings::max_view_images`] the
    /// overflow is one oversized turn the tail rule kept whole.
    #[serde(default)]
    pub view_images: usize,
    /// The current turn's bytes at the last request.
    pub tail_bytes: usize,
    /// The current turn's estimated tokens at the last request.
    #[serde(default)]
    pub tail_tokens: u64,
    /// The log's wire bytes at the last request, what the
    /// conversation actually holds, each message counted once. The
    /// usage split below sums the same content once per request;
    /// this is the once.
    #[serde(default)]
    pub log_bytes: usize,
    /// The log's estimated tokens by category at the last request,
    /// counted once.
    #[serde(default)]
    pub log_content: LogContent,
    /// Input tokens the providers reported, summed over requests,
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
    /// own numbers, the ground truth the stepped design optimizes.
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
    /// the mirror that measured them, so they stay this side's,
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
            self.block_tokens = saved.block_tokens;
            self.band_bytes = saved.band_bytes;
            self.band_tokens = saved.band_tokens;
            self.view_images = saved.view_images;
            self.tail_bytes = saved.tail_bytes;
            self.tail_tokens = saved.tail_tokens;
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
    /// The worker answered, but the target range left the block (a
    /// merge consumed it); the text is dropped and the stand-in stays.
    Dropped,
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
    /// Per-message estimated tokens, parallel to `log`: the Usage
    /// page's unit, maintained beside `sizes`.
    tokens: Vec<u64>,
    /// [`Vec`]`sizes` summed once, maintained by `observe` and read by
    /// the planner and the counters. The whole-log walk this replaces
    /// re-serialized every tool call's arguments on every request.
    total_bytes: usize,
    /// [`Vec`]`tokens` summed once, maintained beside `total_bytes`.
    total_tokens: u64,
    /// Per-message image counts, parallel to `log`: the image cap's
    /// unit, maintained beside `sizes`.
    images: Vec<usize>,
    /// The log's estimated tokens by category, maintained by `observe`
    /// the same way; `render` snapshots it into `stats.log_content`.
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
    /// The skills whose bodies pin whole into the block. Empty, and
    /// nothing pins.
    skills: Vec<SkillText>,
    /// The shortest pinned body: the detect gate, below which no
    /// result can carry a body at all. `usize::MAX` with no skills.
    min_skill_body: usize,
    /// The skill reads found in the log, ordered by `call`.
    reads: Vec<Read>,
    /// The skill names a committed section carries whole, with that
    /// section's range: the dedupe that turns re-reads into pointers.
    /// Rebuilt from the sections on every observe, so resume adoption
    /// keeps it true.
    pinned: HashMap<String, Range>,
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

/// A skill whose body pins: a tool result carrying one stays whole in
/// the block instead of compacting to a digest. Set on the mirror at
/// wiring time, from the same discovery the prompt renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillText {
    /// The skill's folder name.
    pub name: String,
    /// The whole `SKILL.md`, frontmatter included.
    pub body: String,
}

/// One skill read found in the log: the call-and-result span and the
/// skill the result carried.
#[derive(Debug, Clone, Copy)]
struct Read {
    /// The log index of the assistant message holding the call.
    call: usize,
    /// The log index of the tool message carrying the body.
    result: usize,
    /// The skill's index in the mirror's skill list.
    skill: usize,
}

/// The bytes of a detect needle: a body's head, distinct per skill,
/// compared before the whole body is.
const NEEDLE_BYTES: usize = 64;

/// The body's head on a char boundary: the cheap prefilter a result
/// must contain before the whole body is compared.
fn needle(body: &str) -> &str {
    let mut end = NEEDLE_BYTES.min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// One request's wire bytes by category, the proportions the
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

/// Adds one message's estimated tokens by category into `content`, the
/// per-message form of the split the counters and the usage shares
/// both read. The same taxonomy the request shares spread the billed
/// input over, minus the pieces that ride no message.
fn add_message_content(content: &mut LogContent, message: &Message) {
    for part in &message.content {
        match part {
            Content::Text(text) => match message.role {
                Role::User => content.user_text += summary::text_tokens(text.len()),
                Role::Assistant => content.assistant_text += summary::text_tokens(text.len()),
                Role::System | Role::Tool => {}
            },
            Content::ToolCall(call) => {
                content.tool_calls +=
                    summary::text_tokens(call.name.len() + call.args.to_string().len());
            }
            Content::ToolResult(result) => {
                content.tool_results += summary::text_tokens(result.content.len());
                // An image is a vision price, not text: the split
                // prices it as one, and the categories still tile the
                // log's tokens whatever the log holds.
                content.tool_results += result.images.len() as u64 * summary::IMAGE_TOKENS;
            }
            // A bare image rides the message's own turn: its vision
            // price joins that role's bucket, so the split still
            // covers every token a user or assistant message carries.
            Content::Image(_) => match message.role {
                Role::User => content.user_text += summary::IMAGE_TOKENS,
                Role::Assistant => content.assistant_text += summary::IMAGE_TOKENS,
                Role::System | Role::Tool => {}
            },
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
            tokens: Vec::new(),
            images: Vec::new(),
            total_bytes: 0,
            total_tokens: 0,
            content: LogContent::default(),
            cut: 0,
            sections: Vec::new(),
            skills_bytes: 0,
            skills: Vec::new(),
            min_skill_body: usize::MAX,
            reads: Vec::new(),
            pinned: HashMap::new(),
            pending_shares: Shares::default(),
            stats: TranscriptStats::default(),
            saved: None,
        }
    }

    /// Folds a request's messages into the log: an extension of what is
    /// already here extends it; anything else, a fresh session, a
    /// resume, replaces it, and the compaction state rebuilds from
    /// nothing (the first request re-derives it; nothing derived is
    /// ever persisted).
    pub fn observe(&mut self, messages: &[Message]) {
        if messages.len() >= self.log.len() && messages[..self.log.len()] == self.log[..] {
            let first = self.log.len();
            for message in &messages[first..] {
                let bytes = summary::message_bytes(message);
                let tokens = summary::message_tokens(message);
                let images = summary::message_images(message);
                self.sizes.push(bytes);
                self.tokens.push(tokens);
                self.images.push(images);
                self.total_bytes += bytes;
                self.total_tokens += tokens;
                add_message_content(&mut self.content, message);
            }
            self.log.extend_from_slice(&messages[first..]);
            self.scan_reads(first);
        } else {
            self.log = messages.to_vec();
            self.sizes = Vec::with_capacity(self.log.len());
            self.tokens = Vec::with_capacity(self.log.len());
            self.images = Vec::with_capacity(self.log.len());
            self.total_bytes = 0;
            self.total_tokens = 0;
            self.content = LogContent::default();
            for index in 0..self.log.len() {
                let bytes = summary::message_bytes(&self.log[index]);
                let tokens = summary::message_tokens(&self.log[index]);
                let images = summary::message_images(&self.log[index]);
                self.sizes.push(bytes);
                self.tokens.push(tokens);
                self.images.push(images);
                self.total_bytes += bytes;
                self.total_tokens += tokens;
                add_message_content(&mut self.content, &self.log[index]);
            }
            self.cut = 0;
            self.sections.clear();
            self.reads.clear();
            self.scan_reads(0);
        }
        // The saved sections are adopted only when they tile a prefix
        // of the log this mirror now holds: the resume then rides the
        // compaction the previous session already paid for, dense
        // from the first request. Anything else, a truncated log, an
        // edited session, falls back to the rebuild. A fresh
        // mirror's first observe lands here too: its log was empty,
        // so the compare above took the extension branch.
        if let Some(saved) = self.saved.take() {
            let mut end = 0usize;
            // The sections ride the session file: the arithmetic is
            // checked, and a wrap, like a gap or a bad length, just
            // fails the tiling and falls back to the rebuild.
            let tiles = !saved.is_empty()
                && saved.iter().all(|section| {
                    match section.range.id.checked_add(section.range.n) {
                        Some(next) if section.range.id == end => {
                            end = next;
                            end <= self.log.len()
                        }
                        _ => false,
                    }
                })
                && end <= self.log.len();
            if tiles {
                self.sections = saved;
                self.cut = end;
            }
        }
        // The pin map reads the committed state, which sections carry
        // bodies whole decides verbatim versus pointer at the next
        // flush, so it rebuilds whenever the sections can have
        // changed, which includes resume adoption swapping all of
        // them.
        self.pinned = self
            .sections
            .iter()
            .flat_map(|section| {
                section
                    .pinned
                    .iter()
                    .map(|name| (name.clone(), section.range))
            })
            .collect();
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
    /// log's end. The range arrives from the model's tool call, so
    /// the arithmetic is checked: a huge `n` is a malformed call
    /// answered with nothing, not a panic.
    pub fn range(&self, range: Range) -> Option<&[Message]> {
        let end = range.id.checked_add(range.n)?;
        self.log.get(range.id..end)
    }

    /// The compaction's benchmark snapshot.
    pub fn stats(&self) -> TranscriptStats {
        self.stats
    }

    /// The image cap: how many whole images one zoom result may
    /// carry, the same bound the band obeys. One number, so one zoom
    /// cannot smuggle past what a view itself may send.
    pub fn max_view_images(&self) -> usize {
        self.settings.max_view_images
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
    /// stay, the next request rebuilds both anyway. A conversation
    /// cleared mid-agent starts its accounting over.
    pub fn reset_stats(&mut self) {
        self.stats = TranscriptStats::default();
    }

    /// Sets the skills section's byte share of the system prompt,
    /// measured by the agent that rendered it, since the section's
    /// shape is the template's to decide. Zero (the default) folds the
    /// whole system prompt into `system_prompt_tokens`.
    pub fn set_skills_bytes(&mut self, bytes: usize) {
        self.skills_bytes = bytes;
    }

    /// Sets the skills whose bodies pin whole into the block. A tool
    /// result carrying one becomes its own verbatim section at flush
    /// time instead of a digest; a re-read becomes a pointer to the
    /// section already carrying the body. Safe to call at any point,
    /// setting re-detects over the whole log, and normally called
    /// once at wiring, from the same discovery the prompt rendered.
    pub fn set_skills(&mut self, skills: Vec<SkillText>) {
        self.skills = skills;
        self.min_skill_body = self
            .skills
            .iter()
            .map(|skill| skill.body.len())
            .min()
            .unwrap_or(usize::MAX);
        self.reads.clear();
        self.scan_reads(0);
    }

    /// The skill a tool result carries, if any: the length gate first
    /// (a shorter result cannot contain a body), then the head needle,
    /// then the whole body. Matching by content rather than by the
    /// command means any read that returned the whole file qualifies,
    /// `cat`, `sed`, a subagent's result, and no shell line is
    /// parsed.
    fn carried_skill(&self, result: &ToolResult) -> Option<usize> {
        if result.content.len() < self.min_skill_body {
            return None;
        }
        self.skills.iter().position(|skill| {
            result.content.len() >= skill.body.len()
                && result.content.contains(needle(&skill.body))
                && result.content.contains(&skill.body)
        })
    }

    /// Detects skill reads in the log from `from` on, appending to
    /// [`Transcript::reads`] in order. A read needs the pair intact:
    /// the result, and directly before it an assistant message
    /// carrying exactly that one call. A multi-call turn digests
    /// normally rather than splitting a section around one of its
    /// results.
    fn scan_reads(&mut self, from: usize) {
        for index in from..self.log.len() {
            if self.log[index].role != Role::Tool {
                continue;
            }
            for part in &self.log[index].content {
                let Content::ToolResult(result) = part else {
                    continue;
                };
                let Some(skill) = self.carried_skill(result) else {
                    continue;
                };
                // The call directly before: exactly one ToolCall in
                // the turn (a second call's result would orphan behind
                // the pin), and it must be this result's id. Leading
                // text blocks are fine, a real turn narrates.
                let calls = || {
                    self.log[index - 1]
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            Content::ToolCall(call) => Some(call),
                            _ => None,
                        })
                };
                let paired = index > 0
                    && self.log[index - 1].role == Role::Assistant
                    && match (calls().next(), calls().nth(1)) {
                        (Some(call), None) => call.id == result.id,
                        _ => false,
                    };
                if paired {
                    self.reads.push(Read {
                        call: index - 1,
                        result: index,
                        skill,
                    });
                    break;
                }
            }
        }
    }

    /// The read whose call sits at `index`, if any.
    fn read_starting(&self, index: usize) -> Option<Read> {
        let at = self.reads.partition_point(|read| read.call < index);
        self.reads
            .get(at)
            .copied()
            .filter(|read| read.call == index)
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
                        // An image is one message's worth of context,
                        // here and in the view's budget alike.
                        shares.tool_results += result.images.len() * summary::IMAGE_BYTES;
                    }
                    // Shares spreads tokens over the categories it names,
                    // and none of them is a bare image.
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
    /// longer matches, a concurrent plan rebuilt it, and the text is
    /// dropped: every intermediate state stays a valid view.
    pub fn commit_summary(&mut self, pending: &PendingSummary, text: String) -> bool {
        // The index a pending names is where the section sat when the
        // fill was commissioned. A consolidation in a later flush
        // shifts every section below the merge, so the flush that
        // commissioned the fill and the merge that consolidated it
        // cannot both be told by that number. The range is the stable
        // address, sections tile without overlap, so an index that
        // moved is corrected against it and a range that no section
        // holds (a fill that outlived a second merge into a wider
        // union) fails the commit.
        let index = match self.sections.get(pending.section) {
            Some(section) if section.range == pending.range => pending.section,
            _ => match self
                .sections
                .iter()
                .position(|section| section.range == pending.range)
            {
                Some(index) => index,
                None => return false,
            },
        };
        let section = &mut self.sections[index];
        let mut text = compact::cap_summary(text);
        // Pinned bodies ride after the fill: the worker condenses the
        // range's events, the skill text stays whole beneath it.
        for name in &section.pinned {
            if let Some(skill) = self.skills.iter().find(|skill| &skill.name == name) {
                text.push_str("\n\n");
                text.push_str(&compact::skill_section_text(name, &skill.body));
            }
        }
        section.text = text;
        section.origin = SectionOrigin::Model;
        true
    }

    /// The flush planner: decides whether the budget is breached, and
    /// if so moves the cut and commits the new sections, extractive
    /// stand-ins first, returning the summaries a [`Summarizer`]
    /// should upgrade. Between flushes this touches nothing: the block
    /// is append-only by construction.
    ///
    /// Called under the mirror's lock with `observe` already applied;
    /// never blocks and never calls a model.
    pub fn plan_flush(&mut self) -> Vec<PendingSummary> {
        self.sync_sizes();
        let len = self.log.len();
        let tail_start = self.tail_index();

        // The byte budget's say: where the budget breaches, the cut
        // overshoots to half the allowance, so flushes, the only
        // prefix breaks, stay far apart. Below the budget it says
        // nothing. The walk stops at the cut, the block never
        // un-commits.
        let mut byte_target = self.cut;
        if self.total_bytes > self.settings.max_view_bytes {
            let tail_bytes = self.bytes(tail_start, len);
            let allowance = self.settings.max_view_bytes.saturating_sub(tail_bytes);
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
            if c_min > self.cut {
                let half = allowance / 2;
                let mut c_new = c_min;
                let mut band = self.bytes(c_new, tail_start);
                // The smallest position whose remaining band fits half:
                // walk forward while the band still exceeds it. The break
                // on `next > half` was the inverse and left the band at
                // the full allowance, flushes every request.
                while c_new < tail_start && band > half {
                    band -= self.sizes[c_new];
                    c_new += 1;
                }
                byte_target = byte_target.max(c_new);
            }
        }

        // The image cap's say: the band keeps at most
        // `max_view_images` whole images, oldest out, whatever the
        // byte budget says. The flat charge prices an image at one
        // small message, so bytes alone never bound the count, and a
        // provider rejects the request past its own. Both targets
        // stop at the tail: it is never cut.
        let mut image_target = self.cut;
        let mut band_images: usize = self.images[self.cut..len].iter().sum();
        while image_target < tail_start && band_images > self.settings.max_view_images {
            band_images -= self.images[image_target];
            image_target += 1;
        }

        // Whichever bound binds first moves the cut; the band must
        // not open on a tool result either way: its call would stay
        // behind in the last section. Trailing results come along
        // into the block.
        let mut c_new = byte_target.max(image_target);
        let mut pending = Vec::new();
        if c_new > self.cut {
            while c_new < tail_start && self.log[c_new].role == Role::Tool {
                c_new += 1;
            }
            pending = self.extend_sections(self.cut, c_new);
            self.cut = c_new;
            self.stats.flushes += 1;
            pending.extend(self.consolidate());
        }
        pending
    }

    /// The view of the log: `None` passes it through unchanged, below
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
        self.stats.view_images = self.images[self.cut..].iter().sum();
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
        self.stats.block_tokens = summary::text_tokens(self.stats.block_bytes);
        self.stats.band_bytes = self.bytes(self.cut, len);
        self.stats.band_tokens = self.tokens_between(self.cut, len);
        self.stats.tail_bytes = self.bytes(tail_start, len);
        self.stats.tail_tokens = self.tokens_between(tail_start, len);
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
    /// never under the block. No user message at all, an
    /// assistant-only history, and everything from the cut on is the
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

    /// The estimated tokens of `log[from..to]`, from the token cache.
    fn tokens_between(&self, from: usize, to: usize) -> u64 {
        self.tokens[from..to].iter().sum()
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
        self.tokens.truncate(self.log.len());
        self.images.truncate(self.log.len());
        let mut patched = false;
        while self.sizes.len() < self.log.len() {
            let id = self.sizes.len();
            self.sizes.push(summary::message_bytes(&self.log[id]));
            self.tokens.push(summary::message_tokens(&self.log[id]));
            self.images.push(summary::message_images(&self.log[id]));
            patched = true;
        }
        if patched {
            self.total_bytes = self.sizes.iter().sum();
            self.total_tokens = self.tokens.iter().sum();
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
            // A skill read pins its call-and-result span as one
            // section: the first read commits the body whole, a
            // re-read a pointer to the section already carrying it.
            // No pending goes out for it, so no model fill can rewrite
            // the body into a count line.
            if let Some(read) = self.read_starting(at) {
                let range = Range {
                    id: at,
                    n: read.result + 1 - at,
                };
                let name = self.skills[read.skill].name.clone();
                let section = match self.pinned.get(&name) {
                    Some(carried) => Section {
                        range,
                        text: compact::skill_pointer_text(&name, *carried),
                        origin: SectionOrigin::Extractive,
                        pinned: Vec::new(),
                    },
                    None => {
                        self.pinned.insert(name.clone(), range);
                        let body = self.skills[read.skill].body.clone();
                        Section {
                            range,
                            text: compact::skill_section_text(&name, &body),
                            origin: SectionOrigin::Extractive,
                            pinned: vec![name],
                        }
                    }
                };
                self.sections.push(section);
                at = read.result + 1;
                continue;
            }
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
                // A read call opens its own section: close this chunk
                // before it, whatever the budget says.
                if count > 0 && self.read_starting(stop).is_some() {
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
                pinned: Vec::new(),
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
    /// merge into one, the union range re-derived from the log, not
    /// from the summaries, until the block is at half the cap. Each
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
            let messages = match range
                .id
                .checked_add(range.n)
                .and_then(|end| self.log.get(range.id..end))
            {
                Some(messages) => messages.to_vec(),
                // Sections whose arithmetic does not hold against the
                // log (file-derived, adopted past validation): skip
                // the merge rather than panic on it.
                None => return pending,
            };
            let mut text = compact::section_digest(&messages);
            // The union keeps whatever bodies the merged sections
            // carried: the pin outlives the merge, re-appended after
            // the digest so the block never loses a skill's text. The
            // model fill re-appends them again at commit.
            let mut pinned: Vec<String> = Vec::new();
            for old in [&self.sections[0], &self.sections[1]] {
                for name in &old.pinned {
                    if pinned.contains(name) {
                        continue;
                    }
                    // A claim rides only with its body: the pin map
                    // tells a later re-read this section carries the
                    // skill verbatim, so a skill the discovery no
                    // longer holds is dropped from the union's claim
                    // rather than promised here while its text stays
                    // behind in the digest. The log keeps the read;
                    // a restored skill re-pins on its next read.
                    if let Some(skill) = self.skills.iter().find(|skill| &skill.name == name) {
                        text.push_str("\n\n");
                        text.push_str(&compact::skill_section_text(name, &skill.body));
                        pinned.push(name.clone());
                    }
                }
            }
            self.sections.drain(..2);
            self.sections.insert(
                0,
                Section {
                    range,
                    text,
                    origin: SectionOrigin::Extractive,
                    pinned,
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
/// out, never inside it, never under the mirror's lock, so a slow or
/// failing worker delays nothing.
pub struct TranscriptView {
    inner: Arc<dyn ModelProvider>,
    transcript: Arc<Mutex<Transcript>>,
    summarizer: Option<Arc<dyn Summarizer>>,
}

impl TranscriptView {
    /// The wrapper over `inner`, mirroring into `transcript`, the same
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
                            let upgraded = transcript.commit_summary(&pending, text);
                            transcript.record_worker_call(match upgraded {
                                true => WorkerOutcome::Upgraded,
                                false => WorkerOutcome::Dropped,
                            });
                        }
                        None => transcript.record_worker_call(WorkerOutcome::Failed),
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests;
