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
  calling, live model listing; `GoogleProvider::new(api_key)` implements
  `ModelProvider`
- `rhema-provider-llama-cpp` — llama.cpp `llama-server` adapter (OpenAI-
  compatible streaming chat completions): server URL, optional bearer key
- `rhema-provider-openrouter` — OpenRouter adapter: the full catalog with
  per-model pricing and context lengths, streaming completions by API key
- `rhema-config` — the XDG config file: stored API key (0600) and
  last-used model; env vars always win over the stored key
- `rhema-coding-agent` — the coding-agent driver: built-in workspace tools
  (`read_file`, `list_dir`, `write_file`, `run_command`), per-turn approval
  gating, session save/resume
- `rhema` — the frontend binary: a full-screen terminal UI when attached
  to a terminal, a pipeable one-shot runner when not

## The frontend: `rhema`

```sh
cargo run --bin rhema          # full-screen TUI (or `cargo install`d rhema)
```

### Profiles

Config supports named profiles for contexts like work and personal. A
profile overrides the top-level settings field-wise, so it only needs the
fields where it differs:

```yaml
profile: personal            # used when --profile is not given
default:
  provider: google
  google_api_key: ...
profiles:
  work:
    provider: openrouter
    openrouter_api_key: ...
    model: anthropic/claude-sonnet-4
```

`rhema --profile work` runs the TUI or a headless one-shot under that
profile's provider, keys, and model; logins and model picks while it runs
write back into that profile's section — the same provider can hold
different keys per profile. The `default:` section is what legacy configs
already have, so existing setups keep working unchanged.

Inside the TUI, `/profile` switches mid-session: it lists the profiles
(each in its configured color, with its provider, model, and credential
state), and switching rebuilds the agent while keeping the conversation.
Profiles without a usable key are declined in-band with a hint to /login.
A profile can also set `color: "#RRGGBB"` to choose the tone its name
wears in the footer and picker; unset, personal wears the jade accent and
work the info blue.

First launch opens the login screen: pick a provider — Google Gemini by
API key, or a llama.cpp `llama-server` by URL (optional bearer key;
Anthropic/OpenAI are marked coming soon). The input is validated with a
real model-list call and stored in `~/.config/rhema/config.yaml` with
`0600` permissions. Then pick a model from the live list (text chat
models only — Google rows carry pricing and context window; just type
to filter, arrows to move). A server exposing exactly one model
auto-selects it and skips the picker. After that you land
in chat: responses stream in as rendered markdown (headings, bold,
code in the palette's cyan), `write_file`/`run_command` pop up an
approval modal (y/N), and the status line shows token usage per turn.

Pipeable one-shot mode — plain text on stdout, tool events on stderr,
non-zero exit on failure:

```sh
rhema -p "summarize this diff"            # one turn, then exit
git diff | rhema --print -                # prompt from stdin
rhema -p "run the tests" --no-confirm     # allow gated tools unattended
```

Headless backends resolve from config and environment only (there is no
login prompt): log in once interactively, or set the key env vars. Gated
tools are declined unattended unless `--no-confirm`.

The TUI paints the Jadeite design system — jade-tinted dark ground,
one green accent, status colors reserved for meaning. `--theme light`
switches to the light token set for light terminals; dark is the
design's primary.

`--provider llama-cpp --llama-url http://box:8080` starts on the llama.cpp
backend without logging in; `ctrl-p` still lists that server's models.
Keys: `enter` sends · `ctrl-p` re-opens the model picker (the session is
kept across a model switch) · `esc` declines a pending approval, or asks to
quit when idle · `ctrl-c` quits immediately. Commands: `/help`, `/clear`
(fresh session), `/exit`. The transcript word-wraps and scrolls (`pgup`/
`pgdn` or mouse wheel; terminal text selection works for copy-paste).
Flags: `--workspace`, `--resume`/`--save` (session checkpointing),
`--max-steps`, `--no-confirm`.

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
- Later: compaction, provider fixtures via cassettes
