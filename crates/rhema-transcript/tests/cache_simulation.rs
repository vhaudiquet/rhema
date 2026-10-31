//! A prompt-cache simulation over a long synthetic session.
//!
//! The transcript's promise past the budget is that the request's
//! prefix stays byte-stable between flushes, so a vendor prompt cache
//! serves most of every call. This test measures that property the way
//! the UniiChat design note measures it: it replays thousands of
//! generated coding-session messages through the transcript, profiles
//! every outgoing request, and prices it against a model of Anthropic's
//! cache. The model: entries exist only at marked positions (the
//! system prompt, the tools, the last two user turns, the positions
//! the Anthropic adapter marks today), a lookup serves the newest
//! earlier entry at or below the request's identical prefix, reads
//! cost 0.1x and writes 1.25x. Entry lifetime is modeled coarsely: a
//! scenario can expire every entry at a step, which stands for a user
//! pausing past the vendor TTL.
//!
//! The assertions pin properties, not measured constants: a byte
//! identical prefix between flushes, a full break at every flush and
//! consolidation, requests back under budget after one, and hit-rate
//! and cost floors with headroom under the measured run. The full
//! report prints with `cargo test -p rhema-transcript -- --nocapture`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelOutput, ModelProvider, ModelRequest,
    ProviderError, Role, StopReason, ToolCall, ToolResult, Usage,
};
use rhema_transcript::summary::message_bytes;
use rhema_transcript::{Transcript, TranscriptSettings, TranscriptView};

/// How many steps the main simulation runs; each step appends one to
/// three messages, so the log reaches a few thousand messages.
const STEPS: usize = 1_200;
/// The decorator-path run's steps: same traffic, shorter, its purpose
/// is the recorded hit rate, not the report.
const DECORATOR_STEPS: usize = 400;
/// A user steering line every this many steps; the cache marks sit on
/// the last two user turns, so this sets how far the readable prefix
/// lags the request's end.
const USER_EVERY: usize = 15;
/// The system prompt's size in the simulated request: byte-stable for
/// the whole run, so its cache entry hits on every call.
const SYSTEM_BYTES: usize = 4_000;
/// The tool definitions' size: byte-stable too, always hit.
const TOOLS_BYTES: usize = 24_000;
/// What a cached byte costs to read, Anthropic's listed rate.
const READ_PRICE: f64 = 0.1;
/// What a new byte costs to write, Anthropic's listed rate.
const WRITE_PRICE: f64 = 1.25;

/// A xorshift64* generator. The run must be identical on every
/// machine, so the test carries its own generator rather than a crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn between(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "result",
    }
}

/// A 64-bit FNV-1a identity of one message: the role plus every
/// content block's bytes. Equal identities mean byte-equal messages
/// for this simulation's purposes, which is all the cache model needs.
fn identity(message: &Message) -> u64 {
    fn push(mut h: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }
    let mut h = push(0xcbf2_9ce4_8422_2325, role_label(message.role).as_bytes());
    for content in &message.content {
        match content {
            Content::Text(text) => h = push(h, text.as_bytes()),
            Content::ToolCall(call) => {
                h = push(h, call.id.as_bytes());
                h = push(h, call.name.as_bytes());
                h = push(h, call.args.to_string().as_bytes());
            }
            Content::ToolResult(result) => {
                h = push(h, result.id.as_bytes());
                h = push(h, if result.is_error { b"1" } else { b"0" });
                h = push(h, result.content.as_bytes());
            }
            Content::Image(image) => h = push(h, image.media_type.as_str().as_bytes()),
        }
    }
    h
}

/// One request's profile: per-message identities, cumulative byte
/// ends, and the cache-mark positions. Marks are what the Anthropic
/// adapter marks: the system block, the last tool definition, and the
/// last blocks of the last two user turns.
struct Profile {
    ids: Vec<u64>,
    /// Cumulative message-space bytes through each message.
    ends: Vec<usize>,
    /// Mark positions, ascending, in the full address space.
    marks: Vec<usize>,
}

/// One request's priced outcome.
struct Priced {
    /// Message bytes identical to the previous request's prefix.
    shared: usize,
    /// Bytes served from the cache.
    read: usize,
    /// Full request bytes, system and tools included.
    total: usize,
}

/// A model of Anthropic's prompt cache: entries at marks only, each
/// lookup reads the newest earlier entry at or below the request's
/// identical prefix, reads at [`READ_PRICE`], writes at
/// [`WRITE_PRICE`]. No TTL: the run never pauses.
#[derive(Default)]
struct CacheSim {
    prev: Option<Profile>,
    /// Summed over the run: full request bytes, prefix bytes, bytes
    /// read, bytes written.
    input: u64,
    prefix: u64,
    read: u64,
    write: u64,
}

impl CacheSim {
    fn price(&mut self, request: &[Message]) -> Priced {
        let mut ids = Vec::with_capacity(request.len());
        let mut ends = Vec::with_capacity(request.len());
        let mut msg_bytes = 0usize;
        let mut user_marks: Vec<usize> = Vec::new();
        for message in request {
            msg_bytes += message_bytes(message);
            ends.push(msg_bytes);
            ids.push(identity(message));
            if message.role == Role::User {
                user_marks.push(SYSTEM_BYTES + TOOLS_BYTES + msg_bytes);
            }
        }
        // Four breakpoints, the API's maximum: system, tools, the last
        // two user turns.
        let mut marks = vec![SYSTEM_BYTES, SYSTEM_BYTES + TOOLS_BYTES];
        let last_two: Vec<usize> = user_marks.into_iter().rev().take(2).collect();
        marks.extend(last_two.into_iter().rev());

        let total = SYSTEM_BYTES + TOOLS_BYTES + msg_bytes;
        let shared = match &self.prev {
            None => 0,
            Some(prev) => {
                let k = prev
                    .ids
                    .iter()
                    .zip(ids.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                if k == 0 { 0 } else { prev.ends[k - 1] }
            }
        };
        // Even after a full break the system and tools bytes are
        // identical, and their entries persist: the constants ride
        // every request's prefix.
        let prefix = if self.prev.is_some() {
            SYSTEM_BYTES + TOOLS_BYTES + shared
        } else {
            0
        };
        let offset = SYSTEM_BYTES + TOOLS_BYTES + shared;
        let read = self
            .prev
            .as_ref()
            .map(|prev| {
                prev.marks
                    .iter()
                    .copied()
                    .filter(|mark| *mark <= offset)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        self.input += total as u64;
        self.prefix += prefix as u64;
        self.read += read as u64;
        self.write += total.saturating_sub(read) as u64;
        self.prev = Some(Profile { ids, ends, marks });
        Priced {
            shared,
            read,
            total,
        }
    }

    /// Assumes every entry expired, as one does when a user stops
    /// thinking past the vendor TTL: the next request reads nothing
    /// and re-writes from scratch.
    fn forget(&mut self) {
        self.prev = None;
    }

    /// Bytes read over bytes that were identical to the previous
    /// request: how much of the prefix the cache served.
    fn prefix_ratio(&self) -> f64 {
        if self.prefix == 0 {
            return 0.0;
        }
        self.read as f64 / self.prefix as f64
    }

    /// Bytes read over all input bytes: the provider's hit rate.
    fn hit_rate(&self) -> f64 {
        if self.input == 0 {
            return 0.0;
        }
        self.read as f64 / self.input as f64
    }

    /// What the run cost against sending the same bytes uncached.
    fn saving(&self) -> f64 {
        let cost = READ_PRICE * self.read as f64 + WRITE_PRICE * self.write as f64;
        if cost == 0.0 {
            return 1.0;
        }
        self.input as f64 / cost
    }
}

/// Appends one step's traffic: a user steering line every
/// [`USER_EVERY`] steps, an assistant turn with a tool call a bit over
/// half the time, and the call's result. Results are small outputs
/// most of the time and file-dump sized the rest.
fn append_step(log: &mut Vec<Message>, rng: &mut Rng, step: usize, calls: &mut u32) {
    if step.is_multiple_of(USER_EVERY) {
        log.push(Message::text(
            Role::User,
            format!(
                "steering #{}: keep going, module {} matters most",
                step,
                rng.below(12)
            ),
        ));
    }
    let mut content = vec![Content::Text(format!(
        "working step {}: {}",
        step,
        ["plan", "edit", "test", "review"][rng.below(4)]
    ))];
    let mut call_id = None;
    if rng.below(100) < 55 {
        *calls += 1;
        let id = format!("c{calls}");
        let name = ["bash", "read", "edit", "grep"][rng.below(4)];
        let args = serde_json::json!({
            "command": format!("cargo test -p crate-{} --features full", rng.below(9)),
            "path": format!("src/module{}/lib.rs", rng.below(20)),
        });
        content.push(Content::ToolCall(ToolCall {
            id: id.clone(),
            name: name.to_owned(),
            args,
            provider_data: serde_json::Value::Null,
        }));
        call_id = Some(id);
    }
    log.push(Message {
        role: Role::Assistant,
        content,
    });
    if let Some(id) = call_id {
        let payload = if rng.below(100) < 25 {
            "x".repeat(rng.between(8_000, 14_000))
        } else {
            format!(
                "ok: compiled {} crates\n{}",
                rng.below(9),
                "log line\n".repeat(rng.below(50))
            )
        };
        let result = if rng.below(100) < 5 {
            ToolResult::failure(id, "exit 1: link failed, missing symbol")
        } else {
            ToolResult::success(id, payload)
        };
        log.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(result)],
        });
    }
}

/// The same traffic and seed for both runs.
fn seeded_run() -> (Rng, Vec<Message>, u32) {
    (
        Rng::new(0x5EED_2026_1031),
        vec![Message::text(
            Role::User,
            "fix the build and keep the tests green",
        )],
        0,
    )
}

/// The per-step cache properties every scenario must hold, plus the
/// aggregates a test pins floors on.
struct SimResult {
    sim: CacheSim,
    stats: rhema_transcript::TranscriptStats,
    messages: usize,
    flush_steps: Vec<usize>,
    max_request_bytes: usize,
    max_request_after_flush: usize,
    /// Bytes written cold by expired-cache requests, summed.
    pause_write: u64,
}

/// Replays [`STEPS`] steps of generated traffic through a transcript
/// with `settings`, pricing every request against a fresh
/// [`CacheSim`], and asserts the per-step properties on the way: the
/// prefix is byte identical between flushes, every flush and
/// consolidation breaks it at byte zero, requests come back under
/// budget after a flush, and a `pause_every` step reads nothing at
/// all, the expired cache's whole bill.
fn simulate(label: &str, settings: TranscriptSettings, pause_every: usize) -> SimResult {
    let mut transcript = Transcript::new(settings);
    let mut sim = CacheSim::default();
    let (mut rng, mut log, mut calls) = seeded_run();

    let mut prev_request_bytes = 0usize;
    let mut max_request_bytes = 0usize;
    let mut max_request_after_flush = 0usize;
    let mut flush_steps: Vec<usize> = Vec::new();
    let mut pause_write = 0u64;

    for step in 0..STEPS {
        append_step(&mut log, &mut rng, step, &mut calls);
        let paused = pause_every > 0 && step > 0 && step.is_multiple_of(pause_every);
        if paused {
            sim.forget();
        }
        let before = transcript.stats();
        let view = transcript.view(&log);
        let after = transcript.stats();
        let request: &[Message] = view.as_deref().unwrap_or(&log);
        let priced = sim.price(request);
        let msg_bytes = priced.total - SYSTEM_BYTES - TOOLS_BYTES;
        max_request_bytes = max_request_bytes.max(msg_bytes);

        let broke =
            after.flushes != before.flushes || after.consolidations != before.consolidations;
        if broke {
            if after.flushes != before.flushes {
                flush_steps.push(step);
                max_request_after_flush = max_request_after_flush.max(msg_bytes);
                assert!(
                    msg_bytes <= settings.max_view_bytes,
                    "{label} step {step}: the request after a flush is {msg_bytes} bytes, over the budget"
                );
            }
            assert_eq!(
                priced.shared, 0,
                "{label} step {step}: a flush or consolidation must break the prefix at byte zero"
            );
        } else if paused {
            assert_eq!(
                priced.read, 0,
                "{label} step {step}: a pause must cold the whole cache, not just the prefix"
            );
            pause_write += u64::try_from(priced.total).unwrap_or(0);
        } else if prev_request_bytes > 0 {
            assert_eq!(
                priced.shared, prev_request_bytes,
                "{label} step {step}: the prefix changed without a flush or consolidation"
            );
        }
        prev_request_bytes = msg_bytes;
    }

    let stats = transcript.stats();
    SimResult {
        sim,
        stats,
        messages: log.len(),
        flush_steps,
        max_request_bytes,
        max_request_after_flush,
        pause_write,
    }
}

fn report(label: &str, result: &SimResult) {
    let sim = &result.sim;
    let stats = result.stats;
    let gaps: Vec<usize> = result.flush_steps.windows(2).map(|w| w[1] - w[0]).collect();
    eprintln!(
        "\n=== {label}: {STEPS} steps, {} messages ===",
        result.messages
    );
    eprintln!(
        "log: {} bytes; requests: {} (pass-through {}, viewed {})",
        stats.log_bytes, stats.requests, stats.pass_through, stats.viewed
    );
    eprintln!(
        "flushes: {} at steps {:?}; gaps {gaps:?}; consolidations: {}",
        stats.flushes, result.flush_steps, stats.consolidations
    );
    eprintln!(
        "sections: {}; block: {} bytes; last band: {} bytes",
        stats.sections, stats.block_bytes, stats.band_bytes
    );
    eprintln!(
        "avg request: {} bytes; max after flush: {}",
        sim.input / stats.requests,
        result.max_request_after_flush
    );
    eprintln!(
        "max request: {} bytes; prefix read: {:.1}%; hit rate: {:.1}%; write: {} bytes; cost saving: {:.2}x",
        result.max_request_bytes,
        sim.prefix_ratio() * 100.0,
        sim.hit_rate() * 100.0,
        sim.write,
        sim.saving()
    );
    if result.pause_write > 0 {
        eprintln!(
            "expired-cache pauses: {} bytes re-written cold",
            result.pause_write
        );
    }
}

#[test]
fn a_long_session_keeps_its_prefix_cached_between_flushes() {
    let settings = TranscriptSettings::default();
    let result = simulate("default settings", settings, 0);
    report("default settings", &result);
    let sim = &result.sim;

    assert!(
        result.stats.flushes >= 3,
        "the workload never engaged the budget: {} flushes",
        result.stats.flushes
    );
    assert!(
        result.max_request_bytes <= settings.max_view_bytes + 20_000,
        "a request reached {} bytes, far past the budget plus one step of growth",
        result.max_request_bytes
    );
    assert!(
        sim.prefix_ratio() >= 0.70,
        "the cache read {:.1}% of the prefix, under the 70% floor",
        sim.prefix_ratio() * 100.0
    );
    assert!(
        sim.hit_rate() >= 0.55,
        "the hit rate {:.1}% fell under the 55% floor",
        sim.hit_rate() * 100.0
    );
    assert!(
        sim.saving() >= 2.0,
        "the cache saved only {:.2}x against uncached sending",
        sim.saving()
    );
}

/// The same run against a tight block cap, so consolidations fire and
/// their full prefix break is priced too: with the default cap the run
/// above never reaches one, and the consolidation path would go
/// unmeasured.
#[test]
fn consolidations_break_the_prefix_and_keep_the_budget() {
    let settings = TranscriptSettings {
        max_block_bytes: 2_500,
        ..TranscriptSettings::default()
    };
    let result = simulate("tight block cap", settings, 0);
    report("tight block cap", &result);

    assert!(
        result.stats.consolidations >= 3,
        "the tight cap produced only {} consolidations",
        result.stats.consolidations
    );
    assert!(
        result.sim.prefix_ratio() >= 0.70,
        "under consolidations the cache read {:.1}% of the prefix, under the 70% floor",
        result.sim.prefix_ratio() * 100.0
    );
    assert!(
        result.sim.hit_rate() >= 0.55,
        "the hit rate {:.1}% fell under the 55% floor",
        result.sim.hit_rate() * 100.0
    );
}

/// The same run with the cache expired every 150 steps: a user who
/// stops to think past the vendor TTL. Each pause re-writes the whole
/// request cold; the assertions say the run stays cheap anyway, and
/// the report shows what the pauses add on top of the flushes.
#[test]
fn pauses_cold_the_cache_and_the_run_stays_cheap() {
    let settings = TranscriptSettings::default();
    let result = simulate("paused cache", settings, 150);
    report("paused cache", &result);

    assert!(
        result.pause_write > 0,
        "the pauses never engaged: {} bytes written cold",
        result.pause_write
    );
    assert!(
        result.sim.prefix_ratio() >= 0.70,
        "with pauses the cache read {:.1}% of the prefix, under the 70% floor",
        result.sim.prefix_ratio() * 100.0
    );
    assert!(
        result.sim.hit_rate() >= 0.75,
        "with pauses the hit rate {:.1}% fell under the 75% floor",
        result.sim.hit_rate() * 100.0
    );
    assert!(
        result.sim.saving() >= 2.0,
        "with pauses the cache saved only {:.2}x against uncached sending",
        result.sim.saving()
    );
}

/// Reports the cache model's numbers as token usage, so the mirror's
/// recorded hit rate can be checked against the same model the
/// property test prices.
struct SimulatedProvider(Arc<Mutex<CacheSim>>);

#[async_trait]
impl ModelProvider for SimulatedProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let priced = self.0.lock().unwrap().price(&req.messages);
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, "ok"),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: (priced.total / 4) as u32,
                cached_input_tokens: (priced.read / 4) as u32,
                output_tokens: 10,
            },
        })
    }
}

struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

#[tokio::test]
async fn the_decorators_recorded_hit_rate_matches_the_cache_model() {
    let sim = Arc::new(Mutex::new(CacheSim::default()));
    let mirror = Arc::new(Mutex::new(Transcript::new(TranscriptSettings::default())));
    let view = TranscriptView::new(Arc::new(SimulatedProvider(sim.clone())), mirror.clone());

    let (mut rng, mut log, mut calls) = seeded_run();
    for step in 0..DECORATOR_STEPS {
        append_step(&mut log, &mut rng, step, &mut calls);
        let req = ModelRequest {
            system: None,
            messages: log.clone(),
            tools: Vec::new(),
            params: ChatParams::new("sim"),
        };
        let mut sink = NoopSink;
        view.complete(req, &mut sink)
            .await
            .expect("the simulated provider answers");
    }

    let recorded = mirror.lock().unwrap().stats();
    let modeled = sim.lock().unwrap().hit_rate();
    let rate = recorded
        .cache_hit_rate()
        .expect("the run recorded usage on every request");
    eprintln!("decorator run: recorded hit rate {rate:.3}, modeled {modeled:.3}");
    assert!(
        (rate - modeled).abs() < 1e-3,
        "the mirror recorded {rate}, the model priced {modeled}"
    );
    assert!(
        rate >= 0.5,
        "the decorator's own hit rate {rate} fell under the floor"
    );
}
