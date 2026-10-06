# Rhema Agent Framework — Design Spec v1.0

*An opinionated alternative to OpenClaw-style agent frameworks, built around a
minimal core, a shared primitives layer, and leaf plugins — informed directly
by concrete failures observed running OpenClaw in production use.*

Companion document: `resource-scoped-dispatch.md` (v1.1) contains the
full rationale, state-machine diagrams, and worked examples for the
SessionManager primitive summarized in §4 below. This document is the
system-level spec; that one is the primitive-level deep dive.

---

## 0. Scope and status

This is a v1 design spec, not an implementation. Sections are marked
**Decided** (a firm invariant, changing it later is expensive) or **Open**
(a real decision still pending, safe to resolve during implementation
without destabilizing the rest of the design).

---

## 0.5 Why build from scratch rather than extend OpenClaw or ZeroClaw

**Decided.** The SessionManager primitive and the channel-as-tool stance
could plausibly be retrofitted onto an existing framework (ZeroClaw's traits
in particular are clean enough that this was a real option, seriously
considered). The reason to build from scratch anyway is not that either
existing framework is structurally incapable of hosting these ideas — it's
that the goal is to own and control the minimal-concept-count invariant
itself, permanently, rather than to patch two specific symptoms onto a
codebase whose scope is set by someone else's roadmap. This is a deliberate
trade of build cost (a second model-provider client, a plugin system, a
persistence layer, all from zero) for architectural ownership, made with
eyes open rather than as an implied side effect of "Rust sounded right."
If the actual goal ever turns out to be narrower than this — just the
primitive, just the Telegram behavior — extending ZeroClaw remains the
cheaper path and is worth revisiting if the from-scratch cost turns out to
be more than it's worth in practice.

## 1. Guiding principles

These aren't aspirational — each one is a direct response to a specific,
observed failure mode in existing frameworks, and every section below is an
application of one or more of them.

1. **The model should never be the thing doing detection, only response.**
   Polling, diffing, matching, and dispatch decisions stay outside the model
   loop as deterministic code. A turn only happens once there's something
   worth reasoning about. *(Response to: heartbeat/cron burning tokens on a
   fixed timer regardless of whether anything changed.)*
2. **Messaging platforms are tools, not sessions.** No external channel is a
   symmetric, session-owning peer to the primary interactive surface.
   *(Response to: multi-channel session fragmentation, and messaging
   platforms as a large unauthenticated inbound attack surface.)*
3. **Same resource, same session, always.** Concurrent access to one
   external resource (an issue, a PR, a thread) is resolved by a single
   shared primitive, not re-derived per integration. *(Response to: every
   plugin author hand-rolling their own dispatch-locking logic.)*
4. **Exclusivity and visibility are separate guarantees.** Only one Handler
   may act on a resource at a time, but new information about that resource
   must reach whoever's working on it immediately, not after they finish.
   *(Response to: naive re-implementation from scratch on every re-dispatch.)*
5. **Context is earned, not injected.** Nothing goes into the model's context
   window by default; it's loaded lazily, scoped to what the current Session
   actually needs. *(Response to: wholesale injection of multiple config
   files into every single turn.)*
6. **Isolation is the deployment's job, not the agent's.** The core carries
   no sandboxing of its own; it assumes it already runs inside a hardened
   deployment boundary, and instead focuses on tool-permission scoping,
   which *is* the agent's job.
7. **A resident process is not the same thing as a process that burns tokens
   on a schedule.** These were conflated under one "heartbeat" feature
   elsewhere; here they're fully decoupled.

## 2. Architecture overview

Three tiers, each with a different stability/change cadence:

```
┌─────────────────────────────────────────────────────────┐
│  PLUGINS   (leaf integrations — optional, many)          │
│  github-gitea-watch · telegram · slack · ...             │
│  → depend on core contracts + specific modules            │
└─────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────┐
│  MODULES   (shared implementations — few, swappable)      │
│  session-store · model-provider(anthropic) ·               │
│  model-provider(openai-compatible) · memory-store          │
│  → implement core-defined contracts                       │
└─────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────┐
│  CORE      (agent loop, tool dispatch, contracts,         │
│             module/plugin loader) — small, stable          │
└─────────────────────────────────────────────────────────┘
```

**Decided:** Core defines *contracts* (traits); modules provide
*implementations*; plugins are *consumers* that also register new tools,
Sources, and skills. A plugin never redefines a contract, only depends on
one. This is the same shape as ZeroClaw's `zeroclaw-api`-plus-crates split —
intentionally reused because it's simply correct structure for this problem,
not because ZeroClaw is otherwise the model to follow.

**Decided:** the SessionManager contract counts as core-required, not
optional — the interactive loop depends on it unconditionally, so in
practice its reference implementation ships bundled with core even though it
lives in its own module crate. What must be singular is the *contract*, not
the shipping order: the v1 step-engine crate (see `../SPEC.md`) deliberately
lands before any SessionManager exists, with the loop's driver standing in
until the SessionManager module lands — a sequencing choice, not a
contradiction.

**Open:** whether a second implementation of a contract can be registered
alongside the default, and how conflicts are resolved if two are present.
Given the single-node/single-user invariants (§7), the pragmatic default is
*exactly one active implementation per contract per deployment*, selected at
build/config time — no runtime multi-provider resolution needed for v1.

## 3. Core: agent loop

The core loop is deliberately unglamorous:

```
loop:
  turn_input   = next input for this Session (user message, tool result,
                 or inbox item delivered via poll_inbox — see §4)
  model_output = provider.complete(session.history + turn_input)
  if model_output is tool_calls:
      for call in tool_calls:
          result = tool_registry.invoke(call, session.tool_allowlist)
          session.history.append(call, result)
      checkpoint(session)      # persist history/plan incrementally
      continue
  else:
      session.history.append(model_output)
      checkpoint(session)
      return model_output       # turn ends on a text-only response
```

**Decided:** the loop itself has no knowledge of Telegram, GitHub, or any
specific integration. It knows Sessions, tools, and a provider. Everything
else is a plugin registering into the tool registry or the Source registry.

**Decided:** `checkpoint()` happens after every tool-call round, not only at
turn end — this is what makes a crash mid-implementation resumable rather
than silently losing work (see RSD spec §7 for the persistence model this
relies on).

**Decided — poll_inbox batching:** `poll_inbox()` is only ever called at
step boundaries the loop was already going to hit (after a tool-call round,
before the next `complete()` call) — it never triggers an extra model call
by itself. When it returns a non-empty batch, however many items it
contains get folded into exactly **one** reasoning step, not one
`complete()` per item. This was ambiguous in v1.0 of this spec; without it,
a busy inbox on a LEASED session silently reintroduces the per-event token
cost the whole primitive exists to avoid.

**Open:** a Handler stuck inside one long-running tool call (waiting on CI,
say) has no natural step boundary to poll at for a while. Whether
long-running tools need their own internal checkpoint-and-poll cadence, or
whether it's acceptable for inbox items to simply wait for the tool call to
return, is unresolved — worth deciding once a real long-running tool exists
to test against rather than speculatively now.

**Open:** exact `checkpoint()` frequency/cost tuning — worth prototyping
against a real workload before fixing a default, as noted in the RSD spec.

## 4. Core: SessionManager (summary)

Full spec lives in `resource-scoped-dispatch.md`. Summary of the
contract as used throughout this document:

```
trait SessionManager {
  fn acquire(key: ResourceKey) -> Result<(Lease, Session, ToolAllowlist), Conflict>
  fn renew(lease: Lease) -> Result<Lease, Expired>
  fn poll_inbox(lease: Lease) -> Vec<InboxItem>
  fn checkpoint(lease: Lease, session: Session) -> Result<(), Conflict>
  fn release(lease: Lease, outcome: Outcome) -> Result<(), Conflict>
}
```

**Decided:** this is the *only* mechanism by which any code — core's own
interactive loop, a Source-driven Handler, or the agent spawning a
sub-session — gets exclusive, resumable, live-interruptible access to an
ongoing unit of work. There is no second locking/scheduling mechanism
anywhere else in the system. Three distinct callers all go through it:

- **External triggers** (§6.1): a Source resolves an event to a
  `ResourceKey` via adapter-supplied `KeyFn`/`ClassifyFn` and calls
  `acquire()`.
- **Inbound human messages** (§6.2): a channel resolves which Session an
  incoming message belongs to, using native threading metadata where
  available, and calls `acquire()` on the resolved (or newly minted) key.
- **Agent-initiated sub-sessions** (§6.3): the agent itself calls `acquire()`
  with a freshly-minted key to delegate a subtask.

**Decided:** `ToolAllowlist` is returned alongside the Session at `acquire()`
time, derived from `ClassifyFn`'s output (§8) — permission scoping happens
before the Handler starts, not as a runtime decision inside it.

## 5. Core: contracts other than SessionManager

```
trait ModelProvider {
  fn complete(history: Vec<Turn>, tools: Vec<ToolSpec>) -> ModelOutput
}

trait Tool {
  fn spec() -> ToolSpec              // name, description, param schema
  fn invoke(args: Value) -> ToolResult
}

trait Source {
  fn register(key_fn: KeyFn, classify_fn: Option<ClassifyFn>)
  // Sources push Events into ingest(); see RSD spec §5.
}
```

**Decided:** tool descriptions live entirely in `ToolSpec`, passed through
the provider's native tool-calling schema. There is no separate prose
document describing tools — that was pure duplication in prior systems (§8).

## 6. Session as the universal primitive

### 6.1 External triggers
Covered fully in the RSD companion spec. A Source (poller or webhook)
resolves events to resource keys and calls `acquire()`; concurrent events on
the same key are delivered live via `poll_inbox()`, never as a second
`acquire()`.

### 6.2 Inbound message resolution
**Decided:** for any channel offering native threading (Telegram
`reply_to_message_id`, Slack `thread_ts`, email `In-Reply-To`, a GitHub
comment already scoped to a thread), `KeyFn` resolves the existing key
deterministically — no heuristic needed.

**Decided:** the live, foreground interactive surface (a terminal or WebUI
session you have open right now) never goes through resolution at all —
every message on an open connection belongs to that connection's Session by
construction.

**Open:** the fallback heuristic for genuinely threadless async re-entry (a
cold DM with no reply metadata). Bias toward minting a new Session when
uncertain — a false "new session" costs the user a reminder; a false
"continuation" silently biases the response. Exact heuristic (recency
window, sender identity, simple keyword overlap) to be decided during
implementation, and treated as a fallback path, not the default mechanism.
Worth stating precisely what this is and isn't: it's deterministic code that
runs only in response to an actual inbound message and never calls the
model itself — the "no heartbeat" invariant (§7) specifically rules out an
*unconditional, periodic, model-invoking* timer, and this fallback is
neither periodic nor model-invoking. It's event-triggered classification,
not a resurrected heartbeat. Naming it clearly this way is worth doing
explicitly so it doesn't get conflated with the mechanism it was built to
replace.

**Open:** session discoverability. Defaulting to "new Session unless clearly
a reply" means Sessions proliferate; a browse/search/manual-merge UI is a
real subsystem this creates, not a free side effect. Not blocking for v1,
but should not be an afterthought.

### 6.3 Agent-initiated sub-sessions
**Decided:** child Sessions carry a `parent_key` field. On `release()`, a
summarized outcome is delivered into the parent Session's Inbox via the same
`poll_inbox()` path any other event uses — "spawn a sub-session, get
notified when done" requires no new mechanism beyond what §4 already
provides.

**Decided:** the agent should reach for sub-session spawning as a first
option for distinct subtasks, rather than working everything inline in one
ever-growing main Session — this is the direct fix for main-session bloat
observed elsewhere.

## 7. Deployment invariants

| Invariant | Status |
|---|---|
| Single user, no multi-tenant ACLs in session keys | **Decided** |
| Single node, single process — no distributed consensus | **Decided.** Sufficient for the intended single-user scope; revisiting this would mean re-architecture, not tuning, so treat it as costly to change rather than literally permanent. |
| Language: Rust | **Decided** |
| Plugins run in-process (compiled/linked in, not sandboxed) | **Decided** |
| No agent-level sandboxing — deployment (e.g. hardened k8s pod) owns isolation | **Decided** |
| Resident daemon owns all Session state | **Decided** |
| No periodic, unconditional model-invoking timer (no heartbeat) | **Decided** |
| Storage: embedded, **SQLite** (`rusqlite`) as the stated default | **Decided.** `sled` was considered but has a less mature ecosystem; only revisit if a real write-contention profile shows up. |

**Storage/contract coupling:** the storage backend is swappable *behind* the
`SessionManager` contract (§4) — a module implements the trait, core and
plugins depend on the trait, not on SQLite directly. "Ships bundled with
core" (§2) describes the reference implementation's default distribution,
not a hardcoded dependency; a plugin pulling in the reference impl pulls in
SQLite as a consequence of using the default module, not because the
contract requires it.

**Note on in-process + no sandboxing:** this decision addresses blast-radius
containment (a compromised plugin can't escape the host) via the deployment
layer, deliberately. It does *not* address tool-permission scoping (can a
given Handler merge a PR, send an external message) — that's a separate,
still-live concern handled in §8, not delegated to infrastructure.

## 8. Tool permission scoping

**Decided:** `ClassifyFn` output (a profile like `AUTHOR` / `REVIEWER` /
`ISSUE_IMPLEMENTER`, or a channel-derived trust level) maps to a
`ToolAllowlist`, attached to the Session at `acquire()` time (§4). The model
never decides its own permissions mid-run; the allowlist is fixed before the
Handler's first reasoning step.

**Open:** the actual profile → allowlist mapping is per-deployment
configuration, not something to hardcode into core. v1 default: permissive
(most profiles get most tools), with scoping tightened per-integration as
real need appears — matches the "give both `merge_pr` and `comment` to
REVIEWER for now" starting point already decided on.

## 9. Context and memory strategy

Replacing the prior "inject several config files every turn" pattern:

| Old pattern | New pattern |
|---|---|
| Multiple always-injected files (identity, tools, environment, memory, heartbeat config, user info) | One small always-injected bootstrap doc: identity, tone, purpose, and an explicit instruction to check memory/tools when relevant |
| Prose tool descriptions in a separate doc | Eliminated — tool schemas already carry name/description/params through the provider's native tool-calling format |
| Heartbeat monitoring config file | Eliminated — no heartbeat mechanism exists in this design |
| Memory as a force-injected file | `memory_search` tool over a real store, invoked by the model only when it decides it's relevant |
| Environment/project context injected globally every turn | Loaded per-Session at Session start, scoped to that Session, the same lazy pattern skills already use |

**Open:** exact memory-store shape — recency-sorted summaries vs. embedding
-based semantic recall. Recency-sorted is simpler to ship first; embeddings
can replace it later behind the same `memory_search` tool interface without
changing anything upstream.

## 10. Model providers

**Decided:** two native `ModelProvider` implementations, not a single
generic OpenAI-shaped client:

- **`anthropic-native`** — speaks the Messages API directly. Required to
  keep extended thinking and prompt caching, both of which matter for a
  Session-accumulating-context workload; both are lost or degraded going
  through an OpenAI-compatibility shim.
- **`openai-compatible`** — covers DeepSeek V4 (Flash/Pro) today, and
  transparently covers any future provider speaking the same wire format
  (Groq, local vLLM, etc.) without a new module.

**Open:** whether per-Session provider selection is a v1 feature (e.g. route
cheap/high-volume Handlers to DeepSeek Flash, reserve Claude for
higher-stakes work) or a later addition once the two providers exist and are
individually proven out.

## 11. Reference plugins (leaf tier)

### 11.1 `github-gitea-watch`
A Source (poller, gated by a cheap deterministic check per the detection
principle in §1) + `KeyFn` (`{host}:{owner}/{repo}#{number}`) + `ClassifyFn`
(AUTHOR/REVIEWER/ISSUE_IMPLEMENTER) + a skill teaching the model the
conversational vocabulary for configuring a watch. Calls the core
SessionManager directly; ships no concurrency logic of its own. Full worked
example (including the mid-implementation-comment scenario) in the RSD spec
§8.5.

### 11.2 `telegram`
**Decided:** Telegram is registered as a **tool** (`notify`,
`ask_approval`, `spawn_background_task`) plus a narrow, signed webhook
`Source` for a small fixed set of inbound trigger types. It is never
registered as a channel with open, symmetric conversational access. This is
the direct fix for both multi-channel session fragmentation and
unauthenticated-inbound attack surface (§1, principle 2).

## 12. Summary of what's genuinely novel here

Relative to OpenClaw and ZeroClaw specifically:

- **SessionManager as a single, core-level, three-caller primitive**
  (external triggers, inbound resolution, agent delegation) — neither
  framework has a unified mechanism for this; OpenClaw has five
  uncoordinated ones (heartbeat, cron, automations, webhooks, hooks) and
  ZeroClaw has none identified.
- **Channels demoted to tools** — a philosophical position neither
  framework takes; both treat messaging platforms as symmetric session
  surfaces.
- **Minimal concept-count as the actual invariant**, not binary size or
  boot time — ZeroClaw is small in bytes/milliseconds while carrying broad
  built-in scope (hardware peripherals, commerce gating, an SOP engine);
  this design is small in the number of things core knows about, full stop.

What's intentionally *not* novel: the three-tier core/module/plugin split
and trait-based contract structure, which mirrors ZeroClaw's own crate
layout because that part of ZeroClaw's engineering is simply sound and worth
reusing rather than reinventing for its own sake.
