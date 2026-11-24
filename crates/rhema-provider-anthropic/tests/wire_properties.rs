//! Property tests for the Anthropic wire assembler.
//!
//! A server hands the assembler a stream of [`StreamEvent`]s whose
//! sizes, split points, and payloads it does not control. The contract
//! is that the assembled output and the emitted events depend only on
//! the content those events carry, never on how it was fragmented:
//! text pieces reassemble, tool-call JSON pieces parse back to the
//! args the server sent, the usage tally folds every wire frame, and
//! the thinking block replayed on a call is the one that led to it.

use hegel::TestCase;
use hegel::generators::{self as gs, Generator, PrintableGenerator, TextGenerator};
use rhema_core::{Content, Event, EventSink, Role, StopReason, ToolCall, Usage};
use rhema_provider_anthropic::wire::{
    ChunkAssembler, StreamEvent, WireContentBlock, WireDelta, WireStartMessage, WireUsage,
};

/// A JSON value small enough to serialize fast and varied enough to
/// stress the string splitting: its strings carry the characters JSON
/// escapes, so the split pieces cross escapes, quotes, and astral
/// characters.
#[derive(Debug, Clone)]
enum GenJson {
    Str(String),
    Int(i32),
    Flag(bool),
    List(Vec<GenJson>),
    Map(Vec<(String, GenJson)>),
}

/// Strings with the characters that stress the JSON text round-trip:
/// escapes, quotes, the delimiter set, and astral characters.
fn payload() -> TextGenerator {
    gs::text()
        .min_size(0)
        .max_size(24)
        .alphabet("ab\"\\ \t{}[]:,é😀")
}

/// The recursive generator a derive cannot express: scalars are the
/// leaves, lists and objects the branches.
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

/// One content block of the generated stream: the payload pieces its
/// deltas carry, plus the opening state the `content_block_start` names.
#[derive(Debug, Clone)]
enum ScriptBlock {
    Text {
        pieces: Vec<String>,
    },
    Thinking {
        pieces: Vec<String>,
        signature: Vec<String>,
    },
    Call {
        id: Option<String>,
        name: String,
        args: serde_json::Value,
        pieces: Vec<String>,
    },
}

fn a_block() -> impl PrintableGenerator<ScriptBlock> {
    hegel::one_of!(
        gs::vecs(payload())
            .max_size(6)
            .map(|pieces| ScriptBlock::Text { pieces }),
        hegel::tuples!(
            gs::vecs(payload()).max_size(4),
            gs::vecs(payload()).max_size(2)
        )
        .map(|(pieces, signature)| ScriptBlock::Thinking { pieces, signature }),
        hegel::tuples!(
            gs::optional(gs::text().max_size(8).alphabet("ab-é😀")),
            gs::text().max_size(8).alphabet("ab-é😀"),
            a_json()
        )
        .map(|(id, name, args)| ScriptBlock::Call {
            id,
            name,
            args: to_value(&args),
            pieces: Vec::new(),
        }),
    )
}

/// An interim or final `message_delta`: the usage frame it carries (an
/// event with a `usage` key still counts as a tally frame when every
/// field inside is absent), and the stop reason it names.
#[derive(Debug, Clone)]
struct GenFrame {
    input: Option<u32>,
    output: Option<u32>,
    reads: Option<u32>,
    writes: Option<u32>,
    stop_reason: Option<String>,
}

/// The wire's stop-reason vocabulary, `""` included: the empty string
/// reads as an absent reason.
const REASONS: &[&str] = &[
    "end_turn",
    "tool_use",
    "max_tokens",
    "stop_sequence",
    "pause_turn",
    "refusal",
    "",
];

fn wire_usage(frame: &GenFrame) -> WireUsage {
    WireUsage {
        input_tokens: frame.input,
        output_tokens: frame.output,
        cache_read_input_tokens: frame.reads,
        cache_creation_input_tokens: frame.writes,
    }
}

/// The generated stream, materialized: the opening usage frame, the
/// blocks in their index order, an interim tally frame after each
/// block, and the closing frame that carries the final stop reason.
#[derive(Debug, Clone)]
struct Script {
    start_usage: Option<GenFrame>,
    blocks: Vec<ScriptBlock>,
    interim: Vec<GenFrame>,
    final_frame: GenFrame,
}

/// A frame whose counts stay small enough that folding three of them
/// cannot overflow the tally in a debug build.
fn a_frame() -> impl PrintableGenerator<GenFrame> {
    let count = || gs::optional(gs::integers::<u32>().max_value(100_000));
    let reasons: Vec<String> = REASONS.iter().map(|reason| reason.to_string()).collect();
    hegel::tuples!(
        count(),
        count(),
        count(),
        count(),
        gs::optional(gs::sampled_from(reasons))
    )
    .map(|(input, output, reads, writes, stop_reason)| GenFrame {
        input,
        output,
        reads,
        writes,
        stop_reason,
    })
}

#[hegel::test_helper]
fn a_script(tc: &TestCase) -> Script {
    let mut blocks: Vec<ScriptBlock> = tc.draw(gs::vecs(a_block()).max_size(5));
    // A server correlates a call's deltas by id, so the generated ids
    // stay distinct: a repeated one is dropped and the synthesized
    // per-index id takes over. The assembler itself tracks blocks by
    // index, but this suite reads the deltas back per id.
    let mut seen = std::collections::HashSet::new();
    for block in &mut blocks {
        if let ScriptBlock::Call { id, .. } = block {
            let keep = id
                .as_ref()
                .is_some_and(|id| !id.is_empty() && seen.insert(id.clone()));
            if !keep {
                *id = None;
            }
        }
    }
    // Split each call's arguments at generated char boundaries:
    // deduplicated and capped, so every piece stays inside the text.
    for block in &mut blocks {
        if let ScriptBlock::Call { args, pieces, .. } = block {
            let json = serde_json::to_string(args).expect("serializes");
            let mut cuts: Vec<usize> = tc.draw(
                gs::vecs(gs::integers::<usize>().max_value(json.chars().count())).max_size(6),
            );
            cuts.sort_unstable();
            cuts.dedup();
            let chars: Vec<char> = json.chars().collect();
            let mut bounds = vec![0];
            bounds.extend(cuts);
            bounds.push(chars.len());
            *pieces = bounds
                .windows(2)
                .map(|pair| chars[pair[0]..pair[1]].iter().collect())
                .collect();
        }
    }
    Script {
        start_usage: tc.draw(gs::optional(a_frame())),
        interim: tc.draw(gs::vecs(a_frame()).max_size(4)),
        blocks,
        final_frame: tc.draw(a_frame()),
    }
}

/// The stream's `StreamEvent`s, in the order a well-formed Anthropic
/// stream sends them: `message_start`, then per block the start, its
/// deltas, and its stop (an interim tally frame riding the stop), then
/// the final `message_delta` and `message_stop`.
fn stream_events(script: &Script) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut start = StreamEvent {
        kind: "message_start".into(),
        ..StreamEvent::default()
    };
    start.message = Some(WireStartMessage {
        id: Some("msg_1".into()),
        usage: script.start_usage.as_ref().map(wire_usage),
    });
    events.push(start);

    for (index, block) in script.blocks.iter().enumerate() {
        let index = index as u32;
        let mut start = StreamEvent {
            kind: "content_block_start".into(),
            index: Some(index),
            ..StreamEvent::default()
        };
        start.content_block = Some(match block {
            ScriptBlock::Text { .. } => WireContentBlock {
                kind: "text".into(),
                ..WireContentBlock::default()
            },
            ScriptBlock::Thinking { .. } => WireContentBlock {
                kind: "thinking".into(),
                ..WireContentBlock::default()
            },
            ScriptBlock::Call { id, name, .. } => WireContentBlock {
                kind: "tool_use".into(),
                id: id.clone(),
                name: Some(name.clone()),
                ..WireContentBlock::default()
            },
        });
        events.push(start);

        let deltas: Vec<WireDelta> = match block {
            ScriptBlock::Text { pieces } => pieces
                .iter()
                .map(|piece| WireDelta {
                    kind: Some("text_delta".into()),
                    text: Some(piece.clone()),
                    ..WireDelta::default()
                })
                .collect(),
            ScriptBlock::Thinking { pieces, signature } => pieces
                .iter()
                .map(|piece| WireDelta {
                    kind: Some("thinking_delta".into()),
                    thinking: Some(piece.clone()),
                    ..WireDelta::default()
                })
                .chain(signature.iter().map(|piece| WireDelta {
                    kind: Some("signature_delta".into()),
                    signature: Some(piece.clone()),
                    ..WireDelta::default()
                }))
                .collect(),
            ScriptBlock::Call { pieces, .. } => pieces
                .iter()
                .map(|piece| WireDelta {
                    kind: Some("input_json_delta".into()),
                    partial_json: Some(piece.clone()),
                    ..WireDelta::default()
                })
                .collect(),
        };
        for delta in deltas {
            let mut event = StreamEvent {
                kind: "content_block_delta".into(),
                index: Some(index),
                ..StreamEvent::default()
            };
            event.delta = Some(delta);
            events.push(event);
        }

        let mut stop = StreamEvent {
            kind: "content_block_stop".into(),
            index: Some(index),
            ..StreamEvent::default()
        };
        // An interim frame stays reason-free so the final frame's
        // reason is the one the model checks; its tally rides any
        // event, here the block stop.
        if let Some(frame) = script.interim.get(index as usize) {
            stop.usage = Some(wire_usage(frame));
        }
        events.push(stop);
    }

    let mut final_delta = StreamEvent {
        kind: "message_delta".into(),
        ..StreamEvent::default()
    };
    final_delta.delta = Some(WireDelta {
        stop_reason: script.final_frame.stop_reason.clone(),
        ..WireDelta::default()
    });
    final_delta.usage = Some(wire_usage(&script.final_frame));
    events.push(final_delta);

    events.push(StreamEvent {
        kind: "message_stop".into(),
        ..StreamEvent::default()
    });
    events
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

/// The input-side components a frame has set, mirroring the retained
/// state the assembler keeps between frames.
#[derive(Debug, Default)]
struct Inputs {
    input: u32,
    writes: u32,
    reads: u32,
}

/// One wire frame folded into the running tally, on the inclusive
/// convention the module documents: a component the frame names is
/// overwritten, one it leaves out keeps the value a previous frame set,
/// and the input total is recomputed from the retained components.
fn fold(mut tally: Usage, inputs: &mut Inputs, frame: &GenFrame) -> Usage {
    if let Some(input) = frame.input {
        inputs.input = input;
    }
    if let Some(writes) = frame.writes {
        inputs.writes = writes;
    }
    if let Some(reads) = frame.reads {
        inputs.reads = reads;
    }
    if let Some(output) = frame.output {
        tally.output_tokens = output;
    }
    tally.input_tokens = inputs.input + inputs.writes + inputs.reads;
    tally.cached_input_tokens = inputs.reads;
    tally
}

#[hegel::test(test_cases = 300)]
fn any_split_stream_assembles_to_the_documented_output(tc: TestCase) {
    let script = a_script(&tc);

    let mut assembler = ChunkAssembler::new();
    let mut collector = Collector::default();
    for event in stream_events(&script) {
        assembler
            .push(event, &mut collector)
            .expect("a well-formed stream never errors");
    }
    let output = assembler.finish();

    // The streamed deltas reassemble, whatever the split points: the
    // caller's pane and the committed message carry the same text.
    let text: String = script
        .blocks
        .iter()
        .flat_map(|block| match block {
            ScriptBlock::Text { pieces } => pieces.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(collector.text, text, "text deltas diverged from the pieces");

    let thinking: String = script
        .blocks
        .iter()
        .flat_map(|block| match block {
            ScriptBlock::Thinking { pieces, .. } => pieces.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(
        collector.thinking, thinking,
        "thinking deltas diverged from the pieces"
    );

    // Every call announced itself once, in block order, under the id
    // the start event names (empty and absent ids fall back to the
    // index), and its argument pieces concatenated back to exactly the
    // JSON the server sent.
    let mut expected_starts = Vec::new();
    for (index, block) in script.blocks.iter().enumerate() {
        if let ScriptBlock::Call { id, name, .. } = block {
            let id = id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{index}"));
            expected_starts.push((id, name.clone()));
        }
    }
    assert_eq!(
        collector.starts, expected_starts,
        "call announcements diverged"
    );

    for (index, block) in script.blocks.iter().enumerate() {
        if let ScriptBlock::Call { id, args, .. } = block {
            let id = id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{index}"));
            let got: String = collector
                .args
                .iter()
                .filter(|(delta_id, _)| *delta_id == id)
                .map(|(_, piece)| piece.clone())
                .collect();
            assert_eq!(got, serde_json::to_string(args).unwrap());
        }
    }

    // The tally folds every frame that carried a usage key, in order,
    // and the final output carries the last fold.
    let call_blocks = script
        .blocks
        .iter()
        .filter(|block| matches!(block, ScriptBlock::Call { .. }))
        .count();
    let mut tally = Usage::default();
    let mut inputs = Inputs::default();
    let mut expected_usage = Vec::new();
    if let Some(start) = &script.start_usage {
        tally = fold(tally, &mut inputs, start);
        expected_usage.push(tally);
    }
    // Every interim frame rode a block stop, and there is one stop per
    // block, so the frames beyond the block count never reach the wire.
    for frame in script.interim.iter().take(script.blocks.len()) {
        tally = fold(tally, &mut inputs, frame);
        expected_usage.push(tally);
    }
    tally = fold(tally, &mut inputs, &script.final_frame);
    expected_usage.push(tally);
    assert_eq!(collector.usage, expected_usage, "the tally diverged");
    assert_eq!(output.usage, tally, "the final usage diverged");

    // The committed message: the assembled content and the stop
    // reason, thinking replay riding the calls it led to.
    assert_eq!(output.message.role, Role::Assistant);
    let mut expected_content: Vec<Content> = Vec::new();
    let mut pending: Option<(String, String)> = None;
    for (block_index, block) in script.blocks.iter().enumerate() {
        match block {
            ScriptBlock::Text { pieces } => {
                let text = pieces.concat();
                if !text.is_empty() {
                    expected_content.push(Content::Text(text));
                }
            }
            ScriptBlock::Thinking { pieces, signature } => {
                let text = pieces.concat();
                if !text.is_empty() {
                    pending = Some((text, signature.concat()));
                }
            }
            ScriptBlock::Call { id, name, args, .. } => {
                let provider_data = pending
                    .take()
                    .filter(|(_, signature)| !signature.is_empty())
                    .map(|(thinking, signature)| {
                        serde_json::json!({ "thinking": thinking, "signature": signature })
                    })
                    .unwrap_or(serde_json::Value::Null);
                expected_content.push(Content::ToolCall(ToolCall {
                    id: id
                        .clone()
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| format!("call_{block_index}")),
                    name: name.clone(),
                    args: args.clone(),
                    provider_data,
                }));
            }
        }
    }
    assert_eq!(
        output.message.content, expected_content,
        "the assembled message diverged"
    );

    let expected_stop = if call_blocks > 0 {
        StopReason::ToolUse
    } else {
        match script.final_frame.stop_reason.as_deref() {
            None | Some("") | Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some(other) => StopReason::Other(other.to_owned()),
        }
    };
    assert_eq!(
        output.stop_reason, expected_stop,
        "the stop reason diverged"
    );
}
