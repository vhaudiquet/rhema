# rhema-websearch, web search as shell data

> **Status:** implemented. A local session execs the hidden
> `__search` mode; a sandboxed session with a route to the host installs
> the `search` shell function and posts to the session's host endpoint.
> HTTP sandboxes have no host route yet, so the macro stays off there.
> **Grounding:** SPEC.md §1 (the second-consumer discipline);
> `specs/rhema-runtime-kit.md` (the BASH_ENV provision this macro rides);
> the `see` macro (`crates/rhema-bash/src/vision.rs`) and the
> `job` macro (`crates/rhema-bash/src/jobs.rs`), the two
> existing members of the macro family.
> **One-line promise:** `search "query"` in the agent's shell answers
> with ranked web results, providers configured per profile and tried
> in the user's chosen order.

---

## 0. Scope

Web search is a **macro**, not a tool, and not a core concern. There
is no `WebSearch` in `rhema-core`, no new `ToolResolver`, no engine
change. Search results are text like any other shell output, so the
model-facing surface is a bash function, `search`, the third member
of the `see`/`job` family. If wiring this needed a new type in
`rhema-core`, the boundary would be wrong; it does not.

**In v1:**

- `rhema-websearch`: the `WebSearchProvider` trait, the
  `SearchQuery`/`SearchResult`/`SearchError` vocabulary, the `Chain`
  (ordered fallback over trait objects), the markdown renderer, and a
  `MockSearchProvider` behind a `mock` feature. No HTTP in this crate.
- `rhema-websearch-brave`, `rhema-websearch-exa`,
  `rhema-websearch-tavily`, `rhema-websearch-opencode`: one adapter
  per vendor or gateway, reqwest behind a `native` feature (the
  `rhema-provider-anthropic` shape), offline tests against an
  in-process wiremock.
- `rhema-websearch-registry`: the kind-to-adapter spelling, the
  `rhema-provider-registry` precedent at search scale (§12.5).
- `rhema-config`: per-profile websearch instances and their chain
  order, keys on the same three tiers as provider keys: environment,
  secret store, stored literal.
- `rhema-bash::websearch`: the `search` function's init file,
  the wiring, the prompt section.
- `rhema-host-agent`: one prompt-section field, `websearch`.
- `rhema-code`: the hidden `__search` mode, the per-session host
  endpoint, the session wiring, the settings menu.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Page fetching.** The environment already covers it: `curl`
  for raw pages and APIs, `lightpanda fetch <url> --dump markdown`
  for anything JavaScript renders, and every adapter returns snippets
  plus URLs. A contents/extract wrapper (Tavily `extract`, Exa
  `contents`) is a follow-up once a session shows the need.
- ❌ **Caching.** Every `search` call is one fresh chain run. A model
  that retries a query pays twice; an in-process LRU keyed by the
  query tuple is a follow-up with numbers behind it.
- ❌ **HTTP sandboxes and cloud frontends with no host route.** The
  host endpoint needs a socket the sandbox can reach: a shared mount or
  an SSH forward. HTTP has neither yet, and a cloud frontend has no
  session host process to call.
- ❌ **A public `rhema websearch` CLI.** Settings change through the
  TUI or the file. The MCP verbs got a CLI because servers are
  installed from shell one-liners; nothing here has shown that need.
- ❌ **Freshness knobs beyond `--news`.** Date filters, region and
  language selection, safe search: all three vendors have them, none
  has a use yet. The `SearchQuery` grows the field when one shows.
- ❌ **Merging providers.** The chain stops at the first success, so
  cross-provider dedup and score merging cannot arise. A "query all
  providers and merge" mode is a different feature.

## 1. Architecture

Four layers, each owning one thing:

```
the agent's bash          search "rust async book" --news
        |                 $RHEMA_SEARCH_BIN __search …
        v
rhema-code (__search)     profile name from RHEMA_SEARCH_PROFILE
        |                 settings + secrets -> ordered instances
        v                 registry -> adapters -> Chain
rhema-websearch           Chain: try in order, first success wins
        |                 render_markdown -> stdout
        v
adapter crates            one HTTP call each, reqwest behind `native`
```

- The **vocabulary and the chain** live in `rhema-websearch`. A
  `SearchQuery` is what the CLI builds from argv; a `Vec<SearchResult>`
  is what an adapter answers; the `Chain` is the fallback policy. The
  chain holds `(name, Arc<dyn WebSearchProvider>)` pairs, so the
  frontend names each entry after its config instance (`brave-main`)
  and error text reads like the config does.
- The **macro half** lives in `rhema-bash::websearch`: the init
  file that defines the function, the env map, the prompt section. It
  holds no HTTP, no config types, no knowledge of vendors. It depends
  on `rhema-websearch` for nothing; the function's body is one exec,
  exactly like `see`'s.
- The **settings** live in `rhema-config`: two new `Profile` fields.
- The **assembly** lives in the frontend: rhema-code's `__search`
  mode reads the profile, resolves keys through the three tiers,
  builds adapters through the registry, runs the chain, prints. The
  mode is the only place in the tree where settings, secrets, and
  adapters meet, which is the same shape the provider backends have
  (`Backend::named`).

The claw and the cloud harness can reuse the vocabulary, the chain,
and the adapters without the macro: they assemble their own agents,
and a `Chain` behind a trait object serves any driver.

## 2. Shell injection

The provision is `vision::provision_with_bin` with one more env var
and no staging directory. The BASH_ENV lineage grows by one link:
jobs, then vision, then search, then MCP. Each init file sources its
predecessor by literal path, first and guarded (`source '…' || true`),
never through an env var, so the chain terminates after one hop no
matter how many links a process built (the recursion trap
`vision.rs` documents).

The init file:

```bash
# generated by rhema-bash; do not edit
source '/tmp/rhema-vision-…/init.bash' || true
search() { "$RHEMA_SEARCH_BIN" __search "$@"; }
```

The env map a frontend layers over `HostAgentConfig::shell_env`:

| var | value |
|---|---|
| `BASH_ENV` | the init file's path (set last, winning the lineage) |
| `RHEMA_SEARCH_BIN` | the rhema binary the function execs |
| `RHEMA_SEARCH_PROFILE` | the session's profile name |

`RHEMA_SEARCH_PROFILE` is the one addition over the `see` shape: the
mode resolves keys, and keys are per profile. The wiring happens at
session setup, where the profile is already known, so the function
stays stateless and a `/profile` switch on a live session takes
effect at the next session's wiring, like every other per-profile
setting.

No staging directory: search returns text on stdout, nothing needs
attaching after the command. `ShellTools` is untouched.

## 3. Configuration

Three fields on `Profile`, the framework's section of the stored
settings, same file as the provider instances:

```yaml
profiles:
  personal:
    # … provider, model, instances …
    websearch_enabled: true
    websearch:
      brave-main:
        provider: brave
        # secret_ref preferred, api_key the no-store fallback
        secret_ref: "1731…-…-0007"
      exa-work:
        provider: exa
        api_key: "…"
    websearch_order: ["brave-main", "exa-work"]
```

- `websearch_enabled: Option<bool>`, the profile's `search` switch.
  Unset means on; false keeps every instance stored but hands the
  agent no function and no prompt section. The settings page's switch
  row and `/search on|off` write it.
- `websearch: BTreeMap<String, WebSearchInstance>`, keyed by a
  user-chosen name, the `instances` twin. Absent from the file while
  empty.
- `websearch_order: Vec<String>`, the fallback chain. Entries naming
  no instance are skipped at resolution; instances missing from the
  order are inert (the settings screen shows them as not in chain).
  No implicit append: a hand-edited file says what it means.

`WebSearchInstance` is `ProviderInstance` without the routing fields:

```rust
pub struct WebSearchInstance {
    pub provider: String,   // brave | exa | tavily | opencode
    pub api_key: Option<String>,    // redacted in Debug
    pub secret_ref: Option<String>, // store id, fresh_id() shape
}
```

Key resolution, per instance, at use time, the `instance_api_key`
tiers in order:

1. The provider kind's environment variable: `BRAVE_SEARCH_API_KEY`
   (or `BRAVE_API_KEY`), `EXA_API_KEY`, `TAVILY_API_KEY`,
   `OPENCODE_API_KEY` (the turn provider's key).
2. The secret-store entry `secret_ref` names, through the `SecretStore`
   seam.
3. The stored literal `api_key`.

The env-only tier: a profile with no `websearch` section still yields
a chain, one entry per kind whose environment variable resolves, in
the fixed kind order brave, exa, tavily, opencode. A CI machine gets
search with no config file; the availability gate (§10) counts it.

## 4. The adapters

One crate per vendor, one struct each, all behind the same trait:

```rust
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// The vendor spelling the config uses: brave, exa, tavily,
    /// opencode.
    fn kind(&self) -> &'static str;
    async fn search(&self, query: &SearchQuery)
        -> Result<Vec<SearchResult>, SearchError>;
}

pub struct SearchQuery {
    pub text: String,
    pub max_results: u8,  // clamped 1..=20 at the chain's entry
    pub news: bool,
}

pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
```

`SearchError` names the failure class, never the vendor:

```rust
pub enum SearchError {
    Auth,          // 401/403: key wrong, plan wrong, key unset
    RateLimited,   // 429
    Http(u16, String), // other status plus the body's head
    Timeout,       // the per-attempt budget blew
    Transport(String), // reqwest error, rendered
    Parse(String), // the response was not the documented shape
}
```

The mapping each adapter pins:

| | brave | exa | tavily | opencode |
|---|---|---|---|---|
| endpoint | `GET https://api.search.brave.com/res/v1/web/search`, news: `/res/v1/news/search` | `POST https://api.exa.ai/search` | `POST https://api.tavily.com/search` | `POST https://opencode.ai/console/api/websearch` |
| auth | `X-Subscription-Token` header | `x-api-key` header | `Authorization: Bearer` header | `Authorization: Bearer` header |
| query | `q` | `query` | `query` | `query`, plus `providerID: "opencode"` |
| count | `count` (1..=20) | `numResults` | `max_results` | none; the route returns up to eight, the adapter trims |
| news | the news endpoint | `category: "news"` | `topic: "news"` | none; the route has no news index |
| snippet | `web.results[].description` (news: `results[].description`) | `contents.text` capped at request time, trimmed | `results[].content` | `results[].content` |
| extras sent | none | `contents: { text: { maxCharacters: 500 } }` | `include_answer: false`, `include_raw_content: false`, `search_depth: "basic"` | none |

OpenCode's kind is Console's hosted search, enabled per workspace by
its owner and billed at $0.01 per successful search. A Go key is a
Console key, so a Go subscription reaches the same route. The
response names the provider that answered; a name other than
`opencode` is a `Parse`, since another provider's results reported as
this kind's would misattribute the bill.

Every adapter takes the key in its constructor, returns results in
vendor order (no re-sorting: the vendor's ranking is the product),
and maps non-2xx statuses through the taxonomy above. A vendor
changing its JSON breaks one adapter's `Parse` arm and the chain
falls through, it does not break the feature. Vendor docs are the
source of truth; this table pins only what v1 sends and reads.

The `native` feature carries reqwest, default on, off in wasm builds
(the `rhema-provider-anthropic` shape). The base crate's trait is
transport-free, so a future host-fetch adapter for the cloud worker
implements the same trait behind its own feature.

## 5. Fallback semantics

The `Chain` is the product's policy, in the base crate so it is
testable offline and shared by every consumer:

- Attempts run **in order, sequentially**. No racing: racing bills
  every provider for one query, and the fallback exists to survive
  failures, not to shave milliseconds.
- **Every error falls through**, auth, rate limit, transport,
  timeout, parse. There is no error a retry on another provider
  cannot fix, so the chain treats them all the same and remembers
  each reason.
- **First success stops the chain**, including a success with zero
  results. An empty result set is an answer, not a failure; falling
  through on it would bill a second provider for a query the first
  answered honestly.
- Budgets: **15 s per attempt**, **45 s for the chain**, both under
  the shell tool's 120 s command timeout, so the model reads the
  chain's own error text instead of a killed command's.
- **All failed** is one error carrying every attempt's name and
  reason, rendered as one line per attempt. Exit code 1, the text on
  stderr, so the model reads it in-band like any failed command.
- Success renders with a header naming the entry that answered:
  `brave-main (brave): 8 results`, so a session whose results look
  wrong can be traced to a provider.

## 6. Output format

Markdown on stdout, numbered, one result per block, snippets capped
at 300 characters (trimmed at a word boundary, ellipsis appended):

```
brave-main (brave): 8 results

1. [Async Book: Getting Started](https://rust-lang.github.io/async-book/)
   The async book …
2. [Tokio tutorial](https://tokio.rs/tokio/tutorial)
   …
```

- Default 8 results, `-n N` sets it, clamped 1..=20.
- A total render cap of 8 KiB: results past it are dropped with a
  notice line, so one verbose provider cannot flood a turn's
  context. The shell tool's own 50 KiB stdout cap never becomes the
  truncation the model sees first.
- `--news` asks each vendor's news index (§4's mapping).
- Exit codes: 0 answered (empty included), 1 every provider failed,
  2 usage error (no query, unknown flag), 4 not wired in this shell
  (no `RHEMA_SEARCH_PROFILE`), the `see` convention.

## 7. Prompt disclosure

`SYSTEM_SEARCH.md` beside the module, `include_str!`-ed as the
section text, the `SYSTEM_SEE.md` arrangement. The system template
gains a `websearch` section after `jobs`, gated `{% if websearch %}`:

```md
You can search the web with the `search` bash function:

    search "rust async book"
    search -n 5 --news "rust 1.90 release"

Results come back as a markdown list: numbered, each a title, URL,
and snippet. Snippets are short; open a page when a result needs a
closer read: `curl` for raw pages, `lightpanda fetch <url> --dump
markdown` for anything JavaScript renders. `-n N` caps results
(default 8, max 20), `--news` asks the news index.
```

The wired-means-advertised rule, same as vision and jobs: a frontend
that wires nothing renders no section, and a session with no
configured providers gets no function and no section. The prompt
never names a provider the session cannot reach.

## 8. Layout

```
crates/
  rhema-websearch/            trait, types, Chain, renderer; mock feature
  rhema-websearch-brave/      -> rhema-websearch
  rhema-websearch-exa/        -> rhema-websearch
  rhema-websearch-tavily/     -> rhema-websearch
  rhema-websearch-opencode/   -> rhema-websearch
  rhema-websearch-registry/   -> rhema-websearch + the four adapters
                              + rhema-config (the profile assembly)
  rhema-bash/                 websearch module; no fleet dependency,
                              the function is strings
  rhema-config/               WebSearchInstance, the three Profile fields,
                              key-tier resolution
  rhema-host-agent/         the `websearch` prompt-section field
  rhema-code/                 __search mode, search_wire.rs (one wiring
                              per session key, the gate), setup wiring,
                              the settings screen
```

`rhema-websearch` depends on `tokio` (time, for the budgets) and
`async-trait`; serde is not among its needs, the types never
serialize. The registry crate is one `build(kind, api_key) ->
Result<Arc<dyn WebSearchProvider>, BuildError>` over four arms, kept
a crate, not a function in each frontend, for the reason
`rhema-provider-registry`'s docs give: the spelling lives in one
place, so a fifth vendor is one arm there and no frontend edits.

## 9. How we know it works

- `rhema-websearch`: the chain against `MockSearchProvider`s. First
  success stops; every error class falls through and lands in the
  aggregate; empty is success; the budgets fire (a mock that sleeps);
  the renderer caps and trims; unordered mock entries never run.
- Adapters: wiremock per crate. The auth header lands; the request
  carries the clamped count and the news shape; a documented response
  parses to `SearchResult`s; 401, 429, 500, a malformed body map to
  their taxonomy arms; a giant snippet trims.
- `rhema-bash` (via `init`): init file text (predecessor line, guard, the
  function's shape), env map contents, section verbatim.
- `rhema-config`: the tiers (env wins over secret store wins over
  literal), Debug redaction, order entries naming no instance skipped,
  instances missing from the order inert, unknown `provider` spelling
  fails registry build at use time, not load time.
- `rhema-code`: `__search` end-to-end over a settings file pointing
  at the registry's mock (the registry grows a `mock` feature for
  this), exit codes 0/1/2/4; the settings screen's rows, add flow,
  and reorder.

No live-network test lands in CI; each adapter crate keeps a
feature-gated `live` test like `rhema-provider-anthropic/tests/live.rs`,
run by hand with a key in the environment.

## 10. Consumers and wiring

**`rhema-host-agent`.** One field on `HostAgentConfig`:
`websearch: Option<String>`, the prompt section, verbatim, the
`vision` and `jobs` twins. The field renders through the template's
section; the crate gains no dependency on any websearch crate.

**`rhema-code`, TUI sessions.** In `app/setup.rs`, after the vision
link, before MCP:

```
jobs -> vision -> search -> MCP     (the BASH_ENV lineage)
```

The gate is `configured` plus a route to the host endpoint.
`configured` is: the profile's `websearch_enabled` is on (unset counts
as on), and it holds at least one instance whose key resolves, or any of
the four environment variables is set. The switch closes both tiers, so
a disabled profile wires nothing even on a machine with an environment
key. `search_wire::wire_for_session` resolves the chain once on the host
and asks `cap::wire` for the session endpoint. A local session execs
`rhema __search`; a sandboxed one gets the shell function that posts its
argv to the endpoint, and keys never enter the sandbox. HTTP has no route, so `cap::wire` returns `None` and the macro
and its prompt section stay off. A backend with no forward path, such
as an LXD VM, still gets the function but its calls exit 4.

**`rhema-code`, headless (`rhema -p`).** `run/headless.rs` extends
the same wiring, the same gate, the same profile.

**`rhema-code`, the settings menu.** A new `SettingsSection::WebSearch`
in the sidebar, after Providers (it is per-profile credentials; the
position is a one-line move if it reads wrong). Rows, one per
instance: chain position, instance name, kind, where the key lives
(secret store, stored, environment). Actions:

- Enter on the switch row: flip `websearch_enabled`, persist, and
  rebuild the agent.
- `n`: add. The dialog asks kind (Brave, Exa, Tavily, OpenCode),
  instance name
  (default from the kind), API key (masked input). The key goes
  through `place_key`: secret store first, stored literal when the
  secret store refuses, the provider-key behavior. New instances append
  to `websearch_order`.
- `d`: remove, from the map and the order.
- `↑`/`↓` on a row: move it within `websearch_order`.
- An instance missing from the order renders at the bottom, marked
  not in chain; promoting it appends it.

The switch is the feature's off state. No instances or an empty
order is a lower-level off: the switch keeps the configuration and
stops the wiring. An OpenCode login on the Providers page asks, once
the credential lands, whether the same key should serve hosted
search; yes appends the companion instance to `websearch_order`.

**Later consumers.** The claw pulls the adapter crates and assembles
a `Chain` behind its own profile policy; nothing in v1 changes for it.
The cloud frontends have no host endpoint yet, so they wire nothing.

## 11. Commit sequence

One component per commit, scopes per AGENTS.md:

1. `feat(websearch)`: the base crate, trait, types, chain, renderer,
   mock feature.
2. `feat(websearch)`: the brave adapter (or whichever lands first;
   one commit per adapter).
3. `feat(websearch)`: the registry crate.
4. `feat(config)`: the three Profile fields, the instance type, key
   tiers.
5. `feat(agent-tools)`: the macro, init file, prompt section.
6. `feat(agent)`: the `websearch` config field and template section.
7. `feat(rhema-code)`: the `__search` mode, `search_wire.rs`, session
   and headless wiring.
8. `feat(rhema-code)`: the settings menu.
9. `docs`: README section; AGENTS.md gains the `websearch` scope (the
   macro rides the existing `agent-tools` scope).

Each commit is green on the three checks in AGENTS.md.

## 12. Decisions

1. **A bash function, not a tool.** Search results are shell data.
   The macro costs `rhema-core` nothing, inherits the approval gate,
   the output caps, and the replay rules of `bash`, and reaches the
   model through a prompt section a frontend controls. A native tool
   would add a resolver, spec plumbing, and a second approval path to
   reach the same text.
2. **Per profile, in the framework's settings section.** Work and
   personal carry different vendors and keys, the same reason
   `mcp_servers` is per profile. No side yaml file in v1: instances
   are written by the UI, and the file's `deny_unknown_fields`
   strictness covers the new keys. A user-authored global file can
   follow the mcp.yaml precedent if hand-editing demand shows.
3. **The chain lives in the base crate, not the frontend.** The
   fallback rules are the feature's behavior; they belong with the
   vocabulary where tests run offline and every consumer gets the
   same policy.
4. **Keys never enter the shell environment.** The function execs the
   binary; the binary resolves the secret store and the file in-process.
   `env` in the agent's shell shows `RHEMA_SEARCH_PROFILE`, never a
   key, and the prompt section says nothing about keys.
5. **A registry crate for one arm per kind.** The alternative,
   `build` in each frontend, is the drift `rhema-provider-registry`
   exists to kill. Four arms now, the fifth vendor is one arm later,
   and the claw gets the same spelling for free.
6. **Sequential fallback, first success wins, empty is success.** The
   chain is a reliability ladder, not a meta-search. Racing bills
   every provider; falling through on zero results bills two
   providers for one honest answer.
7. **`--news` in v1.** Brave, Exa and Tavily map it to one parameter
   or endpoint, and "what happened today" is the query a snippet
   index answers worst without it. OpenCode's hosted route has no
   news index, so the flag leaves its query unchanged; the adapter's
   docs say so. Freshness dates, regions, safe search wait for a
   use.
8. **Markdown, not JSON.** The shell returns text; the numbered list
   is the model's reading shape, and the renderer caps it. A
   `--json` flag would serve callers that do not exist yet.
9. **Env names follow the vendors' own.** `BRAVE_SEARCH_API_KEY`,
   `EXA_API_KEY`, `TAVILY_API_KEY`, `OPENCODE_API_KEY`, the spellings
   their docs and SDKs use, with `BRAVE_API_KEY` accepted as a second
   name. OpenCode's name is the turn provider's, one key for the
   workspace. Config vocabulary stays internal (`brave`, `exa`,
   `tavily`, `opencode`).
10. **Unordered instances are inert.** The order is the whole chain,
    not a priority hint. Silent appends would make a hand edit mean
    something its author did not write.
11. **OpenCode hosted search is a kind beside the vendors.** It is
    Console's hosted search behind a key the user already holds, so it
    joins the chain like the rest, with no new field: the key is
    `OPENCODE_API_KEY`, the instance's provider is `opencode`, and a
    workspace that never enabled hosted search fails as an `Http`
    the chain falls through. The route has no count or news field;
    the caller's count trims the eight results, and `--news` cannot
    narrow it (decision 7).

## 13. Follow-ups

- Page contents: an optional `contents` capability on the trait, one
  adapter at a time, or `lightpanda` guidance if snippets keep
  proving enough.
- A query LRU in the chain, session-scoped, keyed by the query tuple,
  when repeat billing shows up in a usage page.
- A public `rhema websearch` verb set (add, list, remove, order),
  writing the same fields the TUI writes.
- A user-authored global `websearch.yaml` beside `mcp.yaml`, if
  hand-editing demand shows.
- The HTTP host route: the endpoint is a Unix socket today, so HTTP
  needs a TCP endpoint or another transport. Cloud frontends need the
  same route before they can wire the macro.
- Per-instance knobs (safe search, search depth, regions) once a use
  names one.
