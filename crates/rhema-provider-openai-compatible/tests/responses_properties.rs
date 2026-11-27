//! Property tests for the Responses-API assembler.
//!
//! The wire lets a server shape one call many ways: its name may ride
//! the `added` item or only the `done` item, its arguments may stream
//! as deltas or arrive whole on `done`, a delta may name its call by
//! call id, by item id, or not at all (the most recent call takes it),
//! and reasoning deltas may stream ahead of, between, or after the
//! fragments. The contract, same as the chat side: text reassembles
//! into one part, each call's pieces parse back to the JSON the
//! server sent, a call claims the reasoning pooled when it starts
//! (and when its deltas or its done item find it unstarted), and the
//! terminal event alone settles the usage and the stop reason.

use hegel::TestCase;
use hegel::generators::{self as gs, Generator, PrintableGenerator, TextGenerator};
use rhema_core::{Content, Event, EventSink, Role, StopReason, ToolCall, Usage};
use rhema_provider_openai_compatible::responses::{
    ResponsesAssembler, WireIncompleteDetails, WireInputTokensDetails, WireOutputItem,
    WireResponseBody, WireResponsesUsage, WireStreamEvent,
};

/// Strings with the characters that stress the JSON text round-trip.
fn payload() -> TextGenerator {
    gs::text()
        .min_size(0)
        .max_size(24)
        .alphabet("ab\"\\ \t{}[]:,é😀")
}

#[derive(Debug, Clone)]
enum GenJson {
    Str(String),
    Int(i32),
    Flag(bool),
    List(Vec<GenJson>),
}

fn a_json() -> impl PrintableGenerator<GenJson> {
    gs::recursive(
        hegel::one_of!(
            payload().map(GenJson::Str),
            gs::integers::<i32>().map(GenJson::Int),
            gs::booleans().map(GenJson::Flag),
        ),
        |json| gs::vecs(json.clone()).max_size(3).map(GenJson::List),
    )
}

fn to_value(generated: &GenJson) -> serde_json::Value {
    match generated {
        GenJson::Str(text) => serde_json::Value::String(text.clone()),
        GenJson::Int(int) => serde_json::Value::from(*int),
        GenJson::Flag(flag) => serde_json::Value::Bool(*flag),
        GenJson::List(items) => serde_json::Value::Array(items.iter().map(to_value).collect()),
    }
}

/// One call of the generated stream, with the expectations a compliant
/// assembler must meet: the public id, the announced names, the args
/// JSON, and the reasoning it claims.
#[derive(Debug, Clone)]
struct ScriptCall {
    call_id: Option<String>,
    item_id: Option<String>,
    /// The name on the `added` item (possibly empty: unstarted).
    added_name: String,
    /// The name on the `done` item; drawn only when the added name was
    /// empty, so the done item is what starts the call.
    done_name: Option<String>,
    args: serde_json::Value,
    pieces: Vec<String>,
    /// Whether the `done` item carries the whole arguments text.
    done_args: bool,
    reasoning: String,
    /// The public id: call id, else item id, else `call_{n}`.
    id: String,
}

/// One event of the generated stream, before materialization.
#[derive(Debug, Clone)]
enum GenPiece {
    Text(String),
    /// A reasoning delta, under either spelling.
    Reasoning(String),
    /// `response.output_item.added` for call `i`.
    Added(usize),
    /// An arguments delta for call `i`; `named` spells the ids the
    /// call carries (which may be none, the bare case).
    Delta(usize, bool),
    /// `response.output_item.done` for call `i`.
    Done(usize),
    /// The terminal event, with the usage it carries.
    Terminal(Terminal),
}

#[derive(Debug, Clone)]
struct Terminal {
    completed: bool,
    reason: Option<String>,
    usage: Option<GenUsage>,
}

#[derive(Debug, Clone)]
struct GenUsage {
    input: Option<u32>,
    output: Option<u32>,
    cached: Option<u32>,
}

fn a_usage() -> impl PrintableGenerator<GenUsage> {
    let count = || gs::optional(gs::integers::<u32>().max_value(100_000));
    hegel::tuples!(count(), count(), count()).map(|(input, output, cached)| GenUsage {
        input,
        output,
        cached,
    })
}

#[derive(Debug, Clone)]
struct Script {
    pieces: Vec<GenPiece>,
    calls: Vec<ScriptCall>,
}

#[hegel::test_helper]
fn a_script(tc: &TestCase) -> Script {
    // The calls: distinct non-empty ids where present, so the delta
    // targeting stays unambiguous, the way servers spell it.
    let drawn: Vec<(GenJson,)> = tc.draw(gs::vecs(hegel::tuples!(a_json())).max_size(4));
    let mut calls: Vec<ScriptCall> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (args,) in drawn {
        let mut draw_id = || {
            let id: Option<String> = tc.draw(gs::optional(gs::text().max_size(6).alphabet("ab-é")));
            match id {
                Some(id) if !id.is_empty() && seen.insert(id.clone()) => Some(id),
                _ => None,
            }
        };
        let call_id = draw_id();
        let item_id = draw_id();
        let json = to_value(&args);
        let full = serde_json::to_string(&json).expect("serializes");
        let mut cuts: Vec<usize> =
            tc.draw(gs::vecs(gs::integers::<usize>().max_value(full.chars().count())).max_size(5));
        cuts.sort_unstable();
        cuts.dedup();
        let chars: Vec<char> = full.chars().collect();
        let mut bounds = vec![0];
        bounds.extend(cuts);
        bounds.push(chars.len());
        let pieces: Vec<String> = bounds
            .windows(2)
            .map(|pair| chars[pair[0]..pair[1]].iter().collect())
            .collect();
        calls.push(ScriptCall {
            call_id,
            item_id,
            added_name: tc.draw(gs::text().max_size(8).alphabet("ab-é")),
            done_name: None,
            args: json,
            pieces,
            done_args: tc.draw(gs::booleans()),
            reasoning: String::new(),
            id: String::new(),
        });
    }
    // The public id, resolved at registration: call id, else item id,
    // else the synthesized index. A call unnamed at added may still be
    // named by its done item.
    for (index, call) in calls.iter_mut().enumerate() {
        call.id = call
            .call_id
            .clone()
            .or_else(|| call.item_id.clone())
            .unwrap_or_else(|| format!("call_{index}"));
        if call.added_name.is_empty() && tc.draw(gs::booleans()) {
            call.done_name = Some(tc.draw(gs::text().max_size(8).alphabet("ab-é")));
        }
    }

    // The stream: free text and reasoning up front, then per call its
    // added, its deltas (some spelling the ids, some bare), and its
    // done, with reasoning streamed around the events of a call, then
    // the terminal event. The pool replays the claim rule as the
    // stream is built: a call claims it when it starts, when a delta
    // names it, and when its done item finds it unstarted.
    let mut pieces: Vec<GenPiece> = tc.draw(
        gs::vecs(hegel::one_of!(
            payload().map(GenPiece::Text),
            payload().map(GenPiece::Reasoning),
        ))
        .max_size(6),
    );
    let mut pool = String::new();
    for piece in &pieces {
        if let GenPiece::Reasoning(text) = piece {
            pool.push_str(text);
        }
    }
    for (index, call) in calls.iter_mut().enumerate() {
        let lead = tc.draw(gs::integers::<usize>().max_value(2));
        for _ in 0..lead {
            let text = tc.draw(payload());
            pool.push_str(&text);
            pieces.push(GenPiece::Reasoning(text));
        }
        pieces.push(GenPiece::Added(index));
        if !call.added_name.is_empty() {
            // Started at the added item: it claims the pool.
            call.reasoning = std::mem::take(&mut pool);
        }
        for piece in std::mem::take(&mut call.pieces) {
            // A delta names its call and claims whatever pooled,
            // started or not.
            call.reasoning.push_str(&std::mem::take(&mut pool));
            let named = tc.draw(gs::booleans());
            pieces.push(GenPiece::Delta(index, named));
            call.pieces.push(piece);
        }
        // Reasoning between the call's last delta and its done item
        // still pools for the done item to claim.
        let mid = tc.draw(gs::integers::<usize>().max_value(2));
        for _ in 0..mid {
            let text = tc.draw(payload());
            pool.push_str(&text);
            pieces.push(GenPiece::Reasoning(text));
        }
        if call.added_name.is_empty() {
            // The done item starts the call, whatever it names, and
            // claims the pool.
            call.reasoning.push_str(&std::mem::take(&mut pool));
        }
        pieces.push(GenPiece::Done(index));
    }
    // Reasoning pooled after the last claim never reaches a call.
    let usage = tc.draw(gs::optional(a_usage()));
    pieces.push(GenPiece::Terminal(Terminal {
        completed: tc.draw(gs::booleans()),
        reason: tc.draw(gs::optional(gs::sampled_from(
            ["max_output_tokens", "content_filter", ""]
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        ))),
        usage,
    }));

    Script { pieces, calls }
}

/// The wire events, in order. The delta pieces back-fill here: each
/// `Delta` event takes the next piece of its call, and a bare delta
/// (no ids spelled) is one that arrives without naming its call.
fn events(script: &Script) -> Vec<WireStreamEvent> {
    let mut counters = vec![0usize; script.calls.len()];
    script
        .pieces
        .iter()
        .map(|piece| match piece {
            GenPiece::Text(text) => WireStreamEvent {
                kind: "response.output_text.delta".into(),
                delta: Some(text.clone()),
                ..WireStreamEvent::default()
            },
            GenPiece::Reasoning(text) => WireStreamEvent {
                kind: "response.reasoning_text.delta".into(),
                delta: Some(text.clone()),
                ..WireStreamEvent::default()
            },
            GenPiece::Added(index) => {
                let call = &script.calls[*index];
                WireStreamEvent {
                    kind: "response.output_item.added".into(),
                    item: Some(WireOutputItem {
                        kind: Some("function_call".into()),
                        item_id: call.item_id.clone(),
                        call_id: call.call_id.clone(),
                        name: Some(call.added_name.clone()),
                        ..WireOutputItem::default()
                    }),
                    ..WireStreamEvent::default()
                }
            }
            GenPiece::Delta(index, named) => {
                let call = &script.calls[*index];
                let piece = call.pieces[counters[*index]].clone();
                counters[*index] += 1;
                let (call_id, item_id) = if *named {
                    (call.call_id.clone(), call.item_id.clone())
                } else {
                    (None, None)
                };
                WireStreamEvent {
                    kind: "response.function_call_arguments.delta".into(),
                    delta: Some(piece),
                    call_id,
                    item_id,
                    ..WireStreamEvent::default()
                }
            }
            GenPiece::Done(index) => {
                let call = &script.calls[*index];
                WireStreamEvent {
                    kind: "response.output_item.done".into(),
                    item: Some(WireOutputItem {
                        kind: Some("function_call".into()),
                        item_id: call.item_id.clone(),
                        call_id: call.call_id.clone(),
                        name: Some(
                            call.done_name
                                .clone()
                                .unwrap_or_else(|| call.added_name.clone()),
                        ),
                        arguments: call
                            .done_args
                            .then(|| serde_json::to_string(&call.args).unwrap()),
                        ..WireOutputItem::default()
                    }),
                    ..WireStreamEvent::default()
                }
            }
            GenPiece::Terminal(terminal) => {
                let mut response = WireResponseBody {
                    status: Some(
                        if terminal.completed {
                            "completed"
                        } else {
                            "incomplete"
                        }
                        .into(),
                    ),
                    ..WireResponseBody::default()
                };
                if !terminal.completed {
                    response.incomplete_details =
                        terminal.reason.clone().map(|reason| WireIncompleteDetails {
                            reason: Some(reason),
                        });
                }
                if let Some(usage) = &terminal.usage {
                    response.usage = Some(WireResponsesUsage {
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        input_tokens_details: usage.cached.map(|cached| WireInputTokensDetails {
                            cached_tokens: Some(cached),
                        }),
                    });
                }
                WireStreamEvent {
                    kind: if terminal.completed {
                        "response.completed".into()
                    } else {
                        "response.incomplete".into()
                    },
                    response: Some(response),
                    ..WireStreamEvent::default()
                }
            }
        })
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

#[hegel::test(test_cases = 300)]
fn any_event_order_assembles_to_the_documented_output(tc: TestCase) {
    let script = a_script(&tc);
    let mut assembler = ResponsesAssembler::new();
    let mut collector = Collector::default();
    for event in events(&script) {
        assembler
            .push(event, &mut collector)
            .expect("a well-formed stream never errors");
    }
    let was_completed = assembler.was_completed();
    let output = assembler.finish();

    // The text reassembles whole; the reasoning arrives in order.
    let text: String = script
        .pieces
        .iter()
        .filter_map(|piece| match piece {
            GenPiece::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(collector.text, text, "text deltas diverged from the pieces");
    let thinking: String = script
        .pieces
        .iter()
        .filter_map(|piece| match piece {
            GenPiece::Reasoning(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        collector.thinking, thinking,
        "thinking deltas diverged from the pieces"
    );

    // Every call announced itself exactly once, at the event that
    // started it, under its public id and that event's name.
    let mut expected_starts: Vec<(String, String)> = Vec::new();
    for call in &script.calls {
        if !call.added_name.is_empty() {
            expected_starts.push((call.id.clone(), call.added_name.clone()));
        } else {
            let done_name = call.done_name.clone().unwrap_or_default();
            expected_starts.push((call.id.clone(), done_name));
        }
    }
    assert_eq!(
        collector.starts, expected_starts,
        "call announcements diverged"
    );

    // Each call's deltas concatenate back to exactly the JSON the
    // server sent, whatever the split points and whatever the ids the
    // deltas spelled.
    for call in &script.calls {
        let got: String = collector
            .args
            .iter()
            .filter(|(delta_id, _)| *delta_id == call.id)
            .map(|(_, piece)| piece.clone())
            .collect();
        if !call.pieces.is_empty() {
            assert_eq!(
                got,
                serde_json::to_string(&call.args).unwrap(),
                "argument pieces of {} diverged",
                call.id
            );
        }
    }

    // The committed message: the text, then the named calls in start
    // order, each carrying the reasoning it claimed. A call whose name
    // element never arrived is dropped, the sibling rule: no
    // half-call rides the transcript.
    assert_eq!(output.message.role, Role::Assistant);
    let mut expected_content: Vec<Content> = Vec::new();
    if !text.is_empty() {
        expected_content.push(Content::Text(text.clone()));
    }
    let mut named_calls = 0;
    for call in &script.calls {
        let name = call
            .done_name
            .clone()
            .unwrap_or_else(|| call.added_name.clone());
        if name.is_empty() {
            continue;
        }
        named_calls += 1;
        let provider_data = if call.reasoning.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "reasoning": call.reasoning })
        };
        expected_content.push(Content::ToolCall(ToolCall {
            id: call.id.clone(),
            name,
            args: call.args.clone(),
            provider_data,
        }));
    }
    assert_eq!(
        output.message.content, expected_content,
        "the assembled message diverged"
    );

    // The terminal event alone settles the usage and the stop reason.
    let terminal = script.pieces.iter().find_map(|piece| match piece {
        GenPiece::Terminal(terminal) => Some(terminal.clone()),
        _ => None,
    });
    assert_eq!(was_completed, terminal.is_some(), "the terminal proof lied");
    let expected_tally = terminal.as_ref().and_then(|terminal| {
        terminal.usage.as_ref().map(|usage| Usage {
            cost_usd: None,
            input_tokens: usage.input.unwrap_or(0),
            output_tokens: usage.output.unwrap_or(0),
            cached_input_tokens: usage.cached.unwrap_or(0),
        })
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

    let expected_stop = if named_calls > 0 {
        StopReason::ToolUse
    } else {
        match &terminal {
            None
            | Some(Terminal {
                completed: true, ..
            }) => StopReason::EndTurn,
            Some(Terminal {
                completed: false,
                reason,
                ..
            }) => match reason.as_deref() {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some(other) => StopReason::Other(other.to_owned()),
                None => StopReason::Other("incomplete".to_owned()),
            },
        }
    };
    assert_eq!(
        output.stop_reason, expected_stop,
        "the stop reason diverged"
    );
}
