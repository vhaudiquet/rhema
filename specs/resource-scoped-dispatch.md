# Resource-Scoped Dispatch (RSD)

*A core primitive for safely turning external events into agent sessions, without
every integration reinventing session-concurrency by hand.*

> **Revision note (v1.1):** v1 guaranteed exclusivity (no two Handlers racing
> on the same key) but treated every dispatch as a fresh, stateless Handler
> invocation and made events invisible to an already-running Handler until it
> released. Both are wrong for realistic workloads: a resource key isn't just
> a lock, it's an ongoing conversation, and new information that arrives
> mid-task needs to reach the agent *while it's working*, not after. v1.1
> replaces the invisible Queue with a live Inbox and binds each ResourceKey
> to a persistent Session rather than a throwaway context. Changes are
> integrated inline below; §4 and §5 are the ones that actually changed.

## 1. Problem statement

Any "watch X and react" workflow — GitHub notifications, Gitea, Linear, an email
thread, a Slack channel — has the same shape once you look past the surface API:

1. Events arrive from an external source, either pushed (webhook) or pulled (poller).
2. Events refer to some **external resource** (an issue, a PR, a thread) that
   already has state and history.
3. More than one event can refer to the same resource before the first one has
   been fully handled (a user comments again while the agent is mid-review).
4. Handling an event usually means spawning an agent/subagent session that
   *reasons* about the resource — which is slow (seconds to minutes) relative
   to how fast new events can arrive (milliseconds to seconds).

Every framework that doesn't solve this generically pushes the problem onto
whoever writes the integration. The result, observed independently in at least
two places (a hand-rolled GitHub-notifications poller, a community Linear
plugin with its own dispatch-state table and attempt-number validation), is the
same bespoke state machine rebuilt from scratch per integration. That's the
signal that it's a missing core primitive, not a missing skill.

**Goal:** give every trigger adapter (GitHub, Gitea, Telegram inbound, email,
whatever) a shared guarantee — *at most one live handler per resource key, ever*
— so adapter authors write a key extractor and a classifier, not a lock.

## 2. Design goals

- **At-most-one-active-handler per resource key.** No two sessions reason
  about the same issue/PR/thread concurrently.
- **No event silently dropped or clobbered.** Events that arrive while a
  handler is active are queued, not discarded and not raced.
- **No stuck resources.** A crashed or hung handler must not permanently wedge
  a resource key.
- **No wasted LLM calls.** Detection (is there a new event, does it matter) is
  fully separate from response (spawn a handler) — consistent with the
  gate-before-LLM principle from earlier: dispatch decisions are deterministic
  code, only the handler invokes the model.
- **Adapter-agnostic core, adapter-specific edges.** The primitive doesn't know
  what GitHub or Telegram is. It knows about keys, events, leases, and queues.
- **Survives restarts.** Dispatch state is a durable table, not in-memory only.

## 3. Vocabulary

| Term | Meaning |
|---|---|
| **Source** | Anything that produces raw events: a poller (cron+gate script) or a webhook receiver. |
| **Event** | One unit of "something happened," with a payload and a dedup id. |
| **ResourceKey** | A string identifying the external thing the event is about, e.g. `github:acme/widgets#142`. |
| **KeyFn** | `Event -> ResourceKey`. Pure, adapter-supplied. |
| **ClassifyFn** | `Event -> Classification` (optional). Adapter-supplied. Produces things like `{profile: "REVIEWER", priority: "normal"}`. |
| **Lease** | A time-bounded, renewable claim on a ResourceKey held by one running Handler. |
| **Handler** | The actual agent/subagent invocation that reasons about the resource and takes action. |
| **Session** | Persistent state tied 1:1 to a ResourceKey: conversation history, plan, and pointers to physical artifacts (e.g. a git worktree/branch). Survives across Lease acquisitions. |
| **Inbox** | A live, append-only mailbox on a Session. Events land here *immediately*, whether or not a Lease is currently held, and a running Handler polls it between reasoning steps. |
| **Revision** | Monotonically increasing counter per ResourceKey, bumped on every Lease acquisition. |

## 4. State machine (per ResourceKey)

The key insight in v1.1: **exclusivity and visibility are separate axes.**
Only one Handler may ever be *acting* on a key at once (exclusivity — this
part of v1 was right). But information about the key should reach whoever is
currently working on it *immediately*, regardless of whether a Lease is held
(visibility — this part of v1 was wrong).

```
                 event arrives, no lease held
        ┌────────────────────────────────────────┐
        │                                         ▼
   ┌─────────┐   acquire() (create-or-resume     ┌───────────┐
   │  IDLE   │   Session, new revision)          │  LEASED   │
   └─────────┘ ───────────────────────────────▶  └───────────┘
        ▲                                          │   │  ▲
        │ release(), inbox empty                   │   │  │ renew() (heartbeat)
        │                                           │   │  │ poll_inbox() every step
        │        release(), inbox non-empty         │   │  └───────────────┐
        │        ┌───────────────────────────────────┘   │                 │
        │        ▼                                        │                 │
        │   ┌───────────┐   lease TTL expires,            │                 │
        │   │ RE-ACQUIRE │   no renew()                    │                 │
        │   │ (debounce  │◀─────────────────────────────────────────────────┘
        │   │  window)   │
        │   └─────┬──────┘
        │         │ acquire() — SAME Session resumed, new revision
        └─────────┴──────────────────────────────────▶ back to LEASED

   event arrives while LEASED:
     └──▶ appended directly to the Session's Inbox (no state transition,
          no debounce — the running Handler will see it at its next
          poll_inbox() call, typically between tool calls)
```

Key properties:

- A ResourceKey is in exactly one of `IDLE`, `LEASED`, `RE-ACQUIRE` at any
  time — that part is unchanged from v1.
- `acquire()` is still the only way to move IDLE → LEASED and still atomically
  bumps the revision counter (same optimistic-concurrency shape as before).
  **What's new:** `acquire()` now returns `(Lease, Session)`, where `Session`
  is loaded from durable storage if one already exists for this key, rather
  than a fresh empty context every time. Conversation history, the agent's
  current plan, and pointers to physical state (git worktree, branch, files
  touched so far) all live on the Session, not the Lease, so they survive
  across every re-acquisition.
- Events arriving during `LEASED` no longer wait invisibly — they're appended
  to the Session's Inbox the instant they're ingested. It is the Handler's
  responsibility to call `poll_inbox()` at reasoning-step boundaries (a cheap,
  non-LLM check) and fold any new items into its next turn exactly as if the
  user had spoken up mid-conversation. **The dispatcher never decides whether
  new information should change the plan — it only guarantees delivery.**
  That judgment call stays with the model, consistent with keeping detection
  mechanical and response agentic.
- Debouncing only applies at the IDLE→LEASED transition (coalescing a burst
  of events into one fresh dispatch when nobody is currently working) and at
  RE-ACQUIRE after a release with a non-empty inbox. It never applies to
  events landing on an already-`LEASED` key — those are delivered immediately
  because the cost of delivery is just an append, not a dispatch.
- Lease TTL + required periodic `renew()` calls still guard against a
  crashed/hung Handler wedging the key forever, unchanged from v1.

## 5. Core API surface (language-agnostic)

```
type ResourceKey = string
type Revision     = u64
type LeaseId      = uuid

struct Event {
  id: string            // dedup id: webhook delivery id, notification id, etc.
  source: string         // "github", "gitea", "telegram-inbound", ...
  payload: bytes
  received_at: timestamp
}

struct Lease {
  key: ResourceKey
  id: LeaseId
  revision: Revision
  acquired_at: timestamp
  ttl: duration
  holder: string          // handler/session id
}

struct Session {
  key: ResourceKey
  history: Vec<Turn>              // full conversation/reasoning history for this resource
  plan: Option<Plan>               // agent's current working plan, if any
  artifact_ref: Option<string>     // e.g. git worktree path / branch name
  created_at: timestamp
  last_active: timestamp
}

struct InboxItem {
  event: Event
  delivered_at: timestamp
}

trait Dispatcher {
  // Adapter registers itself once.
  fn register_source(source: string, key_fn: KeyFn, classify_fn: Option<ClassifyFn>)

  // Called by a Source when a new event arrives (push or pull).
  // Internally: dedup by (source, event.id); resolve ResourceKey via KeyFn;
  // if IDLE -> debounce then acquire; if LEASED -> append to that Session's
  // Inbox immediately, no state change; if RE-ACQUIRE -> reset debounce timer.
  fn ingest(source: string, event: Event) -> IngestResult

  // Called by a Handler right after it starts working on a key.
  // Loads the existing Session for `key` if one exists, else creates one.
  fn acquire(key: ResourceKey) -> Result<(Lease, Session), Conflict>

  // Called periodically by a long-running Handler to prove liveness.
  fn renew(lease: Lease) -> Result<Lease, Expired>

  // Called by the Handler's control loop between reasoning steps — cheap,
  // no LLM involved. Returns any InboxItems delivered since the last poll.
  fn poll_inbox(lease: Lease) -> Vec<InboxItem>

  // Called by a Handler to persist updated history/plan/artifact_ref back
  // onto the Session, typically after each reasoning step or before a
  // long-running tool call.
  fn checkpoint(lease: Lease, session: Session) -> Result<(), Conflict>

  // Called by a Handler when done. Non-empty inbox (or a checkpoint arriving
  // after the last poll) triggers RE-ACQUIRE per debounce policy, resuming
  // the SAME Session.
  fn release(lease: Lease, outcome: Outcome) -> Result<(), Conflict>

  // Introspection / ops.
  fn status(key: ResourceKey) -> KeyState
  fn list_active() -> Vec<KeyState>
}

enum IngestResult { Dispatched(Lease, Session) | DeliveredLive | DebouncePending }
enum Outcome { Handled | Failed(reason) | Superseded }
enum Conflict { RevisionMismatch(expected: Revision, actual: Revision) }
```

`acquire()`/`checkpoint()` returning `Conflict::RevisionMismatch` is still the
stale-handler-rejection case: a Handler operating against revision N whose
lease has since expired and been reacquired by someone else (revision N+1)
must abort rather than finalize side effects or overwrite a newer Session
state — same 409-style optimistic concurrency as v1, just now guarding
Session writes too, not only lease ownership.

## 6. Concurrency limits (orthogonal axis)

Per-key exclusivity stops races on the *same* resource. It says nothing about
how many *different* resources can be handled at once. That's a second,
independent knob:

```
struct DispatcherConfig {
  max_concurrent_leases: u32     // global cap, e.g. 3
  max_concurrent_per_source: Map<string, u32>  // per-adapter cap
  debounce_window: duration      // default re-dispatch delay, e.g. 5s
  lease_ttl_default: duration    // default TTL, e.g. 10m
}
```

When the concurrency cap is hit, new `acquire()`-eligible keys wait in a
FIFO admission queue rather than spawning unbounded parallel handlers. This
mirrors the "run up to N issues concurrently" pattern seen in existing
bespoke dispatchers, but as a config knob instead of hand-rolled logic.

**Admission queue eviction rule (v1.1 addition):** an unbounded wait here
just relocates the "acted on stale state" failure mode from the per-key
level to the global level — a PR comment handler that finally runs after
sitting in the admission queue for ten minutes may be acting against a PR
that's since changed underneath it. Each admission-queue entry therefore
carries its own bound (`admission_ttl`, config, default proportional to
`debounce_window`). On expiry:

- the entry is **not silently dropped**;
- it re-enters through the normal `IDLE`/`RE-ACQUIRE` debounce path as if
  its underlying event had just arrived, so a fresh, current read of the
  resource happens whenever it does eventually get admitted, rather than
  running against however-stale a state it was queued with.

This keeps the same "never silently drop or clobber" guarantee from §2
consistent at the admission-queue level, not just the per-key level.

## 7. Persistence

The dispatch table (`ResourceKey -> {state, revision, lease, queue}`) must
survive a process restart, or every crash silently drops in-flight
coordination state. Minimum viable store: a single embedded table (SQLite is
fine) with one row per ResourceKey:

```
resource_key   TEXT PRIMARY KEY
state          TEXT   -- idle | leased | redispatch
revision       INTEGER
lease_id       TEXT NULL
lease_holder   TEXT NULL
lease_expires  INTEGER NULL
queue          JSON    -- pending Event ids, ordered
updated_at     INTEGER
```

On startup, any row with `state = leased` and `lease_expires < now()` is
treated as abandoned and reset to `idle` (or `redispatch` if its queue is
non-empty) before any new events are ingested. This is what makes the
primitive crash-safe rather than merely race-safe.

## 8. Adapter examples

**GitHub/Gitea notifications (pull-based):**
```
key_fn      = |event| format!("{}:{}/{}#{}", event.host, event.owner, event.repo, event.number)
classify_fn = |event| classify_role(event)  // AUTHOR | REVIEWER | ISSUE_IMPLEMENTER
source      = poller, gated by a cheap script (per the earlier gate-before-LLM design),
              emitting one Event per notification thread with the thread's
              updated_at as the dedup id.
```
A burst of three review comments arriving while nobody is currently working
the PR collapses into a single fresh dispatch (debounce at `IDLE`). A burst
arriving *while* a Handler is already working it lands in that Handler's live
Inbox one at a time, no debounce needed, because delivery there is just an
append, not a new dispatch.

**Telegram inbound (push-based, per the tool-not-channel design):**
```
key_fn      = |event| format!("telegram:task:{}", event.task_id)
source      = webhook, receiving only narrow, signed trigger payloads
              ("spawn_background_task", "approve", "cancel") — never open
              conversation, consistent with treating Telegram as an actuator
              rather than a session-owning channel.
```
The same primitive that protects a GitHub PR from double-handling also
protects a background task from being double-spawned if a user taps a button
twice.

## 8.5. Worked example: the mid-implementation comment

Walking through your scenario against v1.1:

1. **Assignment comment arrives.** `ingest()` resolves key `github:acme/widgets#142`.
   Key is `IDLE` → debounce window opens, then `acquire()` creates a fresh
   `Session` (empty history, no artifact yet), returns `(Lease, Session)`.
   Handler starts: reads the issue, plans, creates a worktree, writes it to
   `session.artifact_ref`, starts implementing. Checkpoints periodically.

2. **"No PR yet? What is happening?" arrives mid-implementation.** Key is
   `LEASED` → the event is appended straight to the Session's Inbox, no
   dispatch, no debounce. At its next step boundary the Handler calls
   `poll_inbox()`, sees the new comment, and — because it has the *actual
   current Session state*, not a cold re-read of the issue — can answer from
   what it already knows: "Still in progress, implementing against library
   X, ETA ~20 min" and post that as a comment. No replanning needed, because
   nothing about the plan changed; the agent just has continuity to answer
   accurately instead of re-deriving its own status from scratch.

3. **"Ah, you'll hit bug X, here's the fix" arrives shortly after.** Same
   mechanism: appended to Inbox, picked up at the next `poll_inbox()`. This
   time the content *does* bear on the plan. The Handler (i.e. the model,
   not the dispatcher) decides whether to incorporate it now or after the
   current tool call finishes — that judgment is explicitly out of scope for
   the primitive. What the primitive guarantees is that the information
   arrived in time to matter, as a turn appended to the *same* ongoing
   Session, not as a fresh, context-free re-dispatch after the bug was
   already hit.

4. **Handler finishes, calls `release()`.** Inbox is empty (both comments
   were already consumed live) → key returns to `IDLE` cleanly. If a third
   comment had landed in the gap between the last `poll_inbox()` and
   `release()`, the non-empty inbox would trigger `RE-ACQUIRE`, and the
   *same* Session — same history, same artifact_ref, same plan — would be
   resumed, not recreated.

The failure mode you described (re-reading the issue cold and reimplementing
from scratch) specifically required both v1 gaps at once: invisible-until-
release delivery, and no persistent Session across dispatches. Fixing either
one alone wouldn't have been enough — visibility without session continuity
would let the agent see the comment in time but still forget its own
progress; session continuity without live visibility would preserve progress
but still deliver the bug warning too late.

## 9. What this deliberately does *not* solve

- **Classification/priority logic** — stays 100% adapter-specific (`ClassifyFn`),
  not a core concern.
- **What a Handler is allowed to do once it has a lease** — that's the
  existing tool/skill/plugin policy layer, unchanged.
- **Cross-key dependencies** (e.g. "don't touch PR #142 until #140 merges") —
  out of scope for v1; would be a scheduling layer built on top, not baked in.
- **Distributed/multi-node dispatch** — v1 assumes a single Dispatcher process
  per deployment, matching the single-gateway assumption the rest of these
  frameworks already make.

## 10. Why this belongs in core, not a plugin

**Clarification (v1.1):** "belongs in core" needs to be split into two claims,
because they have different answers.

The *domain-specific* half — Source adapters (GitHub poller, Gitea poller,
Telegram webhook receiver), `KeyFn`/`ClassifyFn` per domain, and the skill
teaching the model how to configure a watch — is correctly plugin-shaped.
Ship it as a package: a code plugin plus a bundled skill, the same pattern
OpenClaw uses for tools/skills/plugins generally. Someone who never installs
the "watch external resources" package pays nothing for it.

The *mechanism* half — `Session`, `Lease`, `Inbox`, revision-guarded
acquire/renew/checkpoint/release — is not actually about polling at all. It's
the general answer to "many possible inputs racing into one ongoing unit of
work, which must survive a crash and must not be duplicated." The core's own
interactive loop is an instance of that same problem (a user's follow-up
message arriving mid-tool-call, a background task completion needing to
interrupt cleanly, a crash needing a clean resume) with a fixed key like
`interactive:main` instead of a resource key. If this mechanism only exists
inside the polling package:

- the interactive loop either doesn't get crash-safety/live-interrupt
  handling at all, or
- core re-derives an equivalent-but-different mechanism for its own purposes,

and either way you end up with more than one notion of "session" in the same
system — the exact fragmentation (heartbeat vs. cron vs. automations vs.
webhooks vs. hooks, five uncoordinated mechanisms) that made OpenClaw's
scheduling story hard to reason about in the first place.

**Resulting split:**

- Core defines a small `SessionManager` contract (the trait in §5, minus
  anything GitHub/Gitea-specific) and ships one reference implementation.
  The core agent loop is its first, permanent consumer, using a fixed key
  for the single interactive session.
- The "watch external resources" package is a plugin + skill bundle that
  calls into the core `SessionManager` rather than reimplementing it —
  Sources, `KeyFn`, `ClassifyFn`, and the skill are the entire package.
- Nothing about this requires the reference implementation to be
  unswappable; it can be a trait with a default backing store, the way
  providers/channels/tools already are. What has to be singular is the
  *contract*, so two plugins can never each invent an incompatible notion
  of what a Session is.
