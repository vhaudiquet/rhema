# rhema-mcp — the MCP client, wired through bash

> **Status:** v2, 2026-11-03. Supersedes the same-day v1 draft; the
> adversary review's blockers and majors are folded in (§12 records what
> changed and why).
> **Grounding:** SPEC.md §0 keeps MCP out of `rhema-core`; this spec is
> the extension that stays out. `specs/rhema-subagents.md` is the
> extension-crate shape to match. `specs/rhema-sandbox.md` §8 owns the
> copy-in gap this rides beside.
> **One-line promise:** configured MCP servers become bash commands
> (`atlassian create_issue --project ENG`), not fifty advertised tools.
> The model discovers them with `--help`, composes them with pipes and
> `jq`, and the tools array never grows.

## 0. Scope

`rhema-mcp` is an **extension crate**: it sits on the existing seams and
changes nothing underneath. `rhema-core` is untouched; the feature builds
without it and the rest of the tree builds without it. Beside it sits
`mcp-to-cli`, an independent mapping crate with **zero `rhema-*`
dependencies** (§4).

**In v1:**

- `mcp-to-cli`: JSON Schema tool catalogs to clap commands, argv matches
  to JSON args. Publishable standalone.
- `rhema-mcp`, the client: the MCP protocol subset (initialize,
  tools/list, tools/call) over two transports, stdio and Streamable
  HTTP.
- The manager: config to live connections, lazy spawn with single-flight
  connect, respawn on death, the per-server tool catalog, call dispatch.
- The bridge: loopback HTTP over a Unix domain socket, by which the
  manager in the agent process serves `rhema mcp` client processes.
- The provision step: one init file, the env map, and a generated bash
  function per configured server.
- `cli`: the `__server` dispatcher behind the generated functions, plus
  the `mcp` meta verbs (`servers`, `tools`, `schema`, `call`).
- `wire`: the `CodingAgentConfig` helper, the rhema-subagents precedent.
- Config: `<config dir>/mcp.yaml`, user-authored only (§3).
- Prompt disclosure: an "MCP servers" section listing servers, never
  individual tools.

**OUT of v1** (each deliberately out, with the reason):

- ❌ **Native tool advertisement.** Each MCP tool never becomes a
  `ToolSpec`. Ten servers would mean fifty schemas in the tools array
  and in the prompt every step. The array grows by zero.
- ❌ **Project-scope config and its approval machinery.** v1 reads the
  user's own config file and nothing else. This is not the approval
  question decided, it is the project-scope question *deferred together
  with its gate*: a `.rhema/mcp.yaml` in a cloned repo is repo-authored
  process execution, which needs a consent gate, and building the gate
  is not v1 work. The design lesson from review is recorded in §12 so it
  is not relearned.
- ❌ **An interpreter (code mode in JS).** Bash already composes: pipes,
  `jq`, loops, redirection. The bridge API is transport-neutral, so a
  QuickJS-style client can sit on top later without redesign.
- ❌ **Resources, prompts, `tools/list_changed`, sampling, roots,
  elicitation, client-initiated pings.** The client consumes tools only.
  Half-open stdio children are detected by the per-request timeout in
  v1; a ping-based liveness probe is a later addition. All additive.
- ❌ **The GET event channel and stream resume for Streamable HTTP.**
  POST-only: single JSON responses and SSE response streams are handled;
  server-initiated streams are not received. Nothing v1 uses requires
  the GET channel (upstream: the server may answer GETs with 405, and
  POST-only clients are legal).
- ❌ **OAuth for HTTP servers.** Static `headers:` from config only.
- ❌ **Remote and sandboxed shells.** ssh forwards no environment by
  default and the provisioned files do not exist on the remote host.
  Wiring is explicit (§9): a frontend on a non-local executor skips MCP
  wiring entirely, so nothing advertises what cannot run. When the
  executor seam grows environment provisioning, this cut is revisited.
- ❌ **Per-tool approval gating.** Every MCP call arrives as a bash
  command, so the existing per-command approval gate covers it exactly
  like `gh` does, wherever that gate is installed.

## 1. Architecture

Three pieces, two trust boundaries, and a generated function per server:

```
 ┌── agent process ────────────────────────────────────────────────┐
 │  CodingAgent ── ShellTools ── bash -c  …  atlassian() { … }     │
 │  McpManager: owns one McpConnection per configured server       │
 │      (lazy spawn; stdio child or Streamable HTTP client)        │
 │  Bridge: axum over a Unix socket in a 0700 session dir          │
 └───────────────┬─────────────────────────────────────────────────┘
                 │ HTTP + bearer token, loopback only
 ┌───────────────┴─────────────────────────────────────────────────┐
 │  `rhema mcp` (the consumer binary, any process)                 │
 │   __server <name> …   builds a clap command from the catalog,   │
 │                       parses argv, calls, prints JSON           │
 │   mcp <verb> …        the meta surface (§5)                     │
 └─────────────────────────────────────────────────────────────────┘
```

The manager must live in the agent process because MCP servers are
stateful across calls: a playwright browser session, a KiCad connection,
an `npx` cold start that must not repeat. The pool outlives any single
bash command; the bridge is the only IPC hop; the CLI subprocess is a
thin HTTP client. The agent itself never calls the manager in-process;
its only customer is the CLI. That asymmetry is what buys the property
that matters: every MCP call is a real, pipeable bash command, and the
tools array never grows.

## 2. Shell injection

The entry points are bash functions, not files on PATH. Verified facts
(bash 5.3 and Apple's /bin/bash 3.2, both tested, and pinned as tests in
§9):

- Non-interactive bash sources `BASH_ENV`, **including for `-c`**
  commands, not only scripts.
- A function defined there is visible in the `-c` command, in pipelines,
  in `$(...)` substitutions, under `set -eu` bodies, in nested `bash -c`
  inside the command, and in `bash script.sh` children.
- Hyphenated function names work on both bashes (`my-server() { … }`).
- Exported functions import only in the post-Shellshock encoding; the
  legacy `name=() { … }` form is dead. Version sensitivity is why
  `BASH_ENV` wins over function import.
- The rejected alternative, a wrapper string prepended to the user's
  command inside the executor, covers only the one shell it munges: a
  nested `bash -c` or script loses the functions.

`provision()` materializes one init file in a `0700` per-agent temp dir:

```bash
# generated by rhema-mcp; do not edit
[ -n "${RHEMA_INHERITED_BASH_ENV:-}" ] && \
  source "$RHEMA_INHERITED_BASH_ENV" || true
mcp() { "$RHEMA_MCP_BIN" mcp "$@"; }
atlassian() { "$RHEMA_MCP_BIN" __server atlassian "$@"; }
blender()   { "$RHEMA_MCP_BIN" __server blender "$@"; }
```

The inherited file is sourced **first and guarded**: a user file that
sets `set -e` and fails must not abort the init (review-verified failure
mode: without the guard, `mcp` is silently undefined and every command
exits 127). Our definitions follow, so the documented interface wins
name collisions inside agent shells; a user who wants their own
`atlassian` overrides it in config (`command_name`, §3) rather than by
fighting the init file.

The env map, handed to `CodingAgentConfig::shell_env`
(`ShellTools::with_env` already layers it over every command):

| var | value |
|---|---|
| `BASH_ENV` | the init file's absolute path |
| `RHEMA_MCP_BIN` | **the binary copied (hardlinked) into the session dir**, not `current_exe()` (§12) |
| `RHEMA_MCP_BRIDGE` | the socket path |
| `RHEMA_MCP_TOKEN` | a random hex string, per agent |
| `RHEMA_MCP_TMP` | the session temp dir; `media/` lives under it (§5) |
| `RHEMA_INHERITED_BASH_ENV` | the pre-existing `BASH_ENV`, if any |

`RHEMA_MCP_BIN` points at a hardlink into the session dir (copy when the
temp dir sits on another filesystem). A `cargo install` mid-session
renames over the installed file; the link's inode survives, so neither
failure mode review found (exec fails 126/127, or the new binary runs
against the old bridge) can occur. `current_exe()` is only the link
source.

Temp dir deletion on drop is best-effort: the outer bash of an in-flight
command has already sourced the file, but a nested bash spawned after
the drop reads a dead path and exits 127, same degradation class as the
stale-bridge case. Accepted and documented.

Known limits, stated rather than fixed: children that exec instead of
spawn bash (`xargs`, `find -exec`, `sudo` env stripping) cannot call
functions; `while read` loops cover those.

## 3. Configuration

```yaml
# <config dir>/mcp.yaml — user-authored, the only scope in v1
servers:
  atlassian:
    description: Jira and Confluence
    command: ["npx", "-y", "@modelcontextprotocol/server-atlassian"]
    env: { JIRA_TOKEN: "…" }
    # startup_timeout: 60   # handshake seconds; npx cold starts. Default 60.
    # timeout: 120          # per-call seconds. Default 120 (§6).
    # command_name: jira    # override the generated function name
  blender:
    description: Blender scene operations
    url: "http://127.0.0.1:9721/mcp"
    headers: { Authorization: "Bearer …" }
```

- One of `command` (stdio) or `url` (Streamable HTTP), never both;
  `description` is required, it feeds the prompt; `deny_unknown_fields`
  per entry, the rhema-config rule. Server names match `[a-z0-9_-]+`
  (they ride URLs and default the function name); `command_name` must
  match `[a-zA-Z_][a-zA-Z0-9_-]*`, hyphens verified legal in bash 3.2
  and 5.x. The name `mcp` is reserved for the meta surface: a server
  claiming it gets no generated function (a debug notice says so) and is
  reachable through `mcp call`.
- Parsing is per-entry and forgiving of the file, strict of the entry:
  `servers` reads as an ordered list of name/entry pairs folded
  first-wins on duplicate names; a malformed entry or a whole-file parse
  failure is skipped with a debug notice and **never fails agent
  construction**. A dead server is `status: error` with a message, and
  its calls fail in-band.
- Disabling rides the frontends, like `disabled_skills`:
  `CodingAgentConfig` gains `disabled_mcp_servers: BTreeSet<String>`;
  `wire` filters before anything spawns.

**Trust.** The user wrote the file; installing is the approval. Nothing
repo-authored reaches the prompt or spawns a process in v1, which is
what makes "no permissions layer" safe to say out loud (§0, §12).

## 4. `mcp-to-cli`: the mapping contract

An independent workspace member, publishable, that knows nothing about
rhema, MCP, processes, or config:

- **In:** a catalog of `(name, description, input_schema)` triples.
- **Out:** a clap command tree (builder API) and, from parsed matches,
  the JSON args object.
- **Deps:** `clap`, `serde_json`, `thiserror`. Nothing else. The zero-
  `rhema-*` dependency list is the enforcement of independence,
  auditable in every PR; publishing later needs no code changes. The
  name is `mcp-to-cli`, not `mcp-to-bash`: it generates no bash. The
  function stubs are rhema's provision contract and stay in rhema-mcp.

The mapping is conservative, with escape hatches at every complexity
level:

| schema shape | CLI form |
|---|---|
| tool name in `[a-zA-Z0-9_-]` | subcommand |
| tool name outside it, or root schema not an object | omitted from the tree; reachable only via `mcp call` |
| description | subcommand and flag help text |
| string, number | `--field value`, parsed per the schema's type |
| string with `enum` | clap `possible_values`: validation and completion free |
| boolean, required | `--force true|false`, never guessed |
| boolean, optional, default false or none | `--force` flag, presence means true |
| boolean, optional, default true | `--force true|false`, never guessed |
| array of scalars | repeatable: `--tag a --tag b` |
| object, array of objects, anyOf/oneOf/allOf, anything else | `--prop_json '{"a":1}'`, verbatim JSON |
| schema `default` | never pre-filled; an omitted key stays absent, the server applies its own default |
| required, when the property is CLI-mappable | clap `required(true)` |
| whole payload at once | `<tool> --json '{"…"}'` (or `-` for stdin); overrides everything and relaxes the tool's required flags; combining with property flags is a usage error |

Property names pass through verbatim; no case translation to guess
wrong. Constraint keywords (`pattern`, `minimum`, `maximum`, `format`) and
unknown extension keys (`x-mcp-header` shows up in the wild) are ignored:
the server stays the validator. API: `command(&catalog) -> clap::Command`,
`args(&Command, &ArgMatches) -> Result<Value, MappingError>`,
`cli_safe(&str) -> bool`. Bare invocation of a generated command prints
the tool list as JSON (the scripting twin of `--help`).

**Real shapes.** The mapping above was written against three real
servers, fetched 2026-11-03; the JSON here is trimmed from their actual
tool definitions, not invented.

`github/github-mcp-server` publishes 143 tools. 77 of them (54%) carry
at least one `enum` (`state: ["open","closed"]`, `sort` and `order` on
every search tool, `event` on reviews); 28 carry `anyOf`/`oneOf`; 13
carry schema defaults, including a boolean default true; the largest
catalog is 26 properties. Property names are snake_case throughout.
`create_pull_request` is the flat case:

```json
{"required": ["owner", "repo", "title", "head", "base"],
 "properties": {"draft": {"type": "boolean"},
                "reviewers": {"type": "array", "items": {"type": "string"}},
                "maintainer_can_modify": {"type": "boolean"}}}
```

which becomes:

```
github create_pull_request --owner acme --repo api --title "Add /health" \
  --head feature/health --base main --draft --reviewer alice --reviewer bob
```

with clap's help built from the schema descriptions:

```
Usage: github create_pull_request --base <BASE> --head <HEAD> --owner <OWNER>
       --repo <REPO> --title <TITLE> [OPTIONS]

Options:
  --base <BASE>            Branch to merge into (required)
  --draft                  Create as draft PR
  --head <HEAD>            Branch containing changes (required)
  --owner <OWNER>          Repository owner (required)
  --repo <REPO>            Repository name (required)
  --reviewers <REVIEWERS>  GitHub usernames or ORG/team-slug team reviewers
                           to request reviews from (repeatable)
  --title <TITLE>          PR title (required)
```

Requiredness reads in three places. The usage line is synthesized
(`override_usage`): clap's own would render `[OPTIONS]` alone, since
the `--json` override makes every required flag conditional
(`required_unless_present`). Mandatory flags also lead the Options
list (display order) and carry a `(required)` mark in their help
text. The error path lists the missing flags by name when an
invocation omits one. The usage order is schema order — the only
order the schema actually carries; the `required` array's order is
convention, not contract, and is not leaned on.

The boolean that defaults true in the wild (`search_repositories`
`minimal_output`) gets the explicit form, never a guess:

```
github search_repositories --query "org:acme language:rust" --minimal_output false
```

`sooperset/mcp-atlassian` is pydantic-shaped: required scalars with long,
instructive descriptions that flow into help verbatim (`project_key`'s
literally ends "always ask the user"), a `pattern` on the project key,
and the classic escape-hatch property, `additional_fields: dict | str |
None`: anyOf, mapping to exactly one row of the table:

```
atlassian create_issue --project_key PROJ --summary "Fix login" \
  --issue_type Bug --assignee "user@example.com" \
  --additional_fields_json '{"priority": {"name": "High"}}'
```

`ahujasid/blender-mcp` shows the coarse end: `execute_blender_code(code,
user_prompt = "")` and `get_scene_info(user_prompt = "", query = None,
root = None, fields: [str], limit: int = 20)`:

```
blender execute_blender_code --code 'import bpy; print(bpy.context.scene.name)'
blender get_scene_info --query chair --fields location --fields topology --limit 5
```

Schema defaults (`limit: 20`) are never pre-filled: omitting `--limit`
omits the key and the server applies its own 20. Nested quoting is the
shell's job, and bash single quotes carry the Python fine. The same
server's `look` tool returns base64 png blocks, sometimes several at
once (asset preview grids), which makes §5's media-dir contract a
mainline need, not an edge case.

The survey changes no row of the table; it ranks them. The enum row is
the most-exercised by far and earns clap `possible_values` (validation
and completion free). The JSON escape rows cover the anyOf/oneOf
fraction, including deep shapes like `projects_write.updated_field`, a
oneOf of object variants, collapsing to one `--updated_field_json`
flag. All three upstream servers are MIT; trimmed inputSchemas ship as
fixtures in `mcp-to-cli` with attribution, so the mapping matrix runs
against real shapes, not just synthetic ones.

## 5. The generated commands and the `mcp` meta surface

Two surfaces, one JSON-on-stdout rule, distinct exit codes.

**Generated commands** (primary; this is what the prompt teaches):

    atlassian --help                       # the server's tools
    atlassian create_issue --help          # one tool's arguments
    atlassian create_issue --project ENG --title "Fix login"
    blender render --json - < args.json    # raw JSON escape hatch

`__server` fetches the catalog from the manager (`GET /catalog`),
builds the clap tree via `mcp-to-cli`, parses argv, POSTs `/call`,
prints the result envelope. Unknown tool or bad flag: clap's error text
on stderr, exit 2 ("did you mean" included; self-teaching where a JSON
parse error is not).

**Meta verbs** (scripting surface; also the fallback for tools the
mapping omitted):

    mcp servers                  # [{name, status, description, error?}]
    mcp tools <server>           # [{name, description}], no schemas
    mcp schema <server> <tool>   # one tool's raw inputSchema
    mcp call <server> <tool> [args-json]
                                 # argv element, or stdin when omitted or "-"

**Envelope.** On success the server's `CallToolResult` prints as JSON:
text, `resource_link`, and `structuredContent` blocks pass through.
`image` and `audio` blocks are written to `$RHEMA_MCP_TMP/media/<sha256>.<ext>`,
paths substituted into the printed JSON; base64 would blow the bash
tool's 50 KiB output cap on the first screenshot, and the model views
the files through the shell like any file. Extensions come from a fixed
allowlist (`png jpg jpeg gif webp wav mp3 ogg`) mapped from the mimeType;
anything else (or a traversal attempt like `../../x`) becomes a text
placeholder with the byte count, and no file is written.

**Exit codes.** `0` ok; `2` usage: bad flags, unknown tool, unparseable
or oversize args JSON (the bridge caps request bodies at 1 MiB); `3`
unknown server; `4` transport: bridge unreachable, spawn or handshake
failed, server down past respawn; `5` the server reported failure
(JSON-RPC error or `isError: true`); `6` timeout; `1` reserved for
internal faults and never printed by design. Note on `6`: in the default
agent path the shell's own 120s timeout kills the command first, so the
model sees the shell timeout, not this code; `6` is reachable when a
per-server `timeout` is set below the shell cap, or from the user's own
terminal.

**Sizes.** Linux caps one argv element near 128 KiB (`MAX_ARG_STRLEN`,
`E2BIG`), well below the 1 MiB bridge cap: payloads past ~64 KiB ride
stdin (`mcp call … -`, `<tool> --json -`). The prompt's example shows
the stdin form once.

## 6. Manager and connection semantics

- **Lazy connect.** First `tools`, `schema`, `call`, or generated
  command against a server spawns it and runs the handshake; `mcp
  servers` never spawns.
- **Single-flight connect.** A per-server connect lock guards spawn and
  handshake; a caller arriving mid-handshake joins the in-flight result
  instead of spawning a second child. Review-verified race: two
  concurrent calls hitting a dead server's cooldown window must not
  produce two playwright browsers (or two KiCad connections), one of
  which the manager then forgets.
- **Handshake and revision pin.** The client advertises `2025-06-18`
  exactly. The accept-set is **transport-dependent** (review blocker:
  2024-11-05's HTTP transport delivers responses on a GET SSE stream we
  do not open; accepting it there would hang every call to the shell
  timeout):
  - stdio accepts `2025-06-18`, `2025-03-26`, `2024-11-05`;
  - HTTP accepts `2025-06-18` and `2025-03-26`.
  Newer revisions exist (2025-11-25, 2026-07-28 at writing) and are not
  advertised: the pin is deliberate client policy, grown only when a
  revision's deltas are implemented and reviewed, never "latest". A
  server that insists on an unsupported version is disconnected with a
  clear message. `startup_timeout` bounds the handshake (default 60 s).
- **Catalog.** `tools/list` follows pagination cursors to the end,
  cached per connection until death or `mcp tools <server> --refresh`.
  The cache feeds `GET /tools` (names and descriptions only, the
  disclosure boundary) and `GET /catalog` (full inputSchemas, what
  `__server` builds clap from; schemas may enter the CLI process, that
  is its job; they never enter the prompt unsolicited).
  `tools/list_changed` is read and ignored (debug log), per §0.
- **Death and respawn.** A dead server is `status: error` with its
  stderr tail (last 8 KiB, ring buffer). The next call after a 5 s
  cooldown respawns it through the same single-flight lock. In-flight
  calls at death fail in-band, exit 4.
- **Framing discipline.** Stdio servers speak newline-delimited
  JSON-RPC. A stdout line failing to parse, or exceeding 4 MiB, kills
  the connection (status error, stderr tail kept), never the manager:
  the process holding every server's credentials is never an unbounded
  reader, matching the tree's capture-window discipline elsewhere.
- **Cancellation.** A bridge client that disconnects mid-call cancels
  its pending request: dropped from the pending map, and
  `notifications/cancelled` sent best-effort and **unconditionally**
  (review: cancellation is a core utility, there is no capability to
  negotiate). A late result is discarded.
- **Timeouts.** Per-call `timeout`, default 120 s, aligned with the
  bash tool's own cap: a longer call is moot while the invoking shell
  dies first. Config may lower it, never raise it in v1.
- **Streamable HTTP.** POST with `Accept: application/json,
  text/event-stream`; both response shapes handled; `Mcp-Session-Id`
  stored from initialize and resent; `MCP-Protocol-Version` sent on
  every subsequent request; notification POSTs (`notifications/initialized`)
  answered `202 Accepted` with no body and read as success; a `404` on a
  request carrying a session id means the session expired: re-initialize
  once, then fail in-band. `headers:` from config sent verbatim. No GET
  channel, no resume, no OAuth (§0).
- **Secrets.** Config `env` values, `headers` values, and the bridge
  token never appear in error strings, notices, or logs. Server stderr
  tails are the server's own output; they are capped and that is all.

## 7. Prompt disclosure

The renderer gains one section, and it renders **only when MCP is
wired** (§9: a sandboxed frontend wires nothing, so nothing advertises
what cannot run). The template decides placement, like skills and tools.

```markdown
## MCP servers

Configured MCP servers are shell work, not advertised tools. Each
server is a bash command; --help documents it, output is JSON, jq
narrows it.

    atlassian --help                       # a server's tools
    atlassian create_issue --help          # one tool's arguments
    atlassian create_issue --project ENG --title "Fix login"
    blender render --json - < args.json    # raw JSON escape hatch
    mcp servers | mcp tools <server> | mcp call <server> <tool> <json>

Flat arguments are flags; nested values take --prop_json. One line
per server follows.

- atlassian (command: atlassian): Jira and Confluence
- blender (command: blender): Blender scene operations
```

Content comes from config only; descriptions are user-written yaml, and
tool descriptions arrive only as tool output, which this tree treats as
data. An MCP server never gets prose into the system prompt. The
per-server line shows the command name (the shadowing disclosure:
generated commands shadow same-named user commands inside agent shells;
`command_name` resolves a collision honestly).

Byte cost, stated honestly: the tutorial block is ~800 bytes rendered;
each server line is 70 to 110 bytes. Ten servers cost about 2 KiB of
prompt; the same ten advertised natively cost fifteen to twenty-five
kilobytes of schemas every step. `config.system` replacement drops the
section whole, exactly like the skills section.

## 8. Layout

```
crates/mcp-to-cli/
  Cargo.toml        # clap, serde_json, thiserror. No rhema deps.
  src/lib.rs        # command(), args(), cli_safe(), MappingError
  tests/            # the mapping table, row by row

crates/rhema-mcp/
  Cargo.toml        # serde, serde_json, thiserror, tokio, axum, clap;
                    # mcp-to-cli; rhema-config; rhema-coding-agent only
                    # for `wire`
  src/
    lib.rs          # re-exports; #![forbid(unsafe_code)]; module map
    protocol.rs     # MCP types + the subset's JSON-RPC envelope
    client.rs       # one connection: stdio child or HTTP; pending map,
                    # per-request timeouts, line cap, 404-reinit
    manager.rs      # config → connections, single-flight connect,
                    # catalog, dispatch, respawn
    bridge.rs       # axum over the Unix socket; /servers /tools
                    # /catalog /schema /call; token; 1 MiB cap
    provision.rs    # init file, function generation (via mcp-to-cli's
                    # cli_safe), binary hardlink, env map, drop guard
    cli.rs          # __server dispatcher + the mcp meta verbs
    wire.rs         # the CodingAgentConfig helper
  tests/            # fake stdio server by re-exec (§9), end-to-end
```

Dependencies point outward: `rhema-config` owns the yaml types and the
one file read (the config dir is its job; no workspace scan exists in
v1); `rhema-coding-agent` only behind `wire`; never the reverse.
`rhema-core` is not a dependency of either crate. The socket file is
`chmod 0600` after bind (std `PermissionsExt`, no unsafe); the token is
cheap insurance beside the kernel's check.

## 9. How we know it works

1. `cargo fmt --all`, `cargo clippy --workspace --all-targets
   --all-features -- -D warnings`, `cargo test --workspace
   --all-features` stay green at every commit.
2. **The mapping table is a test matrix** in `mcp-to-cli`: every row of
   §4 (enums to possible_values, repeatable arrays, `--prop_json`,
   defaults never pre-filled, `cli_safe` skips, `--json` replacement,
   bare invocation listing), driven through trimmed real schemas
   vendored as fixtures with attribution (github-mcp-server's
   `create_pull_request`, `search_repositories`, `projects_write`
   excerpts; an anyOf `additional_fields`; a defaults-carrying
   get_scene_info stand-in).
3. **Fake server, no node, no network.** Tests re-exec the test binary
   with a script env var: it plays a scripted stdio MCP server
   (handshake, listed tools, canned calls, configurable misbehavior:
   version mismatch, garbage line, oversized line, hang, death, spawn
   marker file for single-flight assertions).
4. **Negotiation**: handshake against each supported revision per
   transport; a mismatch disconnects with the clear message; the
   transport-dependent accept-set is pinned (a canned HTTP server
   replying 2024-11-05 is refused).
5. **Lifecycle**: death mid-call exits 4 and the next call respawns
   after cooldown; two concurrent calls on a dead server produce **one**
   child (the marker file has one line).
6. **Framing**: a 5 MiB single line and a garbage line each kill the
   connection with status error, and the manager stays alive for the
   next call.
7. **The bash facts are pinned, not remembered**: the init file
   provides working functions in `-c`, pipelines, substitutions,
   `set -eu` bodies, nested `bash -c`, and `bash script.sh`, on the
   system bash; a hostile inherited file (`set -e; false`) leaves `mcp`
   defined; a hyphenated function name resolves.
8. **Media**: an image block lands under `$RHEMA_MCP_TMP/media` with an
   allowlisted extension and its path in the JSON; a traversal mimeType
   and an unknown mimeType produce placeholders and no file.
9. **Bridge**: requests without the token are refused; bodies over
   1 MiB are refused; `/tools` carries no schemas and `/catalog` does.
10. **HTTP MUSTs**: the protocol-version header rides the second
    request; a 202 notification POST reads as success; a 404 with a
    session id triggers exactly one re-initialization.
11. **Provision**: the env map is complete; the hardlinked (or copied)
    binary executes; the drop guard removes the dir best-effort.
12. **End-to-end over the mock provider** (wire tests): the model calls
    `bash` with an `mcp` or generated command, the result lands in
    history as any tool result, and the prompt section matches the
    wired config.
13. **Config**: first-wins duplicates, malformed entries and files skip
    with notices, unknown fields reject per entry, `command` xor `url`,
    name and `command_name` validation, the reserved `mcp` name.

## 10. Consumers and wiring: four steps, two frontends

Explicit at the frontends; the driver never discovers servers on its
own.

1. Build the `McpManager` from the config file.
2. Start the bridge it serves (socket path, token).
3. `provision()` → hand the env map to `CodingAgentConfig::shell_env`.
4. Route the subcommand: `rhema mcp …` and `claw mcp …` each delegate to
   `rhema_mcp::cli::run`, which reads the env vars. Outside a wired
   agent shell it exits 4 with an explanation.

**Sandboxed frontends skip wiring entirely**: today that is rhema-code
under a non-local sandbox profile. The claw and the default local shell
wire. ACP and cloudflare-code sessions wire nothing in v1 and carry no
prompt section; stated here so absence is a decision, not an omission.
Review grounding: `ExecRequest` carries no environment today, so a
provisioned env map cannot reach a sandboxed executor anyway; when the
executor seam grows env provisioning, this cut is revisited (§0).

The claw's CLI is flat flags today (`--config`, `--once`, `--mock`);
`claw mcp` is its first subcommand, a deliberate CLI-shape change, and
the README note rides the same commit. Both frontends set
`disabled_mcp_servers` from their stored settings, the
`disabled_skills` mechanism.

## 11. Commit sequence

1. `docs: add the mcp-to-cli and mcp scopes to AGENTS.md`
2. `feat(mcp-to-cli)`: the mapping crate, test matrix riding along
3. `feat(config)`: mcp.yaml types and the single-file read
4. `feat(mcp)`: protocol.rs + client.rs + the fake server
5. `feat(mcp)`: manager.rs (single-flight, framing cap, catalog)
6. `feat(mcp)`: bridge.rs + provision.rs + cli.rs
7. `feat(agent-tools)`: the prompt section parameter;
   `feat(agent)`: `disabled_mcp_servers` + section wiring (the
   end-to-end wire tests land here, with the parameter they need)
8. `feat(rhema-code)`: the `rhema mcp` route + frontend wiring, README
   section riding the same commit (user-facing, per AGENTS.md)
9. `feat(claw)`: the `claw mcp` route + wiring, README note riding
10. `docs`: link the spec; retire rhema-code README's "no MCPs" line
    (that line's removal belongs to the feature commits 8–9 if they
    land together; this commit is the cross-links)

## 12. Decisions

- **Extension crates, `rhema-core` untouched.** SPEC.md §0 kept MCP out
  of the core on purpose; `rhema-core` is not a dependency of either
  crate.
- **`mcp-to-cli` as a workspace member with zero `rhema-*` deps**, not
  a separate repo consumed as a moving pin: independence is enforced by
  the dependency list, auditable per PR, and the crate publishes later
  with no code changes. The standalone-project ambition survives the
  shape; a future `mcp-to-cli` binary (config in, shell stubs out) is
  an addition there, not a refactor here.
- **Generated per-server commands over a god CLI.** The model writes
  flags (`atlassian create_issue --project ENG`), a form it has seen
  millions of, not bespoke JSON-in-bash quoting. The god CLI stays as
  meta surface and escape hatch. Mapping is conservative with
  `--prop_json` and `--json` at every complexity level; schemas are
  arbitrary, so the mapping guarantees a path for anything, and clever
  handling of nested shapes is explicitly not v1.
- **Hand-rolled protocol and client**, over `rmcp` and jsonrpsee.
  Stated honestly after review: `rmcp` (the official SDK) ships stdio
  and Streamable HTTP client transports, client-side SSE, and auth, and
  would provide maintained negotiation, the 404-reinit dance, and auth
  flows for free. Rejected anyway: it owns the service lifecycle in its
  own tower-shaped terms inside `manager.rs`, drags schemars, and pins
  the negotiation policy to its release cadence. jsonrpsee has no stdio
  transport and would bring hyper and tower to replace a pending map.
  Reversal touches `protocol.rs` and `client.rs` only.
- **`BASH_ENV` injection**, over command-string munging (loses nested
  shells) and exported functions (post-Shellshock encoding only).
  Guarded source order per review; every fact pinned as a test.
- **Functions, never PATH executables.** Nothing to distribute, one
  temp dir to clean; the xargs limit is accepted and documented.
- **Hardlink-or-copy the binary into the session dir**, over
  referencing `current_exe()`: a mid-session `cargo install` renames
  the file out from under the env var (exec 126/127) or slides a new
  binary against the old bridge; a link to the old inode survives both.
- **Manager in the agent process; bridge over a Unix socket + token.**
  Stateful servers must outlive bash commands; UDS beats loopback TCP
  locally on every axis that matters (no port to pick, kernel-enforced
  connect permissions; `curl --unix-socket` debugs it fine).
- **Global-only config, no approval layer.** The user wrote the file;
  install is the approval. The project-scope question is deferred
  together with its gate, and the review lesson is recorded for that
  day: env values are executable input (`NODE_OPTIONS`, `LD_PRELOAD`,
  `BASH_ENV` in a server env map turn an approved command into
  arbitrary code), so any future fingerprint covers the **full env
  map, values included**, not keys.
- **Progressive disclosure by default.** The tools array never grows;
  the prompt lists servers; schemas travel only to the CLI process on
  demand; the four meta verbs and `--help` are the discovery story.
- **stdio + Streamable HTTP; the revision pin is client policy** with a
  transport-dependent accept-set; everything else additive later, each
  cut per-feature in §0 so it can land without touching this design.
