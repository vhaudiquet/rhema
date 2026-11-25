# rhema-decision, decision models as shell data

> **Status:** v1 spec, 2026-11-23, written alongside the code it
> describes; revised the same day: named instances and one default
> per profile, the turn providers' shape, in place of the fallback
> chain the first draft had; and OpenRouter as a third kind, the
> aggregator's Decisions route.
> **Grounding:** SPEC.md §1 (the second-consumer discipline);
> `specs/rhema-mcp.md` §2 (the BASH_ENV provision this macro rides);
> `specs/rhema-websearch.md` (the macro family's fourth member, whose
> shape this one follows: a seam crate, one adapter crate per vendor,
> a registry, a bash function, a hidden binary mode).
> **One-line promise:** `decide` in the agent's shell asks a decision
> model (TypeSafe's Jev, Cloudflare's Clef, or one OpenRouter routes
> to) typed questions about a state and prints calibrated
> probabilities, from the profile's default decision instance, one
> call, no fallback.

---

## 0. Scope

A decision model (a System One model) reads a state and typed
questions and answers each with a probability distribution in one
forward pass. It generates no text. The category exists for the
judgments an agent needs before it acts: triage, routing, yes/no
screens, severity ratings. Jev introduced the `/v1/systemone` wire in
September 2026; Clef and a dozen open models speak it too.

Decision models are a **macro**, not a tool, and not a core concern.
There is no `Decision` in `rhema-core`, no new `ToolResolver`, no
engine change. Answers are text like any other shell output, so the
model-facing surface is a bash function, `decide`, the fourth member
of the `see`/`job`/`search` family. If wiring this needed a new type
in `rhema-core`, the boundary would be wrong; it does not.

**In v1:**

- `rhema-decision`: the `DecisionProvider` trait, the
  `DecisionRequest`/`Question`/`Answer`/`DecisionResponse`
  vocabulary (which serializes to the System One wire as it stands),
  the `DecisionError` classes, `decide` (the validated, budgeted
  call), the text renderer, and a `MockDecisionProvider` behind a
  `mock` feature. No HTTP here.
- `rhema-decision-jev`: the TypeSafe adapter, `POST
  https://api.typesafe.ai/v1/systemone`, reqwest behind a `native`
  feature, offline tests against wiremock.
- `rhema-decision-clef`: the Workers AI adapter, `POST
  /client/v4/accounts/{id}/ai/run/@cf/cloudflare/clef[-flash]`, the
  response unwrapped from Workers AI's envelope.
- `rhema-decision-openrouter`: the aggregator's adapter, `POST
  https://openrouter.ai/api/alpha/decisions`, Jev's wire with the
  model as a slug: one key to Jev, Clef and forty-odd other System
  One models, three of them free.
- `rhema-decision-registry`: the kind-to-adapter spelling and the
  profile-to-provider resolution, the `rhema-websearch-registry`
  precedent; `default_model`, what a blank `model` asks, so the
  settings page shows the same default `build` sends.
- `rhema-config`: `DecisionInstance` (kind, key, model, account id,
  base url), `DecisionSettings` (the instances and the default), and
  the `Profile` field `decision`, keys on the same three tiers as
  every other key.
- `rhema-bash::decision`: the `decide` function's init file, the
  wiring, the prompt section.
- `rhema-host-agent`: one prompt-section field, `decision`.
- `rhema-code`: the hidden `__decision` mode, the session wiring,
  and the settings page: **Decision models** in the sidebar, its
  rows, the add dialog over the four kinds, the default, removal.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Images and `box` questions.** Jev and Clef both read images;
  the `see` macro already stages images into the model's context, and
  a decision over an image is a different feature with its own
  plumbing. `Answer::Other` keeps a `box` answer from failing a
  response whole.
- ❌ **Long context.** Jev's 262k mode is a header; nothing here sends
  it. A state that does not fit is the vendor's 400, which the model
  reads as `Invalid`.
- ❌ **Caching.** Every `decide` call is one fresh vendor call.
- ❌ **Sandboxed and cloud sessions.** Local-only, gated exactly like
  vision, jobs, search, and MCP: a session whose bash runs elsewhere
  cannot exec the frontend's binary, and a cloud sandbox holds no
  secret store and no config file.
- ❌ **A public `rhema decision` CLI.** Settings change through the
  file. Nothing here has shown the need the MCP verbs had.

## 1. Architecture

```
the agent's bash          decide noul "state" "question?"
        |                 $RHEMA_DECISION_BIN __decision …
        v
rhema-code (__decision)   profile from RHEMA_DECISION_PROFILE
        |                 settings + secrets -> the instance
        v                 registry -> adapter -> decide()
rhema-decision            validate, one call, budget
        |                 render -> stdout
        v
adapter crates            one HTTP call, reqwest behind `native`
```

- The **vocabulary and the call** live in `rhema-decision`. A
  `DecisionRequest` is a JSON state plus named questions; a
  `DecisionResponse` is the answers under the same names plus the
  model and the usage. `decide(provider, request)` validates the
  request, makes the one call under a 15 s budget, and returns the
  response or the failure; the frontend holds the provider as `Arc<dyn
  DecisionProvider>` and names it in the output by its kind (`jev`).
- The **macro half** lives in `rhema-bash::decision`: the init file,
  the env map, the prompt section. It depends on `rhema-decision` for
  nothing; the function's body is one exec, like `see`'s.
- The **settings** live in `rhema-config`.
- The **assembly** lives in the frontend: rhema-code's `__decision`
  mode reads the profile, resolves keys through the tiers, builds the
  adapter through the registry, makes the call, prints. The mode is
  the only place where settings, secrets, and adapters meet.

## 2. The wire

Every vendor answers the same body (Jev's route is
`POST /v1/systemone`, Clef's is Workers AI's, OpenRouter's is
`POST /api/alpha/decisions` with the model as a slug):

```json
{
  "model": "jev-latest",
  "state": "I was charged twice. Please help.",
  "questions": {
    "billing": {"type": "noul", "instructions": "Is this about billing?",
                "criteria": {"true": "money moved wrongly", "false": "anything else"}},
    "team":    {"type": "choice", "instructions": "Which team handles this",
                "criteria": {"payments": "Refunds and charges", "technical": "Bugs"}},
    "urgency": {"type": "score", "instructions": "How urgent",
                "criteria": ["Can wait", "This week", "Today"]}
  }
}
```

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "billing": {"type": "noul", "noul": 0.98},
    "team":    {"type": "choice", "choice": "payments", "confidence": 0.9,
                "probabilities": {"payments": 0.95, "technical": 0.05}},
    "urgency": {"type": "score", "score": 1.7, "confidence": 0.55,
                "legend": {"0": "Can wait", "1": "This week", "2": "Today"},
                "probabilities": {"0": 0.1, "1": 0.1, "2": 0.8}}
  },
  "usage": {"input_tokens": 120, "output_tokens": 12}
}
```

- `noul`: the probability that the statement holds. No confidence
  field: the number is the whole answer.
- `choice`: the top option, a probability per option, a confidence
  derived from how peaked the distribution is. 2 to 255 options.
- `score`: a probability-weighted position that can fall between
  levels, the levels by number, a probability per level, a
  confidence. 2 to 10 levels, lowest first.
- `instructions` and every criteria description accept a string or
  any JSON structure; the vendors read a structure as labeled data.

The seam's types serialize to exactly this shape (`#[serde(tag =
"type")]` on `Question` and `Answer`), so an adapter's request is the
request plus its `model`, and its response is this crate's response
parsed back. A `model` key in hand-written JSON is read and dropped:
the model is the configured instance's, never the request's, because
Clef's model picks the route.

## 3. Configuration

One section on `Profile`, the framework's section of the stored
settings: the instances, keyed by a name the user chooses, and the
one `decide` asks by default. The turn providers' shape (`instances`
plus `instance`) under one heading, with the kind and the model
riding the instance. A profile holds as many as it likes; no call
falls from one to another, and nothing lets a call pick another.

```yaml
profiles:
  personal:
    decision:
      instance: jev                  # the default; unset names the first
      instances:
        jev:
          provider: jev
          secret_ref: "1731…-…-0007"     # or api_key, the no-store fallback
          model: jev-preview             # jev-latest when unset
        clef:
          provider: clef
          api_key: "…"                   # a Workers AI API token
          account_id: "…"                # the account the model runs on
          model: clef-flash              # clef when unset
        local:
          provider: jev
          api_key: "any"
          base_url: http://localhost:30000   # any /v1/systemone server
        router:
          provider: openrouter
          api_key: "…"                   # an OpenRouter key, the turn providers'
          model: cloudflare/clef-flash   # any slug the catalog lists; typesafe/jev-1.14 when unset
```

- `decision: Option<DecisionSettings>`, absent from the file while
  unset (a nulled section reads as absent), holding `instances:
  BTreeMap<String, DecisionInstance>` (absent while empty) and
  `instance: Option<String>`, the default. Must name an instance to
  count; unset or dangling, the first instance by name is the
  default, the `instance` rule for turn providers. Two instances may
  share a `secret_ref`: one key, `jev-latest` and `jev-preview` as
  two instances to compare. The section is strict: an instance's key
  spelled at the section level, or the other way round, is refused.
- The default instance whose kind nothing answers to, whose model the
  kind lacks, whose key resolves nowhere, or whose Clef account is
  missing is an error naming the instance, never a fall to another
  instance or to the environment tier.

Key resolution, at use time, for the default instance: the vendor
kind's environment variable (`TYPESAFE_API_KEY`; `CLOUDFLARE_AUTH_TOKEN`
or `CLOUDFLARE_API_TOKEN`; `OPENROUTER_API_KEY`, the turn providers'
variable), then the secret-store entry `secret_ref` names, then the
stored literal. The account id resolves the same way against
`CLOUDFLARE_ACCOUNT_ID`, then the stored `account_id`.

The environment tier: a profile with no `decision` instance still
yields a provider, the first kind in the fixed order jev, clef,
openrouter whose variable resolves (clef needs both its token and the
account variable). A vendor's own key comes before the aggregator's.
A CI machine gets decisions with no config file.

## 4. The adapters

| | jev | clef | openrouter |
|---|---|---|---|
| endpoint | `POST https://api.typesafe.ai/v1/systemone` | `POST https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/{model}` | `POST https://openrouter.ai/api/alpha/decisions` |
| auth | `Authorization: Bearer <key>` | `Authorization: Bearer <token>` | `Authorization: Bearer <key>` |
| model | a body field, default `jev-latest` | the route and a body field, `clef` or `clef-flash` | a body field, a slug, default `typesafe/jev-1.14` |
| response | the System One body as it stands | under Workers AI's `{result, success, errors, messages}` envelope | the System One body plus `id`, `provider`, `usage.cost`; no `legend` on a score |
| refused request | 400/422 with `detail` | 400 with `errors[].message` | 400 with `error.message` |

OpenRouter's Decisions route is the same wire as Jev's: the router
normalized every vendor under one contract, which is the proof that
Jev and Clef are one API and not two, and the reason this adapter is
Jev's thirty lines with another route. It differs in three small
places, each carried by the seam and not the adapter: a provider may
decline one question and answer the rest (`{"type": "refusal"}`, an
`Answer::Refusal`: not answered, never a no); a score answer carries
no `legend` (Jev echoes the rubric back, the router does not, so the
seam's legend defaults empty and the renderer never read it); 402 is
out of credits, a `Http` with the router's message. The route is
alpha, the router's own docs say the shape may move: a change breaks
this adapter's `Parse` arm and nothing else. Not modelled, by design:
the `box` question (image localization), multimodal `state` parts and
`assets`. A request using them fails the seam's validation before
any call, which is right for a text-only `decide`.

Every adapter takes its credentials in its constructor, maps non-2xx
statuses through `DecisionError::from_status` (400 and 422 are
`Invalid` with the body's detail extracted; 401 and 403 `Auth`; 429
`RateLimited`; anything else `Http` with the detail), and a body that
is not the documented shape is `Parse`. A vendor changing its JSON
breaks one adapter's `Parse` arm and nothing else.

The `native` feature carries reqwest, default on. The base crate's
trait is transport-free, so a future host-fetch adapter for the
cloud worker implements the same trait behind its own feature.

## 5. Call semantics

`decide` is the product's policy, in the base crate so it is
testable offline and shared by every consumer:

- **One provider, one call.** The profile's default instance names
  the provider and the model; a failure is the failure the model
  reads, with no vendor behind it.
  Racing or falling back would bill a second vendor for a request the
  first one's answer or refusal already settled.
- **Validation before the call.** At least one question, a non-blank
  name per question, 2 to 255 choice options, 2 to 10 score levels,
  checked here so the common breakage costs nothing; a malformed
  request is `Invalid` before the vendor is asked.
- **A vendor's 400 is `Invalid` too,** with the body's detail. The
  fix is in the request, and the same bytes fail the same way again.
- Budget: **15 s for the call**, under the shell tool's command
  timeout, so the model reads the call's own error text instead of a
  killed command's.

## 6. Output format

One header naming the provider kind and the model that answered,
the answer count, and the tokens (the vendors bill per input token);
then one line per question:

```
jev jev-1.13.0: 3 answer(s), 328 in / 34 out

billing: 0.98 yes
team: payments [payments 0.95, technical 0.05] confidence 0.90
urgency: 1.70 on 0-2 [0 0.10, 1 0.10, 2 0.80] confidence 0.55
```

- A noul reads `yes` at 0.9 and above, `no` at 0.1 and below,
  `uncertain` between. The number is the answer; the word is a
  reading of it.
- A question the provider refused (OpenRouter's per-question
  `refusal`) reads `refused by the provider: not answered`. It never
  reads as a no, and the other questions answer as usual.
- A choice lists its options most likely first; a score lists its
  levels in numeric order and states the scale. Past twelve entries
  the tail folds into a count; the pick always shows.
- A total render cap of 8 KiB, answers past it dropped with a notice.
- The header ends with the charge when the vendor reports one
  (`, 0.000038 credits`; OpenRouter does, the vendors do not).
- `--json` prints the response, pretty (the answers, the model, the
  usage with its charge), nothing of this crate's rendering. No flag
  picks an instance (§11, decision 8).
- Exit codes: 0 answered, 1 the provider failed or the default
  instance did not build (the text names the entry), 2 usage (no verb,
  an unknown flag, JSON that is not a request), 3 the request was
  refused (the seam's validation or the vendor's 400; the fix is in
  the request, not in a retry), 4 not wired in
  this shell (no `RHEMA_DECISION_PROFILE`), the `see` convention with
  one more code.

## 7. The shorthand

The JSON request is the whole vocabulary; three verbs cover the
common case in one line, the question's name being the verb's:

```
decide noul "state" "yes/no question"
decide choice "state" "what to decide" id[=description]...
decide score "state" "what to rate" "level 0" "level 1" ...
```

A bare option id describes itself (`technical` is its own
description); `id=description` says more. Flags come before the verb:
`--json`, and `-f FILE` to read the request from a file instead of
stdin.

## 8. Layout

```
crates/
  rhema-decision/            trait, types, decide(), renderer; mock feature
  rhema-decision-jev/        -> rhema-decision
  rhema-decision-clef/       -> rhema-decision
  rhema-decision-openrouter/ -> rhema-decision
  rhema-decision-registry/   -> rhema-decision + the three adapters
                              + rhema-config (the profile resolution)
  rhema-bash/                decision module; no fleet dependency,
                              the function is strings
  rhema-config/              DecisionInstance, the two Profile
                              fields, key-tier resolution
  rhema-host-agent/          the `decision` prompt-section field
  rhema-code/                __decision mode, decision_wire.rs (one wiring
                              per session key, the gate), setup wiring
```

`rhema-decision` depends on `serde`, `serde_json` (the vocabulary
rides JSON), `tokio` (time, for the budget), and `async-trait`. The
registry crate is `build(instance, key) -> Result<Arc<dyn
DecisionProvider>, BuildError>` over three arms plus `resolve(profile,
secrets) -> Result<Arc<dyn DecisionProvider>, ResolveError>`, the
default instance, else the environment tier, kept a crate
for the reason `rhema-provider-registry`'s docs give: the spelling
lives in one place, so a third vendor is one arm there and no
frontend edits.

## 9. How we know it works

- `rhema-decision`: the vocabulary serializes to the documented wire
  shapes and parses the documented responses; the call against
  `MockDecisionProvider`s (the answer comes back and the vendor sees
  the request as sent; a malformed request costs no call; the
  vendor's refusal and every other failure come back as is; the
  budget fires; the renderer sorts, folds, caps).
- Adapters: wiremock per crate. The auth header lands; the route and
  body carry the model; a documented response parses; 401, 429, 500,
  a 400 with `detail` or `errors[].message`, a malformed body map to
  their taxonomy arms; Clef's envelope unwraps under `result` and
  fails under `success: false`; the Workers AI routes differ per
  model; OpenRouter's route carries the key and the attribution
  headers, the docs' response parses (a score without its legend), a
  `refusal` is an answer and not a failure, a 400 with
  `error.message` is `Invalid` and a 402 a `Http`.
- `rhema-decision-registry`: every kind builds; the errors name their
  cause; the default is the named instance else the first; keys
  resolve through the secret store; the default never falls to another
  vendor or the environment, and a keyless, unknown, or mis-modeled
  one is its own error naming the entry; the environment tier, for a
  profile holding no instance, walks the fixed order (a vendor's key
  before the aggregator's) and needs Clef's account; a held instance
  wires even keyless; a `base_url` routes a call to a localhost
  server, for the jev and the openrouter kinds; a blank model goes
  out on the wire as `default_model`'s spelling, per kind.
- `rhema-bash` (via `init`): init file text, env map contents,
  section verbatim.
- `rhema-config`: the section round-trips and is strict at both
  levels, nulled reads as absent, an empty section is no instance,
  the default is the named instance else the first; the tiers, Debug
  redaction, the key placed on the secret store or the file, the
  migration over every instance, unknown fields loud, the section
  absent while unset.
- `rhema-code`: `parse` over every verb and flag shape, `--instance`
  refused as usage (the model picks nothing), JSON requests parsed,
  `run` over a mock provider for the
  lines, the json, and the exit codes (a malformed shorthand and a
  malformed JSON request both exit 3 and never reach the vendor), the
  wiring gate closing with nothing held and nothing served, a run
  inside a tokio runtime. The settings page: the rows show the kind
  as stored, the model (the kind's default for a blank), the
  endpoint, a missing key, and the default as `active_instance`
  rules it; the add row is the last row; the dialog walks each
  kind's stages and back (Jev and OpenRouter: name, key; Clef adds
  the account; the custom endpoint adds the URL, refused without
  `http://`, and opens the key line on `local`); a paste lands in
  the input and never on the kind list; through `on_key`, an add
  places its key on the secret store and persists, Clef's account and the
  endpoint land in the file, enter on a row persists the default,
  removal takes the default and the secret-store entry with it and an
  emptied section leaves the profile; the frames at 80 columns: one
  add row, the hints inside the dialog's width, the key masked to its
  length, the empty page's hint.

No live-network test lands in CI. By hand, the mode was run against a
local server speaking the wire, through the real binary, a temp
`XDG_CONFIG_HOME`, and a `decision` section whose instances hold a
`base_url`: the shorthand verbs, the JSON on stdin, `--json`, the
default (`instance` named, else the first), a keyless default (exit
1, the entry named), and an invalid request (exit 3, no vendor
call). The same against a server speaking OpenRouter's route.

## 10. Consumers and wiring

**`rhema-host-agent`.** One field on `HostAgentConfig`:
`decision: Option<String>`, the prompt section, verbatim. The
template gains a `## Decision models` section after `## Web search`,
gated `{% if decision %}`.

**`rhema-code`, TUI and headless sessions.** In `app/setup.rs` and
`run/headless.rs`, after the search link, before MCP:

```
jobs -> vision -> search -> decision -> MCP     (the BASH_ENV lineage)
```

The gate is `sandboxed.is_none() && configured`, where `configured`
is `is_configured(..)`: the profile holds a decision instance (even
one that does not build, so the call reports the entry's problem
rather than the function vanishing), or the environment serves a
kind. The
gate needs the same resolution the mode runs, so it lives in one
place, `decision_wire.rs`, called by both.

**`rhema-code`, the settings page.** `SettingsSection::Decision`,
  "Decision models" in the sidebar after Web search, built from the
  web search page: `screens/decision.rs` holds the rows
  (`decision_rows`, the profile's instances in name order, the
  default marked as [`DecisionSettings::active_instance`] rules it,
  then the add row) and the add dialog (`AddDecisionDialog`: the
  kind list, then the kind's own stages, the key always last). The
  kinds are the registry's three plus a fourth row, "Custom Jev
  endpoint", which is the `jev` kind with a `base_url` (decision 5)
  and opens the key line on `local`, since a server without auth
  takes any text and `build` refuses a blank. The app's handlers sit
  beside the web search ones in `app/settings.rs`: a submit places
  the key through `Profile::place_decision_key`, persists, and
  rebuilds the agent (the first instance flips the wiring gate);
  enter on a row writes `decision.instance` and persists with no
  rebuild, the mode reads the profile per call; a removal drops the
  instance, clears the default when it was the one, deletes the
  secret-store entry, drops an emptied section, and rebuilds when the
  last one went. The dialog's rendering is the web search dialog's,
  lifted into `render_staged_dialog` over the kind list and the one
  input line. No slash command of its own: `/settings` is the way
  in, as for Web search.

**Later consumers.** The claw pulls the adapter crates and holds a
provider behind the trait, resolved by its own profile policy. The cloud harness wires
nothing (`rhema-cloud-agent/src/assembly.rs` sets `decision: None`).

## 11. Decisions

1. **A bash function, not a tool.** Answers are shell data. The
   macro costs `rhema-core` nothing, inherits the approval gate, the
   output caps, and the replay rules of `bash`, and reaches the model
   through a prompt section a frontend controls.
2. **The System One wire is the vocabulary.** Every vendor speaks
   it, and a dozen open models' servers speak it. The seam's types
   serialize to it directly; no per-vendor translation layer exists
   for Jev or OpenRouter at all, and Clef's differs only by the
   envelope and the route.
3. **The model is the instance's, not the request's.** Clef's model
   picks the route, so a request cannot carry one for the adapter to
   honor. A `model` key in pasted JSON is dropped.
4. **One provider, no chain.** The first draft walked a fallback chain
   like web search's. A decision is one calibrated answer to one
   request, not a ranked list whose shape every vendor shares: two
   vendors answering the same questions give two distributions, and
   "the first that worked" is a coin flip the user never sees. The
   profile names one default instance, the call asks it, and a
   failure is reported as what it is. The seam still validates first
   so the common breakage costs nothing, and a vendor's 400 is still
   `Invalid`: the fix is in the request either way.
5. **`base_url` on the instance.** The self-hosted System One servers
   (Ollaya, vLLM, SGLang, llama.cpp) are how most open decision
   models get served; a Jev kind against a `base_url` reaches all of
   them with no new kind.
6. **The settings page is the web search one, plus the kinds' own
   stages.** The first draft left the page out; it landed with the
   feature. The rows, the add row, the removal confirm, the key
   placement, and the rebuild rules are copied unchanged; what the
   page adds is a dialog whose stages depend on the kind (Clef asks
   for an account, the custom endpoint for a URL) and a default set
   with enter, since one instance answers and the model never picks
   (decision 8). The model is left to the file: a slug is not a thing
   to type into a dialog, and the page shows what a blank asks.
7. **Named instances and one default, the turn providers' shape.** A
   profile holds any number of decision instances and names the one
   `decide` asks, as it holds `instances` and names `instance`. The
   kind and the model ride the instance instead of standing beside
   the map as `provider` and `model` do for turns: a decision instance
   is one vendor's credential and one model, a (provider, model) pair
   with a key, so the second model to compare is a second entry
   sharing the first one's `secret_ref`, not a second field to keep
   coherent.
8. **One section, `decision`, and no instance flag.** The instances
   and the default sit under `decision:` as the sandbox's facts sit
   under `sandbox:`, not as two more keys on the profile. The model
   gets `decide` and no `--instance`: which vendor answers is the
   profile's choice, the user's to change in the file, not a switch
   a turn reaches for mid-task. A second instance exists to switch
   to, or to compare by hand, not to be picked per call.
9. **OpenRouter is a kind, not a fallback.** Its Decisions route is
   the System One wire, so it is an adapter like the others, and the
   user's choice of it is an instance like the others; nothing walks
   from a vendor to it when a call fails (decision 4). What it buys
   is one key to Jev, Clef and forty-odd other models by slug, three
   of them free, which is how a profile with no vendor account gets
   decisions. Its alpha shape is this adapter's `Parse` to break; a
   refusal it returns is an answer (§6), never a silent no.
