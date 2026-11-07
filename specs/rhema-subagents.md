# rhema-subagents — the subagent extension

> **Status:** v1, 2026-10-17 (decisions in §8).
> **Grounding:** SPEC.md §1 (the second-consumer discipline) and
> `specs/rhema-memory.md` (the crate-shape precedent: contract plus tools
> over `rhema-core` only).
> **One-line promise:** delegation as one tool — a subagent is a fresh
> `Agent` on a fresh `Session` whose final text comes back as the tool
> result.

---

## 0. Scope

`rhema-subagents` is an **extension crate**: it sits on top of the
existing seams and changes nothing underneath. `rhema-core` is untouched;
the feature builds without it and the rest builds without it.

**In v1:**

- `SubagentDef` — one discovered subagent: a folder with a `SUBAGENT.md`
  whose frontmatter carries the trigger and whose body is the subagent's
  system prompt. Discovery mirrors skills: global directory plus the
  workspace's `.rhema/subagents`, project shadowing global by folder name.
- `SubagentTools` — a `ToolResolver` advertising one tool, `subagent`,
  whose spec description carries the catalog (each subagent's name and
  trigger), so the model learns when to delegate without a renderer
  change.
- `wire` — the explicit frontend helper: discover, and when anything was
  found, attach a `SubagentTools` to a `CodingAgentConfig`.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Engine support.** No nesting in `rhema-core`, no subagent events,
  no session trees. The inner turn is driven by the tool, exactly like
  the shell drives a process. If wiring this needed a new type in
  `rhema-core`, the boundary would be wrong (SPEC.md §7 test 5); it does
  not.
- ❌ **Persistent subagent sessions.** A subagent's session lives for one
  tool call. The parent's committed tool call is the durable record; a
  crashed subagent is an error result and the parent re-delegates.
- ❌ ~~**Per-subagent providers.**~~ Resolved since v1 (§8, and §4's
  *Where a run rides*): one provider instance — the session's — by
  default, with a frontend-installed provider factory behind the
  `<provider>/<model>` spelling for the runs that name another
  provider. The factory is a value the frontend carries, not a
  discovery the crate runs: credentials stay with the caller.
- ❌ **Structured nested events / live transcript trees.** Inner output
  rides the existing progress channel (§4). A dedicated UI nesting is a
  later decision.
- ❌ **Per-subagent tool allowlists.** With one built-in tool (`bash`)
  there is nothing to allowlist yet.

## 1. What a subagent is here

The parent agent calls `subagent(name, prompt)`. The tool resolves the
name against the discovered definitions, builds a fresh `Agent` with the
definition's body as system prompt, runs a bounded step loop over a fresh
`Session` whose first message is `prompt`, and returns the final
assistant text as the `ToolResult`. The parent never sees the subagent's
history, only the report — and the report lands in the parent's history
like any tool result, so commit-before-execute, revive-on-resume, and
cancel-path repair keep working unchanged.

The act of delegation is the unit of approval, not each command inside
the subagent.

## 2. Layout

```
crates/rhema-subagents/
  Cargo.toml
  definitions/
    general-purpose/SUBAGENT.md   # the built-in, embedded at build time
  src/
    lib.rs          # re-exports; #![forbid(unsafe_code)]
    def.rs          # SubagentDef, discovery, frontmatter
    tools.rs        # SubagentTools: the `subagent` tool and the inner loop
    wire.rs         # the CodingAgentConfig helper
  tests/            # end-to-end over the mock provider
```

Dependencies point outward to the existing stack: `rhema-core`,
`rhema-agent-tools`, `rhema-coding-agent` and `rhema-config` (both only
for `wire`), never the reverse. The two seams this feature consumes from
`rhema-agent-tools` and `rhema-coding-agent` are default-inert additions
(§5); they exist for composition, not for subagents specifically.

## 3. Definitions

A subagent is a folder holding a `SUBAGENT.md`:

```markdown
---
description: Reviews a diff for correctness and reports findings. Use for a second opinion before committing.
model: anthropic/claude-haiku
max_steps: 128
---

You are a code reviewer. ...full system prompt...
```

- `description` (required, non-empty) — the trigger, shown to the parent
  in the `subagent` tool's catalog.
- `model` (optional) — the model identifier the subagent's requests
  carry, spelled `<provider>/<model>` to ride another logged-in
  provider (the first `/`'s head must name a provider kind; anything
  else reads as one bare id on the session's provider — an
  OpenRouter-style id, say) or a bare model id on the session's own;
  without it, the call's `model` argument, then the session's model.
- `max_steps` (optional, ≥ 1) — the subagent's step budget; without it,
  the crate default (256).
- The body (required, non-empty) — plain text, never rendered: it is
  appended verbatim under the subagent's stamped system prompt, so a
  definition written for another harness behaves as written.

The system prompt one run carries is the parent's own, computed once
at wire time off the config: the base template rendered over the
workspace, its skills (the disabled dropped), the environment facts,
the user preferences, and the tool list the subagent actually carries
(the shell). The definition's body lands after it, separated by a
blank line. `config.system`, the total-replacement escape hatch,
replaces the stamp whole, exactly as it replaces the parent's; a
stamp that cannot render warns and leaves the bodies bare — the
parent's own construction fails on the same template.

Discovery reads `<config dir>/subagents` and
`<workspace>/.rhema/subagents`, project shadowing global by folder name.
A folder is a subagent only when its `SUBAGENT.md` parses with a
non-empty description and a non-empty body; anything else is not a
subagent, not an error. A subagent without instructions has no prompt to
run on, so the body is as mandatory as the trigger.

One definition is never read from disk at runtime: the built-in
`general-purpose`, the delegation every agent gets, lives in the crate
at `definitions/general-purpose/SUBAGENT.md` and is embedded at build
time, parsed by the same frontmatter code, ahead of both directories —
so the catalog, and with it the `subagent` tool, always exist,
filesystem or none. Its frontmatter carries no `model` (the parent's)
and no `max_steps` (the default), and a definition of the same name —
global or project — shadows it, exactly like project shadows global.
Being a real `SUBAGENT.md`, it keeps the format honest: the built-in
cannot drift into a shape a user could not have written.

## 4. Semantics of one `subagent` call

```
resolve(call, ctx):
  def   = defs by call.args.name (unknown name → error result)
  ride  = resolve_model(call.args.model, def.model)   # §4 "Where a run rides"
  inner = ShellTools(workspace); install the parent turn's approval hook
  resolver = inner, plus a nested SubagentTools when depth allows
  agent = Agent(ride.provider, resolver, stamped(config) + def.body, params(ride.model))
  session = fresh; push user message `call.args.prompt`
  for step in 0..def.max_steps:
      if ctx.cancelled → error result "cancelled"
      out = agent.step(session, sink, ctx')
      forward inner TextDelta/ToolOutputDelta to the parent's progress
      if out.stop_reason != ToolUse → return the report
  error result "hit its step budget"
```

- **Approval is inherited, not granted.** Before the inner loop, the tool
  reads the hook installed on the parent's `ShellTools` for the current
  turn and installs it on the inner one; the hook is cleared on the way
  out. Inner approvals therefore flow through the same bridge and the
  same modal as the parent's. No hook installed (never the case mid-turn,
  but the fail-safe default stands) means inner calls are rejected.
- **Where a run rides.** The call's optional `model` argument wins over
  the definition's `model` frontmatter, which wins over the session's
  provider and model. The spelling splits on its first `/`: a head
  naming a provider kind resolves through the frontend's factory (the
  `subagent_providers` config field, one `Backend` per logged-in kind);
  a head naming no kind stays whole — a bare id on the session's own
  provider. A kind whose credentials do not resolve fails the call
  naming what is available, never a silent ride on the session's
  provider; the taught list in the spec description is the factory's
  wire-time answers, while the resolution itself probes live, so a
  `/login` mid-session serves jobs started after it. A background
  restart re-resolves the spelling the call carried.
- **The inner context** clones the parent's `cancelled` flag, so a user
  stop reaches the subagent's tools, and the loop checks it between
  steps. The `run_id` carries the parent's: `turn-3` becomes
  `turn-3/subagent:code-reviewer`.
- **Progress rides the existing channel.** Inner text and tool output are
  forwarded as complete lines through `resolve_streaming`'s progress
  callback, so the parent's tool call shows the subagent working with no
  UI change. Progress is ephemeral, exactly like shell output; only the
  report enters history.
- **Failure is in-band.** An unknown name, a cancelled run, a step-budget
  overrun, a provider error, or an empty report returns
  `ToolResult::failure` with the reason; the parent reacts in-band
  (SPEC.md §4). The tool never panics and never aborts the parent's turn.
- **The report is capped** at the shell's output limit (50 KiB), tail
  kept — the conclusion of a report sits at its end, as an error sits at
  the end of a command's output.
- **Depth is bounded.** A `SubagentTools` is built with a remaining
  nesting budget (default 1: no subagent spawns another). A nested level
  gets the budget minus one, and at zero the `subagent` tool is simply
  not advertised.

## 5. Consumers and wiring: two inert seams, then `wire`

The feature is explicit at the frontends; the driver never discovers
subagents on its own.

1. **`rhema-agent-tools`: `Overlay`**, a two-resolver combiner. `specs`
   lists the primary's specs first, then the extras, extras winning name
   collisions; `resolve` consults the extra first, then the primary;
   `replay_safety` likewise. Plus `ShellTools::current_approval`, the
   getter the inheritance in §4 reads. Both are composition material any
   driver may use.
2. **`rhema-coding-agent`: `CodingAgentConfig`** grows
   `tools: Option<Arc<ShellTools>>` (bring your own shell; `None` builds
   one as today) and `extra_resolver: Option<Arc<dyn ToolResolver>>`
   (`None` is today's behavior byte for byte). When set, the agent runs
   `Overlay::new(shell, extra)` and `stamped_system` renders the merged
   spec list, so the prompt and the tools array cannot drift.
3. **`wire(provider, config) -> config`** — discover (the built-in
   `general-purpose` plus the folders), drop the config's
   `disabled_subagents` names (the persisted off switch; the built-in
   answers to its folder name like any definition, and a set holding
   every name leaves the config unwired), then stamp the subagents'
   base prompt off the config (the parent's own sections, the shell's
   workspace), build the shell and the `SubagentTools` — installing the
   config's `subagent_providers` factory when the frontend carries one —
   set both fields, return. The frontends apply the prompt sections before the
   wire, so the stamp carries them. Each frontend calls it at
   its agent-construction sites (TUI, headless one-shot; the claw and
   ACP when they adopt the feature). The `/system` preview goes through
   the same filtered discovery so what is previewed is what turns
   send.

## 6. How we know it works

1. `cargo fmt --all`, `cargo clippy --workspace --all-targets
   --all-features -- -D warnings`, `cargo test --workspace
   --all-features` stay green at every commit.
2. **Discovery** mirrors the skills suite: global plus workspace sorted
   by name, project shadowing global, malformed folders skipped, missing
   directories discover to nothing.
3. **One call, one report**: the mock provider scripts an inner agent
   that calls `bash` then answers; the tool returns the final text, the
   inner bash ran, and the parent's history carries only the call and
   the report.
4. **Failure is in-band**: an unknown name, a scripted provider error,
   and an exhausted step budget each return an error result without
   touching the parent's turn.
5. **Approval inheritance**: with a hook installed on the parent shell,
   inner gated calls consult it; with none, they are rejected.
6. **Depth**: at the default budget the inner resolver does not offer
   `subagent`; with a budget of 2, the nested call resolves and the
   third level is where it stops.
7. **Cancellation**: a pre-set flag stops the loop before the next step.
8. **`wire`**: with no definitions on disk the built-in alone is the
   catalog; with one, the resolver is the overlay and the prompt lists
   both tools.

## 7. Explicitly not decided here

- Usage accounting: a subagent's token spend is invisible to the parent's
  totals in v1.
- A `--no-subagents` flag or per-workspace opt-out.
- Subagent definitions read from anything but the two directories.

## 8. Decisions

- **Separate crate, pure extension** — `rhema-subagents` exists; nothing
  depends on it; `rhema-core` is untouched.
- **One tool, named `subagent`** — `task` read as too generic; the tool
  takes `name` and `prompt`.
- **Approval: inherit the parent turn's hook** — read from the parent's
  `ShellTools` at resolve time, never a blanket `AllowAll`.
- **Explicit wiring** — frontends call `wire`; the driver stays
  subagent-agnostic, the honest reading of "pure extension".
- **A built-in `general-purpose` definition** — the tool's `name`
  argument needs a catalog to name into; the built-in guarantees one,
  and a definition of the same name shadows it. A generic delegation is
  always useful; a definition is only a system prompt with a trigger.
- **Cross-provider rides resolve through a frontend-installed factory**
  — the `<provider>/<model>` spelling (the model picker's own
  convention) names the provider, the crate builds nothing itself: the
  `subagent_providers` config field carries the credential answers, and
  with none installed the session's provider is the only one, named as
  such in the spec. A restart re-resolves, so a re-run aims where the
  credentials point when it runs.
