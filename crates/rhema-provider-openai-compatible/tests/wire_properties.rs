//! Property tests for the OpenAI-compatible chat-completions assembler.
//!
//! A server hands the assembler `chat.completion.chunk`s whose sizes,
//! split points, and payload spellings it does not control: text may
//! arrive as bare strings or typed content chunks, reasoning under
//! either spelling, tool calls as fragments keyed by index, usage on
//! the final chunk. The contract is that the assembled output and the
//! emitted events depend only on the content the chunks carry, never
//! on how it was fragmented: text reassembles into one part (chunk
//! boundaries are transport artifacts), argument pieces parse back to
//! the JSON the server sent, a call claims the reasoning that led to
//! it, and the usage chunk replaces the tally wholesale.

use hegel::TestCase;
use hegel::generators::{self as gs, Generator, PrintableGenerator, TextGenerator};
use rhema_core::{Content, Event, EventSink, Role, StopReason, ToolCall, Usage};
use rhema_provider_openai_compatible::wire::{
    ChatCompletionChunk, ChunkAssembler, OPENAI, OPENAI_COMPATIBLE, WireChoice, WireDelta,
    WireDeltaContent, WireDeltaPart, WireDeltaPartValue, WireFunctionDelta,
    WirePromptTokensDetails, WireToolCallDelta, WireUsage,
};

/// Strings with the characters that stress the JSON text round-trip:
/// escapes, quotes, the delimiter set, and astral characters.
fn payload() -> TextGenerator {
    gs::text()
        .min_size(0)
        .max_size(24)
        .alphabet("ab\"\\ \t{}[]:,é😀")
}

/// A JSON value small enough to serialize fast, recursive where a
/// derive cannot follow.
#[derive(Debug, Clone)]
enum GenJson {
    Str(String),
    Int(i32),
    Flag(bool),
    List(Vec<GenJson>),
    Map(Vec<(String, GenJson)>),
}

fn a_json() -> impl PrintableGenerator<GenJson> {
    gs::recursive(
        hegel::one_of!(
            payload().map(GenJson::Str),
            gs::integers::<i32>().map(GenJson::Int),
            gs::booleans().map(GenJson::Flag),
        ),
        |json| {
            hegel::one_of!(
                gs::vecs(json.clone()).max_size(3).map(GenJson::List),
                gs::vecs(hegel::tuples!(
                    gs::text().max_size(6).alphabet("abé😀"),
                    json
                ))
                .max_size(3)
                .map(GenJson::Map),
            )
        },
    )
}

fn to_value(generated: &GenJson) -> serde_json::Value {
    match generated {
        GenJson::Str(text) => serde_json::Value::String(text.clone()),
        GenJson::Int(int) => serde_json::Value::from(*int),
        GenJson::Flag(flag) => serde_json::Value::Bool(*flag),
        GenJson::List(items) => serde_json::Value::Array(items.iter().map(to_value).collect()),
        GenJson::Map(entries) => entries
            .iter()
            .map(|(key, value)| (key.clone(), to_value(value)))
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into(),
    }
}

/// One tool-call fragment of the generated stream. The first fragment
/// of an index carries the id and the function name; continuations
/// carry only argument text, though servers that echo the id and name
/// on every fragment stay within the contract.
#[derive(Debug, Clone)]
struct Frag {
    index: u32,
    id: Option<String>,
    name: Option<String>,
    piece: Option<String>,
}

/// One delta of the generated stream: a chunk carries one concern, so
/// the model reads the stream linearly.
#[derive(Debug, Clone)]
enum GenPiece {
    /// Visible text, the bare-string content spelling.
    Text(String),
    /// Visible text, inside a parts array.
    PartText(String),
    /// Visible text, a typed `text` chunk inside a parts array.
    TypedText(String),
    /// Reasoning under the DeepSeek/GLM spelling.
    ReasoningFirst(String),
    /// Reasoning under the OpenRouter spelling, alone.
    ReasoningSecond(String),
    /// Both spellings at once: the first wins, the second is ignored.
    ReasoningBoth(String, String),
    /// Reasoning, a typed `thinking` chunk with a text payload.
    ThinkText(String),
    /// Reasoning, a typed `thinking` chunk with inner text items.
    ThinkInner(String),
    /// A tool-call fragment.
    Fragment(Frag),
}

/// One call of the generated stream, with the expectations a compliant
/// assembler must meet: the args its fragments reassemble to, and the
/// reasoning its first fragment claims (or a later fragment absorbs).
#[derive(Debug, Clone)]
struct ScriptCall {
    index: u32,
    id: Option<String>,
    name: String,
    args: serde_json::Value,
    pieces: Vec<Option<String>>,
    reasoning: String,
}

/// The generated stream, materialized: the deltas in order, the
/// finish reason, and the optional final usage chunk.
#[derive(Debug, Clone)]
struct Script {
    pieces: Vec<GenPiece>,
    calls: Vec<ScriptCall>,
    finish_reason: Option<String>,
    usage: Option<GenUsage>,
}

#[derive(Debug, Clone)]
struct GenUsage {
    prompt: Option<u32>,
    completion: Option<u32>,
    cached: Option<u32>,
    deepseek_hit: Option<u32>,
}

/// The wire's finish-reason vocabulary: the documented mappings plus a
/// provider-specific spelling that lands on `Other`, and the empty
/// string that reads as an absent reason.
const FINISH_REASONS: &[&str] = &[
    "stop",
    "tool_calls",
    "function",
    "length",
    "content_filter",
    "custom_reason",
    "",
];

fn a_usage() -> impl PrintableGenerator<GenUsage> {
    let count = || gs::optional(gs::integers::<u32>().max_value(100_000));
    hegel::tuples!(count(), count(), count(), count()).map(|(prompt, completion, cached, hit)| {
        GenUsage {
            prompt,
            completion,
            cached,
            deepseek_hit: hit,
        }
    })
}

fn wire_usage(generated: &GenUsage) -> WireUsage {
    WireUsage {
        prompt_tokens: generated.prompt,
        completion_tokens: generated.completion,
        prompt_tokens_details: generated.cached.map(|cached| WirePromptTokensDetails {
            cached_tokens: Some(cached),
        }),
        prompt_cache_hit_tokens: generated.deepseek_hit,
        prompt_cache_miss_tokens: None,
    }
}

/// The reasoning spellings that feed the unclaimed pool; `Both` only
/// contributes its first member.
fn pool_contribution(piece: &GenPiece) -> Option<String> {
    match piece {
        GenPiece::ReasoningFirst(text)
        | GenPiece::ReasoningSecond(text)
        | GenPiece::ThinkText(text)
        | GenPiece::ThinkInner(text) => Some(text.clone()),
        GenPiece::ReasoningBoth(first, _) => Some(first.clone()),
        _ => None,
    }
}

#[hegel::test_helper]
fn a_script(tc: &TestCase) -> Script {
    // The calls: distinct indices (a server keys fragments by index),
    // each with a valid args JSON split at generated char boundaries.
    let drawn: Vec<(u32, GenJson)> =
        tc.draw(gs::vecs(hegel::tuples!(gs::integers::<u32>().max_value(4), a_json())).max_size(4));
    let mut indices: Vec<u32> = drawn.iter().map(|(index, _)| *index).collect();
    indices.sort_unstable();
    indices.dedup();

    let mut calls: Vec<ScriptCall> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for (index, (_, args)) in indices.iter().zip(drawn.iter()) {
        let json = serde_json::to_string(&to_value(args)).expect("serializes");
        let mut cuts: Vec<usize> =
            tc.draw(gs::vecs(gs::integers::<usize>().max_value(json.chars().count())).max_size(6));
        cuts.sort_unstable();
        cuts.dedup();
        let chars: Vec<char> = json.chars().collect();
        let mut bounds = vec![0];
        bounds.extend(cuts);
        bounds.push(chars.len());
        let pieces: Vec<Option<String>> = bounds
            .windows(2)
            .map(|pair| Some(chars[pair[0]..pair[1]].iter().collect()))
            .collect();
        let id: Option<String> = tc.draw(gs::optional(gs::text().max_size(6).alphabet("ab-nullé")));
        // Distinct, non-empty, not the "null" literal: anything else
        // collapses onto the synthesized per-index id.
        let id = match id {
            Some(id) if !id.is_empty() && id != "null" && seen_ids.insert(id.clone()) => Some(id),
            _ => None,
        };
        calls.push(ScriptCall {
            index: *index,
            id,
            name: tc.draw(gs::text().max_size(8).alphabet("ab-é")),
            args: to_value(args),
            pieces,
            reasoning: String::new(),
        });
    }

    // A free opening run: text and reasoning, no fragments yet. The
    // reasoning pools unclaimed until a call's first fragment takes it.
    let mut pieces: Vec<GenPiece> = tc.draw(
        gs::vecs(hegel::one_of!(
            payload().map(GenPiece::Text),
            payload().map(GenPiece::PartText),
            payload().map(GenPiece::TypedText),
            payload().map(GenPiece::ReasoningFirst),
            payload().map(GenPiece::ReasoningSecond),
            hegel::tuples!(payload(), payload())
                .map(|(first, second)| GenPiece::ReasoningBoth(first, second)),
            payload().map(GenPiece::ThinkText),
            payload().map(GenPiece::ThinkInner),
        ))
        .max_size(8),
    );
    let mut pool = String::new();
    for piece in &pieces {
        if let Some(text) = pool_contribution(piece).filter(|text| !text.is_empty()) {
            pool.push_str(&text);
        }
    }

    // The fragments, call by call in a drawn order, so chunk order and
    // index order can disagree. The pool is replayed fragment by
    // fragment, the claim rule the module documents: a call's first
    // fragment claims everything pooled before it; reasoning that
    // streams between a call's own fragments lands on that call; what
    // streams after its last fragment stays pooled for the next one,
    // and what is left at the end never reaches a call at all.
    let order: Vec<usize> = tc.draw(gs::permutations((0..calls.len()).collect::<Vec<_>>()));
    let echoes: Vec<bool> = tc.draw(
        gs::vecs(gs::booleans())
            .min_size(calls.len())
            .max_size(calls.len()),
    );
    for call_index in order {
        let lead = tc.draw(gs::integers::<usize>().max_value(2));
        for _ in 0..lead {
            let piece = GenPiece::ReasoningFirst(tc.draw(payload()));
            if let Some(text) = pool_contribution(&piece).filter(|text| !text.is_empty()) {
                pool.push_str(&text);
            }
            pieces.push(piece);
        }
        let call = &mut calls[call_index];
        let echo = echoes[call_index];
        let fragments = std::mem::take(&mut call.pieces);
        let mut first = true;
        for piece in fragments {
            // The fragment's claim, replayed exactly: take the pool,
            // claim it whole on the first fragment, append it on the
            // interleaved ones.
            let pending = std::mem::take(&mut pool);
            if first {
                call.reasoning = pending;
            } else if !pending.is_empty() {
                call.reasoning.push_str(&pending);
            }
            let (id, name) = if first || echo {
                (call.id.clone(), Some(call.name.clone()))
            } else {
                (None, None)
            };
            first = false;
            pieces.push(GenPiece::Fragment(Frag {
                index: call.index,
                id,
                name,
                piece,
            }));
            // Reasoning between a call's own fragments reaches the
            // caller live and pools for the call's next fragment.
            let mid = tc.draw(gs::integers::<usize>().max_value(2));
            for _ in 0..mid {
                let piece = GenPiece::ReasoningFirst(tc.draw(payload()));
                if let Some(text) = pool_contribution(&piece).filter(|text| !text.is_empty()) {
                    pool.push_str(&text);
                }
                pieces.push(piece);
            }
        }
    }
    // Reasoning left after the last group stays unclaimed: the model
    // never ascribes it to a call, though it still reached the caller.

    Script {
        pieces,
        calls,
        finish_reason: tc.draw(gs::optional(gs::sampled_from(
            FINISH_REASONS
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        ))),
        usage: tc.draw(gs::optional(a_usage())),
    }
}

/// The chunks, in wire order: one concern per content chunk, then the
/// finish-reason chunk, then the usage chunk.
fn chunks(script: &Script) -> Vec<ChatCompletionChunk> {
    script
        .pieces
        .iter()
        .map(|piece| {
            let mut delta = WireDelta::default();
            match piece {
                GenPiece::Text(text) => delta.content = Some(WireDeltaContent::Text(text.clone())),
                GenPiece::PartText(text) => {
                    delta.content = Some(WireDeltaContent::Parts(vec![WireDeltaPartValue::Text(
                        text.clone(),
                    )]))
                }
                GenPiece::TypedText(text) => {
                    delta.content = Some(WireDeltaContent::Parts(vec![WireDeltaPartValue::Part(
                        WireDeltaPart {
                            kind: Some("text".into()),
                            text: Some(text.clone()),
                            thinking: None,
                        },
                    )]))
                }
                GenPiece::ReasoningFirst(text) => delta.reasoning_content = Some(text.clone()),
                GenPiece::ReasoningSecond(text) => delta.reasoning = Some(text.clone()),
                GenPiece::ReasoningBoth(first, second) => {
                    delta.reasoning_content = Some(first.clone());
                    delta.reasoning = Some(second.clone());
                }
                GenPiece::ThinkText(text) => {
                    delta.content = Some(WireDeltaContent::Parts(vec![WireDeltaPartValue::Part(
                        WireDeltaPart {
                            kind: Some("thinking".into()),
                            text: Some(text.clone()),
                            thinking: None,
                        },
                    )]))
                }
                GenPiece::ThinkInner(text) => {
                    delta.content = Some(WireDeltaContent::Parts(vec![WireDeltaPartValue::Part(
                        WireDeltaPart {
                            kind: Some("thinking".into()),
                            text: None,
                            thinking: Some(vec![WireDeltaPartValue::Text(text.clone())]),
                        },
                    )]))
                }
                GenPiece::Fragment(fragment) => {
                    delta.tool_calls = Some(vec![WireToolCallDelta {
                        index: fragment.index,
                        id: fragment.id.clone(),
                        function: Some(WireFunctionDelta {
                            name: fragment.name.clone(),
                            arguments: fragment.piece.clone(),
                        }),
                    }])
                }
            }
            ChatCompletionChunk {
                choices: vec![WireChoice {
                    delta: Some(delta),
                    finish_reason: None,
                }],
                usage: None,
                error: None,
            }
        })
        .chain(std::iter::once(ChatCompletionChunk {
            choices: vec![WireChoice {
                delta: None,
                finish_reason: script.finish_reason.clone(),
            }],
            usage: None,
            error: None,
        }))
        .chain(script.usage.iter().map(|usage| ChatCompletionChunk {
            choices: Vec::new(),
            usage: Some(wire_usage(usage)),
            error: None,
        }))
        .collect()
}

/// Records what crossed to the caller, in stream order.
#[derive(Default)]
struct Collector {
    text: String,
    thinking: String,
    starts: Vec<(String, String)>,
    args: Vec<(String, String)>,
    usage: Vec<Usage>,
}

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        match event {
            Event::TextDelta(delta) => self.text.push_str(delta),
            Event::ThinkingDelta(delta) => self.thinking.push_str(delta),
            Event::ToolCallStart { id, name } => self.starts.push((id.to_owned(), name.to_owned())),
            Event::ToolCallArgsDelta { id, delta } => {
                self.args.push((id.to_owned(), delta.to_owned()))
            }
            Event::Usage(usage) => self.usage.push(usage),
            _ => {}
        }
    }
}

/// A call's public id: the first fragment's id, with the empty and
/// "null" spellings and an absent id falling back to the index.
fn public_id(call: &ScriptCall) -> String {
    call.id
        .clone()
        .filter(|id| !id.is_empty() && id != "null")
        .unwrap_or_else(|| format!("call_{}", call.index))
}

#[hegel::test(test_cases = 300)]
fn any_chunk_split_assembles_to_the_documented_output(tc: TestCase) {
    let script = a_script(&tc);

    for dialect in [OPENAI_COMPATIBLE, OPENAI] {
        let mut assembler = ChunkAssembler::new(&dialect);
        let mut collector = Collector::default();
        for chunk in chunks(&script) {
            assembler
                .push(chunk, &mut collector)
                .expect("a well-formed stream never errors");
        }
        let output = assembler.finish();

        // The visible text reassembles into one part, whatever the
        // spelling each piece arrived under.
        let text: String = script
            .pieces
            .iter()
            .filter_map(|piece| match piece {
                GenPiece::Text(text) | GenPiece::PartText(text) | GenPiece::TypedText(text) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(collector.text, text, "text deltas diverged from the pieces");

        // The reasoning reassembles under the first spelling that
        // carries a piece; with both spellings present the second is
        // ignored.
        let thinking: String = script.pieces.iter().filter_map(pool_contribution).collect();
        assert_eq!(
            collector.thinking, thinking,
            "thinking deltas diverged from the pieces"
        );

        // Every index announced itself once, at its first fragment,
        // under the id that fragment named and the name it carried.
        let mut expected_starts: Vec<(String, String)> = Vec::new();
        let mut started: Vec<u32> = Vec::new();
        for piece in &script.pieces {
            if let GenPiece::Fragment(fragment) = piece {
                if started.contains(&fragment.index) {
                    continue;
                }
                started.push(fragment.index);
                let id = fragment
                    .id
                    .clone()
                    .filter(|id| !id.is_empty() && id != "null")
                    .unwrap_or_else(|| format!("call_{}", fragment.index));
                let name = fragment
                    .name
                    .clone()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_default();
                expected_starts.push((id, name));
            }
        }
        assert_eq!(
            collector.starts, expected_starts,
            "call announcements diverged"
        );

        // Each call's argument pieces concatenate back to exactly the
        // JSON the server sent, whatever the fragment boundaries.
        for call in &script.calls {
            let got: String = collector
                .args
                .iter()
                .filter(|(delta_id, _)| *delta_id == public_id(call))
                .map(|(_, piece)| piece.clone())
                .collect();
            assert_eq!(
                got,
                serde_json::to_string(&call.args).unwrap(),
                "arguments of call {} diverged",
                call.index
            );
        }

        // The committed message: the buffered text, then the named
        // calls in ascending index order (chunk order may differ), each
        // call carrying the reasoning it claimed. A call whose name
        // element never arrived is dropped, the sibling rule: no
        // half-call rides the transcript nor forces a tool-use verdict.
        assert_eq!(output.message.role, Role::Assistant);
        let mut expected_content: Vec<Content> = Vec::new();
        if !text.is_empty() {
            expected_content.push(Content::Text(text.clone()));
        }
        let mut ordered = script.calls.clone();
        ordered.sort_by_key(|call| call.index);
        let mut named_calls = 0;
        for call in &ordered {
            if call.name.is_empty() {
                continue;
            }
            named_calls += 1;
            let provider_data = if call.reasoning.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!({ "reasoning": call.reasoning })
            };
            expected_content.push(Content::ToolCall(ToolCall {
                id: public_id(call),
                name: call.name.clone(),
                args: call.args.clone(),
                provider_data,
            }));
        }
        assert_eq!(
            output.message.content, expected_content,
            "the assembled message diverged under {:?}",
            dialect.args_key
        );

        // The usage chunk replaces the tally wholesale: the event
        // carries it, the output carries it, absent fields read zero,
        // and DeepSeek's top-level cache split counts as cached.
        let expected_tally = script.usage.as_ref().map(|usage| Usage {
            input_tokens: usage.prompt.unwrap_or(0),
            output_tokens: usage.completion.unwrap_or(0),
            cached_input_tokens: usage.cached.or(usage.deepseek_hit).unwrap_or(0),
        });
        assert_eq!(
            collector.usage,
            expected_tally.iter().copied().collect::<Vec<_>>(),
            "the tally events diverged"
        );
        assert_eq!(
            output.usage,
            expected_tally.unwrap_or_default(),
            "the final usage diverged"
        );

        // The stop reason: a streamed named call outranks the wire's
        // final reason, the siblings' convention.
        let expected_stop = if named_calls > 0 {
            StopReason::ToolUse
        } else {
            match script.finish_reason.as_deref() {
                None | Some("") | Some("stop") => StopReason::EndTurn,
                Some("tool_calls") | Some("function") => StopReason::ToolUse,
                Some("length") => StopReason::MaxTokens,
                Some(other) => StopReason::Other(other.to_owned()),
            }
        };
        assert_eq!(
            output.stop_reason, expected_stop,
            "the stop reason diverged"
        );
    }
}
