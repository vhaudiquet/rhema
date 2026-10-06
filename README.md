# Rhema

ῥῆμα — "the spoken word": words that do things.

Rhema is a small, embeddable, **generic** step engine in Rust for exchanges
with language models. No coding-agent assumptions, no dispatch, no I/O baked
in. The primitive is `step`: ONE exchange — build request → stream → append
assistant message → resolve tool calls → append results. Drivers (a turn
loop, or crate 2's RSD dispatcher) decide everything else.

Spec: [SPEC.md](SPEC.md).

## Status

v1: `rhema-core`, the step engine plus its seams, plus the first consumers.

- `rhema-core` — the step engine and its seams:
  - `Agent::step` — the one-exchange primitive, exact semantics in SPEC.md §4
  - `Session` — caller-owned, fully serializable; checkpointable at any step boundary
  - `ModelProvider` — the only model seam
  - `ToolResolver` — the capability seam; one primitive, not a registry
  - `Event` / `EventSink` — streaming deltas and step lifecycle events
  - `run_turn` — the provided coding-agent driver (~15 lines, not the primitive)
  - `MockProvider` — scripted offline provider, behind the `mock` feature
- `rhema-provider-google` — Gemini adapter: streaming SSE, native function
  calling, `GoogleProvider::new(api_key)` implements `ModelProvider`
- `rhema-coding-agent` — the coding-agent driver: built-in workspace tools
  (`read_file`, `list_dir`, `write_file`, `run_command`), per-turn approval
  gating, session save/resume
- `rhema-cli` — the `rhema` binary: a terminal REPL over the coding agent

## The rhema CLI

```sh
export GEMINI_API_KEY=...        # or GOOGLE_API_KEY
cargo run --bin rhema            # REPL in the current directory
```

```
rhema [OPTIONS] [PROMPT]

  [PROMPT]               one-shot: a single turn, then exit
  -m, --model <MODEL>    model name [default: gemini-2.5-flash]
  -w, --workspace <DIR>  workspace root for tools [default: .]
      --resume <PATH>    resume a saved session file
      --save <PATH>      checkpoint the session after every turn
      --no-confirm       approve gated tool calls without asking
      --max-steps <N>    max steps per turn [default: 16]
```

The model sees four tools, all scoped inside the workspace: `read_file`,
`list_dir`, `write_file`, and `run_command` (`bash -c`, capped output, 120 s
timeout). The two side-effecting ones ask `allow? [y/N]` first unless
`--no-confirm` is set. With `--save`, the session is checkpointed after every
turn and `--resume` continues it later. A failed turn prints one error line
and the REPL keeps going.

The provider has offline tests via a mock HTTP server; the live round-trip
against the real API is deliberately `#[ignore]`d:

```sh
cargo test -p rhema-provider-google --all-features -- --ignored
```

## Usage

```rust
use std::sync::Arc;

use rhema_core::{
    async_trait, run_turn, Agent, AgentError, ChatParams, Content, Event,
    EventSink, Message, MockProvider, ModelOutput, Role, Session, StopReason,
    ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
};

struct Echo;

#[async_trait]
impl ToolResolver for Echo {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new("echo", "Echo the input back", serde_json::json!({}))]
    }
    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success(&call.id, call.args.to_string())
    }
}

// A sink that prints every event; replace with your UI/log/channel.
struct Print;
impl EventSink for Print {
    fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
}

// Any ModelProvider implementation; MockProvider is the scripted offline
// one (feature `mock`). A real HTTP adapter is a future rhema-provider-* crate.
fn provider() -> Arc<dyn rhema_core::ModelProvider> {
    Arc::new(MockProvider::new(vec![ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::Text("done".into())],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]))
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    let mut agent = Agent::builder()
        .provider(provider())
        .resolver(Arc::new(Echo))
        .system("you are a helpful agent")
        .params(ChatParams::new("gemini-2.5-flash"))
        .build()?;
    let mut session = Session::default();
    let ctx = ToolContext::default();
    let input = Message::text(Role::User, "run the echo tool, then summarize");
    let output = run_turn(&mut agent, &mut session, input, &mut Print, &ctx, 8).await?;
    println!("{output:?}");
    Ok(())
}
```

Offline demo with zero network (the mock replays a scripted exchange):

```sh
cargo run -p rhema-core --features mock --example step_demo
```

## Tests

```sh
cargo test --workspace --all-features
```

The suite includes the RSD falsification test (SPEC.md §7 test 5): a
dispatcher holding a `Session` steps, appends an inbox item between steps,
and checkpoints through serde — expressible entirely outside crate 1.

## Roadmap

- Crate 2: RSD `SessionManager` — dispatch, leases, inbox, revisions, SQLite
  (see [specs/resource-scoped-dispatch.md](specs/resource-scoped-dispatch.md))
- Provider crates: `rhema-provider-anthropic`, `rhema-provider-openai-compatible`
- Later: `rhema-tui`, compaction, provider fixtures via cassettes
