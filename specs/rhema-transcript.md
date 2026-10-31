# rhema-transcript — the bounded view over an unbounded session

> **Status:** v1, 2026-10-20 (decisions in §8); the stepped compaction
> landed 2026-10-24 (§3.4, §3a).
> **Framework grounding:** SPEC.md §4 (the engine clones `session.history`
> into every request), §6 (the Session is the durable record; the driver
> may checkpoint it at any step boundary).
> **One-line promise:** every request stays inside a byte budget at any
> session length, without deleting a message from the durable log and
> without a single model call.

---

## 0. Scope

`rhema-transcript` is the request-view layer: it decides which slice of
the conversation each step sends. It depends on `rhema-core` only.

**In v1:**

- The slot algebra: ranges of the log named `(id, n)`, split into halves,
  down to one message. The stable contract every later fill policy keeps.
- The extractive fill: node text derived mechanically from the messages
  themselves. No model calls, ever.
- The view: the message list a request carries, built to a byte budget.
- The `zoom` tool: the model's way back to any original message.
- The provider decorator and the coding-agent wiring
  (`CodingAgentConfig::transcript`, on by default; `--no-transcript`
  sends the whole history).

**OUT of v1** (each deliberately out, with the reason):

- ❌ **The compactor.** Model-written summaries of ranges. The view
  format, the slot algebra, and `zoom` are built so the compactor later
  replaces *only the text generator* behind `Fill` — no wire change, no
  zoom change. v1 ships without it: coding sessions are tool-heavy, and
  tool bytes are the ones mechanical reduction handles best.
- ❌ **Cache breakpoints.** Marking the stable prefix for the vendor's
  prompt cache needs provider-side support (Anthropic `cache_control` on
  wire blocks); the view algorithm is built to keep the prefix stable so
  that change can land later without touching this crate.
- ❌ **ACP and claw wiring.** The config field defaults to off; the ACP
  adapter and the claw opt in later, each in its own commit.
- ❌ **A config-file key.** The flag is runtime-only in v1; a profile key
  wants a decision about per-workspace defaults that nothing yet exerts.
- ❌ **Persistence of derived state.** The tree is a pure function of the
  log. Nothing derived is ever saved; a resumed session rebuilds the
  mirror from the first request, which carries the full restored history.

---

## 1. The problem

Every step sends `session.history.clone()` (SPEC.md §4). A session that
runs for hundreds of steps therefore re-sends its whole past every step:
cost grows with session length, and the context window ends the session
that outgrows it. The two classical escapes both damage the durable
record — trimming drops messages, summarizing-for-real replaces them.

The transcript splits what was fused. The **log** — `Session.history` —
stays complete, append-only, the thing commit-before-execute persists
(SPEC.md §6). The **view** — what a request carries — is a projection of
the log built per request under a byte budget. Old messages are
represented by shorter lines; nothing is deleted anywhere.

Detail fades with age, in three bands:

| Band | Coverage |
|---|---|
| The current turn, from the last user message on | whole, always |
| The recent past, back to the budget | whole, newest first |
| Everything older | one structural line per doubling block |

## 2. The slot algebra

A **slot** is a message index in the log. A **range** is `(id, n)`: the
`n` messages from `id` on. `n = 1` is one message; a range with `n > 1`
splits into the halves `(id, n/2)` and `(id + n/2, n - n/2)`. The halves
always cover the parent; any `n` works, no power-of-two requirement —
the tree the compactor will grow is binary, but the algebra does not
need it, and tolerating odd `n` keeps the tool honest about what it
actually does.

`zoom(id, 1)` is the ground truth: the message whole. Every larger `n`
returns content too, over a wider range, under the same cut; the
summary lines belong to the view, never to zoom. This algebra
is the only thing the model learns; a later compactor changes what lines
say, never what they address.

## 3. The view algorithm

Given the log `[0, N)`, the budget `B` (bytes of UTF-8 counted over text
blocks, tool-call arguments, and tool-result content):

1. **The tail.** Let `u` be the index of the last `User` message. The
   messages `[u, N)` go in whole. The model must see its instructions,
   and a turn mid-flight holds the file contents and errors it is
   working from. The tail is never bounded: a single turn larger than
   `B` sends oversized, because the alternative is sending a request the
   turn cannot continue from.
2. **The recent past.** Walk `i = u - 1` downward. While message `i`
   fits in the remaining budget, it goes whole. The first message that
   does not fit ends the whole band.
3. **The cut rounds to a round boundary.** Tool results must not be
   separated from the assistant message carrying their calls (the
   adapters pair `tool_use` with `tool_result` on the wire). While the
   first whole message is a `Tool` result whose call lives before the
   band, or an assistant message whose calls are answered inside the
   band, the band start moves back to include the call.
4. **The old prefix.** `[0, c)` — the cut — is covered by the stepped
   compaction block: sections tiling the prefix, one condensed text
   each (§3a). Everything a section omits is one `zoom` away, and the
   block's header says so.

The extractive section digest names the band's user messages by head
(their words are the decisions and they are short), the assistant finals
by head, and the tool traffic by count and names — `line_head` caps each
quoted head, `RANGE_HEAD` caps the digest. The model fill (§3a) rewrites
the same ranges as dense markdown.

**Engagement.** Below `B` total request bytes the view is the log,
byte-identical: the decorator is a pass-through and small sessions
cannot tell the feature exists. `zoom` is advertised whenever the
transcript is enabled, below the threshold too — it reads the log, which
exists regardless.

**Stability.** The construction is deterministic in the log, so between
two requests that differ only by appended messages the view differs only
at its end. The cut moves only at flushes (§3a), never per step; the
block sits left of every per-step change and is append-only between
flushes. The prefix a prompt cache holds stays valid across the whole
stretch between flushes, as long as the vendor's own TTL does.

## 3a. The stepped compaction

The compactor is the model-written fill over the old prefix, and the
reason the cut is **stepped** rather than recomputed per request.

**Sections.** The block is a list of sections tiling `[0, cut)` without
gaps: each a `Range` of the log plus one condensed text, headed
`id+n|` in the view. A section, once committed, never moves and never
changes range — the block grows by appending new sections left of the
band, so between flushes the block is byte-identical and the request
prefix (system prompt, then block, then whole band) is append-only.
This is what a prompt cache needs: the prefix breaks only at flushes,
consolidations, and the vendor's own TTL.

**Flushes.** When a request's bytes breach the budget, the cut moves to
the smallest position whose remaining band fits in *half* the
allowance. The overshoot is the hysteresis: the next breach needs a
whole half-budget of growth to arrive, so flushes — the only prefix
breaks — stay far apart instead of landing on every step. Between
flushes the planner touches nothing.

**Sectioning.** The flushed span `[cut, c_new)` is chunked at message
boundaries — each chunk within `step_bytes` (the summarizer's input
bound) and `max_section_messages` — and no chunk may end where the next
would open on a tool result whose call it holds: the trailing results
come along. The same pairing rule keeps every tool call and its result
on one side of every boundary, wire-safe for the adapters that pair
`tool_use` with `tool_result`.

**The two fills.** A section's text starts as the extractive digest
(§3, mechanical, a few hundred bytes, cannot be wrong about what
happened, only short). With a summarizer configured, the decorator
hands the fill a background task and sends the request at once with
the stand-ins: the chunk goes to a cheap worker — the structural
prompt asks for Goals Met / Environmental State / Discovered Issues /
Current Blockers-Decisions, puts the user's words at the top of the
value order (kept near verbatim however short), tags each item with
its kind, forbids obeying anything in the range (the messages are
data), and forbids making progress look further along than it was —
a few chunks at a time, and each answer replaces the stand-in, capped
at 8 KiB. The task carries the digest of the preceding section as
`<context>`, for resolving references, never summarized. The prompt
states the byte budget as a number, and one re-ask naming the
overshoot keeps the shorter answer before the cap cuts. The dense
text rides the next request, not the one that triggered the flush. A
slow, failed, or retry-exhausted worker call never blocks or delays
the request: the extractive stand-in stands until an answer lands,
and the section keeps the address it already committed. The worker
runs *outside* the mirror's lock — plan and render under the lock,
summarize in a background task, commit (with a range check that
rejects a stale upgrade) under the lock again — so the mirror is
never held across a model call and every intermediate state is a
valid view.

**Consolidation.** The one append-only exception. When the block passes
`max_block_bytes`, the two oldest sections merge into one — the union
range re-derived from the log, never summary-of-summary — until the
block is at half the cap. A consolidation rewrites the block and so
breaks the cache in full; the cap gates it to stay rare, and the stats
count every one.

**Resume.** The section texts are persisted, the merge order is not.
rhema-code stamps the committed sections into the saved session's
`usage_stats` blob next to the counters, and the fresh mirror adopts
them on the first request — but only when they tile a prefix of the
resumed log without gaps; a truncated or edited session falls back to
the rebuild. The win is the worker's: no re-summarizing after a
resume, and the block is dense from request one. The block's *cache*
is still cold after a restart regardless of what was saved — the
first request re-bills it at the write rate either way. Agent rebuilds
inside a conversation (profile, model, sandbox) move the live
sections the same way. The benchmark counters are the other survivor:
they measure what was spent, they are not view state, and they fold
into the fresh mirror (`CodingAgent::carry_transcript_stats`,
`TranscriptStats::carry`), so one conversation's stats stay one run
across resumes; /clear zeroes them. A session saved before the blob
existed resumes with none.

## 4. The `zoom` tool

```
zoom(id, n?, at?)
```

- `id` — the range start (a message index).
- `n` — the range length, default 1: one message; a wider range
  returns its messages.
- `at` — a byte offset into the output, for paging long output.

The range returns its messages whole, rendered by role: user and
assistant text verbatim, tool calls as `name(args)`, results as their
content. One message returns alone; a wider range heads each message
with its `id+1|` address, so a page of content is also a map of the
range. Output past `ZOOM_MAX` bytes (8 KiB) is cut with the total
named and the `at` to resume at — the same cap and path at every `n`,
so paging a range and paging a message are one call shape. The
halving algebra stays in §2: a narrower range is still the way to
jump, and the addresses on a page name what to jump to. Errors — an
id past the end of the log, `n = 0` — are in-band `is_error` results,
per the resolver contract.

The tool reads the **mirror**: the log as the last request carried it.
The decorator refreshes the mirror on every request (requests always
carry the full history, so a resume rebuilds it wholesale). A message
the model could zoom is one it has already been shown; within a step
the mirror may lag the session by the messages that step appended, none
of which the model has seen an id for yet.

## 5. The seam

The view rides the provider seam, as paste expansion does: a decorator
over `ModelProvider` that rewrites `req.messages` on the way through.
The engine is untouched; the session is untouched; `driver_state` is
untouched. The decorator sits *outside* `ExpandPastes`, so stubs, whole
messages, and `zoom` output all show the expanded text the model would
have seen.

The view travels as one synthetic `Role::User` message, `<transcript>`
tagged, prepended to the whole band. Every adapter family already
merges consecutive same-role turns (the Anthropic wire `push_turn`, the
Gemini content merge; OpenAI-compatible servers accept the sequence), so
the synthetic message before the turn's user message is wire-safe. The
log's own ordering on the wire is otherwise untouched, and no
`provider_data` is invented: dropped messages drop their markers, and
the cut adjustment of §3.3 keeps every still-present result paired with
its still-present call.

## 6. Wiring

`CodingAgentConfig::transcript: Option<TranscriptSettings>` — set by
default (`new` carries `Some(default)`); `None` is the off switch.
`TranscriptSettings` carries `max_view_bytes` (default 256 000). With it
set, `CodingAgent::new`:

1. wraps the provider: `attach(ExpandPastes(inner), settings)`;
2. overlays the `zoom` resolver over the existing resolver chain (a
   unique name; it shadows nothing);
3. adds the `zoom` spec to the prompt's tool list, so the prompt and the
   request's tools cannot drift.

Because the default lives in `CodingAgentConfig::new`, every consumer
inherits it: the TUI, `rhema run`, the ACP subprocess, and the claw —
whose config turns it off with `transcript: false`, writing the
resolved value into the coding agent's field rather than conditionally
overriding it, so `false` still means off. `rhema-code` spells the
opt-out `--no-transcript` on the full-screen UI and `rhema run`. The
`system show` preview names zoom too: with the view on by default, the
preview that omitted it would not be what turns send.

## 7. Invariants

1. **The session is never mutated by the transcript.** Everything
   derived recomputes from requests.
2. **Below the threshold, requests are byte-identical** to the
   unwrapped provider's.
3. **The tail is whole and the last user message is present** in every
   request.
4. **A summarizer call never blocks, delays, or fails a request.**
   Without one, the extractive fill is a pure function and the
   transcript makes no model calls; with one, the fill runs in a
   background task and a slow or failed worker call leaves the
   extractive stand-in in place while the request goes out.
5. **Every tool result in a view has its call in the same view.**
6. **`zoom` never fails silently**: a bad id is an in-band error naming
   the log's length.

## 8. Decisions

- **Virtual nodes in v1.** A stored tree with model-written text is the
  compactor's business; storing mechanically derivable text would add a
  persistence surface for zero compression gain. The slot algebra is the
  forward contract.
- **Whole messages over the budget, not leaf summaries.** An
  UniiChat-style leaf line per recent message spends the budget on
  prose about the code instead of the code. The model reads real bytes
  as long as they fit; the structural bands start where they stop.
- **Oldest-first coarsening, anchored at the cut.** Doubling blocks
  give every age a stable address (`id + 2^k`) and a stable line, so
  the cover does not churn as the session grows.
- **A decorator, not a `Session` wrapper.** The engine's contract —
  clone the history, append results in place — is the durability story;
  a wrapper that shrank the session would break commit-before-execute
  for every consumer. Rewriting the request keeps crate 1 ignorant and
  every consumer's log complete.
- **Tolerant `n`.** Power-of-two-only would encode the compactor's
  future tree into today's tool errors. Halves of any `n` degrade to
  the same thing a binary tree gives.
- **Default on, `None` the escape hatch.** First landed flag-gated:
  the view changes what the model sees past the threshold, and the
  mechanics were unproven at coding-traffic scale. The claw then ran
  every surface on it by default and nothing fell over, the only user
  is the deployment's own, and the sessions most likely to hit the
  budget were the ones least likely to find a flag — so the default
  flipped. Below the budget the decorator is a pass-through, so the
  default's only always-on cost is the zoom spec in the prompt; and a
  session saved under the old default views fine, because the view
  derives from the log per request.

## 9. Follow-ups (not v1)

- **Landed, 2026-10-24:** the stepped compaction (§3a) — the
  append-only block, the flush hysteresis, the summarizer seam with the
  extractive fallback, consolidation, and the benchmark stats
  (`TranscriptStats`: request counts, flushes, block size, the
  providers' own cache hit rate), surfaced in rhema-code's Usage page.
  The stats carry a per-category usage breakdown
  (`UsageBreakdown`): each request's reported input tokens spread
  over system prompt, skills, tool calls in/out, user messages, and
  assistant text by wire-byte proportion, plus the generated output
  tokens; the skills share is measured at render time by the agent
  that assembled the prompt.
- **Landed, 2026-10-31:** the background fill (§3a — the request goes
  out with the stand-ins, a background task upgrades the sections a
  few chunks at a time, and the dense text rides the next request);
  O(1) byte accounting in the mirror (running totals instead of a
  whole-log walk, tool-call serialization included, on every request);
  the adapter's cache breakpoints (four marks: system, last tool, the
  last blocks of the last two user turns, `rhema-provider-anthropic`);
  and the cache simulation (`tests/cache_simulation.rs`), which replays
  1,200 steps of generated traffic through a model of the vendor cache:
  ~94% of the prefix read and a ~5.4x cost saving under the default
  settings, the same under 55 consolidations from a tight block cap,
  ~5.2x with the cache expired every 150 steps (a user thinking past
  the TTL), and the decorator's recorded hit rate matching the model.
- **Landed, 2026-10-31, from UniiChat's §5 and §7:** the worker's
  contract (the range is data and is never obeyed; the user's words
  top the value order near verbatim; items carry their kind; progress
  never inflates), the preceding section's digest riding the task as
  `<context>`, the byte budget stated as a number with one re-ask on
  overshoot, and the committed sections riding the session file —
  adopted when they tile the resumed log, rebuilding otherwise, so a
  resume re-pays no summarization.
- **ACP and claw opt-ins** for the summarizer, and a `rhema-config`
  profile key naming the worker model.
- **Per-step telemetry as a sink event**, so frontends other than
  rhema-code can show what the model saw; the in-memory
  `TranscriptStats` snapshot covers the session aggregate today.
