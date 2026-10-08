# rhema-memory — the agent memory layer

> **Status:** v1, 2026-10-08. Decisions resolved in §8; implementation
> lands in the following commits.
> **Framework grounding:** `rhema-framework-v1.md` §2 (memory-store is a
> MODULE-tier contract) and §9 (context and memory strategy).
> **One-line promise:** durable, cross-session memory the model *pulls*
> through tools — never context the engine pushes.

---

## 0. Scope

`rhema-memory` is the framework spec's `memory-store` module: one crate
holding the store contract, reference implementations, and the three tools
that expose them. It depends on `rhema-core` only.

**In v1:**

- `MemoryRecord`, `Scope`, `MemoryDraft` — the data. Small, serde-friendly,
  round-trippable.
- The `MemoryStore` contract: `put`, `get`, `search`, `forget`.
- Two implementations: `VolatileStore` (in-process, tests and embedders)
  and `FileMemoryStore` (markdown files, the durable default).
- The v1 ranker: keyword match + recency. No embeddings.
- Three tools — `memory_search`, `memory_save`, `memory_forget` — as a
  `ToolResolver` for seam consumers, with reusable per-tool operations for
  the resident tier's registry shims.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Embeddings / semantic recall.** The framework spec already sequenced
  this: recency+keyword first, embeddings later *behind the same
  `memory_search` interface*. The contract in §3 is written so swapping the
  ranker touches only this crate.
- ❌ **Injection of any memory into context.** Principle 5 (context is
  earned, not injected) is absolute here. The only always-present artifact
  is the consumer's bootstrap instruction to check memory when relevant —
  consumer-owned prose, not this crate's concern.
- ❌ **Automatic consolidation, reflection, "learn from this session."** A
  periodic model-invoking memory pass is the heartbeat invariant (§7) with
  new paint. All writes in v1 are model-invoked tool calls.
- ❌ **Compaction.** Different axis: compaction shrinks one session's
  history; memory persists *across* sessions. A driver may later write a
  session summary as an ordinary record via `put` — a consumer policy, not
  crate machinery.
- ❌ **Multi-user ACLs, multi-node.** §7 invariants: single user, single
  process.
- ❌ **RSD machinery — no leases, no inbox, no revisions.** A memory
  operation is short, serialized by the store, and durable on return.
  Memory is not a session and must never grow dispatch.

## 1. What memory is here

A **record** is one thing worth remembering: a preference ("commits follow
Conventional Commits with component scopes"), a fact about an environment, a
decision and its why. Atomic records, not documents — one record per fact,
because search ranks records, and a document-sized record is either a
needle lost in hay or hay burying the needle. The body is markdown text,
capped. Tags are freeform strings; the store normalizes them (trimmed,
lowercased, deduplicated) so the model's case wobble can't split one tag
into two.

Every record belongs to a **scope**:

| Scope | Meaning | Example |
|---|---|---|
| `Global` | true wherever this deployment runs | user tone, commit conventions |
| `Workspace(path)` | scoped to one checkout | "this repo deploys via `just release`" |

The layering mirrors skills exactly (global + per-workspace, the same
resolution the coding agent already performs), minus shadowing: search
returns both layers **labeled by scope**, because for recall the model
should see that a workspace fact overrides a global one, not have the
global one silently hidden.

Who writes: the model, through `memory_save`, when a conversation surfaces
something durable. Who reads: the model, through `memory_search`, when a
turn touches something it doesn't have in front of it. Nothing else writes,
nothing else reads. Deterministic capture (auto-summary at session close)
is deliberately absent; if it ever earns its place it arrives as a driver
calling `put`, visible as such.

## 2. Layout

```
crates/rhema-memory/
  Cargo.toml           # deps: rhema-core, serde, serde_json, serde_yaml, thiserror
  src/
    lib.rs             # re-exports; #![forbid(unsafe_code)] #![deny(missing_docs)]
    record.rs          # MemoryRecord, Scope, MemoryDraft, tags, ids, UTC rendering
    store.rs           # trait MemoryStore + MemoryError
    search.rs          # MemoryQuery + the v1 keyword+recency ranker
    volatile.rs        # VolatileStore: in-process, mutex-serialized
    file.rs            # FileMemoryStore: markdown files, the durable default
    tools.rs           # the three tools + MemoryResolver
  tests/
    file_store.rs      # the contract suite + file-specific behavior
    turn_tool_use.rs   # the falsification test (§6)
```

**Naming:** `rhema-rsd`'s in-process store renames `MemoryStore` →
`VolatileStore`, freeing the name for agent memory everywhere; both crates'
in-process backends are then the *same concept under the same name* behind
their respective contracts. (`rhema-code`'s `MemoryStore` — a `ConfigStore`
test backend — is untouched; it is config, not agent memory.)

## 3. Public API

The store trait is **sync** — the same call `rhema-rsd`'s `Store` makes:
operations are short local reads and writes, and the tools that wrap them
are async at the resolver seam anyway. Timestamps are **milliseconds since
the Unix epoch (`u64`)** — the framework's existing convention
(`rhema-rsd`'s `Clock`) — rendered to UTC text only at display edges.

```rust
// record.rs
/// Where a record applies. `Workspace` holds the canonical path; consumers
/// canonicalize, the store takes the scope as opaque data.
pub enum Scope { Global, Workspace(PathBuf) }
/// One thing worth remembering. The store assigns `id` and timestamps and
/// normalizes `tags`; the model supplies only body and tags.
pub struct MemoryRecord {
    pub id: String,             // store-assigned, URL-safe, time-ordered
    pub scope: Scope,
    pub tags: Vec<String>,
    pub body: String,           // markdown, capped (BODY_LIMIT)
    pub created_at: u64,        // millis since the Unix epoch
    pub updated_at: u64,
}
/// What a caller supplies to `put`. `id: None` creates; `Some` replaces
/// that record (created_at preserved, scope re-aimed).
pub struct MemoryDraft {
    pub id: Option<String>,
    pub scope: Scope,
    pub tags: Vec<String>,
    pub body: String,
}

// store.rs — the contract; ranking method is invisible from here
pub trait MemoryStore: Send + Sync {
    /// Insert, or replace the record with this id.
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, MemoryError>;
    /// The record with this id, if any.
    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError>;
    /// Best-first under the store's ranking; an empty query means recent.
    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError>;
    /// Remove by id; `Ok(false)` when absent. Not an error.
    fn forget(&self, id: &str) -> Result<bool, MemoryError>;
}

// search.rs
pub struct MemoryQuery {
    pub text: String,           // keywords; empty = "recent"
    pub scopes: Vec<Scope>,     // empty = every scope
    pub tags: Vec<String>,      // all must match, when non-empty
    pub limit: u32,             // clamped to SEARCH_LIMIT
}

// tools.rs — one resolver, three tools
pub struct MemoryResolver { /* Arc<dyn MemoryStore> + the active Scope */ }
impl ToolResolver for MemoryResolver { /* specs() = the three; resolve() dispatches */ }
```

The contract's load-bearing property: **`search` returns records ranked
best-first and says nothing about how.** When embeddings arrive, they replace
the internals of one implementation; no upstream signature changes. That is
the framework spec §9 requirement, restated as the trait's shape.

`get` earns its place on the trait: `memory_save` with an `id` must
preserve the existing record's scope (a global preference corrected from a
workspace session must not silently migrate), and that requires reading it.

Ids are store-assigned `{UTC second}-{counter in hex}` (e.g.
`20261008T123456-0000002a`), so directory listings read chronologically,
and are validated on every entry point — alphanumeric and `-` only —
because in the file store the id *is* the filename, and a model-supplied id
must never traverse a path.

Tool schemas (descriptions live in the `ToolSpec` and nowhere else):

- `memory_search { query, tags?, limit? }` → the matched records, each with
  id, scope, tags, UTC date, and body — the id is what makes
  `memory_save`(update) and `memory_forget` usable, so search always
  carries it.
- `memory_save { body, tags?, id? }` → insert, or replace when `id` is
  given; returns the id. The description tells the model to search before
  saving, so correction beats duplication.
- `memory_forget { id }` → removed / not found.

Errors are in-band results: `memory_forget` on an unknown id is a failure
*result* saying so, never a crash — the same discipline as every tool in
the framework.

## 4. Semantics

**Scoping at the call site.** The consumer constructs `MemoryResolver` with
the store and the active scope (the coding agent: its workspace; the claw:
the watched repo's checkout). A search spans `Global` plus that one
workspace — never a *different* workspace's records. Global memory is small
and travels everywhere; workspace memory stays home. Saves land in the
active scope; updates preserve the record's existing scope (§3, `get`).

**Ranking, v1.** Case-insensitive keyword match over body and tags (every
query term must appear; a tag hit outranks a body hit), ties broken by
`updated_at` descending, then id — deterministic. Empty query = recent.
Result count capped. These constants are tuning knobs behind the trait, not
contract.

**Concurrency.** The store is `Send + Sync` and serializes mutations
internally (the claw runs handlers across resource keys concurrently).
Record-level, last-write-wins on concurrent `put`s of one id — acceptable
under the single-user invariant; no revision machinery, ever.

**Durability.** A `put` that returned `Ok` survives a crash: the file store
writes to a temporary file, `sync_all`s, and renames — a `put` returns
after the rename. There is no checkpoint step to miss; each write is the
unit of durability.

**File format.** One record, one file: `{id}.md` in a flat directory the
consumer chooses — id order is creation order, `ls` reads chronologically,
and scope lives in the YAML frontmatter rather than the directory tree, so
re-scoping is a rewrite, not a move. The body follows the frontmatter
verbatim; a stray `---` line in a body cannot be confused with the
delimiter (the frontmatter ends at the *second* delimiter line).

## 5. Consumers and wiring

| Consumer | Wiring | Status |
|---|---|---|
| `rhema-coding-agent` | `MemoryResolver` alongside `CodingTools`. The engine takes one resolver, so the coding agent grows a composition seam (chain or merge) — a small `agent`-scoped change there, not here. | next |
| `rhema-autonomous-agent` / `rhema-claw` | The resident registry aggregates named `Tool`s; thin `registry::Tool` shims over `MemoryResolver`'s per-tool operations, plus the three names in `Allowlist::permissive()`. | follows |
| `rhema-code` | Inherits whatever the coding agent wires; `/memory` curation UI is a later roadmap item, not v1. | later |

The bootstrap instruction ("you have a memory; search it when context is
missing; save what will matter tomorrow") is one paragraph in the
consumer's system prompt, per framework §9 — this crate ships no prompt
text, so its voice stays the consumer's.

## 6. How we know it works

1. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` — house gates.
2. **`store_contract`** — the suite every implementation passes
   (`VolatileStore`, `FileMemoryStore`): round-trip, update preserves
   `created_at` and id, scope isolation (a workspace record never surfaces
   in another workspace's search), tag filtering and normalization,
   limit, body limits, id validation, forget idempotence — plus, separately,
   the rank-order assertions, which only honest rankers may replay.
3. **`turn_tool_use.rs` — the falsification test** (SPEC.md §7 test 5's
   pattern): a `MockProvider` scripted to `memory_save` in one turn and
   `memory_search` in the next, driven by `run_turn`, asserts the saved
   record comes back. If wiring memory into a turn needs *anything* new in
   `rhema-core`, the boundary is wrong — fix the boundary, don't add the
   type.
4. **Swappability proof:** the order-agnostic contract suite runs unchanged
   against a store whose ranking is deliberately scrambled — no caller can
   tell, because no caller inspects rank internals. This is what the
   embeddings swap will lean on.
5. File-store specifics: a body containing `---` round-trips; a corrupt
   file is skipped by search, not fatal; re-scoping rewrites in place.
6. `#![forbid(unsafe_code)]`; public items documented; the module list in
   `lib.rs` stays the layout map.

## 7. Explicitly not decided here

Session summaries as memories (episodic recall), a `/memory` curation
surface, cross-workspace search, tag taxonomies, dedup beyond the tool
description's "search before saving". Each is a consumer policy or a later
crate revision; none blocks v1, and the contract grows none of them until a
real workload demands it.

## 8. Decisions

- **Durable backend: markdown files** (decided 2026-10-08). One file per
  record — frontmatter + body — over SQLite. Greppable, hand-editable,
  diffable, matches the skills/sessions precedent, zero new dependencies;
  SQLite arrives when a real query need (FTS, vectors) exists, behind the
  same contract.
- **Vocabulary: `rhema-rsd`'s `MemoryStore` → `VolatileStore`** (decided
  2026-10-08), freeing `MemoryStore` to mean agent memory.
- **Timestamps: `u64` millis**, the framework's `Clock` convention, over
  RFC 3339 strings — one time vocabulary across crates, and no
  lexicographic-ordering traps; UTC text is rendered only for display.
- **The store trait is sync**, following `rhema-rsd::Store`; the async seam
  is the tool resolver above it.
- **ID shape:** `{UTC second}-{counter:08x}`, collision-retried; ids are
  validated (`[0-9A-Za-z-]`, ≤ 64 chars) at every entry point.
- **Body cap: 4 KiB**, opening number, tuned against real records later.

## 9. Follow-ups (not v1)

Embedding-backed ranking behind the unchanged contract · deterministic
session-summary capture as a driver policy · `/memory` curation in
`rhema-code` · cross-workspace search for the resident tier · a shared
`registry::Tool` adapter if the consumer shims sprout a third copy.
