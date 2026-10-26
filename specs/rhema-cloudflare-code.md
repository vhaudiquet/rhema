# Handoff: implement `rhema-cloudflare-code` — the coding agent as a Cloudflare Durable Object

> Handoff prompt for an implementation session. Everything needed is in this
> file; a fresh session will not have the conversation that produced it.
> Verified facts in §1 were established against this tree by compiling —
> re-verify only if the tree has moved since (git log, then the §1 checks).

## 0. Mission

Add a new frontend consumer of `rhema-coding-agent` (alongside `rhema-code` and
`rhema-acp`) named **`rhema-cloudflare-code`**, under `crates/`. It runs the
coding agent inside a Cloudflare **Durable Object**, with the agent's `bash`
tool executing inside Cloudflare's **Sandbox**
(https://developers.cloudflare.com/agents/tools/sandbox/ — a Durable
Object-on-Container product: exec, file ops, idle sleep, persisted workspace).

Result: a WebSocket/HTTP-addressable coding agent whose sessions checkpoint
into DO storage, whose workspace lives in the sandbox container, and which
survives DO/container eviction via the engine's existing crash-resume machinery.

Non-goal: porting the terminal UI, the claw daemon, or the local-only provider
stack to wasm. Non-goal v1: any provider adapter other than Anthropic.

## 1. Verified ground truth (already established; do not re-derive)

Verified in this tree with `cargo check --target wasm32-unknown-unknown`
(add the target first: `rustup target add wasm32-unknown-unknown`; toolchain
pinned at 1.98.1 in `rust-toolchain.toml`):

**Compiles to wasm today, unchanged:**
- `rhema-core` (deps: serde, serde_json, thiserror, async-trait only — no
  tokio, fs, threads, or time)
- `rhema-provider` (including the transport-agnostic `SseDecoder` in
  `src/stream.rs`, which consumes byte slices)
- `rhema-transcript`
- `rhema-rsd`, `rhema-autonomous-agent`, `rhema-memory`, `rhema-config` with
  `--no-default-features` (excludes the `sqlite` feature)

**Does not compile to wasm, with exact reasons:**
- `rhema-agent-tools` (and transitively `rhema-subagents`):
  `crates/rhema-agent-tools/src/shell.rs:137` hardcodes
  `tokio::process::Command::new("bash")`, and the crate enables tokio's
  `process`/`time` features; `mio` does not build on wasm.
- `rhema-provider-anthropic`, two reqwest-on-wasm issues:
  (a) `connect_timeout` does not exist on wasm (`provider.rs:70`) — fetch has
  no socket knobs; (b) `response.bytes_stream()` (`provider.rs:357`) is backed
  by JS objects (`Rc<RefCell<js_sys::futures::Inner>>`) and therefore `!Send`,
  while `#[async_trait]` requires `Send` futures.

**Why `Send` is non-negotiable:** `EventSink: Send`
(`rhema-core/src/event.rs:80`, reason documented at :78-79) — the sink
reference crosses await points inside provider implementations. All
JS-backed wasm-bindgen values are `!Send` by soundness (raw handles into the
JS heap); this cannot be "fixed upstream". The fix is always bridging: run the
`!Send` stream in `wasm_bindgen_futures::spawn_local` and forward plain
`Vec<u8>` chunks over a channel whose receiver *is* `Send` on single-threaded
wasm (plain data is trivially `Send` there). The `async-compat` crate packages
exactly this pattern.

**Runtime facts (workers):** workerd gives wasm no owned event loop, no
sockets, no filesystem, no Rust-side timers. All I/O goes through host
bindings (`fetch`, DO storage, sandbox bindings). `tokio::time` sleeps do not
fire inside workerd; use `worker::Delay`. reqwest-on-wasm has no streaming
bodies worth using — do not attempt to keep it. rusqlite technically
*compiles* on wasm but cannot open files in workerd — compile ≠ works.

**Relevant existing seams (read these files first):**
- `rhema-core/src/provider.rs` — `ModelProvider::complete(&self, ModelRequest,
  &mut dyn EventSink) -> Result<ModelOutput, ProviderError>`. IMPORTANT:
  `ProviderError::is_retryable()` parses the `"... (HTTP <status>): ..."`
  message shape — any new transport must keep adapters formatting errors that
  way, and must map transport failures to
  `ProviderError::Other("network error: ...")` (that prefix is what makes
  them retryable).
- `rhema-provider/src/retry.rs` — `Retrying` decorator: 3 attempts, 1 s base /
  2 s cap, never retries once an event was emitted, retries an empty success.
  Sleeps via `tokio::time` (tests use `start_paused`; see `retry.rs:320`).
- `rhema-agent-tools/src/shell.rs` — `ShellTools`: the ONLY tool is `bash`
  ("reading, listing, and editing are shell work, not separate tools").
  Output caps: 50 KiB / 2000 lines per stream (`BASH_OUTPUT_LIMIT`,
  `BASH_OUTPUT_LINES`); `BASH_TIMEOUT` = 120 s; approval consulted per call
  from `pending: Mutex<Option<Arc<dyn Approval>>>`; no hook installed =
  reject. `ShellTools::builtin_specs()` is the single source of truth for the
  tool spec, and the system-prompt renderer renders the same specs so the
  prompt and the request's tools array cannot drift — preserve this invariant.
- `rhema-agent-tools/src/approval.rs` — `trait Approval: Send + Sync`
  (+ `AllowAll`, `DenyAll`).
- `rhema-coding-agent/src/agent.rs` — the driver: owns one `ShellTools` for
  the agent lifetime, installs the per-turn approval hook, runs
  `revive_dangling_calls` on resume (committed-but-unanswered calls:
  replay-safe tools re-run, others reported interrupted),
  `stamped_system(base, workspace, extra)` assembles the system prompt
  (discovers skills from the global directory internally; sessions save/load
  via `std::fs` at `agent.rs:233/238`).
- `rhema-core/src/session.rs` — `Session { history, driver_state: Value,
  cost_usd }`: plain serde, checkpoint anywhere.
- `rhema-core/src/tool.rs` — `ToolContext { cwd: PathBuf, run_id,
  cancelled: Arc<AtomicBool> }`; `ReplaySafety` per tool.
- `rhema-rsd/src/store.rs:117` — `trait Store { load(key); save(record,
  expected_revision); keys() }` — revision-guarded persistence over any
  store. A DO is the single writer per key, so the revision check can be
  maintained in-memory + persisted counter.
- `AGENTS.md` — conventions; summary in §5 below.

## 2. Target architecture

```
browser/editor client
   |  WebSocket (chat, streaming deltas, approval prompts) + HTTPS
   v
CodingAgent DO  (workers-rs, wasm32-unknown-unknown; one instance per session id)
   |- turn loop ........... rhema-coding-agent over rhema-core::run_turn
   |- step cost control ... rhema-transcript (on by default)
   |- provider ............ Anthropic Messages over host fetch (new transport)
   |- session checkpoints . DO SQLite storage (rev-guarded, rhema-rsd Store impl)
   |- approvals ........... DenyAll default; WS round-trip or per-session policy
   |- memory/subagents .... rhema-memory tools; subagents inherit the sandbox
   `- bash tool ........... -> Sandbox DO (same session name) over HTTP
                                `- container: bash -c in the workspace
```

One DO instance per session (`idFromName(sessionId)`), binding to the sandbox
instance of the same name. Turn driving v1: the turn runs inside the client's
live request while its WebSocket is connected (streaming deltas live);
checkpoint the session after every step so any eviction resumes via
`revive_dangling_calls`. Alarm-driven step queues are a v2 refinement.

## 3. Layout: one crate, strict module hygiene

The crate is `crates/rhema-cloudflare-code/` (auto-added by the workspace's
`members = ["crates/*"]`). Two families of modules, never mixed:

**Platform-neutral modules** (no wasm-bindgen, no `worker`, no fetch deps —
host-side `cargo test` must compile and run them):
- `sandbox` — `trait Sandbox` (object-safe, async): `exec(ExecRequest) ->
  streamed stdout/stderr lines + exit code` (+ v1: workspace seed — git clone
  URL or tarball upload; file get/put if the skills flow needs it).
  `ExecRequest` carries command, cwd, timeout (echo the 120 s default),
  cancellation. `ShellError` maps transport failure so the bash tool result
  stays an in-band `is_error` result, never an abort.
- `shell` — `ShellSandbox`: the `bash` tool resolver (or the sandbox-side
  `ShellExecutor` impl for Phase 1's seam) exposing the SAME spec as
  `ShellTools::builtin_specs()` — reuse it verbatim (its description says
  "Requires user approval", which must stay true).
- `assembly`/config types — model id, api key source, approval policy,
  agent construction.
- Scripted `Sandbox` impl for offline tests (house pattern: `rhema-core`'s
  `mock` feature), so the full turn loop tests host-side with
  `rhema-core`'s `MockProvider`.
- Every platform-neutral module carries a module-level
  `#![forbid(unsafe_code)]`; the crate carries `#![deny(missing_docs)]`.

**The `worker` module** (and ONLY it): the workers-rs glue — the
`#[durable_object] class CodingAgent`, the host-fetch provider transport, the
`!Send` bridge, the `worker::Delay` sleeper, the DO-storage impls
(`rhema-rsd::Store`, `rhema-memory::MemoryStore`; prefer a SQLite-backed DO
class — KV values cap at 128 KiB and sessions grow; record the workers-rs
version you pin and why, it is 0.x and APIs move).

This module is why the crate cannot carry a crate-level
`#![forbid(unsafe_code)]`: the `#[durable_object]` macro emits unsafe
internally and `forbid` cannot be overridden by `allow` in inner scopes.
Document that exception in `src/lib.rs` (precedent for sanctioned exceptions:
`vendor/crossterm` is excluded for toolchain reasons).

## 4. Phased work plan

### Phase 1 — shell seam in `rhema-agent-tools` (unblocks wasm + is the core abstraction)
- Extract the execution half of `shell.rs` into a seam, e.g.
  `trait ShellExecutor: Send + Sync { async fn run(&self, command: &str,
  cwd: &Path, timeout: Duration, cancelled: &AtomicBool, progress: callback)
  -> Result<Executed, ShellError> }` where `Executed` = stdout tail, stderr
  tail, exit code. `ShellTools` keeps: the spec, the approval gate, caps,
  timeout, error-shaping; it delegates execution.
- Local impl = today's `tokio::process` code verbatim, behind a default-on
  feature (e.g. `local-shell`). The crate's tokio `process`/`time` features
  become feature-dependent so the wasm build excludes them.
- Accept: `cargo check -p rhema-agent-tools -p rhema-subagents --target
  wasm32-unknown-unknown --no-default-features` is clean; native tests green
  (`cargo test -p rhema-agent-tools`); existing `ShellTools` behavior
  unchanged (caps, timeout-kill, line-streaming, approval).

### Phase 2 — session/skills/prompt inputs in `rhema-coding-agent` (small)
- Sessions: the cloud crate serializes `Session` itself; if the fs helpers at
  `agent.rs:233/238` are the only consumers, no seam is needed — confirm and
  note it.
- Skills/system prompt: `stamped_system` discovers skills from the filesystem
  internally. Add an input path that accepts an already-collected skill list
  (keep the fs discovery as the default used by `rhema-code`), so the cloud
  crate can feed skills from DO storage or discovered inside the sandbox.
  SYSTEM.md `base` is already a parameter — the cloud crate supplies it from
  DO storage with the built-in default as fallback.
- Accept: native behavior unchanged; the cloud crate can assemble a full
  agent with zero filesystem access.

### Phase 3 — transport seam in the provider family
- Add a `Transport` trait to `rhema-provider` (it is the adapter-family
  base): request in (method/url/headers/json body) → status + a **Send**
  stream of byte chunks out. Contract: JS types must never leak into the
  trait; on wasm, bridge via `spawn_local` + channel (or `async-compat`).
- `rhema-provider-anthropic`: put reqwest behind a default-on `native`
  feature; take `Arc<dyn Transport>`; move `connect_timeout` and the
  "network error: " mapping into the native impl; keep the
  `(HTTP <status>):` error shape; the `bytes_stream` while-loop becomes
  transport-agnostic over the shared `SseDecoder`.
- `rhema-provider/src/retry.rs`: make `Retrying`'s sleep injectable (additive
  builder, e.g. `with_sleeper`), default = today's tokio behavior so existing
  tests stay green. The cloud crate passes a `worker::Delay` sleeper.
- Accept: `cargo check --workspace --all-features` green natively; wasm check
  green for `rhema-provider` and the anthropic wire module with
  `--no-default-features`; no wire-type duplication between native and fetch
  paths.

### Phase 4 — `rhema-cloudflare-code` itself
- Neutral half first: `sandbox` trait + scripted mock, `ShellSandbox`,
  assembly; host-side e2e (scripted sandbox + `MockProvider`): full turn with
  a bash call, approval round-trip, session checkpoint, and a simulated crash
  mid-command whose resume reports the call interrupted (not re-run).
- Then the `worker` module: DO class `CodingAgent` — WS chat surface (stream
  `Event`s as they arrive; approval requests as WS round-trips with
  deny-on-timeout default and per-session policy: deny-all / read-only
  allowlist / allow-all flag), session checkpointing after every step,
  `run_id` per turn, `ToolContext.cancelled` wired to a disconnect/stop
  message; `alarm` reserved for v2.
- Approval plumbing note: approval fires inside tool resolution inside the
  turn task; the WS handler and turn task share workerd's single thread —
  connect them with channels, never shared-blocking waits.
- Secrets via wrangler secrets (`env.secret(...)`), never DO storage.
  Profiles in DO storage; do not use `rhema-config`'s XDG store here.
- Sandbox HTTP client: call the Sandbox DO over its binding (`stub.fetch()`
  with JSON commands). If the current Sandbox surface is JS-RPC-only, write a
  ~30-line JS shim DO/worker that converts plain JSON HTTP → sandbox calls
  (prefer this over fighting JS RPC from Rust).

### Phase 5 — deploy assets + CI
- `deploy/cloudflare/`: wrangler config (`main = build/worker/shim.mjs`,
  `new_sqlite_classes` migration, DO + sandbox namespace bindings, secrets
  docs), the optional JS shim, README notes — sibling of `docker/` and
  `helm/`.
- Add the wasm gate to CI (scope `ci`): `cargo check` over the cloud closure
  with `--target wasm32-unknown-unknown` (the full crate with the `worker`
  feature is the real build; keep it green).
- Accept: a fresh clone with `wrangler dev` + one secret works.

### Phase 6 — docs & conventions debt
- README: component row for `rhema-cloudflare-code` (the README crate table
  is the product map — keep it in sync). AGENTS.md: add commit scope
  `cloudflare-code`. SPEC.md: the design-decision section (why DO + sandbox;
  the replay-safety implications of container eviction; the approval model;
  the turn-driving model).

## 5. Guardrails (from AGENTS.md and this design — non-negotiable)

- Per commit: `cargo fmt --all`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace --all-features`
  green. Conventional commits with component scopes, linear history (rebase,
  no merge commits).
- Do not weaken `#![forbid(unsafe_code)]` / `#![deny(missing_docs)]` anywhere
  except the sanctioned `worker` module exception (documented in §3).
- No wasm-bindgen, fetch, or `worker` dependencies in `rhema-core`,
  `rhema-coding-agent`, `rhema-agent-tools`; none in the platform-neutral
  modules of `rhema-cloudflare-code`. The platform lives at the edges.
- `rhema-core` keeps exactly its two engine consumers (coding agent, RSD
  agent) — the cloud crate is a new *frontend*, not a new engine consumer.
  Dispatch/lease/persistence belongs in `rhema-rsd`, never in the engine.
- Keep the single `bash` spec as the source of truth shared by prompt and
  tools array; never fork the spec text.
- Do not attempt to make reqwest, `tokio::time`, or rusqlite work inside
  workerd. Host bindings or nothing.

## 6. Open questions to resolve from current Cloudflare docs (verify, don't assume)

1. The Sandbox's exact callable surface today (REST-over-DO-fetch vs JS RPC
   only) and its auth between your DO and the sandbox binding. If RPC-only,
   build the JS shim (Phase 4).
2. Sandbox idle-sleep model and workspace persistence guarantees — map onto
   `ReplaySafety` explicitly: a command killed by container sleep/eviction is
   the committed-unanswered-call case; `bash` is `Unsafe` to replay, so
   resume must report it interrupted (existing machinery — verify the wiring
   reaches it).
3. Current workers-rs DO support level: `DurableObject` trait shape, WS
   hibernation, SQLite (`SqlStorage`) bindings — pin the version accordingly.
4. Current CPU-limit and subrequest-limit numbers (per invocation), and
   whether the target plan's limits fit a live-request turn (~1 model fetch +
   1-2 sandbox calls + approval round-trips per step; a 16-step turn ≈ 30-50
   subrequests). If not, implement the alarm-driven step queue (v2) instead
   of fighting limits.

## 7. Definition of done

- [ ] All three gate commands green: fmt / clippy (-D warnings) / test
      (`--workspace --all-features`), plus the wasm check over the cloud
      closure.
- [ ] Offline e2e test (host-side): scripted sandbox + `MockProvider` → full
      turn with a bash call, approval round-trip, session checkpoint, and a
      simulated crash mid-command whose resume reports the call interrupted
      (not re-run).
- [ ] `wrangler dev` e2e: real Anthropic key, chat drives real bash in the
      sandbox; DO restart resumes the session; workspace files persist.
- [ ] README row, AGENTS.md scope, SPEC.md section landed.
- [ ] A short "running it" section in `deploy/cloudflare/` (secrets, deploy,
      limits, pricing caveat for containers).
